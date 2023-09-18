// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License.

#include "stdafx.h"
#include <EtwEnumerator.h>
#include "EtwBuffer.inl"
#include <ws2def.h>
#include <ws2ipdef.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>

/*
This code is in a separate file so that if the user doesn't use the Format
functions they won't need to link against the APIs we use here (_vsnwprintf,
MultiByteToWideChar).
*/

#pragma region Local definitions

// Macros for some recently-defined constants so that this can compile
// using an older Windows SDK.
#define TDH_InTypeManifestCountedString        22 // TDH_INTYPE_MANIFEST_COUNTEDSTRING
#define TDH_InTypeManifestCountedAnsiString    23 // TDH_INTYPE_MANIFEST_COUNTEDANSISTRING
#define TDH_InTypeManifestCountedBinary        25 // TDH_INTYPE_MANIFEST_COUNTEDBINARY
#define TDH_OutTypeCodePointer                 static_cast<_TDH_OUT_TYPE>(37) // TDH_OUTTYPE_CODE_POINTER
#define TDH_OutTypeDateTimeUtc                 static_cast<_TDH_OUT_TYPE>(38) // TDH_OUTTYPE_DATETIME_UTC

#define GUID_PRINTF_VALUE(g) \
    (g).Data1, (g).Data2, (g).Data3, (g).Data4[0], (g).Data4[1], (g).Data4[2], \
    (g).Data4[3], (g).Data4[4], (g).Data4[5], (g).Data4[6], (g).Data4[7]
#define GUID_PRINTF_FORMAT_LOWER  L"%08x-%04x-%04x-%02x%02x-%02x%02x%02x%02x%02x%02x"
#define GUID_PRINTF_FORMAT_UPPER  L"%08X-%04X-%04X-%02X%02X-%02X%02X%02X%02X%02X%02X"

// if OP != ERROR_SUCCESS then set status = result_of_OP and goto Done.
#define CheckWin32(status, op) \
    { \
    LSTATUS const tmpStatus = VerifyLstatus((op)); \
    if (ERROR_SUCCESS != tmpStatus) { status = tmpStatus; goto Done; } \
    } \

// if OP == false then set status = ERROR_OUTOFMEMORY and goto Done.
#define CheckOutOfMem(status, op) \
    { \
    if (!VerifyBool((op))) { status = ERROR_OUTOFMEMORY; goto Done; } \
    } \

// if OP == false then goto Done.
#define CheckAdd(op) \
    { \
    if (!VerifyBool((op))) { goto Done; } \
    } \

// Try to flag a compile error for CheckWin32(anything other than LSTATUS).
static __forceinline LSTATUS VerifyLstatus(LSTATUS op) { return op; }
static LSTATUS VerifyLstatus(int op) = delete;

// Try to flag a compile error for CheckOutOfMem(anything other than bool).
static __forceinline bool VerifyBool(bool op) { return op; }
static bool VerifyBool(int op) = delete;

static char const* const UppercaseHexChars = "0123456789ABCDEF";
static char const* const LowercaseHexChars = "0123456789abcdef";

using Buffer = EtwInternal::Buffer<wchar_t>;

static bool
LowercaseHexMatches(UINT8 num, _In_reads_(2) wchar_t const* pStr)
{
    return
        pStr[0] == LowercaseHexChars[num >> 4] &&
        pStr[1] == LowercaseHexChars[num & 15];
}

/*
Given the provider GUID and the provider name, returns the length of the
provider name that we should display. For normal provider names, returns
wcslen(szProviderName). For provider names that have a _providerGuid suffix,
returns the number of characters before the suffix.
*/
static USHORT
ProviderNameLength(
    GUID const& providerId,
    _In_z_ LPCWSTR szProviderName) noexcept
{
    USHORT cchProviderName = static_cast<USHORT>(wcslen(szProviderName));

    // Trim off the ProviderId suffix, if present.
    if (cchProviderName > 33 &&
        szProviderName[cchProviderName - 33] == L'_')
    {
        auto const pNum = reinterpret_cast<UINT8 const*>(&providerId);
        auto const pStr = szProviderName + (cchProviderName - 32);
        if (LowercaseHexMatches(pNum[3], pStr + 0) &&
            LowercaseHexMatches(pNum[2], pStr + 2) &&
            LowercaseHexMatches(pNum[1], pStr + 4) &&
            LowercaseHexMatches(pNum[0], pStr + 6) &&
            LowercaseHexMatches(pNum[5], pStr + 8) &&
            LowercaseHexMatches(pNum[4], pStr + 10) &&
            LowercaseHexMatches(pNum[7], pStr + 12) &&
            LowercaseHexMatches(pNum[6], pStr + 14) &&
            LowercaseHexMatches(pNum[8], pStr + 16) &&
            LowercaseHexMatches(pNum[9], pStr + 18) &&
            LowercaseHexMatches(pNum[10], pStr + 20) &&
            LowercaseHexMatches(pNum[11], pStr + 22) &&
            LowercaseHexMatches(pNum[12], pStr + 24) &&
            LowercaseHexMatches(pNum[13], pStr + 26) &&
            LowercaseHexMatches(pNum[14], pStr + 28) &&
            LowercaseHexMatches(pNum[15], pStr + 30))
        {
            cchProviderName -= 33;
        }
    }

    return cchProviderName;
}

#pragma endregion

#pragma region Static Append functions

static LSTATUS
AppendPrintfV(
    Buffer& output,
    _Printf_format_string_ LPCWSTR szFormat,
    va_list args) noexcept
{
    LSTATUS status;

    auto const oldSize = output.size();
    for (;;)
    {
        auto const availableCapacity = output.capacity() - oldSize;

#pragma warning(push)
#pragma warning(disable: 4996 28719) // We're ok with failure to nul-terminate.
        int const result = _vsnwprintf(
            output.data() + oldSize,
            availableCapacity,
            szFormat,
            args);
#pragma warning(pop)

        if (result >= 0)
        {
            ASSERT(static_cast<unsigned>(result) <= availableCapacity);
            output.resize_unchecked(result + oldSize);
            status = ERROR_SUCCESS;
            break;
        }
        else if (!output.reserve(output.capacity() + 100))
        {
            status = ERROR_OUTOFMEMORY;
            break;
        }
    }

    return status;
}

static LSTATUS
AppendPrintf(
    Buffer& output,
    _Printf_format_string_ LPCWSTR szFormat,
    ...) noexcept
{
    LSTATUS status;
    va_list args;
    va_start(args, szFormat);
    status = AppendPrintfV(output, szFormat, args);
    va_end(args);
    return status;
}

static LSTATUS
AppendMbcs(
    Buffer& output,
    _In_reads_(cb) LPCCH pb,
    int cb,
    UINT codepage) noexcept
{
    ASSERT(cb >= 0);

    LSTATUS status;

    if (cb <= 0)
    {
        status = ERROR_SUCCESS;
    }
    else
    {
        auto const oldSize = output.size();
        (void)output.reserve(cb + oldSize); // Estimate needed size and reserve it.
        for (;;)
        {
            int result;
            auto const availableCapacity = output.capacity() - oldSize;

            result = MultiByteToWideChar(codepage, 0, pb, cb, output.data() + oldSize, availableCapacity);
            ASSERT(result >= 0);
            ASSERT(static_cast<unsigned>(result) <= availableCapacity);
            if (result != 0)
            {
                output.resize_unchecked(result + oldSize);
                status = ERROR_SUCCESS;
                break;
            }

            status = GetLastError();
            if (status != ERROR_INSUFFICIENT_BUFFER)
            {
                ASSERT(status != ERROR_SUCCESS);
                break;
            }

            result = MultiByteToWideChar(codepage, 0, pb, cb, nullptr, 0);
            ASSERT(result >= 0);
            if (result == 0)
            {
                status = GetLastError();
                ASSERT(status != ERROR_SUCCESS);
                break;
            }

            ASSERT(static_cast<unsigned>(result) > availableCapacity);
            if (!output.reserve(result + oldSize))
            {
                status = ERROR_OUTOFMEMORY;
                break;
            }
        }
    }

    return status;
}

static LSTATUS
AppendWide(
    Buffer& output,
    _In_reads_(cch) LPCWCH pch,
    unsigned cch) noexcept
{
    LSTATUS status;

    auto const oldSize = output.size();
    if (!output.resize(cch + oldSize))
    {
        status = ERROR_OUTOFMEMORY;
    }
    else
    {
        memcpy(output.data() + oldSize, pch, cch * 2);
        status = ERROR_SUCCESS;
    }

    return status;
}

static LSTATUS
AppendWide(
    Buffer& output,
    _In_z_ LPCWSTR sz) noexcept
{
    return AppendWide(output, sz, static_cast<unsigned>(wcslen(sz)));
}

template<unsigned N>
static LSTATUS
AppendLiteral(
    Buffer& output,
    wchar_t const (&stringLiteral)[N])
{
    return AppendWide(output, stringLiteral, N - 1);
}

static LSTATUS
AppendBoolean(
    Buffer& output,
    int value) noexcept
{
    return value != 0
        ? AppendLiteral(output, L"true")
        : AppendLiteral(output, L"false");
}

static LSTATUS
AppendHexDump(
    Buffer& output,
    _In_reads_bytes_(cbData) void const* pData,
    unsigned cbData) noexcept
{
    LSTATUS status;

    auto const oldSize = output.size();
    if (!output.resize(2 + cbData * 2 + oldSize))
    {
        status = ERROR_OUTOFMEMORY;
    }
    else
    {
        wchar_t* pch = output.data() + oldSize;
        *pch++ = L'0';
        *pch++ = L'x';
        for (unsigned i = 0; i != cbData; i++)
        {
            UINT8 val = static_cast<UINT8 const*>(pData)[i];
            pch[i * 2 + 0] = UppercaseHexChars[val >> 4];
            pch[i * 2 + 1] = UppercaseHexChars[val & 0xf];
        }

        status = ERROR_SUCCESS;
    }

    return status;
}

static LSTATUS
AppendAdjustedSystemTime(
    Buffer& output,
    SYSTEMTIME const& st,
    EtwTimestampFormat format,
    int timeZoneBiasMinutes,
    unsigned subseconds,
    unsigned subsecondsDigits)
{
    LSTATUS status;

    if ((format & EtwTimestampFormat_TypeMask) == EtwTimestampFormat_Wpp)
    {
        // WPP-style.
        status = AppendPrintf(output, L"%02u/%02u/%04u-%02u:%02u:%02u.%0*u",
            st.wMonth, st.wDay, st.wYear,
            st.wHour, st.wMinute, st.wSecond,
            subsecondsDigits, subseconds);
    }
    else
    {
        // rfc3339-style.
        status = AppendPrintf(output, L"%04u-%02u-%02uT%02u:%02u:%02u.%0*u",
            st.wYear, st.wMonth, st.wDay,
            st.wHour, st.wMinute, st.wSecond,
            subsecondsDigits, subseconds);
    }

    if (status != ERROR_SUCCESS ||
        (format & EtwTimestampFormat_NoTimeZoneSuffix))
    {
        // No suffix.
    }
    else if (format & EtwTimestampFormat_Local)
    {
        // "+HH:MM" suffix.
        unsigned const absBiasMinutes =
            timeZoneBiasMinutes < 0
            ? -timeZoneBiasMinutes
            : timeZoneBiasMinutes;
        status = AppendPrintf(output,
            L"%lc%02u:%02u",
            timeZoneBiasMinutes < 0 ? L'-' : L'+',
            absBiasMinutes / 60, absBiasMinutes % 60);
    }
    else if (!output.push_back(L'Z'))
    {
        status = ERROR_OUTOFMEMORY;
    }

    return status;
}

static LSTATUS
AppendFileTime(
    Buffer& output,
    UINT64 fileTime,
    EtwTimestampFormat format,
    int timeZoneBiasMinutes,
    bool timeIsUtc)
{
    UINT64 fileTimeAdjusted;
    EtwTimestampFormat formatAdjusted = format;

    if (!timeIsUtc)
    {
        // fileTime is unknown time zone, no conversion possible, no time zone suffix.
        fileTimeAdjusted = fileTime;
        formatAdjusted =
            static_cast<EtwTimestampFormat>(formatAdjusted | EtwTimestampFormat_NoTimeZoneSuffix);
    }
    else if (!(formatAdjusted & EtwTimestampFormat_Local))
    {
        // fileTime is UTC, no conversion wanted.
        fileTimeAdjusted = fileTime;
    }
    else
    {
        // fileTime is UTC, user wants it converted to local time.
        fileTimeAdjusted = EtwEnumerator::AdjustFileTime(fileTime, timeZoneBiasMinutes);
    }

    SYSTEMTIME systemTimeAdjusted = {};
    FileTimeToSystemTime(
        reinterpret_cast<FILETIME const*>(&fileTimeAdjusted), &systemTimeAdjusted);

    unsigned subseconds;
    unsigned subsecondsDigits;
    if (formatAdjusted & EtwTimestampFormat_LowPrecision)
    {
        subseconds = systemTimeAdjusted.wMilliseconds;
        subsecondsDigits = 3;
    }
    else
    {
        subseconds = static_cast<unsigned>(fileTimeAdjusted % 10000000u);
        subsecondsDigits = 7;
    }

    LSTATUS status = AppendAdjustedSystemTime(
        output, systemTimeAdjusted, formatAdjusted,
        timeZoneBiasMinutes, subseconds, subsecondsDigits);
    return status;
}

static LSTATUS
AppendSystemTime(
    Buffer& output,
    SYSTEMTIME systemTime,
    EtwTimestampFormat format,
    int timeZoneBiasMinutes,
    bool timeIsUtc)
{
    SYSTEMTIME systemTimeAdjusted;
    EtwTimestampFormat formatAdjusted = format;

    if (!timeIsUtc)
    {
        // systemTime is unknown time zone, no conversion possible, no time zone suffix.
        systemTimeAdjusted = systemTime;
        formatAdjusted =
            static_cast<EtwTimestampFormat>(formatAdjusted | EtwTimestampFormat_NoTimeZoneSuffix);
    }
    else if (!(formatAdjusted & EtwTimestampFormat_Local))
    {
        // systemTime is UTC, no conversion wanted.
        systemTimeAdjusted = systemTime;
    }
    else
    {
        // systemTime is UTC, user wants it converted to local time.
        UINT64 fileTime = {};
        SystemTimeToFileTime(&systemTime, reinterpret_cast<FILETIME*>(&fileTime));
        UINT64 const fileTimeAdjusted =
            EtwEnumerator::AdjustFileTime(fileTime, timeZoneBiasMinutes);

        systemTimeAdjusted = {};
        FileTimeToSystemTime(
            reinterpret_cast<FILETIME const*>(&fileTimeAdjusted), &systemTimeAdjusted);
    }

    LSTATUS status = AppendAdjustedSystemTime(
        output, systemTimeAdjusted, formatAdjusted,
        timeZoneBiasMinutes, systemTimeAdjusted.wMilliseconds, 3);
    return status;
}

static LSTATUS
AppendIPv4(
    Buffer& output,
    _In_reads_bytes_(4) void const* pData) noexcept
{
    auto pb = static_cast<BYTE const*>(pData);
    return AppendPrintf(output, L"%u.%u.%u.%u",
        pb[0], pb[1], pb[2], pb[3]);
}

