// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License.

#include "stdafx.h"
#include <EtwEnumerator.h>
#include "EtwBuffer.inl"

// Put a definition of EventTraceGuid into this lib.
#define INITGUID
#include <guiddef.h>
DEFINE_GUID ( /* 68fdd900-4a3e-11d1-84f4-0000f80464e3 */
    EventTraceGuid,
    0x68fdd900,
    0x4a3e,
    0x11d1,
    0x84, 0xf4, 0x00, 0x00, 0xf8, 0x04, 0x64, 0xe3
  );

// Macros for some recently-defined constants so that this can compile
// using an older Windows SDK.
#define TDH_TemplateControlGuid                0x4    // TEMPLATE_CONTROL_GUID
#define TDH_InTypeManifestCountedString        22     // TDH_INTYPE_MANIFEST_COUNTEDSTRING
#define TDH_InTypeManifestCountedAnsiString    23     // TDH_INTYPE_MANIFEST_COUNTEDANSISTRING
#define TDH_InTypeManifestCountedBinary        25     // TDH_INTYPE_MANIFEST_COUNTEDBINARY
#define EventNameOffset                        ActivityIDNameOffset
#define EventAttributesOffset                  RelatedActivityIDNameOffset

static size_t
unaligned_wcsnlen(
    _In_count_(cch) wchar_t const UNALIGNED* pch,
    size_t cch)
{
    size_t i;
    if (WSTR_ALIGNED(pch))
    {
        i = wcsnlen((wchar_t const*)pch, cch);
    }
    else
    {
        for (i = 0; i != cch; i += 1)
        {
            if (L'\0' == pch[i])
            {
                break;
            }
        }
    }
    return i;
}

static void
SkipQuotedRegion(LPCWSTR& pIn, wchar_t*& pOut)
{
    while (*pIn)
    {
        wchar_t ch = *pIn++;
        if (ch != L'"')
        {
            *pOut++ = ch;
        }
        else if (*pIn == '"')
        {
            // Doubled quotes. Skip one.
            *pOut++ = '"';
            pIn += 1;
        }
        else
        {
            // Close quote.
            break;
        }
    }
}

template<class FuncType>
static void
SkipTo(LPCWSTR& pIn, wchar_t*& pOut, FuncType isEndChar)
{
    while (*pIn)
    {
        wchar_t ch = *pIn++;
        if (isEndChar(ch))
        {
            break;
        }
        else if (ch != L'"')
        {
            *pOut++ = ch;
        }
        else
        {
            SkipQuotedRegion(pIn, pOut);
        }
    }
}

static int
GetTimeZoneBiasMinutes()
{
    __int64 const unbiased = 1440ll * 10000000 * 60 + 1;
    __int64 biased = 0;
    FileTimeToLocalFileTime(
        reinterpret_cast<FILETIME const*>(&unbiased),
        reinterpret_cast<FILETIME*>(&biased));
    ASSERT(biased > 0);
    return static_cast<int>((biased - unbiased) / (10000000 * 60));
}

enum EtwEnumerator::SubState
    : UCHAR
{
    SubState_None,
    SubState_Error,
    SubState_AfterLastItem,
    SubState_BeforeFirstItem,
    SubState_Value_Scalar,
    SubState_Value_SimpleArrayElement,
    SubState_Value_ComplexArrayElement,
    SubState_ArrayBegin,
    SubState_ArrayEnd,
    SubState_StructBegin,
    SubState_StructEnd,
};

EtwEnumerator::~EtwEnumerator()
{
    // Can't use the compiler-generated destructor because it leads to a link
    // error (missing definition of the destructor for Buffer).
    return;
}

EtwEnumerator::EtwEnumerator(
    EtwEnumeratorCallbacks& enumeratorCallbacks) noexcept
    : m_pTraceEventInfo()
    , m_pEventRecord()
    , m_pbDataEnd()
    , m_pbDataNext()
    , m_pbCooked()
    , m_cbCooked()
    , m_cbRaw()
    , m_cookedInType()
    , m_cbElement()
    , m_stackTop()
    , m_state(EtwEnumeratorState_None)
    , m_subState(SubState_None)
    , m_cbPointerFallback(sizeof(void*))
    , m_lastError(ERROR_SUCCESS)
    , m_timestampFormat(EtwTimestampFormat_Default)
    , m_timeZoneBiasMinutes(GetTimeZoneBiasMinutes())
    , m_ticksToMilliseconds()
    , m_enumeratorCallbacks(enumeratorCallbacks)
    , m_integerValues()
    , m_stack()
    , m_stringBuffer()
    , m_stringBuffer2()
    , m_teiBuffer()
    , m_mapBuffer()
{
    // Note: we capture time zone bias at construction so we get consistent
    // time zone adjustment for the entire trace, even if time zone changes
    // during decoding.
    return;
}

EtwEnumeratorState
EtwEnumerator::State() const noexcept
{
    return static_cast<EtwEnumeratorState>(m_state);
}

LSTATUS
EtwEnumerator::LastError() const noexcept
{
    return m_lastError;
}

void
EtwEnumerator::Clear() noexcept
{
    SetNoneState(ERROR_SUCCESS);
}

EtwEventCategory
EtwEnumerator::GetEventCategory(
    _In_ EVENT_RECORD const* pEventRecord) noexcept
{
    EtwEventCategory category;

    auto const flags = pEventRecord->EventHeader.Flags;
    if (flags & EVENT_HEADER_FLAG_TRACE_MESSAGE)
    {
        category = EtwEventCategory_TmfWpp;
    }
    else if (flags & EVENT_HEADER_FLAG_CLASSIC_HEADER)
    {
        category = EtwEventCategory_Wbem;
    }
    else if (pEventRecord->EventHeader.EventDescriptor.Channel == 0xb)
    {
        category = EtwEventCategory_TraceLogging;
    }
    else
    {
        category = EtwEventCategory_Manifest;
        for (unsigned i = 0; i != pEventRecord->ExtendedDataCount; i += 1)
        {
            if (pEventRecord->ExtendedData[i].ExtType == EVENT_HEADER_EXT_TYPE_EVENT_SCHEMA_TL)
            {
                category = EtwEventCategory_TraceLogging;
                break;
            }
        }
    }

    return category;
}

EtwEventCategory
EtwEnumerator::PreviewEvent(
    _In_ EVENT_RECORD const* pEventRecord) noexcept
{
    auto const eventCategory = GetEventCategory(pEventRecord);

    if (eventCategory == EtwEventCategory_Wbem &&
        pEventRecord->EventHeader.ProviderId == EventTraceGuid)
    {
        if (pEventRecord->EventHeader.EventDescriptor.Opcode == EVENT_TRACE_TYPE_INFO &&
            pEventRecord->UserDataLength >= sizeof(TRACE_LOGFILE_HEADER))
        {
            auto pHeader = static_cast<TRACE_LOGFILE_HEADER*>(pEventRecord->UserData);
            SetTimerResolution(pHeader->TimerResolution);
        }
    }

    m_lastError = m_enumeratorCallbacks.OnPreviewEvent(pEventRecord, eventCategory);
    return ERROR_SUCCESS == m_lastError
        ? eventCategory
        : EtwEventCategory_Error;
}

