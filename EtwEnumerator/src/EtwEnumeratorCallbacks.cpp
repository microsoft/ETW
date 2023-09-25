// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License.

#include "stdafx.h"
#include <EtwEnumerator.h>

LSTATUS __stdcall
EtwEnumeratorCallbacks::OnPreviewEvent(
    _In_ EVENT_RECORD const* pEventRecord,
    EtwEventCategory eventCategory) noexcept
{
    UNREFERENCED_PARAMETER(pEventRecord);
    UNREFERENCED_PARAMETER(eventCategory);
    return ERROR_SUCCESS;
}

LSTATUS __stdcall
EtwEnumeratorCallbacks::GetEventInformation(
    _In_ EVENT_RECORD const* pEvent,
    _In_ ULONG cTdhContext,
    _In_reads_opt_(cTdhContext) TDH_CONTEXT const* pTdhContext,
    _Out_writes_bytes_opt_(*pcbBuffer) TRACE_EVENT_INFO* pBuffer,
    _Inout_ ULONG* pcbBuffer) noexcept
{
    LSTATUS status = TdhGetEventInformation(
        const_cast<EVENT_RECORD*>(pEvent),
        cTdhContext,
        const_cast<TDH_CONTEXT*>(pTdhContext),
        pBuffer,
        pcbBuffer);
    return status;
}

LSTATUS __stdcall
EtwEnumeratorCallbacks::GetEventMapInformation(
    _In_ EVENT_RECORD const* pEvent,
    _In_ EtwPCWSTR pMapName,
    _Out_writes_bytes_opt_(*pcbBuffer) EVENT_MAP_INFO* pBuffer,
    _Inout_ ULONG *pcbBuffer) noexcept
{
    LSTATUS status = TdhGetEventMapInformation(
        const_cast<EVENT_RECORD*>(pEvent),
        const_cast<PWSTR>(pMapName),
        pBuffer,
        pcbBuffer);
    return status;
}

LSTATUS __stdcall
EtwEnumeratorCallbacks::GetParameterMessage(
    _In_ EVENT_RECORD const* pEvent,
    ULONG messageId,
    EtwStringBuilder& parameterMessageBuilder) noexcept
{
    UNREFERENCED_PARAMETER(pEvent);
    UNREFERENCED_PARAMETER(messageId);
    UNREFERENCED_PARAMETER(parameterMessageBuilder);

    // FormatCurrentEvent will fall back to JSON formatting.
    // FormatCurrentEventWithMessage will fail.
    return ERROR_MR_MID_NOT_FOUND;
}

LSTATUS __stdcall
EtwEnumeratorCallbacks::FormatResultCodeValue(
    ResultCodeDomain domain,
    UnderlyingType valueType,
    ULONG value,
    EtwStringBuilder& resultCodeBuilder) noexcept
{
    LSTATUS status;
    DWORD flags = FORMAT_MESSAGE_ALLOCATE_BUFFER | FORMAT_MESSAGE_IGNORE_INSERTS | FORMAT_MESSAGE_FROM_SYSTEM;
    HMODULE hModule = nullptr;
    PCWSTR szDomain;
    ULONG lookupCode;
    PWSTR pMessage;

    switch (domain)
    {
    case ResultCodeDomainWIN32:
        szDomain = L"WIN";
        lookupCode = value;
        goto LookupFromSystem;

    case ResultCodeDomainHRESULT:
        szDomain = L"HR";
        if (value & FACILITY_NT_BIT)
        {
            lookupCode = value & ~FACILITY_NT_BIT;
            goto LookupFromNtdll;
        }
        else
        {
            lookupCode = value;
            goto LookupFromSystem;
        }

    case ResultCodeDomainNTSTATUS:
        szDomain = L"NT";
        lookupCode = value;
        goto LookupFromNtdll;

    default:
        szDomain = L"ERR";
        goto LookupUnknown;
    }

LookupFromNtdll:

    flags = FORMAT_MESSAGE_ALLOCATE_BUFFER | FORMAT_MESSAGE_IGNORE_INSERTS | FORMAT_MESSAGE_FROM_HMODULE;
    hModule = GetModuleHandleW(L"ntdll.dll");
    if (!hModule)
    {
        goto LookupUnknown;
    }

LookupFromSystem:

    pMessage = nullptr;
    FormatMessageW(
        flags,
        hModule,
        lookupCode,
        0,
        reinterpret_cast<PWSTR>(&pMessage),
        0,
        nullptr);

    if (pMessage == nullptr)
    {
    LookupUnknown:

        // e.g. "0x80070002(??)"
        status = resultCodeBuilder.AppendPrintf(
            valueType == UnderlyingTypeHexadecimal
            ? L"0x%lX(%ls=??)"
            :   L"%lu(%ls=??)",
            value, szDomain);
    }
    else
    {
        PCWSTR szTrimmed;
        if (pMessage[0] == '{')
        {
            // Message starts with '{', so search to '}' or newline, whichever comes first.
            // This is common for NTSTATUS messages.
            size_t i = 1;
            while (
                pMessage[i] != 0 &&
                pMessage[i] != '}' &&
                pMessage[i] != '\r' &&
                pMessage[i] != '\n')
            {
                i += 1;
            }

            if (pMessage[i] == '}')
            {
                // Found matching '}', so skip the initial '{'.
                szTrimmed = pMessage + 1;
            }
            else
            {
                // Didn't find a matching '}', so keep the initial '{'.
                szTrimmed = pMessage;
            }

            pMessage[i] = 0;
        }
        else
        {
            size_t i = 0;
            while (
                pMessage[i] != 0 &&
                pMessage[i] != '\r' &&
                pMessage[i] != '\n')
            {
                i += 1;
            }

            szTrimmed = pMessage;
            pMessage[i] = 0;
        }

        // e.g. "HResult Error 0x80070002: The system cannot find the file specified."
        status = resultCodeBuilder.AppendPrintf(
            valueType == UnderlyingTypeHexadecimal
            ? L"0x%lX(%ls=%ls)"
            :   L"%lu(%ls=%ls)",
            value, szDomain, szTrimmed);
        HeapFree(GetProcessHeap(), 0, pMessage);
    }

    return status;
}