static LSTATUS
AppendIPv6(
    Buffer& output,
    _In_reads_bytes_(16) void const* pData) noexcept
{
    LSTATUS status;

    auto const oldSize = output.size();
    if (!output.reserve(46 + oldSize))
    {
        status = ERROR_OUTOFMEMORY;
    }
    else
    {
        union AddressParts {
            UINT8  u8[16];
            UINT16 u16[8];
            UINT32 u32[4];
        };

        auto& ap = *static_cast<AddressParts const UNALIGNED*>(pData);

        unsigned endHex;
        unsigned maxFirst, maxLen;
        unsigned curFirst, curLen;

        // Check special cases:
        // All zeros, IPv4-compatible, IPv4-mapped, or IPv4-translated.
        if ((ap.u32[0] | ap.u32[1]) == 0)
        {
            // Starts with 0:0:0:0
            if ((ap.u32[2] | ap.u32[3]) == 0)
            {
                // Starts with 0:0:0:0:0:0:0:0 - All zeros.
                (void)AppendLiteral(output, L"::");
                status = ERROR_SUCCESS;
                goto Done;
            }
            else if (ap.u16[6] != 0)
            {
                switch (ap.u32[2])
                {
                case 0x00000000:
                    // Starts with 0:0:0:0:0:0 - IPv4-compatible.
                    status = AppendPrintf(output, L"::%u.%u.%u.%u",
                        ap.u8[12], ap.u8[13], ap.u8[14], ap.u8[15]);
                    goto Done;
                case 0xffff0000:
                    // Starts with 0:0:0:0:0:ffff - IPv4-mapped.
                    status = AppendPrintf(output, L"::ffff:%u.%u.%u.%u",
                        ap.u8[12], ap.u8[13], ap.u8[14], ap.u8[15]);
                    goto Done;
                case 0x0000ffff:
                    // Starts with 0:0:0:0:ffff:0 - IPv4-translated.
                    status = AppendPrintf(output, L"::ffff:0:%u.%u.%u.%u",
                        ap.u8[12], ap.u8[13], ap.u8[14], ap.u8[15]);
                    goto Done;
                }
            }
        }

        endHex = (ap.u32[2] & 0xfffffffd) == 0xfe5e0000 // Is middle 0000:5EFE or 0200:5EFE?
            ? 6  // Yes - format last 4 bytes as IPv4 (ISATAP EUI-64).
            : 8; // No - hex digits all the way to the end.

        // Find longest substring of zeroes.
        maxFirst = endHex;
        maxLen = 1; // Pretend we've already found a substring of length 1 after the end.
        curFirst = 0;
        curLen = 0;
        for (unsigned i = 0; i != endHex; i++)
        {
            if (ap.u16[i] == 0)
            {
                curLen += 1;
                if (curLen > maxLen)
                {
                    maxFirst = curFirst;
                    maxLen = curLen;
                }
            }
            else
            {
                // Start a new substring
                curFirst = i + 1;
                curLen = 0;
            }
        }

        // Handle up to the first substring of zeros.

        if (maxFirst != 0)
        {
            // Does not start with a substring of zeros.
            (void)AppendPrintf(output, L"%x",
                _byteswap_ushort(ap.u16[0]));

            for (unsigned i = 1; i != maxFirst; i++)
            {
                (void)AppendPrintf(output, L":%x",
                    _byteswap_ushort(ap.u16[i]));
            }
        }

        if (maxFirst + maxLen < endHex)
        {
            // Has a substring of zeros not at the end.
            // Handle after the first substring of zeros.
            unsigned i = maxFirst + maxLen;
            (void)AppendPrintf(output, L"::%x",
                _byteswap_ushort(ap.u16[i]));
            for (i++; i != endHex; i++)
            {
                (void)AppendPrintf(output, L":%x",
                    _byteswap_ushort(ap.u16[i]));
            }
        }
        else if (maxFirst != endHex)
        {
            // Ends with a substring of zeros.
            (void)AppendLiteral(output, L"::");
        }

        if (endHex == 6)
        {
            (void)AppendPrintf(output, L":%u.%u.%u.%u",
                ap.u8[12], ap.u8[13], ap.u8[14], ap.u8[15]);
        }

        status = ERROR_SUCCESS;
    }

Done:

    return status;
}

static LSTATUS
AppendSockAddr(
    Buffer& output,
    _In_reads_bytes_(cbData) void const* pData,
    unsigned cbData) noexcept
{
    unsigned const SizeOfInet4ThroughAddr = offsetof(sockaddr_in, sin_zero);
    unsigned const SizeOfInet6ThroughAddr = offsetof(sockaddr_in6, sin6_scope_id);
    unsigned const SizeOfInet6ThroughScope = SizeOfInet6ThroughAddr + 4;

    LSTATUS status;

    if (cbData >= 2)
    {
        switch (static_cast<sockaddr const UNALIGNED*>(pData)->sa_family)
        {
        case AF_INET:

            if (cbData >= SizeOfInet4ThroughAddr)
            {
                auto const pSock = static_cast<sockaddr_in const UNALIGNED*>(pData);

                CheckWin32(status, AppendIPv4(output, &pSock->sin_addr));

                if (pSock->sin_port != 0)
                {
                    CheckWin32(status, AppendPrintf(output, L":%u",
                        _byteswap_ushort(pSock->sin_port)));
                }

                status = ERROR_SUCCESS;
                goto Done;
            }
            break;

        case AF_INET6:

            if (cbData >= SizeOfInet6ThroughAddr)
            {
                auto const pSock = static_cast<sockaddr_in6 const UNALIGNED*>(pData);

                if (pSock->sin6_port != 0)
                {
                    CheckOutOfMem(status, output.push_back(L'['));
                }

                CheckWin32(status, AppendIPv6(output, &pSock->sin6_addr));

                if (cbData >= SizeOfInet6ThroughScope &&
                    pSock->sin6_scope_id != 0)
                {
                    CheckWin32(status, AppendPrintf(output, L"%%%u",
                        pSock->sin6_scope_id));
                }

                if (pSock->sin6_port != 0)
                {
                    CheckWin32(status, AppendPrintf(output, L"]:%u",
                        _byteswap_ushort(pSock->sin6_port)));
                }

                status = ERROR_SUCCESS;
                goto Done;
            }
            break;

        case AF_LINK:

            if (cbData >= 10)
            {
                auto const pSock = static_cast<sockaddr_dl const UNALIGNED*>(pData);

                CheckWin32(status, AppendPrintf(output, L"%02X-%02X-%02X-%02X-%02X-%02X",
                    pSock->sdl_data[0],
                    pSock->sdl_data[1],
                    pSock->sdl_data[2],
                    pSock->sdl_data[3],
                    pSock->sdl_data[4],
                    pSock->sdl_data[5]));

                status = ERROR_SUCCESS;
                goto Done;
            }
            break;
        }
    }

    status = AppendHexDump(output, pData, cbData);

Done:

    return status;
}

static LSTATUS
AppendStringAsJson(
    EtwInternal::Buffer<wchar_t>& output,
    _In_reads_(cchInput) wchar_t const* pchInput,
    unsigned cchInput) noexcept
{
    LSTATUS status;

    /*
    Note: the inner loop is optimized in favor of not escaping, at a cost of
    some extra complexity when escaping is needed. Before the loop, we ensure
    that the buffer has capacity for the string assuming nothing needs to be
    escaped. Each time we have to escape something, we do the following:
    - Increment offset by the newly-discovered escape overhead.
    - Ensure that the buffer has room for the original reservation plus the
      total escape overhead.
    - Reset pchOutput to point to the (potentially reallocated) output buffer,
      offset by the number of escape characters so far so that i is still the
      correct index.
    */

    // Minimum number of characters to be added to output after opening quote.
    unsigned const baseSize = cchInput + 1; // unescaped input + closing quote

    // Number of characters in output other than unescaped input characters.
    unsigned offset = output.size() + 1; // Initially: existing content + opening quote

    if (!output.resize(baseSize + offset)) // Initial estimate: assume no escaping needed.
    {
        status = ERROR_OUTOFMEMORY;
    }
    else
    {
        wchar_t* pchOutput = output.data() + offset; // Output buffer.
        pchOutput[-1] = '"'; // Opening quote

        for (unsigned i = 0; i != cchInput; i += 1)
        {
            wchar_t ch = pchInput[i];
            if (ch < 0x20)
            {
                switch (ch)
                {
                case 8:  ch = L'b'; goto SingleCharEscape;
                case 9:  ch = L't'; goto SingleCharEscape;
                case 10: ch = L'n'; goto SingleCharEscape;
                case 12: ch = L'f'; goto SingleCharEscape;
                case 13: ch = L'r'; goto SingleCharEscape;
                }

                // Something like "\u001F": five extra characters.
                offset += 5;
                CheckOutOfMem(status, output.resize(baseSize + offset));
                pchOutput = output.data() + offset;

                *(pchOutput + i - 5) = L'\\';
                *(pchOutput + i - 4) = L'u';
                *(pchOutput + i - 3) = L'0';
                *(pchOutput + i - 2) = L'0';
                *(pchOutput + i - 1) = UppercaseHexChars[ch >> 4];
                *(pchOutput + i - 0) = UppercaseHexChars[ch & 0xf];
            }
            else if (ch == L'"' || ch == L'\\')
            {
            SingleCharEscape:

                // Something like "\\" or "\r": one extra character.
                offset += 1;
                CheckOutOfMem(status, output.resize(baseSize + offset));
                pchOutput = output.data() + offset;

                *(pchOutput + i - 1) = L'\\';
                *(pchOutput + i - 0) = ch;
            }
            else
            {
                pchOutput[i] = ch;
            }
        }

        pchOutput[cchInput] = '"'; // Closing quote
        status = ERROR_SUCCESS;
    }

Done:

    return status;
}

static LSTATUS
AppendStringAsJson(
    EtwInternal::Buffer<wchar_t>& output,
    _In_z_ LPCWSTR szInput) noexcept
{
    return AppendStringAsJson(output, szInput, static_cast<unsigned>(wcslen(szInput)));
}

#pragma endregion

#pragma region EtwStringBuilder

EtwStringBuilder::EtwStringBuilder(
    EtwInternal::Buffer<wchar_t>& buffer) noexcept
    : m_buffer(buffer)
{
    return;
}

LSTATUS __stdcall
EtwStringBuilder::AppendChar(
    EtwWCHAR value) noexcept
{
    return m_buffer.push_back(value) ? ERROR_SUCCESS : ERROR_OUTOFMEMORY;
}

LSTATUS __stdcall
EtwStringBuilder::AppendWide(
    _In_z_ EtwPCWSTR szValue) noexcept
{
    return ::AppendWide(m_buffer, szValue);
}

LSTATUS __stdcall
EtwStringBuilder::AppendPrintf(
    _Printf_format_string_ EtwPCWSTR szFormat,
    ...) noexcept
{
    LSTATUS status;
    va_list args;
    va_start(args, szFormat);
    status = AppendPrintfV(m_buffer, szFormat, args);
    va_end(args);
    return status;
}

#pragma endregion

#pragma region Enums

enum EtwEnumerator::ValueType
    : UCHAR
{
    ValueType_None,
    ValueType_JsonString,      // string, may need to be escaped.
    ValueType_JsonCleanString, // string, does not need to be escaped.
    ValueType_JsonLiteral,     // true, false, null, or finite number.
};

enum EtwEnumerator::Categories
    : UCHAR
{
    CategoryNone,
    CategoryCharacter, // c, C
    CategoryInteger,   // d, i, u, o, x, X, p
    CategoryFloat,     // f, F, e, E, g, G, a, A
    CategoryString,    // s, S, Z
    // Unsupported: n
};

#pragma endregion

#pragma region ParsedPrintf private class

class EtwEnumerator::ParsedPrintf
{
public:

    explicit ParsedPrintf(
        _In_z_ LPCWSTR szFormat) noexcept
        : m_consumed()
        , m_copied()
        , m_copiedBeforePrecision()
        , m_specifier()
        , m_category()
    {
        /*
        Goals:
        1. Determine whether this is an acceptable format string. Be
            conservative - better to ignore the format string (and render
            using default formatting) than to do something that _vsnwprintf
            won't understand (usually resulting in rendering nothing).
        2. Save the flags, width, and precision in m_format.
        3. Skip the length (e.g. "ll", "h", "w", "I64", etc.).
        4. Save the specifier (s, d, u, f, g, etc.) in m_specifier.
        5. Determine m_category based on m_specifier.
        6. Determine the total number of input characters to consume.
            This will be 0 if the format string is invalid.
        */

        unsigned iFormat = 0;
        struct
        {
            UCHAR Minus : 1;
            UCHAR Plus : 1;
            UCHAR Space : 1;
            UCHAR Hash : 1;
            UCHAR Zero : 1;
        } seen = {};
        UCHAR cDigits;

        m_format[m_copied++] = '%';

        for (;; iFormat += 1) // Consume a flag each time through the loop.
        {
            switch (szFormat[iFormat])
            {
                // FLAGS:
            case '-':
                if (seen.Minus)
                {
                    continue; // Already seen this flag. Don't copy again.
                }
                seen.Minus = true;
                break;
            case '+':
                if (seen.Plus)
                {
                    continue; // Already seen this flag. Don't copy again.
                }
                seen.Plus = true;
                break;
            case ' ':
                if (seen.Space)
                {
                    continue; // Already seen this flag. Don't copy again.
                }
                seen.Space = true;
                break;
            case '#':
                if (seen.Hash)
                {
                    continue; // Already seen this flag. Don't copy again.
                }
                seen.Hash = true;
                break;
            case '0':
                if (seen.Zero)
                {
                    continue; // Already seen this flag. Don't copy again.
                }
                seen.Zero = true;
                break;

            // WIDTH:
            case '1':
            case '2':
            case '3':
            case '4':
            case '5':
            case '6':
            case '7':
            case '8':
            case '9':
                cDigits = 0;
                do
                {
                    cDigits += 1;
                    m_format[m_copied++] = szFormat[iFormat++]; // Copy digit
                } while (szFormat[iFormat] >= '0' && szFormat[iFormat] <= '9' && cDigits != MaxDigits);
                goto Precision;
            case '*':
                iFormat += 1; // Ignore width '*'
                goto Precision;

            // PRECISION:
            case '.':
                goto PrecisionYes;

            // LENGTH:
            case 'h':
            case 'l':
                goto LengthHL;
            case 'I':
                goto LengthI;
            case 'j':
            case 'z':
            case 't':
            case 'L':
            case 'w':
                goto LengthJZTLW;

            // SPECIFIERS:
            case 'c':
            case 'C': // Microsoft-specific
                goto SpecifierCharacter;
            case 'd':
            case 'i':
            case 'o':
            case 'u':
            case 'x':
            case 'X':
            case 'p':
                goto SpecifierInteger;
            case 'f':
            case 'F':
            case 'e':
            case 'E':
            case 'g':
            case 'G':
            case 'a':
            case 'A':
                goto SpecifierFloat;
            case 's':
            case 'S': // Microsoft-specific
            case 'Z': // Microsoft-specific
                goto SpecifierString;

            // UNRECOGNIZED:
            default:
                goto Done; // Error
            }

            // Can copy over up to 5 flags.
            m_format[m_copied++] = szFormat[iFormat]; // Copy flag
        }

    Precision:

        if (szFormat[iFormat] == '.')
        {
        PrecisionYes:

            m_copiedBeforePrecision = m_copied;
            m_format[m_copied++] = szFormat[iFormat++]; // Copy '.'

            if (szFormat[iFormat] >= '0' && szFormat[iFormat] <= '9')
            {
                cDigits = 0;
                do
                {
                    cDigits += 1;
                    m_format[m_copied++] = szFormat[iFormat++]; // Copy digit
                } while (szFormat[iFormat] >= '0' && szFormat[iFormat] <= '9' && cDigits != MaxDigits);
            }
            else if (szFormat[iFormat] == '*')
            {
                iFormat += 1; // Ignore precision '*'
            }
        }

        switch (szFormat[iFormat])
        {
        // LENGTH:
        case 'h':
        case 'l':
        LengthHL:
            iFormat += 1; // Ignore length 'h' or 'l'
            if (szFormat[iFormat] == szFormat[iFormat - 1])
            {
                iFormat += 1; // Ignore length 'h' or 'l'
            }
            break;
        case 'I':
        LengthI:
            iFormat += 1; // Ignore length 'I'
            if ((szFormat[iFormat] == '6' && szFormat[iFormat + 1] == '4') ||
                (szFormat[iFormat] == '3' && szFormat[iFormat + 1] == '2'))
            {
                iFormat += 2;  // Ignore length "32" or "64"
            }
            break;
        case 'j':
        case 'z':
        case 't':
        case 'L':
        case 'w':
        LengthJZTLW:
            iFormat += 1; // Ignore length 'j', 'z', 't', 'L', 'w'
            break;

        // SPECIFIERS:
        case 'c':
        case 'C': // Microsoft-specific
            goto SpecifierCharacter;
        case 'd':
        case 'i':
        case 'o':
        case 'u':
        case 'x':
        case 'X':
        case 'p':
            goto SpecifierInteger;
        case 'f':
        case 'F':
        case 'e':
        case 'E':
        case 'g':
        case 'G':
        case 'a':
        case 'A':
            goto SpecifierFloat;
        case 's':
        case 'S': // Microsoft-specific
        case 'Z': // Microsoft-specific
            goto SpecifierString;

        // UNRECOGNIZED:
        default:
            goto Done; // Error
        }

        switch (szFormat[iFormat])
        {
        // SPECIFIERS:
        case 'c':
        case 'C': // Microsoft-specific
        SpecifierCharacter:
            m_category = CategoryCharacter;
            break; // Success
        case 'd':
        case 'i':
        case 'o':
        case 'u':
        case 'x':
        case 'X':
        case 'p':
        SpecifierInteger:
            m_category = CategoryInteger;
            break; // Success
        case 'f':
        case 'F':
        case 'e':
        case 'E':
        case 'g':
        case 'G':
        case 'a':
        case 'A':
        SpecifierFloat:
            m_category = CategoryFloat;
            break; // Success
        case 's':
        case 'S': // Microsoft-specific
        case 'Z': // Microsoft-specific
        SpecifierString:
            m_category = CategoryString;
            break; // Success

        // UNRECOGNIZED:
        default:
            goto Done; // Error
        }

        // Success
        m_specifier = static_cast<char>(szFormat[iFormat++]);
        m_consumed = iFormat;

    Done:

        ASSERT(m_copied <= MaxPrefix);
    }