bool
EtwEnumerator::StartEvent(
    _In_ EVENT_RECORD const* pEventRecord) noexcept
{
    // Note: to maintain consistent behavior, every path through this function
    // should call either SetNoneState() or StartEventWithEventInfo().

    bool succeeded;

    for (;;)
    {
        ULONG cbTei = m_teiBuffer.capacity();
        PTRACE_EVENT_INFO pTei = reinterpret_cast<PTRACE_EVENT_INFO>(m_teiBuffer.data());

        LSTATUS status = m_enumeratorCallbacks.GetEventInformation(
            pEventRecord,
            0,
            nullptr,
            pTei,
            &cbTei);
        if (status == ERROR_SUCCESS)
        {
            succeeded = StartEventWithTraceEventInfo(
                pEventRecord,
                pTei);
            break;
        }
        else if (
            status != ERROR_INSUFFICIENT_BUFFER ||
            m_teiBuffer.capacity() >= cbTei)
        {
            // If we return ERROR_INSUFFICIENT_BUFFER it means
            // GetEventInformation has a bug (it did not set cbTei correctly).
            ASSERT(status != ERROR_INSUFFICIENT_BUFFER);
            succeeded = SetNoneState(status);
            break;
        }
        else if (!m_teiBuffer.reserve(cbTei, false))
        {
            succeeded = SetNoneState(ERROR_OUTOFMEMORY);
            break;
        }
    }

    return succeeded;
}

bool
EtwEnumerator::StartEventWithTraceEventInfo(
    _In_ EVENT_RECORD const* pEventRecord,
    _In_ TRACE_EVENT_INFO const* pTraceEventInfo) noexcept
{
    // Note: to maintain consistent behavior, every path through this function
    // should call either SetNoneState() or ResetImpl().

    bool succeeded;

    if (pTraceEventInfo->DecodingSource == DecodingSourceWPP)
    {
        // The TRACE_EVENT_INFO returned by TDH for TMF-based WPP is not very
        // useful. TMF-based WPP should be decoded using TdhGetWppProperty.
        succeeded = SetNoneState(ERROR_INVALID_PARAMETER);
    }
    else if (!m_integerValues.resize(pTraceEventInfo->PropertyCount, false))
    {
        succeeded = SetNoneState(ERROR_OUTOFMEMORY);
    }
    else
    {
        m_pTraceEventInfo = pTraceEventInfo;
        m_pEventRecord = pEventRecord;
        m_pbDataEnd = static_cast<BYTE const*>(m_pEventRecord->UserData) + m_pEventRecord->UserDataLength;

        // Initialize "last seen value" for each property to 0xffff.
        // This generally ensures we get a bounds error if we try to use
        // an invalid property as an array length or field size.
        memset(m_integerValues.data(), 0xff, m_integerValues.byte_size());

        ResetImpl();
        succeeded = true;
    }

    return succeeded;
}

void
EtwEnumerator::Reset() noexcept
{
    ASSERT(m_state != EtwEnumeratorState_None); // PRECONDITION
    ResetImpl();
}

void
EtwEnumerator::ResetImpl() noexcept
{
    ASSERT(m_pTraceEventInfo != nullptr);
    ASSERT(m_pEventRecord != nullptr);

    m_pbDataNext = static_cast<BYTE const*>(m_pEventRecord->UserData);
    ASSERT(m_pbDataEnd == m_pbDataNext + m_pEventRecord->UserDataLength);

    m_stack.clear();
    m_stackTop.PropertyIndex = 0;
    m_stackTop.PropertyEnd =
        static_cast<USHORT>(m_pTraceEventInfo->TopLevelPropertyCount);

    SetState(EtwEnumeratorState_BeforeFirstItem, SubState_BeforeFirstItem);
    m_lastError = ERROR_SUCCESS;
}

bool
EtwEnumerator::MoveNext() noexcept
{
    ASSERT(m_state >= EtwEnumeratorState_BeforeFirstItem); // PRECONDITION

    bool movedToItem;

    switch (m_subState)
    {
    default:

        movedToItem = SetErrorState(ERROR_INVALID_STATE);
        break;

    case SubState_BeforeFirstItem:

        ASSERT(m_state == EtwEnumeratorState_BeforeFirstItem);
        movedToItem = NextProperty();
        break;

    case SubState_Value_Scalar:

        ASSERT(m_state == EtwEnumeratorState_Value);
        ASSERT(!m_stackTop.IsStruct);
        ASSERT(!m_stackTop.IsArray);

        m_pbDataNext += m_cbRaw;

        // End of property - move to next property.
        m_stackTop.PropertyIndex += 1;
        movedToItem = NextProperty();
        break;

    case SubState_Value_SimpleArrayElement:

        ASSERT(m_state == EtwEnumeratorState_Value);
        ASSERT(!m_stackTop.IsStruct);
        ASSERT(m_stackTop.IsArray);
        ASSERT(m_stackTop.ArrayIndex < m_stackTop.ArrayCount);
        ASSERT(m_cbElement != 0); // Eligible for fast path.

        m_pbDataNext += m_cbRaw;
        m_stackTop.ArrayIndex += 1;

        if (m_stackTop.ArrayCount == m_stackTop.ArrayIndex)
        {
            // End of array.
            SetEndState(EtwEnumeratorState_ArrayEnd, SubState_ArrayEnd);
        }
        else
        {
            // Middle of array - get next element.
            StartValueSimple(); // Fast path for simple array elements.
        }

        m_lastError = ERROR_SUCCESS;
        movedToItem = true;
        break;

    case SubState_Value_ComplexArrayElement:

        ASSERT(m_state == EtwEnumeratorState_Value);
        ASSERT(!m_stackTop.IsStruct);
        ASSERT(m_stackTop.IsArray);
        ASSERT(m_stackTop.ArrayIndex < m_stackTop.ArrayCount);
        ASSERT(m_cbElement == 0); // Not eligible for fast path.

        m_pbDataNext += m_cbRaw;
        m_stackTop.ArrayIndex += 1;

        if (m_stackTop.ArrayCount == m_stackTop.ArrayIndex)
        {
            // End of array.
            SetEndState(EtwEnumeratorState_ArrayEnd, SubState_ArrayEnd);
            m_lastError = ERROR_SUCCESS;
            movedToItem = true;
        }
        else
        {
            // Middle of array - get next element.
            movedToItem = StartValue(); // Normal path for complex array elements.
        }

        break;

    case SubState_ArrayBegin:

        ASSERT(m_state == EtwEnumeratorState_ArrayBegin);
        ASSERT(m_stackTop.IsArray);
        ASSERT(m_stackTop.ArrayIndex == 0);

        if (m_stackTop.ArrayCount == 0)
        {
            // 0-length array.
            SetEndState(EtwEnumeratorState_ArrayEnd, SubState_ArrayEnd);
            m_lastError = ERROR_SUCCESS;
            movedToItem = true;
        }
        else if (m_cbElement != 0)
        {
            ASSERT(!m_stackTop.IsStruct);

            // First element of simple array
            m_cbCooked = m_cbElement;
            m_cbRaw = m_cbCooked;
            SetState(EtwEnumeratorState_Value, SubState_Value_SimpleArrayElement);
            StartValueSimple();
            m_lastError = ERROR_SUCCESS;
            movedToItem = true;
        }
        else if (!m_stackTop.IsStruct)
        {
            // First element of complex array
            SetState(EtwEnumeratorState_Value, SubState_Value_ComplexArrayElement);
            movedToItem = StartValue();
        }
        else
        {
            // First element of array of struct
            StartStruct();
            m_lastError = ERROR_SUCCESS;
            movedToItem = true;
        }

        break;

    case SubState_ArrayEnd:

        ASSERT(m_state == EtwEnumeratorState_ArrayEnd);
        ASSERT(m_stackTop.IsArray);
        ASSERT(m_stackTop.ArrayCount == m_stackTop.ArrayIndex);

        m_stackTop.PropertyIndex += 1;
        movedToItem = NextProperty();
        break;

    case SubState_StructBegin:

        ASSERT(m_state == EtwEnumeratorState_StructBegin);
        if (!m_stack.push_back(m_stackTop))
        {
            movedToItem = SetErrorState(ERROR_OUTOFMEMORY);
        }
        else
        {
            auto& epi = m_pTraceEventInfo->EventPropertyInfoArray[m_stackTop.PropertyIndex];
            m_stackTop.PropertyIndex = epi.structType.StructStartIndex;
            m_stackTop.PropertyEnd = static_cast<USHORT>(m_stackTop.PropertyIndex + epi.structType.NumOfStructMembers);
            movedToItem = NextProperty();
        }

        break;

    case SubState_StructEnd:

        ASSERT(m_state == EtwEnumeratorState_StructEnd);
        ASSERT(m_stackTop.IsStruct);
        ASSERT(m_cbRaw == 0);

        m_stackTop.ArrayIndex += 1;

        if (m_stackTop.ArrayCount != m_stackTop.ArrayIndex)
        {
            ASSERT(m_stackTop.IsArray);
            ASSERT(m_stackTop.ArrayIndex < m_stackTop.ArrayCount);

            // Middle of array - get next element.
            StartStruct();
            m_lastError = ERROR_SUCCESS;
            movedToItem = true;
        }
        else if (m_stackTop.IsArray)
        {
            // End of array.
            SetEndState(EtwEnumeratorState_ArrayEnd, SubState_ArrayEnd);
            m_lastError = ERROR_SUCCESS;
            movedToItem = true;
        }
        else
        {
            // End of property - move to next property.
            m_stackTop.PropertyIndex += 1;
            movedToItem = NextProperty();
        }

        break;
    }

    return movedToItem;
}