static PCWSTR
MapString(
    _In_ EVENT_MAP_INFO const* pMapInfo,
    ULONG index) noexcept
{
    return reinterpret_cast<PCWSTR>(
        reinterpret_cast<const BYTE*>(pMapInfo) + pMapInfo->MapEntryArray[index].OutputOffset);
}

static int
MapStringLen(PCWSTR szMapString)
{
    size_t i = wcslen(szMapString);
    while (i != 0 && szMapString[i - 1] == ' ')
    {
        i -= 1;
    }

    return static_cast<int>(i);
}

LSTATUS __stdcall
EtwEnumeratorCallbacks::FormatMapValue(
    _In_ EVENT_MAP_INFO const* pMapInfo,
    UnderlyingType valueType,
    ULONG value,
    EtwStringBuilder& mapBuilder) noexcept
{
    LSTATUS status = ERROR_NOT_FOUND;
    ULONG matchedBits;
    PCWSTR szMap;

    switch (pMapInfo->Flag & ~EVENTMAP_INFO_FLAG_WBEM_NO_MAP)
    {
    case EVENTMAP_INFO_FLAG_MANIFEST_VALUEMAP:
    case EVENTMAP_INFO_FLAG_WBEM_VALUEMAP:

        // Valuemap:
        if (pMapInfo->Flag & EVENTMAP_INFO_FLAG_WBEM_NO_MAP)
        {
            // Indexed value map:
            if (value < pMapInfo->EntryCount)
            {
                ASSERT(pMapInfo->MapEntryArray[value].Value == value);
                szMap = MapString(pMapInfo, value);
                status = mapBuilder.AppendPrintf(
                    valueType == UnderlyingTypeHexadecimal
                    ? L"0x%lX(%.*ls)"
                    :   L"%lu(%.*ls)",
                    value, MapStringLen(szMap), szMap);
                goto Done;
            }
        }
        else
        {
            // Normal valuemap:
            for (ULONG i = 0; i != pMapInfo->EntryCount; i += 1)
            {
                if (pMapInfo->MapEntryArray[i].Value == value)
                {
                    szMap = MapString(pMapInfo, i);
                    status = mapBuilder.AppendPrintf(
                        valueType == UnderlyingTypeHexadecimal
                        ? L"0x%lX(%.*ls)"
                        :   L"%lu(%.*ls)",
                        value, MapStringLen(szMap), szMap);
                    goto Done;
                }
            }
        }

        status = mapBuilder.AppendPrintf(
            valueType == UnderlyingTypeHexadecimal
            ? L"0x%lX(??)"
            :   L"%lu(??)",
            value);
        goto Done;

    case EVENTMAP_INFO_FLAG_MANIFEST_BITMAP:
    case EVENTMAP_INFO_FLAG_WBEM_BITMAP:
    case EVENTMAP_INFO_FLAG_WBEM_VALUEMAP | EVENTMAP_INFO_FLAG_WBEM_FLAG:

        // Bitmap:
        matchedBits = 0;

        for (ULONG i = 0; i != pMapInfo->EntryCount; i += 1)
        {
            ULONG const mask = pMapInfo->MapEntryArray[i].Value;
            if ((value & mask) == mask)
            {
                if (mask != 0 || value == 0)
                {
                    szMap = MapString(pMapInfo, i);
                    status = status == ERROR_SUCCESS
                        ? mapBuilder.AppendPrintf(
                            L",%.*ls",
                            MapStringLen(szMap), szMap)
                        : mapBuilder.AppendPrintf(
                            valueType == UnderlyingTypeHexadecimal
                            ? L"0x%lX[%.*ls"
                            :   L"%lu[%.*ls",
                            value, MapStringLen(szMap), szMap);
                    if (status != ERROR_SUCCESS)
                    {
                        break;
                    }

                    matchedBits |= mask;
                }
            }
        }

        if (status != ERROR_SUCCESS)
        {
            // Nothing matched.
            PCWSTR szItemName = value == 0
                ? L"" // If value was zero, just show an empty set.
                : L"??"; // If value was nonzero, indicate no match.
            status = mapBuilder.AppendPrintf(
                valueType == UnderlyingTypeHexadecimal
                ? L"0x%lX[%ls]"
                :   L"%lu[%ls]",
                value, szItemName);
        }
        else if (matchedBits != value)
        {
            // Something matched, but some unused bits were left over.
            status = mapBuilder.AppendPrintf(L",0x%lX]", value ^ matchedBits);
        }
        else
        {
            // Everything matched.
            status = mapBuilder.AppendChar(L']');
        }
        goto Done;
    }

Done:

    return status;
}