    unsigned Consumed() const noexcept
    {
        return m_consumed;
    }

    Categories Category() const noexcept
    {
        return m_category;
    }

    // Returns true if Category == String and MakeStringFormat() == "%ls"
    // (i.e. no flags, width, or precision specifiers). If this is true,
    // the caller might be able to avoid unnecessary formatting.
    bool IsPlainString() const noexcept
    {
        return m_category == CategoryString && m_copied == 1;
    }

    // Returns a format string that formats a wchar_t, e.g. "%lc".
    _Ret_z_ LPCWSTR MakeCharacterFormat() noexcept
    {
        __analysis_assert(m_copied <= MaxPrefix);
        ASSERT(m_category == CategoryCharacter);
        m_format[m_copied + 0] = 'l';
        m_format[m_copied + 1] = 'c';
        m_format[m_copied + 2] = '\0';
        return m_format;
    }

    // Returns a format string that formats an 8-bit integer, e.g. "%hhu".
    _Ret_z_ LPCWSTR MakeInt8Format() noexcept
    {
        __analysis_assert(m_copied <= MaxPrefix);
        ASSERT(m_category == CategoryInteger);
        m_format[m_copied + 0] = 'h';
        m_format[m_copied + 1] = 'h';
        m_format[m_copied + 2] = m_specifier == 'p' ? 'X' : m_specifier;
        m_format[m_copied + 3] = '\0';
        return m_format;
    }

    // Returns a format string that formats a 16-bit integer, e.g. "%hu".
    _Ret_z_ LPCWSTR MakeInt16Format() noexcept
    {
        __analysis_assert(m_copied <= MaxPrefix);
        ASSERT(m_category == CategoryInteger);
        m_format[m_copied + 0] = 'h';
        m_format[m_copied + 1] = m_specifier == 'p' ? 'X' : m_specifier;
        m_format[m_copied + 2] = '\0';
        return m_format;
    }

    // Returns a format string that formats a 32-bit integer, e.g. "%u".
    _Ret_z_ LPCWSTR MakeInt32Format() noexcept
    {
        __analysis_assert(m_copied <= MaxPrefix);
        ASSERT(m_category == CategoryInteger);
        m_format[m_copied + 0] = m_specifier == 'p' ? 'X' : m_specifier;
        m_format[m_copied + 1] = '\0';
        return m_format;
    }

    // Returns a format string that formats a 64-bit integer, e.g. "%llu".
    _Ret_z_ LPCWSTR MakeInt64Format() noexcept
    {
        __analysis_assert(m_copied <= MaxPrefix);
        ASSERT(m_category == CategoryInteger);
        m_format[m_copied + 0] = 'l';
        m_format[m_copied + 1] = 'l';
        m_format[m_copied + 2] = m_specifier == 'p' ? 'X' : m_specifier;
        m_format[m_copied + 3] = '\0';
        return m_format;
    }

    // Returns a format string that formats a float/double, e.g. "%f".
    _Ret_z_ LPCWSTR MakeFloatFormat() noexcept
    {
        __analysis_assert(m_copied <= MaxPrefix);
        ASSERT(m_category == CategoryFloat);
        m_format[m_copied + 0] = m_specifier;
        m_format[m_copied + 1] = '\0';
        return m_format;
    }

    // Returns a format string that formats a LPCWSTR, e.g. "%ls".
    _Ret_z_ LPCWSTR MakeStringFormat() noexcept
    {
        // For String category, precision means truncation.
        // For other categories, precision means something else.
        // When converting from another category to string, ignore
        // precision to avoid unexpected truncation.
        // For example, "%.3g" should become "%ls", not "%.3ls".
        if (m_category != CategoryString && m_copiedBeforePrecision != 0)
        {
            m_copied = m_copiedBeforePrecision;
        }

        __analysis_assert(m_copied <= MaxPrefix);
        m_format[m_copied + 0] = 'l';
        m_format[m_copied + 1] = 's';
        m_format[m_copied + 2] = '\0';
        return m_format;
    }

private:

    static constexpr UCHAR MaxDigits = 4;
    static constexpr UCHAR MaxPrefix = 7 + 2 * MaxDigits; // e.g. "%-+0 #1234.1234"
    static constexpr UCHAR MaxSuffix = 4;  // e.g. "llu\0"

    unsigned m_consumed; // How many chars consumed from szFormat, or 0 if invalid.
    UCHAR m_copied;   // How many chars copied from szFormat to m_format.
    UCHAR m_copiedBeforePrecision;
    char m_specifier; // The input format specifier (e.g. d, i, u, o, x, X, etc.)
    Categories m_category;
    wchar_t m_format[MaxPrefix + MaxSuffix]; // The fixed-up format.
};

#pragma endregion

#pragma region FormatContext private class

class EtwEnumerator::FormatContext
{
    struct PropInfo;
    static const UINT8 InitialRecursionLimit = 3;
    static const UINT8 MaxRecursionLimit = 255;

public:

    explicit FormatContext(
        EtwEnumerator& enumerator,
        EtwInternal::Buffer<wchar_t>& output,
        EtwInternal::Buffer<wchar_t>& scratchBuffer) noexcept
        : m_enum(enumerator)
        , m_output(output)
        , m_scratchBuffer(scratchBuffer)
        , m_szEventAttributes(enumerator.EventAttributes())
        , m_cchEventAttributes(
            m_szEventAttributes
            ? static_cast<unsigned>(wcslen(m_szEventAttributes))
            : 0)
        , m_ktime()
        , m_utime()
        , m_removeTrailingSpaceAfterRecursionLevel()
        , m_cpuIndex()
        , m_nameBuffer()
        , m_propInfo()
    {
        auto& eventRec = *m_enum.m_pEventRecord;
        if (0 == (eventRec.EventHeader.Flags &
            (EVENT_HEADER_FLAG_NO_CPUTIME | EVENT_HEADER_FLAG_PRIVATE_SESSION)))
        {
            m_ktime = m_enum.TicksToMilliseconds(eventRec.EventHeader.KernelTime);
            m_utime = m_enum.TicksToMilliseconds(eventRec.EventHeader.UserTime);
        }
        else
        {
            m_ktime = 0;
            m_utime = 0;
        }

        return;
    }

    // Format a string using traditional WPP semantics for %1..%9 variables.
    bool AddPrefix(
        _In_z_ LPCWSTR szFormat) noexcept
    {
        auto& eventRec = *m_enum.m_pEventRecord;

        m_removeTrailingSpaceAfterRecursionLevel = MaxRecursionLimit;
        m_cpuIndex = GetEventProcessorIndex(&eventRec);
        m_nameBuffer.clear();

        // Prefix supports 9 numbered properties (%1..%9)
        CheckOutOfMem(m_enum.m_lastError, m_propInfo.resize(9, false));

        // Not quite the same as AppendCurrentProviderName:
        // Don't copy ProviderName into m_nameBuffer if we don't need to.
        wchar_t const* pchProviderName;
        USHORT cchProviderName;
        if (m_enum.m_pTraceEventInfo->ProviderNameOffset)
        {
            pchProviderName = m_enum.TeiStringNoCheck(m_enum.m_pTraceEventInfo->ProviderNameOffset);
            cchProviderName = ProviderNameLength(m_enum.m_pEventRecord->EventHeader.ProviderId, pchProviderName);
        }
        else
        {
            // ProviderName not set. Use fallback.
            CheckWin32(m_enum.m_lastError, m_enum.AppendCurrentProviderNameFallback(m_nameBuffer));

            // Note: we don't just set pchProviderName = m_nameBuffer.data()
            // because m_nameBuffer might be reallocated for EventName.
            pchProviderName = nullptr; // null means use m_nameBuffer.data().
            cchProviderName = static_cast<USHORT>(m_nameBuffer.size());
        }

        // Not quite the same as AppendCurrentEventName:
        // Don't copy EventName into m_nameBuffer if we don't need to.
        wchar_t const* pchEventName;
        USHORT cchEventName;
        pchEventName = m_enum.EventName();
        if (pchEventName)
        {
            cchEventName = static_cast<USHORT>(wcslen(pchEventName));
        }
        else
        {
            // EventName not set. Use fallback.
            unsigned const iEventName = m_nameBuffer.size();
            CheckWin32(m_enum.m_lastError, m_enum.AppendCurrentEventNameFallback(m_nameBuffer));
            pchEventName = m_nameBuffer.data() + iEventName;
            cchEventName = static_cast<USHORT>(m_nameBuffer.size() - iEventName);
        }

        // PROVIDER = %1
        m_propInfo[1 - 1] = PropInfo(PropInfoAppendValue,
            pchProviderName ? pchProviderName : m_nameBuffer.data(),
            cchProviderName * sizeof(WCHAR),
            TDH_INTYPE_UNICODESTRING);

        // EVENT = %2
        m_propInfo[2 - 1] = PropInfo(PropInfoAppendValue,
            pchEventName,
            cchEventName * sizeof(WCHAR),
            TDH_INTYPE_UNICODESTRING);

        // TID = %3!04X! (special treatment needed for !04X! formatting)
        m_propInfo[3 - 1] = PropInfo(PropInfoAppendValue04X,
            &eventRec.EventHeader.ThreadId, 4, TDH_INTYPE_UINT32, TDH_OUTTYPE_TID);

        // TIME = %4
        m_propInfo[4 - 1] = PropInfo(PropInfoAppendValue,
            &eventRec.EventHeader.TimeStamp, 8, TDH_INTYPE_FILETIME, TDH_OutTypeDateTimeUtc);

        // KTIME = %5!08u! (special treatment needed for !08u! formatting)
        m_propInfo[5 - 1] = PropInfo(PropInfoAppendValue08u,
            &m_ktime, 4, TDH_INTYPE_UINT32);

        // UTIME = %6!08u! (special treatment needed for !08u! formatting)
        m_propInfo[6 - 1] = PropInfo(PropInfoAppendValue08u,
            &m_utime, 4, TDH_INTYPE_UINT32);

        static const UINT32 sequenceNumber = 0;
        // SEQ = %7!u!
        m_propInfo[7 - 1] = PropInfo(PropInfoAppendValue,
            &sequenceNumber, 4, TDH_INTYPE_UINT32);

        // PID = %8!04X! (special treatment needed for !04X! formatting)
        m_propInfo[8 - 1] = PropInfo(PropInfoAppendValue04X,
            &eventRec.EventHeader.ProcessId, 4, TDH_INTYPE_UINT32, TDH_OUTTYPE_PID);

        // CPU = %9!u!
        m_propInfo[9 - 1] = PropInfo(PropInfoAppendValue,
            &m_cpuIndex, 4, TDH_INTYPE_UINT32);

        AddFormatImpl(szFormat, 0);

    Done:

        return m_enum.m_lastError == ERROR_SUCCESS;
    }

    // Format a string using event property values for %1, %2, etc. variables.
    bool AddCurrentEvent(
        _In_z_ LPCWSTR szFormat,
        bool removeTrailingSpace) noexcept
    {
        ASSERT(m_enum.m_state == EtwEnumeratorState_BeforeFirstItem);

        m_removeTrailingSpaceAfterRecursionLevel =
            removeTrailingSpace ? InitialRecursionLimit : MaxRecursionLimit;

        auto const cTopLevelProperties =
            m_enum.m_pTraceEventInfo->TopLevelPropertyCount;

        m_enum.MoveNext(); // Move to first item in event.
        if (m_enum.m_lastError != ERROR_SUCCESS)
        {
            goto Done;
        }

        // Event string supports a numbered property for each top-level property.
        CheckOutOfMem(m_enum.m_lastError, m_propInfo.resize(cTopLevelProperties, false));

        // Fill in the PropInfo for each top-level property.

        for (unsigned iProperty = 0; iProperty != cTopLevelProperties; iProperty += 1)
        {
            ASSERT(m_enum.m_state > EtwEnumeratorState_BeforeFirstItem);

            if (m_enum.m_state == EtwEnumeratorState_Value)
            {
                // Simple property. Will be rendered with AppendValueWithMapName.
                // Save cooked data location, type, MapName, so we can call it later.
                auto& epi = m_enum.m_pTraceEventInfo->EventPropertyInfoArray[iProperty];
                m_propInfo[iProperty] = PropInfo(
                    PropInfoAppendValueRecurse,
                    m_enum.m_pbCooked,
                    m_enum.m_cbCooked,
                    static_cast<_TDH_IN_TYPE>(m_enum.m_cookedInType),
                    static_cast<_TDH_OUT_TYPE>(epi.nonStructType.OutType),
                    m_enum.TeiString(epi.nonStructType.MapNameOffset));

                m_enum.MoveNext();
                if (m_enum.m_lastError != ERROR_SUCCESS)
                {
                    goto Done;
                }
            }
            else
            {
                // Complex property. Will be rendered with AppendCurrentItemAsJsonAndMoveNext.
                auto const pbPropStart = m_enum.m_pbDataNext;

                m_enum.MoveNextSibling();
                if (m_enum.m_lastError != ERROR_SUCCESS)
                {
                    goto Done;
                }

                // Save raw data location and property index so we can call it later.
                m_propInfo[iProperty] = PropInfo(PropInfoAppendCurrentItemAsJson,
                    pbPropStart,
                    static_cast<USHORT>(m_enum.m_pbDataNext - pbPropStart),
                    static_cast<USHORT>(iProperty));
            }
        }

        ASSERT(m_enum.m_state == EtwEnumeratorState_AfterLastItem);

        AddFormatImpl(szFormat, InitialRecursionLimit);

    Done:

        return m_enum.m_lastError == ERROR_SUCCESS;
    }

private:

    bool AddFormatImpl(
        _In_z_ LPCWSTR szFormatString,
        UINT8 const recursionLimit) noexcept
    {
        LPCWSTR p;
        unsigned i;
        UINT8 effectiveRecursionLimit = recursionLimit;

        // Is szFormatString within m_scratchBuffer?
        bool const formatFromScratch =
            szFormatString >= m_scratchBuffer.data() &&
            szFormatString <= m_scratchBuffer.data() + m_scratchBuffer.size();
        if (formatFromScratch)
        {
            // szFormatString is within scratchBuffer, so we'll need to reset
            // p any time we call a subroutine that might use m_scratchBuffer.
            p = m_scratchBuffer.data();
            i = static_cast<unsigned>(szFormatString - p);
        }
        else
        {
            // szFormatString is not related to m_scratchBuffer.
            p = szFormatString;
            i = 0;
        }

        for (;;)
        {
            auto const iChunkStart = i;
            wchar_t ch;

            // Find the next '%' (if any).
            for (;;)
            {
                ch = p[i];
                if (ch == L'\0' || ch == L'%')
                {
                    break;
                }
                i += 1;
            }

            // Append a chunk of normal text.
            CheckWin32(m_enum.m_lastError, AppendWide(m_output, p + iChunkStart, i - iChunkStart));

            // If end of string, we're done.
            if (ch == L'\0')
            {
                // TDH adds an extra space to EventMessage. Remove it.
                if (recursionLimit == m_removeTrailingSpaceAfterRecursionLevel &&
                    i != 0 && p[i - 1] == L' ')
                {
                    ASSERT(m_output.size() != 0);
                    ASSERT(m_output[m_output.size() - 1] == L' ');
                    m_output.resize_unchecked(m_output.size() - 1);
                }

                break;
            }

            auto const iPercent = i; // Index of the '%' character.
            i += 1; // Consume the first '%' character.
            ch = p[i];

            if (ch == L'!')
            {
                // e.g. %!NAME!

                // Consume "!NAME".
                do
                {
                    i += 1;
                } while (p[i] >= L'A' && p[i] <= L'Z');

                auto const cchVarName = i - (iPercent + 2);

                if (p[i] != L'!' || cchVarName < 1)
                {
                    // Not a valid variable name, pass "%!NAME" through to output.
                    CheckWin32(m_enum.m_lastError, AppendWide(m_output, &p[iPercent], i - iPercent));
                }
                else
                {
                    i += 1; // Consume trailing '!'
                    CheckWin32(m_enum.m_lastError, AppendVariable(p + iPercent, i - iPercent));
                }

                // Safe to "continue" only if we haven't touched m_scratchBuffer.
                ASSERT(!formatFromScratch || p == m_scratchBuffer.data());
                continue;
            }

            bool const doublePercent = (ch == L'%');
            if (doublePercent)
            {
                i += 1; // Consume the second '%' character.
                ch = p[i];
            }

            if (ch < L'0' || ch > L'9')
            {
                if (doublePercent &&
                    ch == L'%' &&
                    p[i + 1] >= L'0' &&
                    p[i + 1] <= L'9')
                {
                    // Treat %%%2 as "%%" + "%2".
                    CheckWin32(m_enum.m_lastError, AppendLiteral(m_output, L"%%"));
                }
                else
                {
                    // In all other cases, consume one '%' char at a time.
                    i -= doublePercent; // Restore the second '%' character.
                    CheckOutOfMem(m_enum.m_lastError, m_output.push_back(L'%'));
                }

                // Safe to "continue" only if we haven't touched m_scratchBuffer.
                ASSERT(!formatFromScratch || p == m_scratchBuffer.data());
                continue;
            }

            unsigned index = ch - L'0';
            for (;;)
            {
                i += 1; // Consume a digit.
                ch = p[i];
                if (ch < L'0' || ch > L'9')
                {
                    break;
                }

                index = index * 10 + ch - L'0';
            }

            if (doublePercent)
            {
                auto const scratchOldSize = m_scratchBuffer.size();

                {
                    EtwStringBuilder scratchBuilder(m_scratchBuffer);
                    m_enum.m_lastError = m_enum.m_enumeratorCallbacks.GetParameterMessage(
                        m_enum.m_pEventRecord, index, scratchBuilder);
                    if (m_enum.m_lastError != ERROR_SUCCESS)
                    {
                        goto Done;
                    }
                }

                // Trim trailing CR/LF.
                LPWSTR pch = m_scratchBuffer.data() + scratchOldSize;
                unsigned cch = m_scratchBuffer.size() - scratchOldSize;
                while (cch != 0 && (pch[cch - 1] == L'\r' || pch[cch - 1] == L'\n'))
                {
                    cch -= 1;
                }

                if (effectiveRecursionLimit == 0)
                {
                    CheckWin32(m_enum.m_lastError, AppendWide(m_output, pch, cch));
                }
                else
                {
                    m_scratchBuffer.resize_unchecked(cch + scratchOldSize);
                    CheckOutOfMem(m_enum.m_lastError, m_scratchBuffer.push_back(L'\0')); // Ensure nul-termination.
                    pch = m_scratchBuffer.data() + scratchOldSize; // push_back may have reallocated.
                    CheckAdd(AddFormatImpl(pch, effectiveRecursionLimit - 1));
                }

                // Clean up our use of scratch buffer.
                m_scratchBuffer.resize_unchecked(scratchOldSize);
            }
            else if (ch != L'!')
            {
                // e.g. %2
                CheckAdd(AddProperty(index, effectiveRecursionLimit));
            }
            else if (p[i + 1] == L'S' && p[i + 2] == L'!')
            {
                // e.g. %2!S!

                // Note: After seeing a !S!, MessageRender.cpp disables
                // recursion for the rest of the current format string, not
                // just the item that used the formatting. EtwEnumerator only
                // disables recursion for the current item.

                i += 3; // Consume "!S!".
                CheckAdd(AddProperty(index, 0)); // No recursion.
            }
            else
            {
                // e.g. %2!08x!

                ParsedPrintf format(&p[i + 1]);
                if (format.Consumed() != 0 &&
                    p[i + 1 + format.Consumed()] == L'!')
                {
                    // Valid printf format string.
                    i += format.Consumed() + 2; // Consume "!format!".
                    CheckAdd(AddProperty(index, 0, // No recursion.
                            format.IsPlainString() ? nullptr : &format));
                }
                else
                {
                    // Not a printf format string. Ignore it, don't consume it.
                    CheckAdd(AddProperty(index, effectiveRecursionLimit));
                }
            }

            if (formatFromScratch)
            {
                p = m_scratchBuffer.data(); // We may have reallocated scratchBuffer.
            }
        }

        m_enum.m_lastError = ERROR_SUCCESS;

    Done:

        return m_enum.m_lastError == ERROR_SUCCESS;
    }

    static void ReplaceNulWithSpace(
        _Inout_updates_(c) wchar_t* p,
        unsigned c) noexcept
    {
        for (unsigned i = 0; i != c; i += 1)
        {
            if (p[i] == '\0')
            {
                p[i] = L' ';
            }
        }
    }

    bool AddProperty(
        unsigned index,
        UINT8 recursionLimit,
        _Inout_opt_ ParsedPrintf* pPrintf = nullptr) noexcept
    {
        if (index - 1u >= m_propInfo.size())
        {
            // Index out of range.
            CheckWin32(m_enum.m_lastError, AppendPrintf(m_output, L"[IndexOutOfRange:%%%u]", index));
        }
        else
        {
            auto& pi = m_propInfo[index - 1];

            if (pi.InUse)
            {
                // Infinite recursion:
                // The expansion of "...%4..." includes the string "...%4...".
                CheckWin32(m_enum.m_lastError, AppendPrintf(m_output, L"[IndexRecursion:%%%u]", index));
                m_enum.m_lastError = ERROR_SUCCESS;
                goto Done;
            }

            pi.InUse = true;

            switch (pi.Type)
            {
            case PropInfoAppendCurrentItemAsJson:

                // Move enumerator to the property, load it.
                ASSERT(m_enum.m_stack.size() == 0);
                m_enum.m_pbDataNext = pi.RawData;
                m_enum.m_stackTop.PropertyIndex = pi.PropertyIndex;
                m_enum.NextProperty();
                CheckWin32(m_enum.m_lastError, m_enum.m_lastError);

                // No recursion or nul-cleanup needed.
                if (pPrintf == nullptr)
                {
                    // Render directly to output.
                    CheckAdd(m_enum.AddCurrentItemAsJsonAndMoveNext(
                        m_output, m_scratchBuffer, EtwJsonItemFlags_None));
                }
                else
                {
                    // Render item to m_scratchBuffer, then printf to m_output.
                    auto const scratchOldSize = m_scratchBuffer.size();

                    CheckAdd(m_enum.AddCurrentItemAsJsonAndMoveNext(
                        m_scratchBuffer, m_output, EtwJsonItemFlags_None));
                    CheckOutOfMem(m_enum.m_lastError, m_scratchBuffer.push_back(L'\0')); // nul-terminate

                    auto value = m_scratchBuffer.data() + scratchOldSize;
                    auto szFormat = pPrintf->MakeStringFormat();
                    CheckWin32(m_enum.m_lastError, AppendPrintf(m_output, szFormat, value));

                    m_scratchBuffer.resize_unchecked(scratchOldSize);
                }
                break;

            case PropInfoAppendValue04X:

                if (pPrintf == nullptr)
                {
                    // No recursion or nul-cleanup needed.
                    ASSERT(pi.CookedDataSize == 4);
                    CheckWin32(m_enum.m_lastError, AppendPrintf(m_output, L"%04X",
                        *reinterpret_cast<UINT32 const UNALIGNED*>(pi.CookedData)));
                    break;
                }
                goto PropInfoAppendValue;

            case PropInfoAppendValue08u:

                if (pPrintf == nullptr)
                {
                    // No recursion or nul-cleanup needed.
                    ASSERT(pi.CookedDataSize == 4);
                    CheckWin32(m_enum.m_lastError, AppendPrintf(m_output, L"%08u",
                        *reinterpret_cast<UINT32 const UNALIGNED*>(pi.CookedData)));
                    break;
                }
                goto PropInfoAppendValue;

            case PropInfoAppendValue:
            PropInfoAppendValue:

                recursionLimit = 0;
                __fallthrough;

            case PropInfoAppendValueRecurse:

                if (pPrintf != nullptr)
                {
                    // There is a custom format string.
                    // If the data type and the format string match, we can
                    // printf directly to output. Otherwise, we render the data
                    // as a string into m_scratchBuffer, then printf the
                    // resulting string into output.

                    ASSERT(recursionLimit == 0);

                    if (pPrintf->Category() == CategoryString)
                    {
                        goto RenderAsString;
                    }

                    switch (pi.CookedInType)
                    {
                    case TDH_INTYPE_INT8:
                    case TDH_INTYPE_UINT8:
                    case TDH_INTYPE_ANSICHAR:

                        ASSERT(pi.CookedDataSize == 1);
                        if (pPrintf->Category() == CategoryInteger)
                        {
                            auto value = *reinterpret_cast<UINT8 const*>(pi.CookedData);
                            auto szFormat = pPrintf->MakeInt8Format();
                            CheckWin32(m_enum.m_lastError, AppendPrintf(m_output, szFormat, value));
                        }
                        else if (pPrintf->Category() == CategoryCharacter)
                        {
                            // Convert input with MB2WC, then render.
                            unsigned cp;
                            switch (pi.OutType)
                            {
                            default:
                            case TDH_OUTTYPE_STRING:
                                cp = CP_ACP;
                                break;
                            case TDH_OUTTYPE_XML:
                            case TDH_OUTTYPE_JSON:
                            case TDH_OUTTYPE_UTF8:
                                cp = CP_UTF8;
                                break;
                            }

                            wchar_t value;
                            if (1 != MultiByteToWideChar(
                                cp, 0, (LPCCH)pi.CookedData, 1, &value, 1))
                            {
                                goto RenderAsString;
                            }

                            // Convert nul to space.
                            if (value == L'\0')
                            {
                                value = L' ';
                            }

                            auto szFormat = pPrintf->MakeCharacterFormat();
                            CheckWin32(m_enum.m_lastError, AppendPrintf(m_output, szFormat, value));
                        }
                        else
                        {
                            goto RenderAsString;
                        }
                        break;

                    case TDH_INTYPE_INT16:
                    case TDH_INTYPE_UINT16:
                    case TDH_INTYPE_UNICODECHAR:

                        ASSERT(pi.CookedDataSize == 2);
                        if (pPrintf->Category() == CategoryInteger)
                        {
                            auto value =
                                pi.OutType == TDH_OUTTYPE_PORT
                                ? _byteswap_ushort(*reinterpret_cast<UINT16 const UNALIGNED*>(pi.CookedData))
                                : *reinterpret_cast<UINT16 const UNALIGNED*>(pi.CookedData);
                            auto szFormat = pPrintf->MakeInt16Format();
                            CheckWin32(m_enum.m_lastError, AppendPrintf(m_output, szFormat, value));
                        }
                        else if (pPrintf->Category() == CategoryCharacter)
                        {
                            auto value = *reinterpret_cast<wchar_t const UNALIGNED*>(pi.CookedData);

                            // Convert nul to space.
                            if (value == L'\0')
                            {
                                value = L' ';
                            }

                            auto szFormat = pPrintf->MakeCharacterFormat();
                            CheckWin32(m_enum.m_lastError, AppendPrintf(m_output, szFormat, value));
                        }
                        else
                        {
                            goto RenderAsString;
                        }
                        break;

                    case TDH_INTYPE_INT32:
                    case TDH_INTYPE_UINT32:
                    case TDH_INTYPE_HEXINT32:
                    case TDH_INTYPE_BOOLEAN:
                    RenderAsInt32:

                        ASSERT(pi.CookedDataSize == 4);
                        if (pPrintf->Category() == CategoryInteger)
                        {
                            auto value = *reinterpret_cast<UINT32 const UNALIGNED*>(pi.CookedData);
                            auto szFormat = pPrintf->MakeInt32Format();
                            CheckWin32(m_enum.m_lastError, AppendPrintf(m_output, szFormat, value));
                        }
                        else
                        {
                            goto RenderAsString;
                        }
                        break;

                    case TDH_INTYPE_INT64:
                    case TDH_INTYPE_UINT64:
                    case TDH_INTYPE_HEXINT64:
                    case TDH_INTYPE_FILETIME:
                    RenderAsInt64:

                        ASSERT(pi.CookedDataSize == 8);
                        if (pPrintf->Category() == CategoryInteger)
                        {
                            auto value = *reinterpret_cast<UINT64 const UNALIGNED*>(pi.CookedData);
                            auto szFormat = pPrintf->MakeInt64Format();
                            CheckWin32(m_enum.m_lastError, AppendPrintf(m_output, szFormat, value));
                        }
                        else
                        {
                            goto RenderAsString;
                        }
                        break;

                    case TDH_INTYPE_FLOAT:

                        ASSERT(pi.CookedDataSize == 4);
                        if (pPrintf->Category() == CategoryFloat)
                        {
                            auto value = *reinterpret_cast<float const UNALIGNED*>(pi.CookedData);
                            auto szFormat = pPrintf->MakeFloatFormat();
                            CheckWin32(m_enum.m_lastError, AppendPrintf(m_output, szFormat, value));
                        }
                        else
                        {
                            goto RenderAsString;
                        }
                        break;

                    case TDH_INTYPE_DOUBLE:

                        ASSERT(pi.CookedDataSize == 8);
                        if (pPrintf->Category() == CategoryFloat)
                        {
                            auto value = *reinterpret_cast<double const UNALIGNED*>(pi.CookedData);
                            auto szFormat = pPrintf->MakeFloatFormat();
                            CheckWin32(m_enum.m_lastError, AppendPrintf(m_output, szFormat, value));
                        }
                        else
                        {
                            goto RenderAsString;
                        }
                        break;

                    case TDH_INTYPE_POINTER:
                    case TDH_INTYPE_SIZET:

                        if (pi.CookedDataSize == 8)
                        {
                            goto RenderAsInt64;
                        }
                        else
                        {
                            goto RenderAsInt32;
                        }
                        break;

                    default:
                    RenderAsString:

                        // Render item to m_scratchBuffer, then printf to m_output.
                        auto const scratchOldSize = m_scratchBuffer.size();
                        CheckAdd(AddSimplePropInfo(m_scratchBuffer, pi));
                        CheckOutOfMem(m_enum.m_lastError, m_scratchBuffer.push_back(L'\0')); // nul-terminate

                        auto value = m_scratchBuffer.data() + scratchOldSize;
                        auto szFormat = pPrintf->MakeStringFormat();
                        CheckWin32(m_enum.m_lastError, AppendPrintf(m_output, szFormat, value));

                        m_scratchBuffer.resize_unchecked(scratchOldSize);
                        break;
                    }

                }
                else if (recursionLimit == 0)
                {
                    // Recursion disabled: render directly into m_output.
                    CheckAdd(AddSimplePropInfo(m_output, pi));
                }
                else
                {
                    // Recursion enabled:
                    // Render into m_scratchBuffer, then format into m_output.
                    auto const scratchOldSize = m_scratchBuffer.size();
                    auto const appendResult = m_enum.AddValueWithMapName(
                        m_scratchBuffer,
                        pi.CookedData,
                        pi.CookedDataSize,
                        static_cast<_TDH_IN_TYPE>(pi.CookedInType),
                        static_cast<_TDH_OUT_TYPE>(pi.OutType),
                        m_enum.m_pEventRecord,
                        pi.MapName);

                    switch (appendResult)
                    {
                    case ValueType_JsonString:

                        ReplaceNulWithSpace(
                            m_scratchBuffer.data() + scratchOldSize,
                            m_scratchBuffer.size() - scratchOldSize);

                        CheckOutOfMem(m_enum.m_lastError, m_scratchBuffer.push_back(L'\0')); // nul-terminate

                        // Treat the value as a format string.
                        CheckAdd(AddFormatImpl(
                            m_scratchBuffer.data() + scratchOldSize,
                            recursionLimit - 1));
                        break;

                    case ValueType_JsonCleanString:
                    case ValueType_JsonLiteral:

                        // No additional processing needed.
                        CheckWin32(m_enum.m_lastError, AppendWide(
                            m_output,
                            m_scratchBuffer.data() + scratchOldSize,
                            m_scratchBuffer.size() - scratchOldSize));
                        break;

                    default:
                        goto Done;
                    }

                    m_scratchBuffer.resize_unchecked(scratchOldSize);
                }
                break;

            default:

                m_enum.m_lastError = ERROR_ASSERTION_FAILURE;
                goto Done;
            }

            pi.InUse = false;
        }

        m_enum.m_lastError = ERROR_SUCCESS;

    Done:

        return m_enum.m_lastError == ERROR_SUCCESS;
    }

    bool
    AddSimplePropInfo(
        Buffer& output,
        PropInfo& pi) const noexcept
    {
        auto const oldSize = output.size();
        auto const appendResult = m_enum.AddValueWithMapName(
            output,
            pi.CookedData,
            pi.CookedDataSize,
            static_cast<_TDH_IN_TYPE>(pi.CookedInType),
            static_cast<_TDH_OUT_TYPE>(pi.OutType),
            m_enum.m_pEventRecord,
            pi.MapName);

        switch (appendResult)
        {
        case ValueType_JsonString:
            ReplaceNulWithSpace(
                output.data() + oldSize,
                output.size() - oldSize);
            ASSERT(m_enum.m_lastError == ERROR_SUCCESS);
            break;

        case ValueType_JsonCleanString:
        case ValueType_JsonLiteral:
            ASSERT(m_enum.m_lastError == ERROR_SUCCESS);
            break;

        default:
            ASSERT(m_enum.m_lastError != ERROR_SUCCESS);
            break;
        }

        return appendResult != 0;
    }