bool
EtwEnumerator::MoveNextSibling() noexcept
{
    ASSERT(m_state >= EtwEnumeratorState_BeforeFirstItem); // PRECONDITION

    bool movedToItem;
    int depth = 0;

    do
    {
        switch (m_subState)
        {
        default:
            break;

        case SubState_ArrayEnd:
        case SubState_StructEnd:
            depth -= 1;
            break;

        case SubState_StructBegin:
            depth += 1;
            break;

        case SubState_ArrayBegin:
            if (m_cbElement != 0)
            {
                // Array of simple elements - skip to next sibling.
                ASSERT(m_state == EtwEnumeratorState_ArrayBegin);
                ASSERT(m_stackTop.IsArray);
                ASSERT(m_stackTop.ArrayIndex == 0);
                ASSERT(!m_stackTop.IsStruct);
                m_pbDataNext += static_cast<unsigned>(m_cbElement) * m_stackTop.ArrayCount;
                m_stackTop.PropertyIndex += 1;
                movedToItem = NextProperty();
                continue; // Don't call MoveNext() for this iteration.
            }
            else
            {
                // Array of complex elements - use MoveNext.
                depth += 1;
            }

            break;
        }

        movedToItem = MoveNext();
    } while (depth > 0 && movedToItem);

    return movedToItem;
}

EtwEventInfo
EtwEnumerator::GetEventInfo() const noexcept
{
    ASSERT(m_state != EtwEnumeratorState_None); // PRECONDITION

    EtwEventInfo value;

    value.Name = EventName();
    value.Reserved_Tags = m_pTraceEventInfo->Tags;
    value.BinaryXmlSize = m_pTraceEventInfo->BinaryXMLSize;
    value.BinaryXml = m_pTraceEventInfo->BinaryXMLOffset == 0
        ? nullptr
        : reinterpret_cast<BYTE const*>(m_pTraceEventInfo) + m_pTraceEventInfo->BinaryXMLOffset;
    //value.DecodeGuid = decoding-source-dependent;
    //value.ControlGuid = decoding-source-dependent;
    //value.EventGuid = decoding-source-dependent;
    value.ProviderName = TeiString(m_pTraceEventInfo->ProviderNameOffset);
    value.LevelName = TeiString(m_pTraceEventInfo->LevelNameOffset);
    value.ChannelName = TeiString(m_pTraceEventInfo->ChannelNameOffset);
    value.KeywordsName = TeiString(m_pTraceEventInfo->KeywordsNameOffset);
    value.TaskName = TaskName();
    value.OpcodeName = OpcodeName();
    value.EventMessage = TeiString(m_pTraceEventInfo->EventMessageOffset);
    value.ProviderMessage = TeiString(m_pTraceEventInfo->ProviderMessageOffset);
    value.EventAttributes = EventAttributes();
    //value.WbemActivityIdName = decoding-source-dependent;
    //value.WbemRelatedActivityIdName = decoding-source-dependent;

    if (m_pTraceEventInfo->DecodingSource == DecodingSourceWbem)
    {
        if (m_pTraceEventInfo->EventGuid == GUID())
        {
            value.DecodeGuid = &m_pTraceEventInfo->ProviderGuid;
            value.ControlGuid = &m_pTraceEventInfo->ProviderGuid;
        }
        else
        {
            value.DecodeGuid = &m_pTraceEventInfo->EventGuid;
            value.ControlGuid = &m_pTraceEventInfo->ProviderGuid;
        }

        value.EventGuid = nullptr;
        value.WbemActivityIdName = TeiString(m_pTraceEventInfo->ActivityIDNameOffset);
        value.WbemRelatedActivityIdName = TeiString(m_pTraceEventInfo->RelatedActivityIDNameOffset);
    }
    else
    {
        value.DecodeGuid = &m_pTraceEventInfo->ProviderGuid;

        if (m_pTraceEventInfo->Flags & TDH_TemplateControlGuid)
        {
            value.ControlGuid = &m_pTraceEventInfo->EventGuid;
            value.EventGuid = nullptr;
        }
        else
        {
            value.ControlGuid = &m_pTraceEventInfo->ProviderGuid;
            value.EventGuid = m_pTraceEventInfo->EventGuid == GUID()
                ? nullptr
                : &m_pTraceEventInfo->EventGuid;
        }

        value.WbemActivityIdName = nullptr;
        value.WbemRelatedActivityIdName = nullptr;
    }

    // PREfast is unhappy about us returning pointers into m_pTraceEventInfo
    // that it can't prove are valid pointers.
#pragma warning(suppress: 26045)
    return value;
}