    /*
    pchVariable is "%!NAME!".
    */
    LSTATUS
    AppendVariable(
        _In_reads_(cchVariable) wchar_t const* pchVariable,
        _In_range_(3, MAXUINT) unsigned cchVariable) const noexcept
    {
        LSTATUS status = ERROR_SUCCESS;

        // Starts with "%!" and ends with "!".
        ASSERT(cchVariable >= 3);
        ASSERT(pchVariable[0] == L'%');
        ASSERT(pchVariable[1] == L'!');
        ASSERT(pchVariable[cchVariable - 1] == L'!');

        // IS_VARNAME("NAME")
        // Assumes that we've already matched the 'N', so only checks "AME".
#define IS_VARNAME(name) ( \
    cchVariable == ARRAYSIZE(L##name) + 2 && \
    0 == memcmp(pchVariable + 3, L##name + 1, (ARRAYSIZE(L##name) - 2) * sizeof(wchar_t)) \
    )

        auto& eventRec = *m_enum.m_pEventRecord;
        auto& tei = *m_enum.m_pTraceEventInfo;

        switch (pchVariable[2])
        {
        case L'A':
            if (IS_VARNAME("ATTRIBS"))
            {
                auto const eventAttributes = m_enum.EventAttributes();
                if (eventAttributes)
                {
                    CheckWin32(status, AppendWide(m_output, eventAttributes));
                }
                goto Done;
            }
            break;
        case L'B':
            if (IS_VARNAME("BANG"))
            {
                goto Exclamation;
            }
            break;
        case L'C':
            if (IS_VARNAME("COMPNAME")) // Alias for MJ
            {
            CompName:
                CheckWin32(status, AppendEventAttribute(L"MJ", 2));
                goto Done;
            }
            if (IS_VARNAME("CPU")) // CPU = %9!d!
            {
                CheckWin32(status, AppendPrintf(m_output, L"%u",
                    GetEventProcessorIndex(&eventRec)));
                goto Done;
            }
            break;
        case L'E':
            if (IS_VARNAME("EVENT")) // EVENT = %2
            {
                CheckWin32(status, m_enum.AppendCurrentEventName(m_output));
                goto Done;
            }
            if (IS_VARNAME("EXCLAMATION"))
            {
            Exclamation:
                CheckOutOfMem(status, m_output.push_back(L'!'));
                goto Done;
            }
            break;
        case L'F':
            if (IS_VARNAME("FILE"))
            {
                CheckWin32(status, AppendEventAttribute(L"FILE", 4));
                goto Done;
            }
            if (IS_VARNAME("FLAGS"))
            {
                goto Keywords;
            }
            if (IS_VARNAME("FUNC"))
            {
                CheckWin32(status, m_enum.AppendCurrentFunctionName(m_output));
                goto Done;
            }
            break;
        case L'K':
            if (IS_VARNAME("KEYWORDS"))
            {
            Keywords:
                CheckWin32(status, m_enum.AppendCurrentKeywordsName(m_output));
                goto Done;
            }
            if (IS_VARNAME("KTIME")) // KTIME = %5
            {
                CheckWin32(status, AppendPrintf(m_output, L"%u", m_ktime));
                goto Done;
            }
            break;
        case L'L':
            if (IS_VARNAME("LEVEL"))
            {
                CheckWin32(status, m_enum.AppendCurrentLevelName(m_output));
                goto Done;
            }
            if (IS_VARNAME("LINE"))
            {
                CheckWin32(status, AppendEventAttribute(L"LINE", 4));
                goto Done;
            }
            break;
        case L'M':
            if (IS_VARNAME("MJ")) // aka COMPNAME
            {
                goto CompName;
            }
            if (IS_VARNAME("MN")) // aka SUBCOMP
            {
                goto SubComp;
            }
            break;
        case L'P':
            if (IS_VARNAME("PROVIDER")) // PROVIDER = %1
            {
                CheckWin32(status, m_enum.AppendCurrentProviderName(m_output));
                goto Done;
            }
            if (IS_VARNAME("PID")) // PID = %8!04X!
            {
                CheckWin32(status, AppendPrintf(m_output, L"%04X",
                    eventRec.EventHeader.ProcessId));
                goto Done;
            }
            if (IS_VARNAME("PTIME"))
            {
                // ptime is only valid for private sessions where ProviderId != EventTraceGuid.
                if (0 != (eventRec.EventHeader.Flags & EVENT_HEADER_FLAG_PRIVATE_SESSION) &&
                    eventRec.EventHeader.ProviderId != EventTraceGuid)
                {
                    CheckWin32(status, AppendPrintf(m_output, L"%llu",
                        eventRec.EventHeader.ProcessorTime));
                }
                goto Done;
            }
            if (IS_VARNAME("PCT") ||
                IS_VARNAME("PERCENT"))
            {
                CheckOutOfMem(status, m_output.push_back(L'%'));
                goto Done;
            }
            break;
        case L'S':
            if (IS_VARNAME("SUBCOMP")) // alias for MN
            {
            SubComp:
                CheckWin32(status, AppendEventAttribute(L"MN", 2));
                goto Done;
            }
            if (IS_VARNAME("SEQ"))
            {
                CheckOutOfMem(status, m_output.push_back(L'0'));
                goto Done;
            }
            break;
        case L'T':
            if (IS_VARNAME("TAGS"))
            {
                CheckWin32(status, AppendPrintf(m_output, L"0x%X",
                    tei.Tags));
                goto Done;
            }
            if (IS_VARNAME("TID")) // TID = %3!04X!
            {
                CheckWin32(status, AppendPrintf(m_output, L"%04X",
                    eventRec.EventHeader.ThreadId));
                goto Done;
            }
            if (IS_VARNAME("TIME")) // TIME = %4
            {
                CheckWin32(status, AppendFileTime(m_output, eventRec.EventHeader.TimeStamp.QuadPart,
                    m_enum.m_timestampFormat, m_enum.m_timeZoneBiasMinutes, true));
                goto Done;
            }
            break;
        case L'U':
            if (IS_VARNAME("UTIME")) // UTIME = %6
            {
                CheckWin32(status, AppendPrintf(m_output, L"%u", m_utime));
                goto Done;
            }
            break;
        }

        // Not a recognized variable name, pass "%!NAME!" through to output.
        CheckWin32(status, AppendWide(m_output, pchVariable, cchVariable));

    Done:

        return status;
    }

    LSTATUS
    AppendEventAttribute(
        _In_count_(cName) wchar_t const* pName,
        unsigned cName) const noexcept
    {
        LSTATUS status;

        if (m_cchEventAttributes == 0)
        {
            status = ERROR_SUCCESS;
        }
        else
        {
            status = m_enum.AppendEventAttribute(
                m_output,
                m_szEventAttributes,
                m_cchEventAttributes,
                pName,
                cName);
            if (status == ERROR_NOT_FOUND)
            {
                status = ERROR_SUCCESS;
            }
        }

        return status;
    }

private:

    enum PropInfoType
        : UCHAR
    {
        PropInfoNone,
        PropInfoAppendCurrentItemAsJson, // Render with AppendCurrentItemAsJsonAndMoveNext.
        PropInfoAppendValue,             // Render with AppendValue.
        PropInfoAppendValueRecurse,      // Render with AppendValue, enable recursion.
        PropInfoAppendValue04X,          // Render with AppendValue, default format is "%04X".
        PropInfoAppendValue08u,          // Render with AppendValue, default format is "%08u".
        PropInfoMax
    };

    struct PropInfo
    {
        PropInfo() noexcept = default;

        // Constructor for type = PropInfoAppendCurrentItemAsJson.
        PropInfo(
            PropInfoType type,
            _In_reads_(cbRawData) BYTE const* pbRawData,
            USHORT cbRawData,
            USHORT propertyIndex) noexcept
            : RawData(pbRawData)
            , RawDataSize(cbRawData)
            , PropertyIndex(propertyIndex)
            , Type(type)
            , InUse(false)
        {
            ASSERT(type == PropInfoAppendCurrentItemAsJson);
            return;
        }

        // Constructor for type = PropInfoAppendValue.
        PropInfo(
            PropInfoType type,
            _In_reads_bytes_(cbCookedData) void const* pbCookedData,
            USHORT cbCookedData,
            _TDH_IN_TYPE cookedInType,
            _TDH_OUT_TYPE outType = TDH_OUTTYPE_NULL,
            LPCWSTR szMapName = nullptr) noexcept
            : CookedData(static_cast<BYTE const*>(pbCookedData))
            , CookedDataSize(cbCookedData)
            , CookedInType(static_cast<USHORT>(cookedInType))
            , OutType(static_cast<USHORT>(outType))
            , MapName(szMapName)
            , Type(type)
            , InUse(false)
        {
            ASSERT(
                type > PropInfoAppendCurrentItemAsJson &&
                type < PropInfoMax);
            return;
        }

#pragma warning(push)
#pragma warning (disable:4201)  // nameless struct/union.
#pragma pack(push, 2) // Minor optimization: remove unneeded padding between OutType and Type.
        union
        {
            struct // Type == PropInfoAppendCurrent
            {
                _Field_size_bytes_(RawDataSize) BYTE const* RawData;
                USHORT RawDataSize;
                USHORT PropertyIndex;
            };

            struct // Type == PropInfoAppendValue
            {
                LPCWSTR MapName;
                _Field_size_bytes_(CookedDataSize) BYTE const* CookedData;
                USHORT CookedDataSize;
                USHORT CookedInType;
                USHORT OutType;
            };
        };
#pragma pack(pop)
#pragma warning(pop)

        PropInfoType Type;
        bool InUse;
    };

private:

    EtwEnumerator& m_enum;
    EtwInternal::Buffer<wchar_t>& m_output; // Final product goes here.
    EtwInternal::Buffer<wchar_t>& m_scratchBuffer; // Temp strings go here.
    _Field_size_(m_cchEventAttributes) LPCWSTR const m_szEventAttributes;
    unsigned const m_cchEventAttributes;
    unsigned m_ktime;
    unsigned m_utime;
    UINT8 m_removeTrailingSpaceAfterRecursionLevel;
    _Field_size_(m_cParameterModules) HMODULE const* m_pParameterModules;
    unsigned m_cParameterModules;
    unsigned m_cpuIndex;
    EtwInternal::Buffer<wchar_t, 64> m_nameBuffer; // Storage for fallback %1 and %2 prefix variables.
    EtwInternal::Buffer<PropInfo, 16> m_propInfo; // 16 top-level properties without heap allocation
};

#pragma endregion

#pragma region Private methods

LSTATUS
EtwEnumerator::AppendResultCode(
    EtwInternal::Buffer<wchar_t>& output,
    _In_reads_bytes_(4) void const* pData,
    int domain,        // EtwEnumeratorCallbacks::ResultCodeDomain
    int type) noexcept // EtwEnumeratorCallbacks::UnderlyingType
{
    EtwStringBuilder outputBuilder(output);
    return m_enumeratorCallbacks.FormatResultCodeValue(
        static_cast<EtwEnumeratorCallbacks::ResultCodeDomain>(domain),
        static_cast<EtwEnumeratorCallbacks::UnderlyingType>(type),
        *static_cast<UINT32 UNALIGNED const*>(pData),
        outputBuilder);
}

LSTATUS
EtwEnumerator::AppendCurrentProviderName(
    EtwInternal::Buffer<wchar_t>& output) const noexcept
{
    LSTATUS status;

    if (m_pTraceEventInfo->ProviderNameOffset)
    {
        auto teiString = TeiStringNoCheck(m_pTraceEventInfo->ProviderNameOffset);
        status = AppendWide(output,
            teiString, ProviderNameLength(m_pEventRecord->EventHeader.ProviderId, teiString));
    }
    else
    {
        status = AppendCurrentProviderNameFallback(output);;
    }

    return status;
}

LSTATUS
EtwEnumerator::AppendCurrentProviderNameFallback(
    EtwInternal::Buffer<wchar_t>& output) const noexcept
{
    // No ProviderName set. Fall back to ProviderId.
    // Note: AppendCurrentEventAsJson assumes this won't need JSON escaping.
    return AppendPrintf(output, GUID_PRINTF_FORMAT_UPPER,
        GUID_PRINTF_VALUE(m_pEventRecord->EventHeader.ProviderId));
}

LSTATUS
EtwEnumerator::AppendCurrentEventName(
    EtwInternal::Buffer<wchar_t>& output) const noexcept
{
    LSTATUS status;

    auto const eventName = EventName();
    if (eventName)
    {
        status = AppendWide(output, eventName);
    }
    else
    {
        status = AppendCurrentEventNameFallback(output);
    }

    return status;
}

LSTATUS
EtwEnumerator::AppendCurrentEventNameFallback(
    EtwInternal::Buffer<wchar_t>& output) const noexcept
{
    LSTATUS status;

    // No EventName set. Try FILE+LINE.
    LPCWSTR const szEventAttributes = EventAttributes();
    unsigned const cchEventAttributes = szEventAttributes
        ? static_cast<unsigned>(wcslen(szEventAttributes))
        : 0;
    status = AppendEventAttribute(output,
        szEventAttributes, cchEventAttributes, L"FILE", 4);
    if (status == ERROR_SUCCESS)
    {
        status = AppendEventAttribute(output,
            szEventAttributes, cchEventAttributes, L"LINE", 4);
        if (status == ERROR_SUCCESS)
        {
            goto Done;
        }
    }

    if (status != ERROR_NOT_FOUND)
    {
        goto Done;
    }

    // FILE+LINE not available. Fall back to IDvVERSION.
    status = AppendPrintf(output, L"%uv%u",
        m_pEventRecord->EventHeader.EventDescriptor.Id,
        m_pEventRecord->EventHeader.EventDescriptor.Version);

Done:

    return status;
}

LSTATUS
EtwEnumerator::AppendCurrentKeywordsName(
    EtwInternal::Buffer<wchar_t>& output) const noexcept
{
    LSTATUS status;

    status = ERROR_SUCCESS;

    if (m_pTraceEventInfo->KeywordsNameOffset)
    {
        auto pch = TeiStringNoCheck(m_pTraceEventInfo->KeywordsNameOffset);
        auto cch = static_cast<unsigned>(wcslen(pch));
        while (cch != 0 && pch[cch - 1] == L' ')
        {
            cch -= 1;
        }
        CheckWin32(status, AppendWide(output, pch, cch));
    }
    else
    {
        CheckWin32(status, AppendPrintf(output, L"0x%llX",
            m_pEventRecord->EventHeader.EventDescriptor.Keyword));
    }

Done:

    return status;
}

LSTATUS
EtwEnumerator::AppendCurrentLevelName(
    EtwInternal::Buffer<wchar_t>& output) const noexcept
{
    LSTATUS status;

    if (m_pTraceEventInfo->LevelNameOffset)
    {
        auto pch = TeiStringNoCheck(m_pTraceEventInfo->LevelNameOffset);
        auto cch = static_cast<unsigned>(wcslen(pch));
        while (cch != 0 && pch[cch - 1] == L' ')
        {
            cch -= 1;
        }
        CheckWin32(status, AppendWide(output, pch, cch));
    }
    else
    {
        CheckWin32(status, AppendPrintf(output, L"%u",
            m_pEventRecord->EventHeader.EventDescriptor.Level));
    }

    status = ERROR_SUCCESS;

Done:

    return status;
}

LSTATUS
EtwEnumerator::AppendCurrentFunctionName(
    EtwInternal::Buffer<wchar_t>& output) const noexcept
{
    LSTATUS status;

    LPCWSTR const szEventAttributes = EventAttributes();
    unsigned const cchEventAttributes = szEventAttributes
        ? static_cast<unsigned>(wcslen(szEventAttributes))
        : 0;
    status = AppendEventAttribute(output,
        szEventAttributes, cchEventAttributes, L"FUNC", 4);
    if (status != ERROR_NOT_FOUND)
    {
        goto Done;
    }

    // No FUNC attribute. If the first property of the event is a
    // nul-terminated string named "%!FUNC!", we'll use that.
    if (m_pTraceEventInfo->PropertyCount != 0)
    {
        auto& epi = m_pTraceEventInfo->EventPropertyInfoArray[0];
        if ((epi.Flags & 0x7f) == 0 && // Not a struct, no length, no count, nothing special.
            epi.NameOffset != 0 && // Has a name.
            (epi.nonStructType.InType == TDH_INTYPE_UNICODESTRING ||
             epi.nonStructType.InType == TDH_INTYPE_ANSISTRING) &&
            0 == wcscmp(L"!FUNC!", TeiStringNoCheck(epi.NameOffset)))
        {
            if (epi.nonStructType.InType == TDH_INTYPE_UNICODESTRING)
            {
                auto const pch = static_cast<wchar_t const*>(m_pEventRecord->UserData);
                auto const cch = static_cast<unsigned>(wcsnlen(pch, m_pEventRecord->UserDataLength / sizeof(wchar_t)));
                status = AppendWide(output, pch, cch);
                goto Done;
            }
            else
            {
                auto const pch = static_cast<char const*>(m_pEventRecord->UserData);
                auto const cch = static_cast<unsigned>(strnlen(pch, m_pEventRecord->UserDataLength / sizeof(char)));
                auto const cp = epi.nonStructType.OutType == TDH_OUTTYPE_UTF8 ? CP_UTF8 : CP_ACP;
                status = AppendMbcs(output, pch, cch, cp);
                goto Done;
            }
        }
    }

    // Instead of ERROR_NOT_FOUND, return ERROR_SUCCESS with "" result.
    status = ERROR_SUCCESS;

Done:

    return status;
}

LSTATUS
EtwEnumerator::AppendCurrentNameAsJson(
    EtwInternal::Buffer<wchar_t>& output,
    bool wantSpace) noexcept
{
    LSTATUS status;
    auto& epi = m_pTraceEventInfo->EventPropertyInfoArray[m_stackTop.PropertyIndex];
    LPCWSTR szName = epi.NameOffset ? TeiStringNoCheck(epi.NameOffset) : L"";

    CheckWin32(status, AppendStringAsJson(output, szName));
    CheckWin32(status, AppendWide(output, L": ", wantSpace ? 2 : 1));
    status = ERROR_SUCCESS;

Done:

    return status;
}

bool
EtwEnumerator::AddCurrentEventAsJson(
    EtwInternal::Buffer<wchar_t>& output,
    EtwInternal::Buffer<wchar_t>& scratchBuffer,
    EtwJsonSuffixFlags jsonSuffixFlags) noexcept
{
    ASSERT(m_state == EtwEnumeratorState_BeforeFirstItem);

    auto& desc = m_pEventRecord->EventHeader.EventDescriptor;
    bool needComma = false;

    // items start
    CheckOutOfMem(m_lastError, output.push_back(L'{'));

    {
        auto const oldOutputSize = output.size();
        CheckAdd(AddCurrentItemAsJsonAndMoveNext(
            output, scratchBuffer, EtwJsonItemFlags_Name));
        needComma = oldOutputSize != output.size();
    }

    if (jsonSuffixFlags != 0)
    {
        if (needComma)
        {
            CheckOutOfMem(m_lastError, output.push_back(L','));
        }

        // meta start
        CheckWin32(m_lastError, AppendLiteral(output, LR"("meta":{)"));
        needComma = false;

#define APPEND_COMMA(output) \
            if (needComma) { CheckOutOfMem(m_lastError, output.push_back(L',')); } \
            else { needComma = true; } \

        if (jsonSuffixFlags & EtwJsonSuffixFlags_provider)
        {
            APPEND_COMMA(output);
            CheckWin32(m_lastError, AppendLiteral(output, LR"("provider":)"));
            if (m_pTraceEventInfo->ProviderNameOffset)
            {
                CheckWin32(m_lastError, AppendStringAsJson(output,
                    TeiStringNoCheck(m_pTraceEventInfo->ProviderNameOffset)));
            }
            else
            {
                CheckOutOfMem(m_lastError, output.push_back(L'"'));
                CheckWin32(m_lastError, AppendCurrentProviderNameFallback(output));
                CheckOutOfMem(m_lastError, output.push_back(L'"'));
            }
        }

        if (jsonSuffixFlags & EtwJsonSuffixFlags_event)
        {
            APPEND_COMMA(output);
            CheckWin32(m_lastError, AppendLiteral(output, LR"("event":)"));
            LPCWSTR const eventName = EventName();
            if (eventName)
            {
                CheckWin32(m_lastError, AppendStringAsJson(output, eventName));
            }
            else
            {
                // No EventName set. Use fallback.
                unsigned const oldSize = scratchBuffer.size();
                CheckWin32(m_lastError, AppendCurrentEventNameFallback(scratchBuffer));
                CheckWin32(m_lastError, AppendStringAsJson(output,
                    scratchBuffer.data() + oldSize,
                    scratchBuffer.size() - oldSize));
                scratchBuffer.resize_unchecked(oldSize);
            }
        }

        if (jsonSuffixFlags & EtwJsonSuffixFlags_time)
        {
            APPEND_COMMA(output);
            CheckWin32(m_lastError, AppendLiteral(output, LR"("time":")"));
            CheckWin32(m_lastError, AppendFileTime(
                output,
                m_pEventRecord->EventHeader.TimeStamp.QuadPart,
                static_cast<EtwTimestampFormat>(
                    EtwTimestampFormat_Internet | (m_timestampFormat & EtwTimestampFormat_FlagMask)),
                m_timeZoneBiasMinutes,
                true));
            CheckOutOfMem(m_lastError, output.push_back(L'"'));
        }

        if (jsonSuffixFlags & EtwJsonSuffixFlags_cpu)
        {
            APPEND_COMMA(output);
            CheckWin32(m_lastError, AppendPrintf(output, LR"("cpu":%u)",
                GetEventProcessorIndex(m_pEventRecord)));
        }

        if ((jsonSuffixFlags & EtwJsonSuffixFlags_pid) &&
            m_pEventRecord->EventHeader.ProcessId != 0xffffffff)
        {
            APPEND_COMMA(output);
            CheckWin32(m_lastError, AppendPrintf(output, LR"("pid":%u)",
                m_pEventRecord->EventHeader.ProcessId));
        }

        if ((jsonSuffixFlags & EtwJsonSuffixFlags_tid) &&
            m_pEventRecord->EventHeader.ThreadId != 0xffffffff)
        {
            APPEND_COMMA(output);
            CheckWin32(m_lastError, AppendPrintf(output, LR"("tid":%u)",
                m_pEventRecord->EventHeader.ThreadId));
        }

        if (jsonSuffixFlags & EtwJsonSuffixFlags_id)
        {
            APPEND_COMMA(output);
            CheckWin32(m_lastError, AppendPrintf(output, LR"("id":%u)",
                desc.Id));
        }

        if ((jsonSuffixFlags & EtwJsonSuffixFlags_version) &&
            desc.Version != 0)
        {
            APPEND_COMMA(output);
            CheckWin32(m_lastError, AppendPrintf(output, LR"("version":%u)",
                desc.Version));
        }

        if ((jsonSuffixFlags & EtwJsonSuffixFlags_channel) &&
            desc.Channel != 0)
        {
            APPEND_COMMA(output);
            CheckWin32(m_lastError, AppendLiteral(output, LR"("channel":)"));
            if (m_pTraceEventInfo->ChannelNameOffset != 0)
            {
                CheckWin32(m_lastError, AppendStringAsJson(output,
                    TeiStringNoCheck(m_pTraceEventInfo->ChannelNameOffset)));
            }
            else
            {
                CheckWin32(m_lastError, AppendPrintf(output, L"%u", desc.Channel));
            }
        }

        if ((jsonSuffixFlags & EtwJsonSuffixFlags_level) &&
            desc.Level != 0)
        {
            APPEND_COMMA(output);
            CheckWin32(m_lastError, AppendLiteral(output, LR"("level":)"));
            if (m_pTraceEventInfo->LevelNameOffset != 0)
            {
                CheckWin32(m_lastError, AppendStringAsJson(output,
                    TeiStringNoCheck(m_pTraceEventInfo->LevelNameOffset)));
            }
            else
            {
                CheckWin32(m_lastError, AppendPrintf(output, L"%u", desc.Level));
            }
        }

        if ((jsonSuffixFlags & EtwJsonSuffixFlags_opcode) &&
            desc.Opcode != 0 &&
            // Note: for Wbem, opcode is the event name, so don't show an opcode property.
            m_pTraceEventInfo->DecodingSource != DecodingSourceWbem)
        {
            APPEND_COMMA(output);
            CheckWin32(m_lastError, AppendLiteral(output, LR"("opcode":)"));
            auto const opcodeName = OpcodeName();
            if (opcodeName)
            {
                CheckWin32(m_lastError, AppendStringAsJson(output, opcodeName));
            }
            else
            {
                CheckWin32(m_lastError, AppendPrintf(output, L"%u", desc.Opcode));
            }
        }

        if ((jsonSuffixFlags & EtwJsonSuffixFlags_task) &&
            // Note: for Wbem, we show task name even if task is 0.
            (desc.Task != 0 || m_pTraceEventInfo->DecodingSource == DecodingSourceWbem))
        {
            APPEND_COMMA(output);
            CheckWin32(m_lastError, AppendLiteral(output, LR"("task":)"));
            auto const taskName = TaskName();
            if (taskName)
            {
                CheckWin32(m_lastError, AppendStringAsJson(output, taskName));
            }
            else
            {
                CheckWin32(m_lastError, AppendPrintf(output, L"%u", desc.Task));
            }
        }

        if ((jsonSuffixFlags & EtwJsonSuffixFlags_keywords) &&
            desc.Keyword != 0)
        {
            APPEND_COMMA(output);
            CheckWin32(m_lastError, AppendLiteral(output, LR"("keywords":)"));
            if (m_pTraceEventInfo->KeywordsNameOffset != 0)
            {
                CheckWin32(m_lastError, AppendStringAsJson(output,
                    TeiStringNoCheck(m_pTraceEventInfo->KeywordsNameOffset)));
            }
            else
            {
                CheckWin32(m_lastError, AppendPrintf(output, LR"("0x%llX")", desc.Keyword));
            }
        }

        if ((jsonSuffixFlags & EtwJsonSuffixFlags_tags) &&
            m_pTraceEventInfo->Tags != 0)
        {
            APPEND_COMMA(output);
            CheckWin32(m_lastError, AppendPrintf(output, LR"("tags":"0x%X")",
                m_pTraceEventInfo->Tags));
        }

        if ((jsonSuffixFlags & EtwJsonSuffixFlags_activity) &&
            m_pEventRecord->EventHeader.ActivityId != GUID())
        {
            APPEND_COMMA(output);
            auto& g = m_pEventRecord->EventHeader.ActivityId;
            CheckWin32(m_lastError, AppendPrintf(output,
                LR"("activity":")" GUID_PRINTF_FORMAT_UPPER L"\"",
                GUID_PRINTF_VALUE(g)));
        }

        if (jsonSuffixFlags & EtwJsonSuffixFlags_relatedActivity)
        {
            for (unsigned i = 0; i != m_pEventRecord->ExtendedDataCount; i++)
            {
                if (m_pEventRecord->ExtendedData[i].ExtType == EVENT_HEADER_EXT_TYPE_RELATED_ACTIVITYID &&
                    m_pEventRecord->ExtendedData[i].DataSize == 16)
                {
                    APPEND_COMMA(output);
                    auto& g = *reinterpret_cast<GUID const*>(
                        static_cast<UINT_PTR>(m_pEventRecord->ExtendedData[i].DataPtr));
                    CheckWin32(m_lastError, AppendPrintf(output,
                        LR"("relatedActivity":")" GUID_PRINTF_FORMAT_UPPER L"\"",
                        GUID_PRINTF_VALUE(g)));
                    break;
                }
            }
        }

        if (0 != (m_pEventRecord->EventHeader.Flags & EVENT_HEADER_FLAG_PRIVATE_SESSION))
        {
            // ptime is only valid for private sessions where ProviderId != EventTraceGuid.
            if ((jsonSuffixFlags & EtwJsonSuffixFlags_ptime) &&
                m_pEventRecord->EventHeader.ProviderId != EventTraceGuid)
            {
                APPEND_COMMA(output);
                CheckWin32(m_lastError, AppendPrintf(output, LR"("ptime":%llu)",
                    m_pEventRecord->EventHeader.ProcessorTime));
            }
        }
        else if (0 == (m_pEventRecord->EventHeader.Flags & EVENT_HEADER_FLAG_NO_CPUTIME))
        {
            if (jsonSuffixFlags & EtwJsonSuffixFlags_ktime)
            {
                APPEND_COMMA(output);
                CheckWin32(m_lastError, AppendPrintf(output, LR"("ktime":%u)",
                    TicksToMilliseconds(m_pEventRecord->EventHeader.KernelTime)));
            }

            if (jsonSuffixFlags & EtwJsonSuffixFlags_utime)
            {
                APPEND_COMMA(output);
                CheckWin32(m_lastError, AppendPrintf(output, LR"("utime":%u)",
                    TicksToMilliseconds(m_pEventRecord->EventHeader.UserTime)));
            }
        }

        if (jsonSuffixFlags & EtwJsonSuffixFlags_attribs)
        {
            LPCWSTR const eventAttributes = EventAttributes();
            if (eventAttributes)
            {
                APPEND_COMMA(output);
                CheckWin32(m_lastError, AppendLiteral(output, LR"("attribs":)"));
                CheckWin32(m_lastError, AppendStringAsJson(output, eventAttributes));
            }
        }

        // meta end
        CheckOutOfMem(m_lastError, output.push_back(L'}'));
    }