EtwItemInfo
EtwEnumerator::GetItemInfo() const noexcept
{
    ASSERT(m_state > EtwEnumeratorState_BeforeFirstItem); // PRECONDITION

    UINT32 const IsArrayMask = 1 << 28;
    EtwItemInfo value;

    auto& epi = m_pTraceEventInfo->EventPropertyInfoArray[m_stackTop.PropertyIndex];
    value.Name = epi.NameOffset ? TeiStringNoCheck(epi.NameOffset) : L"";
    value.Reserved_Tags =
        (m_stackTop.IsArray ? IsArrayMask : 0u) |
        (epi.Flags & PropertyHasTags ? epi.Tags : 0u);
    value.InType = static_cast<_TDH_IN_TYPE>(m_cookedInType);
    value.OutType = 0 != (epi.Flags & PropertyStruct)
        ? TDH_OUTTYPE_NULL
        : static_cast<_TDH_OUT_TYPE>(epi.nonStructType.OutType);
    value.ArrayIndex = m_stackTop.ArrayIndex;
    value.ArrayCount = m_stackTop.ArrayCount;
    value.ElementSize = m_cbElement;
    value.DataSize = m_cbCooked;
    value.Data = m_pbCooked;
    value.MapName =
        epi.nonStructType.MapNameOffset == 0 ||
        0 != (epi.Flags & (PropertyHasCustomSchema | PropertyStruct))
        ? nullptr
        : TeiStringNoCheck(epi.nonStructType.MapNameOffset);

    return value;
}

bool
EtwEnumerator::CurrentEventHasEventMessage() const noexcept
{
    ASSERT(m_state != EtwEnumeratorState_None); // PRECONDITION

    return m_pTraceEventInfo->EventMessageOffset != 0;
}

EtwRawDataPosition
EtwEnumerator::GetRawDataPosition() const noexcept
{
    ASSERT(m_state != EtwEnumeratorState_None); // PRECONDITION

    EtwRawDataPosition value;

    value.DataSize = static_cast<USHORT>(m_pbDataEnd - m_pbDataNext);
    value.Data = m_pbDataNext;

    return value;
}

EtwRawItemInfo
EtwEnumerator::GetRawItemInfo() const noexcept
{
    ASSERT(m_state > EtwEnumeratorState_BeforeFirstItem);

    EtwRawItemInfo value;

    auto& epi = m_pTraceEventInfo->EventPropertyInfoArray[m_stackTop.PropertyIndex];
    value.RawInType = (epi.Flags & PropertyStruct)
        ? TDH_INTYPE_NULL
        : static_cast<_TDH_IN_TYPE>(epi.nonStructType.InType);
    value.Flags = epi.Flags;
    value.RawDataSize = m_cbRaw;
    value.RawData = m_pbDataNext;
    value.CustomSchema =
        (epi.Flags & PropertyHasCustomSchema)
        ? TeiString(epi.customSchemaType.CustomSchemaOffset)
        : nullptr;

    return value;
}

bool
EtwEnumerator::FindCurrentEventAttribute(
    _In_z_ EtwPCWSTR szAttributeName,
    _Out_ EtwStringViewZ* pString) noexcept
{
    ASSERT(m_state != EtwEnumeratorState_None); // PRECONDITION

    return FindEventAttribute(EventAttributes(), szAttributeName, pString);
}

bool
EtwEnumerator::FindEventAttribute(
    _In_opt_z_ EtwPCWSTR szEventAttributes,
    _In_z_ EtwPCWSTR szAttributeName,
    _Out_ EtwStringViewZ* pString) noexcept
{
    m_stringBuffer.clear();
    if (szEventAttributes == nullptr)
    {
        m_lastError = ERROR_NOT_FOUND;
    }
    else
    {
        m_lastError = AppendEventAttribute(
            m_stringBuffer,
            szEventAttributes,
            static_cast<unsigned>(wcslen(szEventAttributes)),
            szAttributeName,
            static_cast<unsigned>(wcslen(szAttributeName)));
    }

    return StringViewResult(m_stringBuffer, pString);
}

bool
EtwEnumerator::SplitCurrentEventAttributes(
    _Out_writes_to_(cAttributes, *pcAttributes) EtwAttributeInfo* pAttributes,
    unsigned cAttributes,
    _Out_ unsigned* pcAttributes) noexcept
{
    ASSERT(m_state != EtwEnumeratorState_None); // PRECONDITION

    return SplitEventAttributes(
            EventAttributes(), pAttributes, cAttributes, pcAttributes);
}

bool
EtwEnumerator::SplitEventAttributes(
    _In_opt_z_ EtwPCWSTR szEventAttributes,
    _Out_writes_to_(cAttributes, *pcAttributes) EtwAttributeInfo* pAttributes,
    unsigned cAttributes,
    _Out_ unsigned* pcAttributes) noexcept
{
    auto& output = m_stringBuffer;
    unsigned cActual = 0;
    ASSERT(cAttributes == 0 || pAttributes != nullptr);

    output.clear();

    if (szEventAttributes == nullptr || szEventAttributes[0] == L'\0')
    {
        m_lastError = ERROR_SUCCESS;
    }
    else if (!output.resize(
        static_cast<unsigned>(wcslen(szEventAttributes)) + 1))
    {
        m_lastError = ERROR_OUTOFMEMORY;
    }
    else
    {
        wchar_t* pOut = output.data();
        LPCWSTR pIn = szEventAttributes;
        while (*pIn)
        {
            LPCWSTR pName = pOut;
            SkipTo(pIn, pOut, [](wchar_t ch) { return ch == L'=' || ch == L';'; });
            *pOut++ = L'\0';

            // Does the attribute have a value?
            LPCWSTR pValue;
            if (pIn[-1] == L'=')
            {
                pValue = pOut;
                SkipTo(pIn, pOut, [](wchar_t ch) { return ch == L';'; });
                *pOut++ = L'\0';
            }
            else
            {
                // We got to the end of the attribute without finding '='.
                // Treat as an empty value.
                pValue = &pOut[-1]; // = L""
            }

            ASSERT(pOut <= output.data() + output.size());

            if (pName[0] != L'\0' || pValue[0] != L'\0')
            {
                if (cActual < cAttributes)
                {
                    pAttributes[cActual] = { pName, pValue };
                }

                cActual += 1;
            }
        }

        if (cActual <= cAttributes)
        {
            m_lastError = ERROR_SUCCESS;
        }
        else
        {
            m_lastError = ERROR_INSUFFICIENT_BUFFER;
        }
    }

    *pcAttributes = cActual;
    return m_lastError == ERROR_SUCCESS;
}

unsigned
EtwEnumerator::TimerResolution() const noexcept
{
    // We store number of milliseconds per tick, but return value is number of
    // 100ns units per tick.
    return m_ticksToMilliseconds * 10000;
}

void
EtwEnumerator::SetTimerResolution(unsigned value) noexcept
{
    // Parameter is number of 100ns units per tick, but we store number of
    // milliseconds per tick.
    m_ticksToMilliseconds = value / 10000;
}

unsigned
EtwEnumerator::TicksToMilliseconds(
    unsigned ticks) const noexcept
{
    return ticks * m_ticksToMilliseconds;
}

UCHAR
EtwEnumerator::PointerSizeFallback() const noexcept
{
    return m_cbPointerFallback;
}

void
EtwEnumerator::SetPointerSizeFallback(
    UCHAR value) noexcept
{
    ASSERT(value == 4 || value == 8); // PRECONDITION
    m_cbPointerFallback = value;
}

EtwTimestampFormat
EtwEnumerator::TimestampFormat() const noexcept
{
    return m_timestampFormat;
}

bool
EtwEnumerator::SetTimestampFormat(EtwTimestampFormat value) noexcept
{
    auto const type = value & EtwTimestampFormat_TypeMask;
    auto const nonType = value & ~EtwTimestampFormat_TypeMask;
    if (type <= EtwTimestampFormat_None ||
        type >= EtwTimestampFormat_Max ||
        (nonType & ~EtwTimestampFormat_FlagMask) != 0)
    {
        m_lastError = ERROR_INVALID_PARAMETER;
    }
    else
    {
        m_timestampFormat = value;
        m_lastError = ERROR_SUCCESS;
    }

    return m_lastError == ERROR_SUCCESS;
}

int
EtwEnumerator::TimeZoneBiasMinutes() const noexcept
{
    return m_timeZoneBiasMinutes;
}

void
EtwEnumerator::SetTimeZoneBiasMinutes(_In_range_(-1440, 1440) int value) noexcept
{
    ASSERT(value >= -1440 && value <= 1440); // PRECONDITION
    m_timeZoneBiasMinutes = value;
}

INT64
EtwEnumerator::AdjustFileTimeToLocal(
    INT64 utcFileTime) const noexcept
{
    return AdjustFileTime(utcFileTime, m_timeZoneBiasMinutes);
}

INT64
EtwEnumerator::AdjustFileTime(
    INT64 fileTime,
    int biasMinutes) noexcept
{
    static INT64 const fileTimePerMinute = 10000000 * 60;

    INT64 const fileTimeAdjusted = fileTime + biasMinutes * fileTimePerMinute;
    return fileTimeAdjusted >= 0
        ? fileTimeAdjusted
        : fileTime < 0 || biasMinutes < 0
        ? 0
        : MAXINT64;
}

/*
Called after m_stackTop.PropertyIndex changes to a new property index.
Assumes:
- PropertyIndex <= PropertyEnd.
Sets:
- ArrayIndex, ArrayCount, IsStruct, IsArray.
- m_pbCooked, m_cbCooked, m_cbRaw, m_state.
- m_cookedInType, m_cbElement.
*/
bool
EtwEnumerator::NextProperty() noexcept
{
    ASSERT(m_stackTop.PropertyIndex <=  m_stackTop.PropertyEnd);

    bool movedToItem;

    if (m_stackTop.PropertyEnd == m_stackTop.PropertyIndex)
    {
        // End of current group of properties.
        if (m_stack.size() == 0)
        {
            SetEndState(EtwEnumeratorState_AfterLastItem, SubState_AfterLastItem);
            m_lastError = ERROR_SUCCESS;
            movedToItem = false;
        }
        else
        {
            m_stackTop = m_stack[m_stack.size() - 1];
            m_stack.pop_back();
            m_cookedInType = TDH_INTYPE_NULL;
            m_cbElement = 0;
            SetEndState(EtwEnumeratorState_StructEnd, SubState_StructEnd);
            m_lastError = ERROR_SUCCESS;
            movedToItem = true;
        }
    }
    else
    {
        auto& epi = m_pTraceEventInfo->EventPropertyInfoArray[m_stackTop.PropertyIndex];

        m_stackTop.ArrayIndex = 0;

        if ((epi.Flags & (PropertyStruct | PropertyParamCount | PropertyParamFixedCount)) == 0 &&
            epi.count == 1)
        {
            // Scalar:

            m_stackTop.ArrayCount = 1;
            m_stackTop.IsStruct = false;
            m_stackTop.IsArray = false;

            SetState(EtwEnumeratorState_Value, SubState_Value_Scalar);
            movedToItem = StartValue();
        }
        else
        {
            // Array and/or Struct:

            m_stackTop.IsStruct = 0 != (epi.Flags & PropertyStruct);

            if (epi.Flags & PropertyParamCount)
            {
                // Count comes from the value of a previous property.
                m_stackTop.ArrayCount = m_integerValues[epi.countPropertyIndex];
                m_stackTop.IsArray = true;
                movedToItem = StartArray();
            }
            else
            {
                m_stackTop.ArrayCount = epi.count;

                // Note that PropertyParamFixedCount is a new flag, so it is unset in
                // some cases where it should be set (providers built using older mc.exe),
                // and it is ignored by many decoders. Without the PropertyParamFixedCount
                // flag, decoders will assume that a property is an array if it has
                // either a count parameter or a fixed count other than 1. The
                // PropertyParamFixedCount flag allows for fixed-count arrays with one
                // element to be propertly recognized as arrays (though this is probably
                // not a common or particularly important case).
                if (1 != epi.count ||
                    0 != (epi.Flags & PropertyParamFixedCount))
                {
                    m_stackTop.IsArray = true;
                    movedToItem = StartArray();
                }
                else
                {
                    m_stackTop.IsArray = false;
                    StartStruct();
                    m_lastError = ERROR_SUCCESS;
                    movedToItem = true;
                }
            }
        }
    }

    return movedToItem;
}

/*
Sets:
- m_pbCooked, m_cbCooked, m_cbRaw, m_state.
- m_cookedInType, m_cbElement.
*/
void
EtwEnumerator::StartStruct() noexcept
{
    m_pbCooked = m_pbDataNext;
    m_cbCooked = 0;
    m_cbRaw = 0;
    m_cookedInType = TDH_INTYPE_NULL;
    m_cbElement = 0;
    SetState(EtwEnumeratorState_StructBegin, SubState_StructBegin);
}