    // items end
    CheckOutOfMem(m_lastError, output.push_back(L'}'));

    m_lastError = ERROR_SUCCESS;

Done:

    return m_lastError == ERROR_SUCCESS;
}

bool
EtwEnumerator::AddCurrentItemAsJsonAndMoveNext(
    EtwInternal::Buffer<wchar_t>& output,
    EtwInternal::Buffer<wchar_t>& scratchBuffer,
    EtwJsonItemFlags jsonItemFlags) noexcept
{
    ASSERT(
        m_state == EtwEnumeratorState_BeforeFirstItem ||
        m_state == EtwEnumeratorState_Value ||
        m_state == EtwEnumeratorState_ArrayBegin ||
        m_state == EtwEnumeratorState_StructBegin);

    int depth = 0;
    bool wantComma = false;
    bool includeName = (jsonItemFlags & EtwJsonItemFlags_Name) != 0;
    bool const wantSpace = (jsonItemFlags & EtwJsonItemFlags_Space) != 0;
    unsigned const cchComma = wantSpace ? 2 : 1;
    PCWSTR const pchComma = L", ";

    if (m_state == EtwEnumeratorState_BeforeFirstItem)
    {
        depth += 1;
        includeName = true;
        if (!MoveNext())
        {
            goto Done;
        }
    }

    do
    {
        switch (m_state)
        {
        case EtwEnumeratorState_Value:

            if (wantComma)
            {
                CheckWin32(m_lastError, AppendWide(output, pchComma, cchComma));
            }

            if (!m_stackTop.IsArray && includeName)
            {
                CheckWin32(m_lastError, AppendCurrentNameAsJson(output, wantSpace));
            }

            CheckAdd(AddCurrentValueAsJson(output, scratchBuffer));

            wantComma = true;
            break;

        case EtwEnumeratorState_ArrayBegin:

            if (wantComma)
            {
                CheckWin32(m_lastError, AppendWide(output, pchComma, cchComma));
            }

            if (includeName)
            {
                CheckWin32(m_lastError, AppendCurrentNameAsJson(output, wantSpace));
            }

            CheckOutOfMem(m_lastError, output.push_back(L'['));

            depth += 1;
            wantComma = false;
            break;

        case EtwEnumeratorState_ArrayEnd:

            CheckOutOfMem(m_lastError, output.push_back(L']'));

            depth -= 1;
            wantComma = true;
            break;

        case EtwEnumeratorState_StructBegin:

            if (wantComma)
            {
                CheckWin32(m_lastError, AppendWide(output, pchComma, cchComma));
            }

            if (!m_stackTop.IsArray && includeName)
            {
                CheckWin32(m_lastError, AppendCurrentNameAsJson(output, wantSpace));
            }

            CheckOutOfMem(m_lastError, output.push_back(L'{'));

            depth += 1;
            wantComma = false;
            break;

        case EtwEnumeratorState_StructEnd:

            CheckOutOfMem(m_lastError, output.push_back(L'}'));

            depth -= 1;
            wantComma = true;
            break;

        default:

            m_lastError = ERROR_INVALID_STATE;
            goto Done;
        }

        includeName = true;
    } while (MoveNext() && depth > 0);

Done:

    return m_lastError == ERROR_SUCCESS;
}