/*
Decodes array type, especially m_cookedInType and m_cbElement.
If the array contains simple elements, also computes and validates array size.
Sets:
- m_pbCooked, m_cbCooked, m_cbRaw.
- m_cookedInType, m_cbElement.
*/
bool
EtwEnumerator::StartArray() noexcept
{
    bool movedToItem;
    auto& epi = m_pTraceEventInfo->EventPropertyInfoArray[m_stackTop.PropertyIndex];

    m_pbCooked = m_pbDataNext;
    m_cbCooked = 0;
    m_cbRaw = 0;
    m_cookedInType = epi.nonStructType.InType;
    m_cbElement = 0;
    SetState(EtwEnumeratorState_ArrayBegin, SubState_ArrayBegin);

    if (m_stackTop.IsStruct)
    {
        m_cookedInType = TDH_INTYPE_NULL;
        m_lastError = ERROR_SUCCESS;
        movedToItem = true;
        goto Done;
    }

    // Determine the m_cbElement and m_cookedInType values.
    switch (epi.nonStructType.InType)
    {
    case TDH_INTYPE_INT8:
    case TDH_INTYPE_UINT8:
    case TDH_INTYPE_ANSICHAR:
        m_cbElement = 1;
        break;

    case TDH_INTYPE_INT16:
    case TDH_INTYPE_UINT16:
    case TDH_INTYPE_UNICODECHAR:
        m_cbElement = 2;
        break;

    case TDH_INTYPE_INT32:
    case TDH_INTYPE_UINT32:
    case TDH_INTYPE_HEXINT32:
    case TDH_INTYPE_FLOAT:
    case TDH_INTYPE_BOOLEAN:
        m_cbElement = 4;
        break;

    case TDH_INTYPE_INT64:
    case TDH_INTYPE_UINT64:
    case TDH_INTYPE_DOUBLE:
    case TDH_INTYPE_FILETIME:
    case TDH_INTYPE_HEXINT64:
        m_cbElement = 8;
        break;

    case TDH_INTYPE_GUID:
    case TDH_INTYPE_SYSTEMTIME:
        m_cbElement = 16;
        break;

    case TDH_INTYPE_POINTER:
    case TDH_INTYPE_SIZET:
        m_cbElement = PointerSize();
        break;

    case TDH_INTYPE_NULL:
    case TDH_INTYPE_UNICODESTRING:
    case TDH_INTYPE_ANSISTRING:
    case TDH_INTYPE_BINARY:
    case TDH_INTYPE_SID:
        m_lastError = ERROR_SUCCESS;
        movedToItem = true;
        goto Done;

    case TDH_InTypeManifestCountedString:
    case TDH_INTYPE_COUNTEDSTRING:
    case TDH_INTYPE_REVERSEDCOUNTEDSTRING:
    case TDH_INTYPE_NONNULLTERMINATEDSTRING:
        m_cookedInType = TDH_INTYPE_UNICODESTRING;
        m_lastError = ERROR_SUCCESS;
        movedToItem = true;
        goto Done;

    case TDH_InTypeManifestCountedAnsiString:
    case TDH_INTYPE_COUNTEDANSISTRING:
    case TDH_INTYPE_REVERSEDCOUNTEDANSISTRING:
    case TDH_INTYPE_NONNULLTERMINATEDANSISTRING:
        m_cookedInType = TDH_INTYPE_ANSISTRING;
        m_lastError = ERROR_SUCCESS;
        movedToItem = true;
        goto Done;

    case TDH_InTypeManifestCountedBinary:
    case TDH_INTYPE_HEXDUMP:
        m_cookedInType = TDH_INTYPE_BINARY;
        m_lastError = ERROR_SUCCESS;
        movedToItem = true;
        goto Done;

    case TDH_INTYPE_WBEMSID:
        m_cookedInType = TDH_INTYPE_SID;
        m_lastError = ERROR_SUCCESS;
        movedToItem = true;
        goto Done;

    default:
        movedToItem = SetErrorState(ERROR_UNSUPPORTED_TYPE);
        goto Done;
    }

    // For simple array element types, validate that Count * m_cbElement <= RemainingSize.
    // That way we can skip per-element validation and we can safely expose the array data
    // during ArrayBegin.
    {
        USHORT const cbRemaining = static_cast<USHORT>(m_pbDataEnd - m_pbDataNext);
        unsigned const cbArray = static_cast<unsigned>(m_stackTop.ArrayCount) * m_cbElement;
        if (cbRemaining < cbArray)
        {
            movedToItem = SetErrorState(ERROR_INVALID_DATA);
        }
        else
        {
            m_cbRaw = m_cbCooked = static_cast<USHORT>(cbArray);
            movedToItem = true;
        }
    }

Done:

    return movedToItem;
}