bool
EtwEnumerator::AddCurrentValueAsJson(
    EtwInternal::Buffer<wchar_t>& output,
    EtwInternal::Buffer<wchar_t>& scratchBuffer) noexcept
{
    ASSERT(m_state == EtwEnumeratorState_Value);

    auto const scratchOldSize = scratchBuffer.size();

    auto const result = AddCurrentValue(scratchBuffer);
    auto const pchValue = scratchBuffer.data() + scratchOldSize;
    auto const cchValue = scratchBuffer.size() - scratchOldSize;
    switch (result)
    {
    case ValueType_JsonCleanString:
    {
        ASSERT(m_lastError == ERROR_SUCCESS);
        auto const oldSize = output.size();
        if (!output.resize(oldSize + cchValue + 2))
        {
            m_lastError = ERROR_OUTOFMEMORY;
        }
        else
        {
            auto pchDst = output.data() + oldSize;
            *pchDst = L'"';
            pchDst += 1;
            memcpy(pchDst, pchValue, cchValue * 2);
            pchDst += cchValue;
            *pchDst = L'"';
            m_lastError = ERROR_SUCCESS;
        }
        break;
    }
    case ValueType_JsonString:

        ASSERT(m_lastError == ERROR_SUCCESS);
        m_lastError = AppendStringAsJson(output, pchValue, cchValue);
        break;

    case ValueType_JsonLiteral:

        ASSERT(m_lastError == ERROR_SUCCESS);
        m_lastError = AppendWide(output, pchValue, cchValue);
        break;

    default:

        ASSERT(m_lastError != ERROR_SUCCESS);
        break;
    }

    scratchBuffer.resize_unchecked(scratchOldSize);
    return m_lastError == ERROR_SUCCESS;
}

EtwEnumerator::ValueType
EtwEnumerator::AddCurrentValue(
    Buffer& output) noexcept
{
    ASSERT(m_state == EtwEnumeratorState_Value);

    ValueType result;

    auto& epi = m_pTraceEventInfo->EventPropertyInfoArray[m_stackTop.PropertyIndex];
    if (epi.nonStructType.MapNameOffset == 0 ||
        0 != (epi.Flags & (PropertyHasCustomSchema | PropertyStruct)))
    {
        // No map name.
        result = AddValue(
            output,
            m_pbCooked,
            m_cbCooked,
            static_cast<_TDH_IN_TYPE>(m_cookedInType),
            static_cast<_TDH_OUT_TYPE>(epi.nonStructType.OutType));
    }
    else
    {
        // Has a map name.
        LPCWSTR pMapName = reinterpret_cast<LPCWSTR>(
            reinterpret_cast<BYTE const*>(m_pTraceEventInfo) + epi.nonStructType.MapNameOffset);
        result = AddValueWithMapName(
            output,
            m_pbCooked,
            m_cbCooked,
            static_cast<_TDH_IN_TYPE>(m_cookedInType),
            static_cast<_TDH_OUT_TYPE>(epi.nonStructType.OutType),
            m_pEventRecord,
            pMapName);
    }

    return result;
}

EtwEnumerator::ValueType
EtwEnumerator::AddValueWithMapName(
    Buffer& output,
    _In_reads_bytes_(cbData) void const* pData,
    unsigned cbData,
    _TDH_IN_TYPE inType,
    _TDH_OUT_TYPE outType,
    _In_ EVENT_RECORD const* pEventRecord,
    _In_opt_ EtwPCWSTR pMapName) noexcept
{
    ValueType result;

    if (pMapName != nullptr)
    {
        for (;;)
        {
            ULONG cbMapInfo = m_mapBuffer.capacity();
            EVENT_MAP_INFO* pMapInfo = reinterpret_cast<EVENT_MAP_INFO*>(m_mapBuffer.data());

            m_lastError = m_enumeratorCallbacks.GetEventMapInformation(
                pEventRecord,
                pMapName,
                pMapInfo,
                &cbMapInfo);
            if (m_lastError == ERROR_SUCCESS)
            {
                // Found map information. Use it to format value.
                result = AddValueWithMapInfo(
                    output,
                    pData,
                    cbData,
                    inType,
                    outType,
                    pMapInfo);
                goto Done;
            }
            else if (m_lastError == ERROR_NOT_FOUND)
            {
                break;
            }
            else if (
                m_lastError != ERROR_INSUFFICIENT_BUFFER ||
                m_mapBuffer.capacity() >= cbMapInfo)
            {
                // If we return ERROR_INSUFFICIENT_BUFFER it means
                // GetEventMapInformation has a bug (it did not set cbMapInfo correctly).
                ASSERT(m_lastError != ERROR_INSUFFICIENT_BUFFER);
                result = ValueType_None;
                goto Done;
            }
            else if (!m_mapBuffer.reserve(cbMapInfo))
            {
                m_lastError = ERROR_OUTOFMEMORY;
                result = ValueType_None;
                goto Done;
            }
        }
    }

    // Did not find map information. Format without it.
    result = AddValue(output, pData, cbData, inType, outType);

Done:

    return result;
}

EtwEnumerator::ValueType
EtwEnumerator::AddValueWithMapInfo(
    Buffer& output,
    _In_reads_bytes_(cbData) void const* pData,
    unsigned cbData,
    _TDH_IN_TYPE inType,
    _TDH_OUT_TYPE outType,
    _In_opt_ EVENT_MAP_INFO const* pMapInfo) noexcept
{
    UINT32 value;
    EtwEnumeratorCallbacks::UnderlyingType valueType;

    if (pMapInfo == nullptr)
    {
        goto NoMapInfo;
    }

    switch (inType)
    {
    case TDH_INTYPE_UINT8:
        if (cbData != 1)
        {
            goto NoMapInfo;
        }
        value = *static_cast<UINT8 const*>(pData);
        valueType = outType == TDH_OUTTYPE_HEXINT8
            ? EtwEnumeratorCallbacks::UnderlyingTypeHexadecimal
            : EtwEnumeratorCallbacks::UnderlyingTypeUnsigned;
        break;

    case TDH_INTYPE_UINT16:
        if (cbData != 2)
        {
            goto NoMapInfo;
        }
        value = *static_cast<UINT16 const UNALIGNED*>(pData);
        valueType = outType == TDH_OUTTYPE_HEXINT16
            ? EtwEnumeratorCallbacks::UnderlyingTypeHexadecimal
            : EtwEnumeratorCallbacks::UnderlyingTypeUnsigned;
        break;

    case TDH_INTYPE_UINT32:
        if (cbData != 4)
        {
            goto NoMapInfo;
        }
        value = *static_cast<UINT32 const UNALIGNED*>(pData);
        valueType = outType == TDH_OUTTYPE_HEXINT32
            ? EtwEnumeratorCallbacks::UnderlyingTypeHexadecimal
            : EtwEnumeratorCallbacks::UnderlyingTypeUnsigned;
        break;

    case TDH_INTYPE_HEXINT32:
        if (cbData != 4)
        {
            goto NoMapInfo;
        }
        value = *static_cast<UINT32 const UNALIGNED*>(pData);
        valueType = EtwEnumeratorCallbacks::UnderlyingTypeHexadecimal;
        break;

    default:
        goto NoMapInfo;
    }

    {
        EtwStringBuilder outputBuilder(output);
        m_lastError = m_enumeratorCallbacks.FormatMapValue(pMapInfo, valueType, value, outputBuilder);
    }

    ValueType result;
    if (m_lastError == ERROR_SUCCESS)
    {
        result = ValueType_JsonString;
    }
    else if (m_lastError != ERROR_NOT_FOUND)
    {
        result = ValueType_None;
    }
    else
    {
    NoMapInfo:
        // Fall back to non-map formatting.
        result = AddValue(output, pData, cbData, inType, outType);
    }

    return result;
}

EtwEnumerator::ValueType
EtwEnumerator::AddValue(
    Buffer& output,
    _In_reads_bytes_(cbData) void const* pData,
    unsigned cbData,
    _TDH_IN_TYPE inType,
    _TDH_OUT_TYPE outType) noexcept
{
    ValueType type;

#pragma warning(push)
#pragma warning(disable: 4063) // case '...' is not a valid value for switch of enum '...'

    switch (inType)
    {
    case TDH_INTYPE_NULL:
        m_lastError = ERROR_SUCCESS;
        type = ValueType_JsonCleanString;
        break;

    case TDH_INTYPE_UNICODESTRING:
    case TDH_InTypeManifestCountedString:
    case TDH_INTYPE_COUNTEDSTRING:
    case TDH_INTYPE_REVERSEDCOUNTEDSTRING:
    case TDH_INTYPE_NONNULLTERMINATEDSTRING:
    case TDH_INTYPE_UNICODECHAR:
        switch (outType)
        {
        default:
        case TDH_OUTTYPE_STRING:
        case TDH_OUTTYPE_XML:
        case TDH_OUTTYPE_JSON:
            m_lastError = AppendWide(output, static_cast<LPCWCH>(pData), cbData / 2);
            type = ValueType_JsonString;
            break;
        }
        break;

    case TDH_INTYPE_ANSISTRING:
    case TDH_InTypeManifestCountedAnsiString:
    case TDH_INTYPE_COUNTEDANSISTRING:
    case TDH_INTYPE_REVERSEDCOUNTEDANSISTRING:
    case TDH_INTYPE_NONNULLTERMINATEDANSISTRING:
    case TDH_INTYPE_ANSICHAR:
        switch (outType)
        {
        default:
        case TDH_OUTTYPE_STRING:
            m_lastError = AppendMbcs(output, static_cast<LPCCH>(pData), cbData, CP_ACP);
            type = ValueType_JsonString;
            break;
        case TDH_OUTTYPE_XML:
        case TDH_OUTTYPE_JSON:
        case TDH_OUTTYPE_UTF8:
            m_lastError = AppendMbcs(output, static_cast<LPCCH>(pData), cbData, CP_UTF8);
            type = ValueType_JsonString;
            break;
        }
        break;

    case TDH_INTYPE_INT8:
        if (cbData != 1)
        {
            m_lastError = ERROR_INVALID_PARAMETER;
            type = ValueType_None;
        }
        else switch (outType)
        {
        default:
            m_lastError = AppendPrintf(output, L"%d", *static_cast<INT8 const*>(pData));
            type = ValueType_JsonLiteral;
            break;
        case TDH_OUTTYPE_STRING:
            m_lastError = AppendMbcs(output, static_cast<LPCCH>(pData), cbData, CP_ACP);
            type = ValueType_JsonString;
            break;
        }
        break;

    case TDH_INTYPE_UINT8:
        if (cbData != 1)
        {
            m_lastError = ERROR_INVALID_PARAMETER;
            type = ValueType_None;
        }
        else switch (outType)
        {
        default:
            m_lastError = AppendPrintf(output, L"%u", *static_cast<UINT8 const*>(pData));
            type = ValueType_JsonLiteral;
            break;
        case TDH_OUTTYPE_HEXINT8:
            m_lastError = AppendPrintf(output, L"0x%X", *static_cast<UINT8 const*>(pData));
            type = ValueType_JsonCleanString;
            break;
        case TDH_OUTTYPE_STRING:
            m_lastError = AppendMbcs(output, static_cast<LPCCH>(pData), cbData, CP_ACP);
            type = ValueType_JsonString;
            break;
        }
        break;

    case TDH_INTYPE_INT16:
        if (cbData != 2)
        {
            m_lastError = ERROR_INVALID_PARAMETER;
            type = ValueType_None;
        }
        else
        {
            m_lastError = AppendPrintf(output, L"%d", *static_cast<INT16 const UNALIGNED*>(pData));
            type = ValueType_JsonLiteral;
        }
        break;

    case TDH_INTYPE_UINT16:
        if (cbData != 2)
        {
            m_lastError = ERROR_INVALID_PARAMETER;
            type = ValueType_None;
        }
        else switch (outType)
        {
        default:
            m_lastError = AppendPrintf(output, L"%u", *static_cast<UINT16 const UNALIGNED*>(pData));
            type = ValueType_JsonLiteral;
            break;
        case TDH_OUTTYPE_HEXINT16:
            m_lastError = AppendPrintf(output, L"0x%X", *static_cast<UINT16 const UNALIGNED*>(pData));
            type = ValueType_JsonCleanString;
            break;
        case TDH_OUTTYPE_PORT:
            m_lastError = AppendPrintf(output, L"%u",
                _byteswap_ushort(*static_cast<UINT16 const UNALIGNED*>(pData)));
            type = ValueType_JsonLiteral;
            break;
        case TDH_OUTTYPE_STRING:
            m_lastError = AppendWide(output, static_cast<LPCWCH>(pData), cbData / 2);
            type = ValueType_JsonString;
            break;
        }
        break;

    case TDH_INTYPE_INT32:
        if (cbData != 4)
        {
            m_lastError = ERROR_INVALID_PARAMETER;
            type = ValueType_None;
        }
        else switch (outType)
        {
        default:
        DefaultINT32:
            m_lastError = AppendPrintf(output, L"%d", *static_cast<INT32 const UNALIGNED*>(pData));
            type = ValueType_JsonLiteral;
            break;

        case TDH_OUTTYPE_HRESULT:
            m_lastError = AppendResultCode(
                output, pData,
                EtwEnumeratorCallbacks::ResultCodeDomainHRESULT,
                EtwEnumeratorCallbacks::UnderlyingTypeHexadecimal); // Always hex
            if (m_lastError == ERROR_NOT_FOUND)
            {
                goto DefaultINT32;
            }
            type = ValueType_JsonString;
            break;
        }
        break;

    case TDH_INTYPE_UINT32:
        if (cbData != 4)
        {
            m_lastError = ERROR_INVALID_PARAMETER;
            type = ValueType_None;
        }
        else switch (outType)
        {
        default:
        DefaultUINT32:
        case TDH_OUTTYPE_ETWTIME:
        case TDH_OUTTYPE_PID:
        case TDH_OUTTYPE_TID:
            m_lastError = AppendPrintf(output, L"%u", *static_cast<UINT32 const UNALIGNED*>(pData));
            type = ValueType_JsonLiteral;
            break;
        case TDH_OUTTYPE_WIN32ERROR:
            m_lastError = AppendResultCode(
                output, pData,
                EtwEnumeratorCallbacks::ResultCodeDomainWIN32,
                EtwEnumeratorCallbacks::UnderlyingTypeUnsigned); // Follow intype
            if (m_lastError == ERROR_NOT_FOUND)
            {
                goto DefaultUINT32;
            }
            type = ValueType_JsonString;
            break;
        case TDH_OUTTYPE_NTSTATUS:
            m_lastError = AppendResultCode(
                output, pData,
                EtwEnumeratorCallbacks::ResultCodeDomainNTSTATUS,
                EtwEnumeratorCallbacks::UnderlyingTypeHexadecimal); // Always hex
            if (m_lastError == ERROR_NOT_FOUND)
            {
                goto DefaultUINT32;
            }
            type = ValueType_JsonString;
            break;
        case TDH_OUTTYPE_HEXINT32:
        case TDH_OUTTYPE_ERRORCODE:
        case TDH_OutTypeCodePointer:
            m_lastError = AppendPrintf(output, L"0x%X", *static_cast<UINT32 const UNALIGNED*>(pData));
            type = ValueType_JsonCleanString;
            break;
        case TDH_OUTTYPE_IPV4:
            m_lastError = AppendIPv4(output, pData);
            type = ValueType_JsonCleanString;
            break;
        }
        break;

    case TDH_INTYPE_HEXINT32:
    DefaultHEXINT32:
        if (cbData != 4)
        {
            m_lastError = ERROR_INVALID_PARAMETER;
            type = ValueType_None;
        }
        else switch (outType)
        {
        default:
            m_lastError = AppendPrintf(output, L"0x%X", *static_cast<UINT32 const UNALIGNED*>(pData));
            type = ValueType_JsonCleanString;
            break;
        case TDH_OUTTYPE_WIN32ERROR:
            m_lastError = AppendResultCode(
                output, pData,
                EtwEnumeratorCallbacks::ResultCodeDomainWIN32,
                EtwEnumeratorCallbacks::UnderlyingTypeHexadecimal); // Follow intype
            if (m_lastError == ERROR_NOT_FOUND)
            {
                goto DefaultHEXINT32;
            }
            type = ValueType_JsonString;
            break;
        case TDH_OUTTYPE_NTSTATUS:
            m_lastError = AppendResultCode(
                output, pData,
                EtwEnumeratorCallbacks::ResultCodeDomainNTSTATUS,
                EtwEnumeratorCallbacks::UnderlyingTypeHexadecimal); // Always hex
            if (m_lastError == ERROR_NOT_FOUND)
            {
                goto DefaultHEXINT32;
            }
            type = ValueType_JsonString;
            break;
        }
        break;

    case TDH_INTYPE_INT64:
        if (cbData != 8)
        {
            m_lastError = ERROR_INVALID_PARAMETER;
            type = ValueType_None;
        }
        else
        {
            m_lastError = AppendPrintf(output, L"%lld", *static_cast<INT64 const UNALIGNED*>(pData));
            type = ValueType_JsonLiteral;
            break;
        }
        break;

    case TDH_INTYPE_UINT64:
        if (cbData != 8)
        {
            m_lastError = ERROR_INVALID_PARAMETER;
            type = ValueType_None;
        }
        else switch (outType)
        {
        default:
        case TDH_OUTTYPE_ETWTIME:
            m_lastError = AppendPrintf(output, L"%llu", *static_cast<UINT64 const UNALIGNED*>(pData));
            type = ValueType_JsonLiteral;
            break;
        case TDH_OUTTYPE_HEXINT64:
        case TDH_OutTypeCodePointer:
            m_lastError = AppendPrintf(output, L"0x%llX", *static_cast<UINT64 const UNALIGNED*>(pData));
            type = ValueType_JsonCleanString;
            break;
        }
        break;

    case TDH_INTYPE_HEXINT64:
        if (cbData != 8)
        {
            m_lastError = ERROR_INVALID_PARAMETER;
            type = ValueType_None;
        }
        else
        {
            m_lastError = AppendPrintf(output, L"0x%llX", *static_cast<UINT64 const UNALIGNED*>(pData));
            type = ValueType_JsonCleanString;
        }
        break;

    case TDH_INTYPE_FLOAT:
        if (cbData != 4)
        {
            m_lastError = ERROR_INVALID_PARAMETER;
            type = ValueType_None;
        }
        else
        {
            auto const MantissaMask = 0x7f800000u; // 8 bits of exponent
            m_lastError = AppendPrintf(output, L"%g", *static_cast<float const UNALIGNED*>(pData));
            type = MantissaMask == (*static_cast<UINT32 UNALIGNED const*>(pData) & MantissaMask)
                ? ValueType_JsonCleanString // Infinity or NaN
                : ValueType_JsonLiteral;
        }
        break;

    case TDH_INTYPE_DOUBLE:
        if (cbData != 8)
        {
            m_lastError = ERROR_INVALID_PARAMETER;
            type = ValueType_None;
        }
        else
        {
            auto const MantissaMask = 0x7ff0000000000000u; // 11 bits of exponent
            m_lastError = AppendPrintf(output, L"%g", *static_cast<double const UNALIGNED*>(pData));
            type = MantissaMask == (*static_cast<UINT64 UNALIGNED const*>(pData) & MantissaMask)
                ? ValueType_JsonCleanString // Infinity or NaN
                : ValueType_JsonLiteral;
        }
        break;

    case TDH_INTYPE_BOOLEAN:
        if (cbData != 4)
        {
            m_lastError = ERROR_INVALID_PARAMETER;
            type = ValueType_None;
        }
        else
        {
            m_lastError = AppendBoolean(output, *static_cast<INT32 const UNALIGNED*>(pData));
            type = ValueType_JsonLiteral;
        }
        break;

    case TDH_INTYPE_POINTER:
    case TDH_INTYPE_SIZET:
        if (cbData == 8)
        {
            switch (outType)
            {
            default:
            case TDH_OutTypeCodePointer:
                m_lastError = AppendPrintf(output, L"0x%llX", *static_cast<UINT64 const UNALIGNED*>(pData));
                type = ValueType_JsonCleanString;
                break;
            case TDH_OUTTYPE_LONG:
                m_lastError = AppendPrintf(output, L"%lld", *static_cast<UINT64 const UNALIGNED*>(pData));
                type = ValueType_JsonLiteral;
                break;
            case TDH_OUTTYPE_UNSIGNEDLONG:
                m_lastError = AppendPrintf(output, L"%llu", *static_cast<UINT64 const UNALIGNED*>(pData));
                type = ValueType_JsonLiteral;
                break;
            }
        }
        else if (cbData == 4)
        {
            switch (outType)
            {
            default:
            case TDH_OutTypeCodePointer:
                m_lastError = AppendPrintf(output, L"0x%X", *static_cast<UINT32 const UNALIGNED*>(pData));
                type = ValueType_JsonCleanString;
                break;
            case TDH_OUTTYPE_LONG:
                m_lastError = AppendPrintf(output, L"%d", *static_cast<UINT32 const UNALIGNED*>(pData));
                type = ValueType_JsonLiteral;
                break;
            case TDH_OUTTYPE_UNSIGNEDLONG:
                m_lastError = AppendPrintf(output, L"%u", *static_cast<UINT32 const UNALIGNED*>(pData));
                type = ValueType_JsonLiteral;
                break;
            }
        }
        else
        {
            m_lastError = ERROR_INVALID_PARAMETER;
            type = ValueType_None;
        }
        break;

    case TDH_INTYPE_BINARY:
    case TDH_InTypeManifestCountedBinary:
    case TDH_INTYPE_HEXDUMP:
    default:
        switch (outType)
        {
        default:
        case TDH_OUTTYPE_HEXBINARY:
            m_lastError = AppendHexDump(output, pData, cbData);
            type = ValueType_JsonCleanString;
            break;
        case TDH_OUTTYPE_IPV6:
            if (cbData == 16)
            {
                m_lastError = AppendIPv6(output, pData);
                type = ValueType_JsonCleanString;
            }
            else
            {
                m_lastError = AppendHexDump(output, pData, cbData);
                type = ValueType_JsonCleanString;
            }
            break;
        case TDH_OUTTYPE_SOCKETADDRESS:
            m_lastError = AppendSockAddr(output, pData, cbData);
            type = ValueType_JsonCleanString;
            break;
        }
        break;

    case TDH_INTYPE_GUID:
        if (cbData != 16)
        {
            m_lastError = ERROR_INVALID_PARAMETER;
            type = ValueType_None;
        }
        else
        {
            auto pGuid = static_cast<GUID const UNALIGNED*>(pData);
            m_lastError = AppendPrintf(output, L"{" GUID_PRINTF_FORMAT_LOWER L"}",
                GUID_PRINTF_VALUE(*pGuid));
            type = ValueType_JsonCleanString;
        }
        break;

    case TDH_INTYPE_FILETIME:
        if (cbData != 8)
        {
            m_lastError = ERROR_INVALID_PARAMETER;
            type = ValueType_None;
        }
        else
        {
            bool const timeIsUtc = outType == TDH_OutTypeDateTimeUtc ||
                0 != (m_timestampFormat & EtwTimestampFormat_AssumeFileTimeUTC);
            m_lastError = AppendFileTime(output, *static_cast<UINT64 const UNALIGNED*>(pData),
                m_timestampFormat, m_timeZoneBiasMinutes, timeIsUtc);
            type = ValueType_JsonCleanString;
        }
        break;

    case TDH_INTYPE_SYSTEMTIME:
        if (cbData != 16)
        {
            m_lastError = ERROR_INVALID_PARAMETER;
            type = ValueType_None;
        }
        else
        {
            bool const timeIsUtc = outType == TDH_OutTypeDateTimeUtc;
            m_lastError = AppendSystemTime(output, *static_cast<SYSTEMTIME const UNALIGNED*>(pData),
                m_timestampFormat, m_timeZoneBiasMinutes, timeIsUtc);
            type = ValueType_JsonCleanString;
        }
        break;

    case TDH_INTYPE_SID:
    case TDH_INTYPE_WBEMSID:
        if (cbData < 8)
        {
            m_lastError = ERROR_INVALID_PARAMETER;
            type = ValueType_None;
        }
        else
        {
            auto pSid = static_cast<SID const UNALIGNED*>(pData);
            if (pSid->Revision != 1 ||
                cbData != 8u + 4u * pSid->SubAuthorityCount)
            {
                m_lastError = AppendHexDump(output, pData, cbData);
            }
            else
            {
                LARGE_INTEGER ia;
                ia.HighPart =
                    (pSid->IdentifierAuthority.Value[0] << 8) |
                    (pSid->IdentifierAuthority.Value[1] << 0);
                ia.LowPart =
                    (pSid->IdentifierAuthority.Value[2] << 24) |
                    (pSid->IdentifierAuthority.Value[3] << 16) |
                    (pSid->IdentifierAuthority.Value[4] << 8) |
                    (pSid->IdentifierAuthority.Value[5] << 0);
                m_lastError = ia.HighPart != 0
                    ? AppendPrintf(output, L"S-1-0x%llX", ia.QuadPart)
                    : AppendPrintf(output, L"S-1-%u", ia.LowPart);
                for (unsigned i = 0; m_lastError == ERROR_SUCCESS && i != pSid->SubAuthorityCount; i++)
                {
                    m_lastError = AppendPrintf(output, L"-%u", pSid->SubAuthority[i]);
                }
            }
            type = ValueType_JsonCleanString;
        }
        break;
    }

#pragma warning(pop)

    return m_lastError == ERROR_SUCCESS
        ? type
        : ValueType_None;
}