/*
Decodes the current field's data and type.
Sets:
- m_pbCooked, m_cbCooked, m_cbRaw.
- m_cookedInType, m_cbElement.
*/
bool
EtwEnumerator::StartValue() noexcept
{
    ASSERT(!m_stackTop.IsStruct);

    bool movedToItem;
    auto& epi = m_pTraceEventInfo->EventPropertyInfoArray[m_stackTop.PropertyIndex];
    USHORT const cbRemaining = static_cast<USHORT>(m_pbDataEnd - m_pbDataNext);
    USHORT propertyLength;

    m_pbCooked = m_pbDataNext;
    m_cookedInType = epi.nonStructType.InType;
    m_cbElement = 0;

    // Each switch case needs to be sure the following are set correctly:
    // m_cookedInType - default is InType.
    // m_cbElement  - default is 0, should be set for fixed-size types.
    // m_pbCooked   - default is m_pbDataNext.
    // m_cbCooked   - no default, must be set.
    // m_cbRaw      - no default, must be set.
    switch (epi.nonStructType.InType)
    {
        // Note:
        // If this property is an 8/16/32-bit integer, we need to remember the
        // value in case it is needed for a subsequent property's length or
        // count. In these cases, since we have to validate cbRemaining before
        // reading the value, we skip straight to Done to avoid redundant work.

    case TDH_INTYPE_UINT8:
        m_cbRaw = m_cbCooked = m_cbElement = 1;
        if (m_cbRaw <= cbRemaining)
        {
            UINT8 const val = *m_pbDataNext;
            m_integerValues[m_stackTop.PropertyIndex] = val;
            m_lastError = ERROR_SUCCESS;
            movedToItem = true;
            goto Done;
        }
        break;

    case TDH_INTYPE_UINT16:
        m_cbRaw = m_cbCooked = m_cbElement = 2;
        if (m_cbRaw <= cbRemaining)
        {
            UINT16 const val = *reinterpret_cast<UINT16 const UNALIGNED*>(m_pbDataNext);
            m_integerValues[m_stackTop.PropertyIndex] = val;
            m_lastError = ERROR_SUCCESS;
            movedToItem = true;
            goto Done;
        }
        break;

    case TDH_INTYPE_UINT32:
    case TDH_INTYPE_HEXINT32:
        m_cbRaw = m_cbCooked = m_cbElement = 4;
        if (m_cbRaw <= cbRemaining)
        {
            UINT32 const val = *reinterpret_cast<UINT32 const UNALIGNED*>(m_pbDataNext);
            m_integerValues[m_stackTop.PropertyIndex] = static_cast<USHORT>(val > 0xffffu ? 0xffffu : val);
            m_lastError = ERROR_SUCCESS;
            movedToItem = true;
            goto Done;
        }
        break;

    case TDH_INTYPE_INT8:
    case TDH_INTYPE_ANSICHAR:
        m_cbRaw = m_cbCooked = m_cbElement = 1;
        break;

    case TDH_INTYPE_INT16:
    case TDH_INTYPE_UNICODECHAR:
        m_cbRaw = m_cbCooked = m_cbElement = 2;
        break;

    case TDH_INTYPE_INT32:
    case TDH_INTYPE_FLOAT:
    case TDH_INTYPE_BOOLEAN:
        m_cbRaw = m_cbCooked = m_cbElement = 4;
        break;

    case TDH_INTYPE_INT64:
    case TDH_INTYPE_UINT64:
    case TDH_INTYPE_DOUBLE:
    case TDH_INTYPE_FILETIME:
    case TDH_INTYPE_HEXINT64:
        m_cbRaw = m_cbCooked = m_cbElement = 8;
        break;

    case TDH_INTYPE_GUID:
    case TDH_INTYPE_SYSTEMTIME:
        m_cbRaw = m_cbCooked = m_cbElement = 16;
        break;

    case TDH_INTYPE_POINTER:
    case TDH_INTYPE_SIZET:
        m_cbRaw = m_cbCooked = m_cbElement = PointerSize();
        break;

    case TDH_INTYPE_UNICODESTRING:

        if (CurrentPropertyLength(&propertyLength))
        {
            m_cbRaw = m_cbCooked = 2u * propertyLength;
        }
        else
        {
            // Note that while it's technically incorrect to reach end-of-event
            // without a nul-termination, we tolerate that condition here.
            propertyLength = static_cast<USHORT>(unaligned_wcsnlen(
                reinterpret_cast<wchar_t const UNALIGNED*>(m_pbDataNext),
                cbRemaining / 2u));
            m_cbCooked = propertyLength * 2u;
            m_cbRaw =
                cbRemaining / 2u == propertyLength
                ? cbRemaining
                : m_cbCooked + 2u;

            // We've already validated that m_cbRaw <= cbRemaining.
            m_lastError = ERROR_SUCCESS;
            movedToItem = true;
            goto Done;
        }
        break;

    case TDH_INTYPE_ANSISTRING:
        if (CurrentPropertyLength(&propertyLength))
        {
            m_cbRaw = m_cbCooked = propertyLength;
        }
        else
        {
            // Note that while it's technically incorrect to reach end-of-event
            // without a nul-termination, we tolerate that condition here.
            propertyLength = static_cast<USHORT>(strnlen(
                reinterpret_cast<char const*>(m_pbDataNext),
                cbRemaining));
            m_cbCooked = propertyLength;
            m_cbRaw =
                cbRemaining == propertyLength
                ? cbRemaining
                : m_cbCooked + 1u;

            // We've already validated that m_cbRaw <= cbRemaining.
            m_lastError = ERROR_SUCCESS;
            movedToItem = true;
            goto Done;
        }
        break;

    case TDH_INTYPE_BINARY:
        if (!CurrentPropertyLength(&propertyLength) &&
            TDH_OUTTYPE_IPV6 == epi.nonStructType.OutType &&
            TDH_INTYPE_BINARY == epi.nonStructType.InType)
        {
            // Special case for incorrectly-defined IPV6 addresses.
            propertyLength = 16;
        }
        m_cbRaw = m_cbCooked = propertyLength;
        break;

    case TDH_InTypeManifestCountedString:
    case TDH_INTYPE_COUNTEDSTRING:
        m_cookedInType = TDH_INTYPE_UNICODESTRING;
        StartValueCounted();
        m_cbCooked &= ~1u; // Round to multiple of 2.
        break;

    case TDH_INTYPE_REVERSEDCOUNTEDSTRING:
        m_cookedInType = TDH_INTYPE_UNICODESTRING;
        StartValueReversedCounted();
        m_cbCooked &= ~1u; // Round to multiple of 2.
        break;

    case TDH_InTypeManifestCountedAnsiString:
    case TDH_INTYPE_COUNTEDANSISTRING:
        m_cookedInType = TDH_INTYPE_ANSISTRING;
        StartValueCounted();
        break;

    case TDH_INTYPE_REVERSEDCOUNTEDANSISTRING:
        m_cookedInType = TDH_INTYPE_ANSISTRING;
        StartValueReversedCounted();
        break;

    case TDH_InTypeManifestCountedBinary:
        m_cookedInType = TDH_INTYPE_BINARY;
        StartValueCounted();
        break;

    case TDH_INTYPE_HEXDUMP:
        m_cookedInType = TDH_INTYPE_BINARY;
        if (cbRemaining < 4)
        {
            m_cbRaw = 4;
        }
        else
        {
            m_pbCooked = m_pbDataNext + 4;
            m_cbCooked = *reinterpret_cast<UINT16 const UNALIGNED*>(m_pbDataNext);
            m_cbRaw = static_cast<USHORT>(m_cbCooked + 4);
        }
        break;

    case TDH_INTYPE_NONNULLTERMINATEDSTRING:
        m_cookedInType = TDH_INTYPE_UNICODESTRING;
        m_cbCooked = cbRemaining & ~1u; // Round to multiple of 2.
        m_cbRaw = cbRemaining;
        break;

    case TDH_INTYPE_NONNULLTERMINATEDANSISTRING:
        m_cookedInType = TDH_INTYPE_ANSISTRING;
        m_cbCooked = cbRemaining;
        m_cbRaw = cbRemaining;
        break;

    case TDH_INTYPE_NULL:
        m_cbRaw = m_cbCooked = 0;
        m_lastError = ERROR_SUCCESS;
        movedToItem = true;
        goto Done;

    case TDH_INTYPE_SID:
        if (cbRemaining < 8u)
        {
            m_cbRaw = 8u;
        }
        else
        {
            m_cbRaw = m_cbCooked = 8u + (m_pbDataNext[1u] * 4u);
        }
        break;

    case TDH_INTYPE_WBEMSID:
        // A WBEM SID is actually a TOKEN_USER structure followed
        // by the SID. The TOKEN_USER structure is 2 * pointerSize.
        m_cookedInType = TDH_INTYPE_SID;
        propertyLength = 2u * PointerSize();
        if (cbRemaining < propertyLength + 8u)
        {
            m_cbRaw = propertyLength + 8u;
        }
        else
        {
            m_pbCooked += propertyLength;
            m_cbCooked = 8u + (m_pbDataNext[propertyLength + 1u] * 4u);
            m_cbRaw = static_cast<USHORT>(propertyLength + m_cbCooked);
        }
        break;

    default:
        m_cbRaw = m_cbCooked = 0;
        movedToItem = SetErrorState(ERROR_UNSUPPORTED_TYPE);
        goto Done;
    }

    if (cbRemaining < m_cbRaw)
    {
        m_cbRaw = m_cbCooked = 0;
        movedToItem = SetErrorState(ERROR_INVALID_DATA);
    }
    else
    {
        m_lastError = ERROR_SUCCESS;
        movedToItem = true;
    }

Done:

    return movedToItem;
}

void
EtwEnumerator::StartValueSimple() noexcept
{
    ASSERT(m_stackTop.ArrayIndex < m_stackTop.ArrayCount);
    ASSERT(!m_stackTop.IsStruct);
    ASSERT(m_stackTop.IsArray);
    ASSERT(m_cbElement != 0);
    ASSERT(m_cbCooked == m_cbElement);
    ASSERT(m_cbRaw == m_cbElement);
    ASSERT(m_pbDataNext + m_cbRaw <= m_pbDataEnd);
    ASSERT(m_state == EtwEnumeratorState_Value);
    m_pbCooked = m_pbDataNext;
}

void
EtwEnumerator::StartValueCounted() noexcept
{
    unsigned const cbRemaining = static_cast<unsigned>(m_pbDataEnd - m_pbDataNext);
    if (cbRemaining < 2)
    {
        m_cbRaw = 2;
    }
    else
    {
        m_pbCooked = m_pbDataNext + 2;
        m_cbCooked = m_pbDataNext[0] | (static_cast<unsigned>(m_pbDataNext[1]) << 8);
        m_cbRaw = static_cast<USHORT>(m_cbCooked + 2);
    }
}

void
EtwEnumerator::StartValueReversedCounted() noexcept
{
    unsigned const cbRemaining = static_cast<unsigned>(m_pbDataEnd - m_pbDataNext);
    if (cbRemaining < 2)
    {
        m_cbRaw = 2;
    }
    else
    {
        m_pbCooked = m_pbDataNext + 2;
        m_cbCooked = (static_cast<unsigned>(m_pbDataNext[0]) << 8) | m_pbDataNext[1];
        m_cbRaw = static_cast<USHORT>(m_cbCooked + 2);
    }
}

bool
EtwEnumerator::SetNoneState(LSTATUS error) noexcept
{
    m_pTraceEventInfo = nullptr;
    m_pEventRecord = nullptr;
    m_pbDataEnd = nullptr;
    m_pbDataNext = nullptr;
    m_pbCooked = nullptr;
    m_state = EtwEnumeratorState_None;
    m_subState = SubState_None;
    m_lastError = error;
    return false;
}

bool
EtwEnumerator::SetErrorState(LSTATUS error) noexcept
{
    m_state = EtwEnumeratorState_Error;
    m_subState = SubState_Error;
    m_lastError = error;
    return false;
}

void
EtwEnumerator::SetEndState(
    EtwEnumeratorState newState,
    SubState newSubState) noexcept
{
    m_pbCooked = m_pbDataNext;
    m_cbCooked = 0;
    m_cbRaw = 0;
    m_state = newState;
    m_subState = newSubState;
}

void
EtwEnumerator::SetState(
    EtwEnumeratorState newState,
    SubState newSubState) noexcept
{
    m_state = newState;
    m_subState = newSubState;
}

UCHAR
EtwEnumerator::PointerSize() const noexcept
{
    return m_pEventRecord->EventHeader.Flags & EVENT_HEADER_FLAG_32_BIT_HEADER
        ? static_cast<UCHAR>(4)
        : m_pEventRecord->EventHeader.Flags & EVENT_HEADER_FLAG_64_BIT_HEADER
        ? static_cast<UCHAR>(8)
        : m_cbPointerFallback;
}

bool
EtwEnumerator::StringViewResult(
    EtwInternal::Buffer<wchar_t>& output,
    _Out_ EtwStringView* pString) noexcept
{
    bool ok;

    if (m_lastError != ERROR_SUCCESS)
    {
        *pString = {};
        ok = false;
    }
    else
    {
        auto const pch = output.data();
        auto const cch = output.size();

        // If there is room, ensure string is NOT nul-terminated.
        if (cch < output.capacity())
        {
            pch[cch] = 0xFFFD; // Replacement character
        }

        *pString = { pch, cch };
        ok = true;
    }

    return ok;
}

bool
EtwEnumerator::StringViewResult(
    EtwInternal::Buffer<wchar_t>& output,
    _Out_ EtwStringViewZ* pString) noexcept
{
    bool ok;

    if (m_lastError != ERROR_SUCCESS)
    {
        *pString = { L"", 0 };
        ok = false;
    }
    else if (!output.push_back(0))
    {
        *pString = { L"", 0 };
        m_lastError = ERROR_OUTOFMEMORY;
        ok = false;
    }
    else
    {
        *pString = { output.data(), output.size() - 1 };
        ok = true;
    }

    return ok;
}

bool
EtwEnumerator::CurrentPropertyLength(
    _Out_ USHORT* pLength) const noexcept
{
    bool hasLength;
    auto& epi = m_pTraceEventInfo->EventPropertyInfoArray[m_stackTop.PropertyIndex];

    if (0 != (epi.Flags & PropertyParamLength))
    {
        // Length comes from the value of a previous property.
        *pLength = m_integerValues[epi.lengthPropertyIndex];
        hasLength = true;
    }
    else if (
        0 != epi.length ||
        0 != (epi.Flags & PropertyParamFixedLength))
    {
        *pLength = epi.length;
        hasLength = true;
    }
    else
    {
        *pLength = 0;
        hasLength = false;
    }

    return hasLength;
}

_Ret_opt_z_ LPCWSTR
EtwEnumerator::EventName() const noexcept
{
    unsigned eventNameOffset;
    switch (m_pTraceEventInfo->DecodingSource)
    {
    case DecodingSourceWbem:
        eventNameOffset = m_pTraceEventInfo->OpcodeNameOffset;
        break;
    case DecodingSourceTlg:
        eventNameOffset = m_pTraceEventInfo->TaskNameOffset;
        break;
    default:
        eventNameOffset = m_pTraceEventInfo->EventNameOffset;
        break;
    }
    return TeiString(eventNameOffset);
}

_Ret_opt_z_ LPCWSTR
EtwEnumerator::EventAttributes() const noexcept
{
    return
        m_pTraceEventInfo->DecodingSource == DecodingSourceWbem
        ? nullptr
        : TeiString(m_pTraceEventInfo->EventAttributesOffset);
}

_Ret_opt_z_ LPCWSTR
EtwEnumerator::TaskName() const noexcept
{
    return
        m_pTraceEventInfo->DecodingSource == DecodingSourceTlg
        ? nullptr
        : TeiString(m_pTraceEventInfo->TaskNameOffset);
}

_Ret_opt_z_ LPCWSTR
EtwEnumerator::OpcodeName() const noexcept
{
    return
        m_pTraceEventInfo->DecodingSource == DecodingSourceWbem
        ? nullptr
        : TeiString(m_pTraceEventInfo->OpcodeNameOffset);
}

_Ret_opt_z_ LPCWSTR
EtwEnumerator::TeiString(
    ULONG offset) const noexcept
{
    return offset ? TeiStringNoCheck(offset) : nullptr;
}

_Ret_z_ LPCWSTR
EtwEnumerator::TeiStringNoCheck(
    ULONG offset) const noexcept
{
    ASSERT(offset != 0);
    return reinterpret_cast<LPCWSTR>(
        reinterpret_cast<BYTE const*>(m_pTraceEventInfo) + offset);
}

LSTATUS
EtwEnumerator::AppendEventAttribute(
    EtwInternal::Buffer<wchar_t>& output,
    _In_z_count_(cchEventAttributes) EtwPCWSTR szEventAttributes,
    unsigned cchEventAttributes,
    _In_count_(cchAttributeName) EtwWCHAR const* pchAttributeName,
    unsigned cchAttributeName) const noexcept
{
    LSTATUS status;

    auto const oldSize = output.size();

    if (cchEventAttributes == 0)
    {
        status = ERROR_NOT_FOUND;
    }
    else if (!output.reserve(cchEventAttributes + oldSize))
    {
        status = ERROR_OUTOFMEMORY;
    }
    else
    {
        wchar_t* const pOutBegin = output.data() + oldSize;
        LPCWSTR pIn = szEventAttributes;
        while (*pIn)
        {
            wchar_t* pOut;

            // Copy an unescaped name into pOut.
            pOut = pOutBegin;
            SkipTo(pIn, pOut, [](wchar_t ch) { return ch == L'=' || ch == L';'; });

            unsigned const cchCurrentName = static_cast<unsigned>(pOut - pOutBegin);
            bool const matched =
                cchAttributeName == cchCurrentName &&
                0 == memcmp(pOutBegin, pchAttributeName, cchCurrentName * sizeof(wchar_t));

            // Copy an unescaped value into pOut.
            pOut = pOutBegin;
            if (pIn[-1] == L'=')
            {
                SkipTo(pIn, pOut, [](wchar_t ch) { return ch == L';'; });
            }

            if (matched)
            {
                output.resize_unchecked(static_cast<unsigned>(pOut - output.data()));
                status = ERROR_SUCCESS;
                goto Done;
            }
        }

        status = ERROR_NOT_FOUND;
    }

Done:

    return status;
}