#pragma endregion

#pragma region Public methods

bool
EtwEnumerator::FormatCurrentProviderName(
    _Out_ EtwStringViewZ* pString) noexcept
{
    ASSERT(m_state != EtwEnumeratorState_None); // PRECONDITION

    m_stringBuffer.clear();
    m_lastError = AppendCurrentProviderName(m_stringBuffer);
    return StringViewResult(m_stringBuffer, pString);
}

bool
EtwEnumerator::FormatCurrentEventName(
    _Out_ EtwStringViewZ* pString) noexcept
{
    ASSERT(m_state != EtwEnumeratorState_None); // PRECONDITION

    m_stringBuffer.clear();
    m_lastError = AppendCurrentEventName(m_stringBuffer);
    return StringViewResult(m_stringBuffer, pString);
}

bool
EtwEnumerator::FormatCurrentKeywordsName(
    _Out_ EtwStringViewZ* pString) noexcept
{
    ASSERT(m_state != EtwEnumeratorState_None); // PRECONDITION

    m_stringBuffer.clear();
    m_lastError = AppendCurrentKeywordsName(m_stringBuffer);
    return StringViewResult(m_stringBuffer, pString);
}

bool
EtwEnumerator::FormatCurrentLevelName(
    _Out_ EtwStringViewZ* pString) noexcept
{
    ASSERT(m_state != EtwEnumeratorState_None); // PRECONDITION

    m_stringBuffer.clear();
    m_lastError = AppendCurrentLevelName(m_stringBuffer);
    return StringViewResult(m_stringBuffer, pString);
}

bool
EtwEnumerator::FormatCurrentFunctionName(
    _Out_ EtwStringViewZ* pString) noexcept
{
    ASSERT(m_state != EtwEnumeratorState_None); // PRECONDITION

    m_stringBuffer.clear();
    m_lastError = AppendCurrentFunctionName(m_stringBuffer);
    return StringViewResult(m_stringBuffer, pString);
}

bool
EtwEnumerator::FormatCurrentEvent(
    _In_opt_z_ EtwPCWSTR szPrefixFormat,
    EtwJsonSuffixFlags jsonSuffixFlags,
    _Out_ EtwStringViewZ* pString) noexcept
{
    ASSERT(m_state != EtwEnumeratorState_None); // PRECONDITION

    auto& output = m_stringBuffer2;
    auto& scratchBuffer = m_stringBuffer;
    output.clear();
    scratchBuffer.clear();

    Reset();

    FormatContext ctx(*this, output, scratchBuffer);

    if (szPrefixFormat && szPrefixFormat[0])
    {
        CheckAdd(ctx.AddPrefix(szPrefixFormat));
        ASSERT(scratchBuffer.size() == 0);
    }

    if (ULONG const eventMessageOffset = m_pTraceEventInfo->EventMessageOffset;
        eventMessageOffset != 0)
    {
        auto const oldOutputSize = output.size();
        auto const szEventMessage = TeiStringNoCheck(eventMessageOffset);
        ctx.AddCurrentEvent(szEventMessage, true);
        ASSERT(scratchBuffer.size() == 0 || m_lastError != ERROR_SUCCESS);
        if (m_lastError != ERROR_MR_MID_NOT_FOUND)
        {
            goto Done;
        }

        // Event had a message, but we failed to resolve a parameter string.
        // Clean things up, then retry as JSON.
        output.resize_unchecked(oldOutputSize); // Keep prefix.
        scratchBuffer.clear();
        Reset();
    }

    CheckAdd(AddCurrentEventAsJson(output, scratchBuffer,
        jsonSuffixFlags));
    ASSERT(scratchBuffer.size() == 0);

    m_lastError = ERROR_SUCCESS;

Done:

    return StringViewResult(output, pString);
}

bool
EtwEnumerator::FormatCurrentEventWithMessage(
    _In_opt_z_ EtwPCWSTR szPrefixFormat,
    _In_z_ EtwPCWSTR szEventMessage,
    _Out_ EtwStringViewZ* pString) noexcept
{
    ASSERT(m_state != EtwEnumeratorState_None); // PRECONDITION

    auto& output = m_stringBuffer2;
    auto& scratchBuffer = m_stringBuffer;
    output.clear();
    scratchBuffer.clear();

    Reset();

    FormatContext ctx(*this, output, scratchBuffer);

    if (szPrefixFormat && szPrefixFormat[0])
    {
        CheckAdd(ctx.AddPrefix(szPrefixFormat));
    }

    ctx.AddCurrentEvent(szEventMessage, false);

Done:

    return StringViewResult(output, pString);
}

bool
EtwEnumerator::FormatCurrentEventPrefix(
    _In_z_ EtwPCWSTR szPrefixFormat,
    _Out_ EtwStringViewZ* pString) noexcept
{
    ASSERT(m_state != EtwEnumeratorState_None); // PRECONDITION

    auto& output = m_stringBuffer2;
    auto& scratchBuffer = m_stringBuffer;
    output.clear();
    scratchBuffer.clear();

    FormatContext ctx(*this, output, scratchBuffer);
    ctx.AddPrefix(szPrefixFormat);
    return StringViewResult(output, pString);
}

bool
EtwEnumerator::FormatCurrentEventAsJson(
    _In_opt_z_ EtwPCWSTR szPrefixFormat,
    EtwJsonSuffixFlags jsonSuffixFlags,
    _Out_ EtwStringViewZ* pString) noexcept
{
    ASSERT(m_state != EtwEnumeratorState_None); // PRECONDITION

    auto& output = m_stringBuffer2;
    auto& scratchBuffer = m_stringBuffer;
    output.clear();
    scratchBuffer.clear();

    Reset();

    if (szPrefixFormat && szPrefixFormat[0])
    {
        FormatContext ctx(*this, output, scratchBuffer);
        CheckAdd(ctx.AddPrefix(szPrefixFormat));
    }

    CheckAdd(AddCurrentEventAsJson(
        output, scratchBuffer, jsonSuffixFlags));

    m_lastError = ERROR_SUCCESS;

Done:

    return StringViewResult(output, pString);
}

bool
EtwEnumerator::FormatCurrentItemAsJsonAndMoveNextSibling(
    EtwJsonItemFlags jsonItemFlags,
    _Out_ EtwStringViewZ* pString) noexcept
{
    ASSERT( // PRECONDITION
        m_state == EtwEnumeratorState_BeforeFirstItem ||
        m_state == EtwEnumeratorState_Value ||
        m_state == EtwEnumeratorState_ArrayBegin ||
        m_state == EtwEnumeratorState_StructBegin);

    auto& output = m_stringBuffer2;
    auto& scratchBuffer = m_stringBuffer;
    output.clear();
    scratchBuffer.clear();

    AddCurrentItemAsJsonAndMoveNext(
        output, scratchBuffer, jsonItemFlags);
    return StringViewResult(output, pString);
}

bool
EtwEnumerator::FormatCurrentValue(
    _Out_ EtwStringView* pString) noexcept
{
    ASSERT(m_state == EtwEnumeratorState_Value); // PRECONDITION

    m_stringBuffer.clear();
    AddCurrentValue(m_stringBuffer);
    return StringViewResult(m_stringBuffer, pString);
}

bool
EtwEnumerator::FormatValueWithMapName(
    _In_reads_bytes_(cbData) void const* pData,
    unsigned cbData,
    _TDH_IN_TYPE inType,
    _TDH_OUT_TYPE outType,
    _In_ EVENT_RECORD const* pEventRecord,
    _In_opt_ EtwPCWSTR pMapName,
    _Out_ EtwStringView* pString) noexcept
{
    m_stringBuffer.clear();
    AddValueWithMapName(m_stringBuffer, pData, cbData, inType, outType, pEventRecord, pMapName);
    return StringViewResult(m_stringBuffer, pString);
}

bool
EtwEnumerator::FormatValueWithMapInfo(
    _In_reads_bytes_(cbData) void const* pData,
    unsigned cbData,
    _TDH_IN_TYPE inType,
    _TDH_OUT_TYPE outType,
    _In_opt_ EVENT_MAP_INFO const* pMapInfo,
    _Out_ EtwStringView* pString) noexcept
{
    m_stringBuffer.clear();
    AddValueWithMapInfo(m_stringBuffer, pData, cbData, inType, outType, pMapInfo);
    return StringViewResult(m_stringBuffer, pString);
}

bool
EtwEnumerator::FormatValue(
    _In_reads_bytes_(cbData) void const* pData,
    unsigned cbData,
    _TDH_IN_TYPE inType,
    _TDH_OUT_TYPE outType,
    _Out_ EtwStringView* pString) noexcept
{
    m_stringBuffer.clear();
    AddValue(m_stringBuffer, pData, cbData, inType, outType);
    return StringViewResult(m_stringBuffer, pString);
}

#pragma endregion
