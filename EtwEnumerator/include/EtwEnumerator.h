// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License.

/*
Defines the EtwEnumerator class.
EtwEnumerator helps decode and format ETW events.
*/

#pragma once
#include <tdh.h>

#pragma warning(push)
#pragma warning(disable:4201)  // nameless struct/union.

// Forward declarations of types from this header:
class EtwEnumerator;                // Performs decoding of ETW events.
enum EtwEventCategory : unsigned;   // Category of event.
enum EtwEnumeratorState : UCHAR;    // Current state of an EtwEnumerator.
struct EtwEventInfo;                // Information about the event: name, provider, etc.
struct EtwAttributeInfo;            // The name and value of an event attribute.
struct EtwRawDataPosition;          // Technical details about the raw event payload.
struct EtwItemInfo;                 // Information about the current item in the event.
struct EtwRawItemInfo;              // Technical details about the current item in the event.
struct EtwStringView;               // Counted string returned from a Format method.
struct EtwStringViewZ;              // Nul-terminated string returned from a Format method.
enum EtwJsonItemFlags : unsigned;   // Selects options to use when formatting an item as JSON.
enum EtwJsonSuffixFlags : unsigned; // Selects the metadata to include in a JSON string.
enum EtwTimestampFormat : unsigned; // Controls timestamp formatting.
class EtwEnumeratorCallbacks;       // Abstract base class for customizing EtwEnumerator.
using EtwWCHAR = __wchar_t;         // Use native wchar_t for this API.
using EtwPCWSTR = _Null_terminated_ __wchar_t const*; // Nul-terminated __wchar_t string.

namespace EtwInternal
{
    /*
    Simple vector (resizable array).
    Supports only POD types (does not support items with constructors,
    destructors, or copy operators). Does not initialize new items.
    Optionally supports pre-allocating capacity within the Buffer object.
    */
    template<class T, unsigned StaticCapacity = 0>
    class Buffer
        : public Buffer<T, 0>
    {
        // This implementation is used when StaticCapacity != 0.
        // It inherits from the Buffer<T, 0> specialization and adds a static
        // buffer.
    public:
        Buffer() noexcept;
    private:
        T m_staticData[StaticCapacity];
    };

    template<class T>
    class Buffer<T, 0>
    {
        // This specialization is used when StaticCapacity == 0.
        // It has no static buffer.
        // Buffer<T, N> derives from this to add a static buffer.
    public:
        using value_type = T;
        using size_type = unsigned;
        Buffer(Buffer const&) = delete;
        Buffer& operator=(Buffer const&) = delete;
        constexpr Buffer() noexcept;
        ~Buffer() noexcept;
        size_type byte_size() const noexcept;
        size_type size() const noexcept;
        size_type capacity() const noexcept;
        T const* data() const noexcept;
        T* data() noexcept;
        T* begin() noexcept;
        T* end() noexcept;
        T const& operator[](size_type i) const noexcept; // precondition: i < size
        T& operator[](size_type i) noexcept; // precondition: i < size
        void clear() noexcept;
        void resize_unchecked(size_type newSize) noexcept; // precondition: newSize <= capacity
        bool push_back(T const& value) noexcept;
        void pop_back() noexcept; // precondition: size != 0
        bool reserve(size_type requiredCapacity, bool keepExistingData = true) noexcept;
        bool resize(size_type newSize, bool keepExistingData = true) noexcept;
    protected:
        // Calling this constructor implies that room for staticCapacity
        // instances of T has been reserved in memory immediately after the
        // Buffer<T, 0> object, i.e. at location (this + 1).
        explicit Buffer(size_type staticCapacity) noexcept;
        static size_type const MaxCapacity =
            size_type(~size_type(0) / sizeof(T)) - 1;
    private:
        bool Grow(size_type requiredCapacity, bool keepExistingData) noexcept; // precondition: capacity < requiredCapacity
        _Field_size_(m_capacity) T* m_pData; // If m_pData != (this + 1) then assume m_pData is a heap allocation.
        size_type m_size;
        size_type m_capacity;
    };
}
// namespace EtwInternal

/*
EtwEnumerator helps decode and format ETW events.

- Encapsulates tricky decoding rules behind a simple interface.
- Supports high performance event processing.
- Presents each field with its name, type, and value (binary data) in the
  order the field appears in the event's payload.
- Provides helper methods to format individual field values as strings.
- Provides helper methods to format the entire event as a string, either using
  the event's message string (if it has one) or using JSON.
- Supports structured data, i.e. arrays and nested structures.
- Exposes the information from TRACE_EVENT_INFO in a user-friendly manner,
  e.g. provider name, event name, control GUID, decode GUID. Developer should
  not need to use TRACE_EVENT_INFO directly when using this class.
- Exposes the information from EVENT_PROPERTY_INFO in a user-friendly manner,
  e.g. field name, field types, field decoding. Developer should not need to
  use EVENT_PROPERTY_INFO directly when using this class.
- Non-goal: Does not decode TMF-based WPP events.
- Non-goal: Does not attempt to expose information easily accessible from
  the EVENT_RECORD (e.g. does not expose EVENT_DESCRIPTOR or ActivityId).
  Developer should access these values from the EVENT_RECORD as needed.

General usage:

1. Developer's code initializes a callback context object that contains the
   objects needed for decoding an event. The context object should include an
   EtwEnumerator object.
2. Developer's code invokes OpenTrace with:
   - either an ETL filename in EVENT_TRACE_LOGFILE.LogFileName,
     or an ETW realtime session name in EVENT_TRACE_LOGFILE.LoggerName,
   - a pointer to the developer's EventRecordCallback function in
     EVENT_TRACE_LOGFILE.EventRecordCallback,
   - a pointer to the developer's callback context in
     EVENT_TRACE_LOGFILE.Context,
   - the correct flags in EVENT_TRACE_LOGFILE.ProcessTraceMode (must include
     PROCESS_TRACE_MODE_EVENT_RECORD; if processing a realtime session, must
     include PROCESS_TRACE_MODE_REAL_TIME; if using the FormatCurrentEvent
     methods, must NOT include PROCESS_TRACE_MODE_RAW_TIMESTAMP).
   OpenTrace returns a TRACEHANDLE.
2. Developer's code invokes ProcessTrace with the TRACEHANDLE. ProcessTrace
   invokes the developer's EventRecordCallback function once for each
   pEventRecord in the trace.
3. The developer's EventRecordCallback implementation uses the context's
   EtwEnumerator object to obtain information about the event.
   a. Get the pointer to the developer's callback context from
      pEventRecord->UserContext, and use the etwEnumerator from the context.
   b. Call etwEnumerator.PreviewEvent to update the etwEnumerator's metadata
      and to determine the event category.
   c. If the category is TmfWpp, use TdhGetProperty or TdhGetWppProperty to
      decode the event instead of using EtwEnumerator. EtwEnumerator cannot
      decode TMF-based WPP.
   d. If the category is Wbem and pEventRecord->EventHeader.ProviderId equals
      EventTraceGuid, consider skipping the event. Wbem EventTrace events
      contain trace metadata, not normal event data, and are not usually shown
      in decoded traces.
   e. Call etwEnumerator.StartEvent() to look up decoding information and
      initialize the enumeration. (Or use StartEventWithTraceEventInfo() if
      you want to look up the decoding information yourself.)
   f. Use etwEnumerator.GetEventInfo() as needed to access event properties
      like provider name and event name.
   g. If you want to access individual field values, use the MoveNext(),
      State(), and GetItemInfo() methods to enumerate the event items.
      - Call etwEnumerator.MoveNext() to move to the next item in the event.
        Stop enumerating when MoveNext() returns false.
      - Call etwEnumerator.State() to determine the kind of item at the
        enumerator's current position: start/end of array, start/end of
        structure, or individual field value.
      - Call etwEnumerator.GetItemInfo() to get information about each item
        such as the name and type.
      - Handle the item as necessary. Optionally use FormatCurrentValue() to
        convert the value into a string.
   h. Use FormatCurrentEvent (or other Format methods) as needed to generate a
      string representation of the event data.
   i. Handle the end of event. Optionally use GetRawDataPosition() to
      determine whether any undecoded data is present in the event payload.

Example usage pattern, assuming the caller has received a pEventRecord from
ProcessTrace (i.e. this example would be the developer's EventRecordCallback
function):

// The code that calls OpenTrace should set logfile.Context to the address of
// a context object containing an EtwEnumerator object. The
// MyEventRecordCallback function below assumes that logfile.Context was set
// to an instance of MyEventRecordCallbackContext.
struct MyEventRecordCallbackContext
{
    EtwEnumerator Enumerator;
    // .. Other context data goes here.
};

void WINAPI
MyEventRecordCallback(
    _In_ EVENT_RECORD* pEventRecord)
{
    LSTATUS status;

    MyEventRecordCallbackContext* pContext =
        static_cast<MyEventRecordCallbackContext*>(pEventRecord->UserContext);
    EtwEnumerator& e = pContext->Enumerator;

    EtwEventCategory eventCategory = e.PreviewEvent(pEventRecord);
    if (eventCategory == EtwEventCategory_Error)
    {
        // Error occurred in the OnPreviewEvent callback.
        // Can only occur if you have overridden OnPreviewEvent.
        // ... Report the problem?
        status = e.LastError();
    }
    else if (eventCategory == EtwEventCategory_TmfWpp)
    {
        // EtwEnumerator does not support TMF-based WPP.
        // ... Process the event using TdhGetProperty or TdhGetWppProperty?
        status = ERROR_NOT_SUPPORTED;
    }
    else if (eventCategory == EtwEventCategory_Wbem &&
        pEventRecord->EventHeader.ProviderId == EventTraceGuid)
    {
        // EventTrace events are special events containing trace metadata.
        // They should usually not be included in the decoded output.
        status = ERROR_SUCCESS;
    }
    else if (!e.StartEvent(pEventRecord))
    {
        // Usually means GetTraceEventInformation was unable to look up
        // the decoding information for the event.
        // ... Report problem?
        status = e.LastError();
    }
    else
    {
        // You might want to enumerate event values or convert the event to a
        // string. Both are shown below.

        // ***********************************************
        // How to enumerate values in the event:
        // ***********************************************

        EtwEventInfo eventInfo = e.GetEventInfo();
        // ... Process start of event using the data in eventInfo.

        // Loop until we reach the end of event or encounter an error:
        while (e.MoveNext())
        {
            EtwItemInfo itemInfo;

            switch (e.State())
            {
            case EtwEnumeratorState_Value:
                itemInfo = e.GetItemInfo();
                // ... Process value using data in itemInfo. Optionally use
                //     FormatCurrentValue to convert value to string.
                break;
            case EtwEnumeratorState_ArrayBegin:
                itemInfo = e.GetItemInfo();
                // ... Process start of array using data in itemInfo.
                break;
            case EtwEnumeratorState_ArrayEnd:
                // ... Process end of array.
                break;
            case EtwEnumeratorState_StructBegin:
                itemInfo = e.GetItemInfo();
                // ... Process start of struct using data in itemInfo.
                break;
            case EtwEnumeratorState_StructEnd:
                // ... Process end of struct.
                break;
            default:
                // For forward-compatibility, other states should be ignored.
                break;
            }
        }

        if (e.State() == EtwEnumeratorState_Error)
        {
            // Failed enumeration: MoveNext() encountered an error.
            // ... Report problem?
            status = e.LastError();
            goto Done;
        }

        // Successful enumeration: MoveNext() reached the end of the event.
        ASSERT(e.State() == EtwEnumeratorState_AfterLastItem);
        ASSERT(e.LastError() == ERROR_SUCCESS);

        // ... Process end of event.
        // Optionally use GetRawDataPosition to determine whether any undecoded
        // binary data remains at the end of the event payload.

        // ***********************************************
        // How to convert the entire event to a string:
        // ***********************************************

        EtwStringViewZ eventString;
        if (!e.FormatCurrentEvent(
            L"[%9]%8.%3::%4 [%1]", // This is a common event prefix.
            EtwJsonSuffixFlags_Default, // JSON used if event has no message.
            &eventString)) // Receives formatted prefix + formatted event.
        {
            // ... Report problem?
            status = e.LastError();
            goto Done;
        }

        // ... Use data in eventString.

        status = ERROR_SUCCESS;
    }

Done:

    return;
}
*/
class EtwEnumerator
{
public:

    EtwEnumerator(EtwEnumerator const&) = delete;
    EtwEnumerator& operator=(EtwEnumerator const&) = delete;
    ~EtwEnumerator();

    /*
    Initializes a new instance of the EtwEnumerator class that will use
    TdhGetEventInformation() and TdhGetEventMapInformation() as needed to
    find decoding information, will use FormatMessage() to convert result
    codes to strings (i.e. to get a string corresponding to E_OUTOFMEMORY),
    and will perform simple enum formatting.

    Note: A default-constructed EtwEnumerator cannot resolve "%%n" parameter
    strings. The FormatCurrentEvent() method of a default-constructed
    EtwEnumerator will ignore any event message that contains "%%n" parameter
    strings and will fall back to JSON formatting for those events.

    Note: Use of the EtwEnumerator default constructor will cause your program
    to depend on TDH.dll. If you want to avoid a direct dependency on TDH.dll
    (e.g. if you want to dynamically-load TDH), define a callback object and
    use the other constructor.
    */
    EtwEnumerator() noexcept;

    /*
    Initializes a new instance of the EtwEnumerator class that will use the
    specified enumeratorCallbacks instance for decoding information lookup,
    enumeration formatting, error code formatting, and parameter string
    lookup.

    The enumeratorCallbacks object will be used as follows:

    - PreviewEvent() will call enumeratorCallbacks.OnPreviewEvent().
    - StartEvent() will call enumeratorCallbacks.GetEventInformation().
    - FormatCurrentValue(), FormatValueWithMapName(), and FormatCurrentEvent()
      may call enumeratorCallbacks.FormatResultCodeValue().
    - FormatCurrentValue(), FormatValueWithMapName(), and FormatCurrentEvent()
      may call enumeratorCallbacks.GetEventMapInformation() and
      enumeratorCallbacks.FormatMapValue().
    - FormatCurrentEvent() and FormatCurrentEventWithMessage() may call
      enumeratorCallbacks.GetParameterMessage() to resolve "%%n" parameter
      strings in event messages.
    */
    explicit
    EtwEnumerator(
        EtwEnumeratorCallbacks& enumeratorCallbacks) noexcept;

    /*
    Gets the current state of the enumerator.
    */
    EtwEnumeratorState State() const noexcept;

    /*
    Gets status for the most recent operation.
    */
    LSTATUS LastError() const noexcept;

    /*
    Sets State to None.
    Should be called when the pEventRecord or pTraceEventInfo pointers become
    invalid to prevent inadvertent access to dangling pointers.
    */
    void Clear() noexcept;

    /*
    Returns the category of an event: TmfWpp, Wbem, Manifest, or TraceLogging.
    This returns the same value as PreviewEvent but does not invoke the callback
    and does not update the EtwEnumerator's metadata tracking.
    */
    static EtwEventCategory GetEventCategory(
        _In_ EVENT_RECORD const* pEventRecord) noexcept;

    /*
    Tracks metadata from the event and determines the event's category.

    By default, PreviewEvent will always return a valid category other than
    Error. It will only return Error if you override OnPreviewEvent() with an
    implementation that returns an error.

    For best results, call PreviewEvent for every event in the trace, even
    events for which you won't be calling StartEvent. That way, the
    enumerator's metadata (e.g. the TimerResolution property) will always stay
    up to date, allowing for correct formatting of KTIME and UTIME variables.

    If PreviewEvent returns TmfWpp, you should not use the EtwEnumerator to
    decode the event. EtwEnumerator does not support TMF-based WPP events.
    Instead, use TdhGetProperty or TdhGetWppProperty to decode the event.
    */
    EtwEventCategory PreviewEvent(
        _In_ EVENT_RECORD const* pEventRecord) noexcept;

    /*
    Starts decoding the specified event. This method uses
    enumeratorCallbacks.GetEventInformation() to obtain decoding information.

    On success, changes the state to BeforeFirstItem and returns true.
    On failure, changes the state to None and returns false. Check LastError()
    for details.

    Fails with ERROR_INVALID_PARAMETER if the event is a TMF-based WPP event.
    Fails with various error codes for decoding problems, e.g.
    ERROR_INVALID_DATA, ERROR_OUTOFMEMORY, ERROR_UNSUPPORTED_TYPE,
    ERROR_NOT_FOUND, etc.

    Note that the enumerator stores the pEventRecord pointer (does not copy
    the referenced data), so the pEventRecord pointer must remain valid and
    unchanged while you are processing the data with this enumerator (e.g.
    until you call Clear, make another call to StartEvent, or destroy the
    EtwEnumerator instance).
    */
    bool StartEvent(
        _In_ EVENT_RECORD const* pEventRecord) noexcept;

    /*
    Starts decoding the specified event using the caller-provided decoding
    information.

    On success, changes the state to BeforeFirstItem and returns true.
    On failure, changes the state to None and returns false. Check LastError()
    for details.

    Fails with ERROR_INVALID_PARAMETER if the event is a TMF-based WPP event.
    Fails with various error codes for decoding problems, e.g.
    ERROR_OUTOFMEMORY, ERROR_INVALID_DATA, ERROR_UNSUPPORTED_TYPE, etc.

    Note that the enumerator stores the pEventRecord and pTraceEventInfo
    pointers (does not copy the referenced data), so the pEventRecord and
    pTraceEventInfo pointers must remain valid and unchanged while you are
    processing the data with this enumerator (e.g. until you call Clear,
    make another call to StartEvent, or destroy the EtwEnumerator instance).
    */
    bool StartEventWithTraceEventInfo(
        _In_ EVENT_RECORD const* pEventRecord,
        _In_ TRACE_EVENT_INFO const* pTraceEventInfo) noexcept;

    /*
    Changes the state to BeforeFirstItem, i.e. sets the enumerator to the
    position it had immediately after the call to StartEvent before any calls
    to MoveNext.

    PRECONDITION: State != None, i.e. this can be called after a successful
    call to StartEvent or StartEventWithTraceEventInfo, until a call to Clear.

    Requires that the pEventRecord and pTraceEventInfo pointers that were
    passed to StartEvent or StartEventWithTraceEventInfo are still valid and
    that the referenced content remains unmodified.
    */
    void Reset() noexcept;

    /*
    Moves the enumerator to the next item in the current event.

    PRECONDITION: State >= BeforeFirstItem, i.e. this can be called from a
    successful call to StartEvent() until end of enumeration (until MoveNext()
    returns false).

    Returns true if moved to a valid item. Returns false at end of enumeration.
    Enumeration may end for no more fields (state == AfterLastItem) or for
    decoding error (state == Error). Check LastError() for details.

    Typically called in a loop until it returns false, e.g.:

        if (!e.StartEvent()) return e.LastError();
        while (e.MoveNext()) { ... }
        return e.LastError(); // Returns ERROR_SUCCESS if State == AfterLastItem.
    */
    bool MoveNext() noexcept;

    /*
    Moves the enumerator to the next sibling of the current logical item.
    If the current item is ArrayBegin or StructBegin, this efficiently moves
    to AFTER the corresponding ArrayEnd or StructEnd. Otherwise, this is the
    same as MoveNext().

    PRECONDITION: State >= BeforeFirstItem, i.e. this can be called from a
    successful call to StartEvent() until end of enumeration (until MoveNext()
    returns false).

    Returns true if moved to a valid item. Returns false at end of enumeration.
    Enumeration may end for no more fields (state == AfterLastItem) or for
    decoding error (state == Error). Check LastError() for details.

    This method is useful to efficiently skip past an array of fixed-size
    items (i.e. an array where ElementSize is nonzero) if you process all of
    the array items within the ArrayBegin state. This method is also useful if
    you are only interested in the top-level fields of an event.
    */
    bool MoveNextSibling() noexcept;

    /*
    Gets information that applies to the current event, e.g. the provider
    name, event name (if available), control GUID, decode GUID.

    PRECONDITION: State != None, i.e. this can be called after a successful
    call to StartEvent or StartEventWithTraceEventInfo, until a call to Clear.
    */
    EtwEventInfo GetEventInfo() const noexcept;

    /*
    Gets information that applies to the current item, e.g. the item's name,
    the item's type (integer, string, float, etc.), and the item's value. The
    current item changes each time MoveNext is called.

    PRECONDITION: State > BeforeFirstItem. This can be called when positioned
    at a valid item (i.e. after MoveNext() returns true).
    */
    EtwItemInfo GetItemInfo() const noexcept;

    /*
    Returns true if the current event has a non-null EventMessage (i.e. if the
    event has a format string). Returns false if the event does not have an
    EventMessage (i.e. if EventMessage is null).

    PRECONDITION: State != None, i.e. this can be called after a successful
    call to StartEvent or StartEventWithTraceEventInfo, until a call to Clear.

    Note that the EventMessage value can be obtained by calling GetEventInfo.
    */
    bool CurrentEventHasEventMessage() const noexcept;

    /*
    Gets technical details about the current event's raw payload data.

    PRECONDITION: State != None, i.e. this can be called after a successful
    call to StartEvent or StartEventWithTraceEventInfo, until a call to Clear.

    Gets the remaining event payload, i.e. the event data that has not yet
    been decoded. The data position can change each time MoveNext is called.
    The data position can be used after all fields have been decoded (i.e.
    when state == AfterLastItem) to determine whether the event contains any
    trailing data (data not described by the decoding information).
    */
    EtwRawDataPosition GetRawDataPosition() const noexcept;

    /*
    Gets technical details about the current item.

    PRECONDITION: State > BeforeFirstItem. This can be called when positioned
    at a valid item (i.e. after MoveNext() returns true).
    */
    EtwRawItemInfo GetRawItemInfo() const noexcept;

    /*
    Finds the value of the named attribute (e.g. "FILE", "LINE", "FUNC") in
    the current event's EventAttributes string.

    PRECONDITION: State != None, i.e. this can be called after a successful
    call to StartEvent or StartEventWithTraceEventInfo, until a call to Clear.

    On success, returns true. On failure, returns false. Check LastError()
    for details. Possible errors include:

    - ERROR_NOT_FOUND if the event's attribute string does not have the
      requested attribute.
    - ERROR_OUTOFMEMORY for decoding problems.

    FindCurrentEventAttribute is the same as FindEventAttribute except that it
    uses the current event's EventAttributes property as the source of the
    attribute string.

    The returned Data pointer points into a buffer maintained within the
    EtwEnumerator object. This buffer becomes invalid the next time a
    non-const method is invoked on this EtwEnumerator object (e.g. Clear,
    StartEvent, MoveNext, SplitEventAttributesFromCurrent,
    FormatValueFromCurrent, etc.) or when the EtwEnumerator object is
    destroyed.
    */
    bool FindCurrentEventAttribute(
        _In_z_ EtwPCWSTR szAttributeName,
        _Out_ EtwStringViewZ* pString) noexcept;

    /*
    Finds the value of the named attribute (e.g. "FILE", "LINE", "FUNC") in
    the specified attribute string.

    On success, returns true. On failure, returns false. Check LastError()
    for details. Possible errors include:

    - ERROR_NOT_FOUND if szEventAttributes does not have the requested
      attribute.
    - ERROR_OUTOFMEMORY for decoding problems.

    The szEventAttributes string is assumed to be a semicolon-separated list
    of Name=Value pairs, or can be null or empty (indicating no attributes).
    Values can be quoted (e.g. for a value that contains a semicolon), and
    quotes can be included in a value by doubling them.

    For example, the following attributes string:

        ONE=Value1;TWO="SubValue1;""SubValue2"";SubValue3"

    would be interpreted as containing the following name/value pairs:

        ONE = Value1
        TWO = SubValue1;"SubValue2";SubValue3

    The returned Data pointer points into a buffer maintained within the
    EtwEnumerator object. This buffer becomes invalid the next time a
    non-const method is invoked on this EtwEnumerator object (e.g. Clear,
    StartEvent, MoveNext, SplitEventAttributesFromCurrent,
    FormatValueFromCurrent, etc.) or when the EtwEnumerator object is
    destroyed.
    */
    bool FindEventAttribute(
        _In_opt_z_ EtwPCWSTR szEventAttributes,
        _In_z_ EtwPCWSTR szAttributeName,
        _Out_ EtwStringViewZ* pString) noexcept;

    /*
    Splits the current event's EventAttributes string into name-value pairs.

    PRECONDITION: State != None, i.e. this can be called after a successful
    call to StartEvent or StartEventWithTraceEventInfo, until a call to Clear.

    On success, returns true. On failure, returns false. Check LastError()
    for details.

    - Fails with ERROR_INSUFFICIENT_BUFFER and sets *pcAttributes to the
      number of attributes present if cAttributes is too small.
    - Fails with ERROR_OUTOFMEMORY or ERROR_INVALID_DATA for decoding problems.

    SplitEventAttributesFromCurrent is the same as
    SplitEventAttributesFromString except that it uses the current event's
    EventAttributes property as the source of the attribute string.

    pcAttributes must not be null. It receives the number of attributes
    present in the current event.

    pAttributes receives the attribute values. It must point at a buffer with
    room for cAttributes EtwAttributeInfo elements. (pAttributes may be null
    if cAttributes is 0.)

    The returned Name and Value pointers point into a buffer
    maintained within the EtwEnumerator object. This buffer becomes invalid
    the next time a non-const method is invoked on this EtwEnumerator object
    (e.g. Clear, StartEvent, MoveNext, SplitEventAttributesFromCurrent,
    FormatValueFromCurrent, etc.) or when the EtwEnumerator object is
    destroyed.
    */
    bool SplitCurrentEventAttributes(
        _Out_writes_to_(cAttributes, *pcAttributes) EtwAttributeInfo* pAttributes,
        unsigned cAttributes,
        _Out_ unsigned* pcAttributes) noexcept;

    /*
    Splits the provided event attributes string into name-value pairs.

    On success, returns true. On failure, returns false. Check LastError()
    for details.

    - Fails with ERROR_INSUFFICIENT_BUFFER and sets *pcAttributes to the
      number of attributes present if cAttributes is too small.
    - Fails with ERROR_OUTOFMEMORY or ERROR_INVALID_DATA for decoding problems.

    The szEventAttributes string is assumed to be a semicolon-separated list
    of Name=Value pairs (can also be null or empty, indicating no attributes).
    Values can be quoted (e.g. for a value that contains a semicolon), and
    quotes can be included in a value by doubling them.

    For example, the following attributes string:

        ONE=Value1;TWO="SubValue1;""SubValue2"";SubValue3"

    would be split into the following name/value pairs:

        ONE = Value1
        TWO = SubValue1;"SubValue2";SubValue3

    pcAttributes must not be null. It receives the number of attributes
    present in the provided string.

    pAttributes receives the attribute values. It must point at a buffer with
    room for cAttributes EtwAttributeInfo elements. (pAttributes may be null
    if cAttributes is 0.)

    The returned Name and Value pointers point into a buffer
    maintained within the EtwEnumerator object. This buffer becomes invalid
    the next time a non-const method is invoked on this EtwEnumerator object
    (e.g. Clear, StartEvent, MoveNext, SplitEventAttributesFromCurrent,
    FormatValueFromCurrent, etc.) or when the EtwEnumerator object is
    destroyed.
    */
    bool SplitEventAttributes(
        _In_opt_z_ EtwPCWSTR szEventAttributes,
        _Out_writes_to_(cAttributes, *pcAttributes) EtwAttributeInfo* pAttributes,
        unsigned cAttributes,
        _Out_ unsigned* pcAttributes) noexcept;

    /*
    Formats the current event's provider name as if by %!PROVIDER!.

    PRECONDITION: State != None, i.e. this can be called after a successful
    call to StartEvent or StartEventWithTraceEventInfo, until a call to Clear.

    On success, returns true. On failure, returns false. Check LastError()
    for details.

    If the provider has no name, returns the provider GUID. Otherwise, if the
    provider's name includes a GUID suffix, returns the provider name without
    the suffix. Otherwise, returns the provider name.

    The returned Data pointer points into a buffer maintained within the
    EtwEnumerator object. This buffer becomes invalid the next time a
    non-const method is invoked on this EtwEnumerator object (e.g. Clear,
    StartEvent, MoveNext, SplitEventAttributesFromCurrent,
    FormatValueFromCurrent, etc.) or when the EtwEnumerator object is
    destroyed.
    */
    bool FormatCurrentProviderName(
        _Out_ EtwStringViewZ* pString) noexcept;

    /*
    Formats the current event's name as if by %!EVENT!.

    PRECONDITION: State != None, i.e. this can be called after a successful
    call to StartEvent or StartEventWithTraceEventInfo, until a call to Clear.

    On success, returns true. On failure, returns false. Check LastError()
    for details.

    If the event has no name, returns EventId + "v" + EventVersion.

    The returned Data pointer points into a buffer maintained within the
    EtwEnumerator object. This buffer becomes invalid the next time a
    non-const method is invoked on this EtwEnumerator object (e.g. Clear,
    StartEvent, MoveNext, SplitEventAttributesFromCurrent,
    FormatValueFromCurrent, etc.) or when the EtwEnumerator object is
    destroyed.
    */
    bool FormatCurrentEventName(
        _Out_ EtwStringViewZ* pString) noexcept;

    /*
    Formats the current event's keywords as if by %!KEYWORDS!.

    PRECONDITION: State != None, i.e. this can be called after a successful
    call to StartEvent or StartEventWithTraceEventInfo, until a call to Clear.

    On success, returns true. On failure, returns false. Check LastError()
    for details.

    If the event has no keyword names, returns an integer e.g. "0x10".

    The returned Data pointer points into a buffer maintained within the
    EtwEnumerator object. This buffer becomes invalid the next time a
    non-const method is invoked on this EtwEnumerator object (e.g. Clear,
    StartEvent, MoveNext, SplitEventAttributesFromCurrent,
    FormatValueFromCurrent, etc.) or when the EtwEnumerator object is
    destroyed.
    */
    bool FormatCurrentKeywordsName(
        _Out_ EtwStringViewZ* pString) noexcept;

    /*
    Formats the current event's level as if by %!LEVEL!.

    PRECONDITION: State != None, i.e. this can be called after a successful
    call to StartEvent or StartEventWithTraceEventInfo, until a call to Clear.

    On success, returns true. On failure, returns false. Check LastError()
    for details.

    If the event has no level name, returns an integer e.g. "5".

    The returned Data pointer points into a buffer maintained within the
    EtwEnumerator object. This buffer becomes invalid the next time a
    non-const method is invoked on this EtwEnumerator object (e.g. Clear,
    StartEvent, MoveNext, SplitEventAttributesFromCurrent,
    FormatValueFromCurrent, etc.) or when the EtwEnumerator object is
    destroyed.
    */
    bool FormatCurrentLevelName(
        _Out_ EtwStringViewZ* pString) noexcept;

    /*
    Formats the current event's function name as if by %!FUNC!.

    PRECONDITION: State != None, i.e. this can be called after a successful
    call to StartEvent or StartEventWithTraceEventInfo, until a call to Clear.

    On success, returns true. On failure, returns false. Check LastError()
    for details.

    If the event has no function name, returns an empty string i.e. "".

    The returned Data pointer points into a buffer maintained within the
    EtwEnumerator object. This buffer becomes invalid the next time a
    non-const method is invoked on this EtwEnumerator object (e.g. Clear,
    StartEvent, MoveNext, SplitEventAttributesFromCurrent,
    FormatValueFromCurrent, etc.) or when the EtwEnumerator object is
    destroyed.
    */
    bool FormatCurrentFunctionName(
        _Out_ EtwStringViewZ* pString) noexcept;

    /*
    Formats the current event as a nul-terminated string. Uses the current
    event's EventMessage. Falls back to formatting as JSON if EventMessage is
    null or if unable to resolve a "%%n" parameter string.

    PRECONDITION: State != None, i.e. this can be called after a successful
    call to StartEvent or StartEventWithTraceEventInfo, until a call to Clear.

    On success, returns true. On failure, returns false. Check LastError()
    for details.

    If the current event has a non-null EventMessage and if EtwEnumerator is
    able to resolve all "%%n" parameter strings, this message returns a string
    formatted as with FormatCurrentEventWithMessage using the specified prefix
    and the current event's EventMessage (i.e. returns a string containing the
    formatted prefix followed by the formatted EventMessage). Otherwise (if
    the event has no EventMessage or if unable to resolve all "%%n" strings),
    this method returns a string formatted as with FormatCurrentEventAsJson
    (returns a string containing the formatted prefix followed by a JSON
    representation of the event's values and metadata).
    Refer to the documentation for FormatCurrentEventWithMessage and
    FormatCurrentEventAsJson for additional details.

    This method moves the enumeration position as it formats the items. After
    this method returns, the enumerator's position is unspecified. Use Reset()
    if necessary to move the enumerator back to the first item in the event.

    The returned Data pointer points into a buffer maintained within the
    EtwEnumerator object. This buffer becomes invalid the next time a
    non-const method is invoked on this EtwEnumerator object (e.g. Clear,
    StartEvent, MoveNext, SplitEventAttributesFromCurrent,
    FormatValueFromCurrent, etc.) or when the EtwEnumerator object is
    destroyed.
    */
    bool FormatCurrentEvent(
        _In_opt_z_ EtwPCWSTR szPrefixFormat,
        EtwJsonSuffixFlags jsonSuffixFlags,
        _Out_ EtwStringViewZ* pString) noexcept;

    /*
    Formats the current event as a nul-terminated string. Uses the specified
    event message.

    PRECONDITION: State != None, i.e. this can be called after a successful
    call to StartEvent or StartEventWithTraceEventInfo, until a call to Clear.

    On success, returns true. On failure, returns false. Check LastError()
    for details. Errors may include:

    - ERROR_MR_MID_NOT_FOUND if unable to resolve a parameter string.
    - ERROR_OUTOFMEMORY, ERROR_INVALID_DATA, etc. for decoding problems.

    This method returns a string containing a formatted prefix (szPrefixFormat
    formatted as with FormatCurrentEventPrefix) followed by a formatted
    szEventMessage.

    This method formats %n variables in szEventMessage using properties from
    the current event. Each %n variable (where n is a positive integer) is
    replaced with the value of the indicated top-level event property. For
    example, %1 would be replaced with the value of the first top-level
    property in the event. Values are replaced as follows:

    - If the corresponding property is complex (an array or a structure), the
      %n variable will be replaced with a JSON representation of the property
      value, e.g. [value1, value2] for an array, or
      {"field1"=value1,"field2"="value2"} for a structure.
    - Otherwise, if the variable is followed by a !format! specifier with a
      printf-style format (e.g. %2!08x!), the variable will be replaced using
      the specified formatting. If the format type does not match the property
      type (e.g. if variable %2!08X! refers to a string property), the
      formatting may be ignored or adjusted. If the property value contains nul
      characters, they will be replaced with spaces. (Note that recursion is
      disabled if a !format! specifier is used. Use a !S! format to disable
      unwanted recursion, potentially improving rendering performance.)
    - Otherwise, the property will be replaced using default formatting, then
      any nul characters will be replaced with spaces, and then the replaced
      string will be recursively formatted (up to an implementation-defined
      recursion depth limit).

    This method formats %%n parameters by calling
    enumeratorCallbacks.GetParameterMessage(). If GetParameterMessage
    succeeds, the %%n parameter is replaced and then the resulting string is
    recursively formatted (up to an implementation-defined recursion depth
    limit). If GetParameterMessage() returns ERROR_NOT_FOUND, the %%n string
    is left unreplaced.

    Note that the default implementation of GetParameterMessage always returns
    ERROR_MR_MID_NOT_FOUND (not ERROR_NOT_FOUND), so %%n parameters will cause
    this function to fail unless the default GetParameterMessage callback is
    overridden.

    This method formats %!variableName! parameters in szEventMessage using
    information from the current event as follows:

    - %!PROVIDER! is replaced with the provider name.
    - %!EVENT! is replaced with the event name.
    - %!TID! is replaced with the thread id.
    - %!TIME! is replaced with the event timestamp.
    - %!KTIME! is replaced with elapsed kernel time, in milliseconds.
      (Unavailable for events from in-process-private sessions.)
    - %!UTIME! is replaced with elapsed user time, in milliseconds.
      (Unavailable for events from in-process-private sessions.)
    - %!PID! is replaced with the process id.
    - %!CPU! is replaced with the CPU number.
    - %!FLAGS! or %!KEYWORDS! is replaced with the event keywords.
    - %!LEVEL! is replaced with the event level.
    - %!ATTRIBS! is replaced with the escaped event attributes (if any).
    - %!FUNC! is replaced with the event function name (if available).
    - %!FILE! is replaced with the event file name (if available).
    - %!LINE! is replaced with the event line number (if available).
    - %!MJ! or %!COMPNAME! is replaced with the event major component name.
    - %!MN! or %!SUBCOMP! is replaced with the event minor component name.
    - %!PTIME! is replaced with elapsed CPU cycles. (Available only for events
      from in-process-private sessions).
    - %!PCT! or %!PERCENT! is replaced with a '%' character.
    - %!BANG! or %!EXCLAMATION! is replaced with a '!' character.

    This method moves the enumeration position as it formats the items. After
    this method returns, the enumerator's position is unspecified. Use Reset()
    if necessary to move the enumerator back to the first item in the event.

    The returned Data pointer points into a buffer maintained within the
    EtwEnumerator object. This buffer becomes invalid the next time a
    non-const method is invoked on this EtwEnumerator object (e.g. Clear,
    StartEvent, MoveNext, SplitEventAttributesFromCurrent,
    FormatValueFromCurrent, etc.) or when the EtwEnumerator object is
    destroyed.
    */
    bool FormatCurrentEventWithMessage(
        _In_opt_z_ EtwPCWSTR szPrefixFormat,
        _In_z_ EtwPCWSTR szEventMessage,
        _Out_ EtwStringViewZ* pString) noexcept;

    /*
    Formats information about the current event as a nul-terminated string.

    PRECONDITION: State != None, i.e. this can be called after a successful
    call to StartEvent or StartEventWithTraceEventInfo, until a call to Clear.

    On success, returns true. On failure, returns false. Check LastError()
    for details. Errors may include:

    - ERROR_OUTOFMEMORY or ERROR_INVALID_DATA for decoding problems.

    The returned string will be a prefix generated by replacing %n and %!NAME!
    parameters in the szPrefixFormat string with information from the current
    event as follows:

    - %1 or %!PROVIDER! is replaced with the provider name.
    - %2 or %!EVENT! is replaced with the event name.
    - %3 or %!TID! is replaced with the thread id.
    - %4 or %!TIME! is replaced with the event timestamp.
    - %5 or %!KTIME! is replaced with elapsed kernel time, in milliseconds.
      (Unavailable for events from in-process-private sessions.)
    - %6 or %!UTIME! is replaced with elapsed user time, in milliseconds.
      (Unavailable for events from in-process-private sessions.)
    - %7 (sequence number) is replaced with 0.
    - %8 or %!PID! is replaced with the process id.
    - %9 or %!CPU! is replaced with the CPU number.
    - %!FLAGS! or %!KEYWORDS! is replaced with the event keywords.
    - %!LEVEL! is replaced with the event level.
    - %!ATTRIBS! is replaced with the escaped event attributes (if any).
    - %!FUNC! is replaced with the event function name (if available).
    - %!FILE! is replaced with the event file name (if available).
    - %!LINE! is replaced with the event line number (if available).
    - %!MJ! or %!COMPNAME! is replaced with the event major component name.
    - %!MN! or %!SUBCOMP! is replaced with the event minor component name.
    - %!PTIME! is replaced with elapsed CPU cycles. (Available only for events
      from in-process-private sessions).
    - %!PCT! or %!PERCENT! is replaced with a '%' character.
    - %!BANG! or %!EXCLAMATION! is replaced with a '!' character.

    The traditional default prefix is "[%9]%8.%3::%4 [%1]" which results in
    something like "[6]2FF0.4158::2018-04-01T00:23:38.6676083Z [MyProvName]".

    The returned Data pointer points into a buffer maintained within the
    EtwEnumerator object. This buffer becomes invalid the next time a
    non-const method is invoked on this EtwEnumerator object (e.g. Clear,
    StartEvent, MoveNext, SplitEventAttributesFromCurrent,
    FormatValueFromCurrent, etc.) or when the EtwEnumerator object is
    destroyed.
    */
    bool FormatCurrentEventPrefix(
        _In_z_ EtwPCWSTR szPrefixFormat,
        _Out_ EtwStringViewZ* pString) noexcept;

    /*
    Formats the current event as a nul-terminated string containing an
    optional prefix followed by a JSON object string.

    PRECONDITION: State != None, i.e. this can be called after a successful
    call to StartEvent or StartEventWithTraceEventInfo, until a call to Clear.

    On success, returns true. On failure, returns false. Check LastError()
    for details. Errors may include:

    - ERROR_OUTOFMEMORY or ERROR_INVALID_DATA for decoding problems.

    This method returns a string containing a prefix (szPrefixFormat formatted
    as with FormatCurrentEventPrefix) followed by a JSON object string.

    The JSON object string will contain event data items (one item for each
    top-level field in the event) followed by a "meta" item containing
    information about the event. Booleans, decimal integers, and finite
    floating-point data items will be unquoted. Other simple data items
    (including hexadecimal integers, infinities, and NaNs) will be quoted.
    Complex data items (structures and arrays) will be converted to JSON
    objects and arrays.

    The content of the "meta" item is configured via the jsonSuffixFlags
    parameter. If jsonSuffixFlags is 0, the "meta" item will be omitted. The
    following items may appear in "meta":
    - "provider" containing the provider's name.
    - "event" containing the event name (or the event's Id and Version if the
      event does not have an assigned name, e.g. "102v0").
    - "time" containing the event timestamp.
    - "cpu" containing the CPU id.
    - "pid" containing the process id.
    - "tid" containing thread id.
    - "level" if the event has a nonzero level.
    - "opcode" if the event has a nonzero opcode.
    - "task" if the event has a nonzero task.
    - "keywords" if the event has a nonzero keyword.
    - "tags" if the event has nonzero tags.
    - "activity" if the event has a nonzero activity id.
    - "relatedActivity" if the event has a related activity id.
    - "ktime" containing elapsed kernel time, in milliseconds, if available.
    - "utime" containing elapsed user time, in milliseconds, if available.
    - "ptime" containing CPU cycle counter for in-process-private sessions.
    - "attribs" containing escaped event attributes (if any).

    Example output using "PREFIX" as the prefix and EtwJsonSuffixFlags_event as flags:

        Event with a name but no values: PREFIX{"meta":{"event":"MyEventName"}}
        Event with no name, INT32=45, BOOL=1, FLOAT=1.2: PREFIX{"Val1":45,"Val2":true,"Val3":1.2,"meta":{"event":"123v0"}}
        Event with no name, HEXINT32=45, FLOAT=Infinity: PREFIX{"Val1":"0x2d","Val2":"inf","meta":{"event":"124v1"}}
        Event with no name, nonzero version, a struct and an array: PREFIX{"Val1":{"a":1,"b":2},"Val2":[1,2,3],"meta":{"event":"125v0"}}

    This method moves the enumeration position as it formats the items. After
    this method returns, the enumerator's position is unspecified. Use Reset()
    if necessary to move the enumerator back to the first item in the event.

    The returned Data pointer points into a buffer maintained within the
    EtwEnumerator object. This buffer becomes invalid the next time a
    non-const method is invoked on this EtwEnumerator object (e.g. Clear,
    StartEvent, MoveNext, SplitEventAttributesFromCurrent,
    FormatValueFromCurrent, etc.) or when the EtwEnumerator object is
    destroyed.
    */
    bool FormatCurrentEventAsJson(
        _In_opt_z_ EtwPCWSTR szPrefixFormat,
        EtwJsonSuffixFlags jsonSuffixFlags,
        _Out_ EtwStringViewZ* pString) noexcept;

    /*
    Formats the current logical item (value, struct, array, or event) as a
    nul-terminated JSON string and moves the enumerator to the logical item's
    next sibling. For enumerated (map) types, automatically obtains map
    information by calling enumeratorCallbacks.GetEventMapInformation().

    PRECONDITION: State is Value, ArrayBegin, StructBegin, or BeforeFirstItem.

    On success, returns true. On failure, returns false. Check LastError()
    for details. Possible errors include:

    - ERROR_OUTOFMEMORY or ERROR_INVALID_DATA for decoding problems.

    If State is BeforeFirstItem, this method returns a string containing a
    comma-separated list of JSON Members (i.e. "FieldName": Value pairs), one
    Member for each top-level item in the event. In this case, it changes the
    enumerator position to AfterLastItem.

    Otherwise, if the EtwJsonItemFlags_Name flag is specified, this method
    returns a string containing single JSON Member (i.e. a JSON Value with a
    "FieldName": prefix). Otherwise, this method returns a string containing a
    single JSON Value. In both cases, this method changes the enumerator
    position as if by a single call to MoveNextSibling().

    Booleans, decimal integers, and finite floating-point data values will be
    unquoted. Other simple data values (including hexadecimal integers, enums,
    infinities, and NaNs) will be quoted. Complex items (structures and arrays)
    will be converted to JSON Objects and Arrays.

    If a value has an associated map, this method will automatically invoke
    enumeratorCallbacks.GetEventMapInformation() as needed to load the map
    decoding information. If unable to load map decoding information, the
    value will be decoded as an integer.

    The returned Data pointer points into a buffer maintained within the
    EtwEnumerator object. This buffer becomes invalid the next time a
    non-const method is invoked on this EtwEnumerator object (e.g. Clear,
    StartEvent, MoveNext, SplitEventAttributesFromCurrent,
    FormatValueFromCurrent, etc.) or when the EtwEnumerator object is
    destroyed.
    */
    bool FormatCurrentItemAsJsonAndMoveNextSibling(
        EtwJsonItemFlags jsonItemFlags,
        _Out_ EtwStringViewZ* pString) noexcept;

    /*
    Formats the current value as a counted string. For enumerated (map) types,
    automatically obtains map information by calling
    enumeratorCallbacks.GetEventMapInformation().

    PRECONDITION: State == Value.

    On success, returns true. On failure, returns false. Check LastError()
    for details. Possible errors include:

    - ERROR_OUTOFMEMORY or ERROR_INVALID_DATA for decoding problems.

    If the value has an associated map, this method will automatically invoke
    enumeratorCallbacks.GetEventMapInformation() as needed to load the map
    decoding information. If unable to load map decoding information, the
    value will be decoded as an integer.

    The returned Data pointer points into a buffer maintained within the
    EtwEnumerator object. This buffer becomes invalid the next time a
    non-const method is invoked on this EtwEnumerator object (e.g. Clear,
    StartEvent, MoveNext, SplitEventAttributesFromCurrent,
    FormatValueFromCurrent, etc.) or when the EtwEnumerator object is
    destroyed.
    */
    bool FormatCurrentValue(
        _Out_ EtwStringView* pString) noexcept;

    /*
    Formats the specified value as a counted string. For enumerated (map)
    types, automatically obtains map information by calling
    enumeratorCallbacks.GetEventMapInformation().

    On success, returns true. On failure, returns false. Check LastError()
    for details. Possible errors include:

    - ERROR_INVALID_PARAMETER if cbData is incompatible with inType.
    - ERROR_OUTOFMEMORY or ERROR_INVALID_DATA for decoding problems.

    If pMapName is null, assumes that the value is not a map.
    If pMapName is non-null, this method automatically invokes
    enumeratorCallbacks.GetEventMapInformation() as needed to load the map
    decoding information based on the specified event record and map name. If
    unable to load map decoding information, the value will be decoded as an
    integer.

    Note that this function assumes that the pData and cbData values have the
    same semantics as the Data and DataSize fields of EtwItemInfo, i.e. that
    they skip the size parts of counted types, that they exclude string
    nul-termination, and that they skip the TOKEN_USER part of a WBEMSID.

    The returned Data pointer points into a buffer maintained within the
    EtwEnumerator object. This buffer becomes invalid the next time a
    non-const method is invoked on this EtwEnumerator object (e.g. Clear,
    StartEvent, MoveNext, SplitEventAttributesFromCurrent,
    FormatValueFromCurrent, etc.) or when the EtwEnumerator object is
    destroyed.
    */
    bool FormatValueWithMapName(
        _In_reads_bytes_(cbData) void const* pData,
        unsigned cbData,
        _TDH_IN_TYPE inType,
        _TDH_OUT_TYPE outType,
        _In_ EVENT_RECORD const* pEventRecord,
        _In_opt_ EtwPCWSTR pMapName,
        _Out_ EtwStringView* pString) noexcept;

    /*
    Formats the specified value as a counted string. For enumerated (map)
    types, uses the specified map decoding information.

    On success, returns true. On failure, returns false. Check LastError()
    for details. Possible errors include:

    - ERROR_INVALID_PARAMETER if cbData is incompatible with inType.
    - ERROR_OUTOFMEMORY or ERROR_INVALID_DATA for decoding problems.

    If pMapInfo is null, assumes that the value is not a map.
    If pMapInfo is non-null, the value is assumed to be a map and pMapInfo
    will be used to decode the value.

    Note that this function assumes that the pData and cbData values have the
    same semantics as the Data and DataSize fields of EtwItemInfo, i.e. that
    they skip the size parts of counted types, that they exclude string
    nul-termination, and that they skip the TOKEN_USER part of a WBEMSID.

    The returned Data pointer points into a buffer maintained within the
    EtwEnumerator object. This buffer becomes invalid the next time a
    non-const method is invoked on this EtwEnumerator object (e.g. Clear,
    StartEvent, MoveNext, SplitEventAttributesFromCurrent,
    FormatValueFromCurrent, etc.) or when the EtwEnumerator object is
    destroyed.
    */
    bool FormatValueWithMapInfo(
        _In_reads_bytes_(cbData) void const* pData,
        unsigned cbData,
        _TDH_IN_TYPE inType,
        _TDH_OUT_TYPE outType,
        _In_opt_ EVENT_MAP_INFO const* pMapInfo,
        _Out_ EtwStringView* pString) noexcept;

    /*
    Formats the specified value as a counted string. Does not consider
    enumeration (map) type information -- enumerations will be formatted as
    their underlying integer type.

    On success, returns true. On failure, returns false. Check LastError()
    for details. Possible errors include:

    - ERROR_INVALID_PARAMETER if cbData is incompatible with inType.
    - ERROR_OUTOFMEMORY or ERROR_INVALID_DATA for decoding problems.

    Note that this function assumes that the pData and cbData values have the
    same semantics as the Data and DataSize fields of EtwItemInfo, i.e. that
    they skip the size parts of counted types, that they exclude string
    nul-termination, and that they skip the TOKEN_USER part of a WBEMSID.

    The returned Data pointer points into a buffer maintained within the
    EtwEnumerator object. This buffer becomes invalid the next time a
    non-const method is invoked on this EtwEnumerator object (e.g. Clear,
    StartEvent, MoveNext, SplitEventAttributesFromCurrent,
    FormatValueFromCurrent, etc.) or when the EtwEnumerator object is
    destroyed.
    */
    bool FormatValue(
        _In_reads_bytes_(cbData) void const* pData,
        unsigned cbData,
        _TDH_IN_TYPE inType,
        _TDH_OUT_TYPE outType,
        _Out_ EtwStringView* pString) noexcept;

    /*
    Returns the number of 100ns units per "timer tick". This value is used to
    format the KTIME and UTIME variables (i.e. for converting the KernelTime
    and UserTime tick counts into milliseconds).
    */
    unsigned TimerResolution() const noexcept;

    /*
    Sets the number of 100ns units per "timer tick". This value is used to
    format the KTIME and UTIME variables (i.e. for converting the KernelTime
    and UserTime tick counts into milliseconds).

    The value to use here comes from the TimerResolution value of the
    TRACE_LOGFILE_HEADER structure. Use PreviewEvent to automatically update
    TimerResolution.
    */
    void SetTimerResolution(
        unsigned value) noexcept;

    /*
    Helper function that converts an EVENT_HEADER KernelTime or UserTime value
    into milliseconds, based on the current TimerResolution property.
    */
    unsigned TicksToMilliseconds(
        unsigned ticks) const noexcept;

    /*
    Returns the size that will be used for POINTER and SIZE_T fields when the
    event does not specify its own pointer size. This size will be 4 or 8.
    This value is not used very often since most events specify their own
    pointer size.
    */
    UCHAR PointerSizeFallback() const noexcept;

    /*
    This method is not needed very often. The default behavior is usually ok.

    PRECONDITION: value is 4 or 8.

    Sets the size to use for POINTER and SIZE_T fields when the event does not
    specify its own pointer size. The default pointer size fallback is the
    size of a pointer in the current (decoder) process, i.e. 4 if the
    enumerator is running in a 32-bit process, 8 if the enumerator is running
    in a 64-bit process. Changes to this setting take effect on the next
    StartEvent or MoveNext.

    Most events have a flag indicating whether the event originated from a
    32-bit process or a 64-bit process. If the flag is present, the size from
    the flag is used. WBEM/MOF events from very old trace files might not have
    the flag, in which case the pointer size fallback is used.
    */
    void SetPointerSizeFallback(
        UCHAR value) noexcept;

    /*
    Returns the format that will be used when formatting timestamps.
    This affects both event timestamps (e.g. from %!TIME! variables in the
    prefix) and FILETIME or SYSTEMTIME fields in the event.
    The default format is EtwTimestampFormat_Default.
    */
    EtwTimestampFormat TimestampFormat() const noexcept;

    /*
    Sets the format that will be used when formatting timestamps.
    This affects both event timestamps (e.g. from %!TIME! variables in the
    prefix) and FILETIME or SYSTEMTIME fields in the event.
    The default value is EtwTimestampFormat_Default.

    Returns true and updates format if value is a valid timestamp format.
    Returns false and does nothing if value is not a valid timestamp format.
    */
    bool SetTimestampFormat(EtwTimestampFormat value) noexcept;

    /*
    Gets the time zone bias that will be used when converting time stamps to
    local time (used only if the TimestampFormat has the
    EtwTimestampFormat_Local flag set).
    */
    int TimeZoneBiasMinutes() const noexcept;

    /*
    Sets the time zone bias that will be used when converting time stamps to
    local time (used only if the TimestampFormat has the
    EtwTimestampFormat_Local flag set). The default value is based on the
    local time zone when the EtwEnumerator object is constructed.

    PRECONDITION: value must be in the range -1440 and 1440 (i.e. bias must be
    24 hours or less).
    */
    void SetTimeZoneBiasMinutes(
        _In_range_(-1440, 1440) int value) noexcept;

    /*
    Adds TimeZoneBiasMinutes() to the specified utcFileTime value. Returns the
    adjusted time. Adjustment is performed using saturation, i.e. returns 0
    instead of a negative value and returns MAXINT64 instead of overflowing.
    */
    INT64 AdjustFileTimeToLocal(
        INT64 utcFileTime) const noexcept;

    /*
    Adds biasMinutes to the specified fileTime value. Returns the adjusted
    time. Adjustment is performed using saturation, i.e. returns 0 instead of a
    negative value and returns MAXINT64 instead of overflowing.
    */
    static INT64 AdjustFileTime(
        INT64 fileTime,
        int biasMinutes) noexcept;

private:

    struct StackEntry
    {
        USHORT PropertyIndex;
        USHORT PropertyEnd;
        USHORT ArrayIndex;
        USHORT ArrayCount;
        bool IsStruct;
        bool IsArray;
    };

    enum SubState : UCHAR;
    enum ValueType : UCHAR;
    enum Categories : UCHAR;
    class ParsedPrintf;
    class FormatContext;

private:

    void ResetImpl() noexcept;

    bool NextProperty() noexcept;

    void StartStruct() noexcept;

    bool StartArray() noexcept;

    bool StartValue() noexcept;

    void StartValueSimple() noexcept;

    void StartValueCounted() noexcept;

    void StartValueReversedCounted() noexcept;

    // Always returns false.
    bool SetNoneState(LSTATUS error) noexcept;

    // Always returns false.
    bool SetErrorState(LSTATUS error) noexcept;

    void SetEndState(
        EtwEnumeratorState newState,
        SubState newSubState) noexcept;

    void SetState(
        EtwEnumeratorState newState,
        SubState newSubState) noexcept;

    UCHAR PointerSize() const noexcept;

    bool
    StringViewResult(
        EtwInternal::Buffer<EtwWCHAR>& output,
        _Out_ EtwStringView* pString) noexcept;

    bool
    StringViewResult(
        EtwInternal::Buffer<EtwWCHAR>& output,
        _Out_ EtwStringViewZ* pString) noexcept;

    bool CurrentPropertyLength(
        _Out_ USHORT* pLength) const noexcept;

    _Ret_opt_z_ EtwPCWSTR EventName() const noexcept;

    _Ret_opt_z_ EtwPCWSTR EventAttributes() const noexcept;

    _Ret_opt_z_ EtwPCWSTR TaskName() const noexcept;

    _Ret_opt_z_ EtwPCWSTR OpcodeName() const noexcept;

    _Ret_opt_z_ EtwPCWSTR TeiString(
        ULONG offset) const noexcept;

    _Ret_z_ EtwPCWSTR TeiStringNoCheck(
        ULONG offset) const noexcept;

    LSTATUS AppendEventAttribute(
        EtwInternal::Buffer<EtwWCHAR>& output,
        _In_z_count_(cchEventAttributes) EtwPCWSTR szEventAttributes,
        unsigned cchEventAttributes,
        _In_count_(cchAttributeName) EtwWCHAR const* pchAttributeName,
        unsigned cchAttributeName) const noexcept;

    LSTATUS AppendResultCode(
        EtwInternal::Buffer<EtwWCHAR>& output,
        _In_reads_bytes_(4) void const* pData,
        int domain,         // EtwEnumeratorCallbacks::ResultCodeDomain
        int type) noexcept; // EtwEnumeratorCallbacks::UnderlyingType

    LSTATUS AppendCurrentProviderName(
        EtwInternal::Buffer<EtwWCHAR>& output) const noexcept;

    LSTATUS AppendCurrentProviderNameFallback(
        EtwInternal::Buffer<EtwWCHAR>& output) const noexcept;

    LSTATUS AppendCurrentEventName(
        EtwInternal::Buffer<EtwWCHAR>& output) const noexcept;

    LSTATUS AppendCurrentEventNameFallback(
        EtwInternal::Buffer<EtwWCHAR>& output) const noexcept;

    LSTATUS AppendCurrentKeywordsName(
        EtwInternal::Buffer<EtwWCHAR>& output) const noexcept;

    LSTATUS AppendCurrentLevelName(
        EtwInternal::Buffer<EtwWCHAR>& output) const noexcept;

    LSTATUS AppendCurrentFunctionName(
        EtwInternal::Buffer<EtwWCHAR>& output) const noexcept;

    LSTATUS AppendCurrentNameAsJson(
        EtwInternal::Buffer<EtwWCHAR>& output,
        bool wantSpace) noexcept;

    bool AddCurrentEventAsJson(
        EtwInternal::Buffer<EtwWCHAR>& output,
        EtwInternal::Buffer<EtwWCHAR>& scratchBuffer,
        EtwJsonSuffixFlags jsonSuffixFlags) noexcept;

    bool AddCurrentItemAsJsonAndMoveNext(
        EtwInternal::Buffer<EtwWCHAR>& output,
        EtwInternal::Buffer<EtwWCHAR>& scratchBuffer,
        EtwJsonItemFlags jsonItemFlags) noexcept;

    bool AddCurrentValueAsJson(
        EtwInternal::Buffer<EtwWCHAR>& output,
        EtwInternal::Buffer<EtwWCHAR>& scratchBuffer) noexcept;

    ValueType AddCurrentValue(
        EtwInternal::Buffer<EtwWCHAR>& output) noexcept;

    ValueType AddValueWithMapName(
        EtwInternal::Buffer<EtwWCHAR>& output,
        _In_reads_bytes_(cbData) void const* pData,
        unsigned cbData,
        _TDH_IN_TYPE inType,
        _TDH_OUT_TYPE outType,
        _In_ EVENT_RECORD const* pEventRecord,
        _In_opt_ EtwPCWSTR pMapName) noexcept;

    ValueType AddValueWithMapInfo(
        EtwInternal::Buffer<EtwWCHAR>& output,
        _In_reads_bytes_(cbData) void const* pData,
        unsigned cbData,
        _TDH_IN_TYPE inType,
        _TDH_OUT_TYPE outType,
        _In_opt_ EVENT_MAP_INFO const* pMapInfo) noexcept;

    ValueType AddValue(
        EtwInternal::Buffer<EtwWCHAR>& output,
        _In_reads_bytes_(cbData) void const* pData,
        unsigned cbData,
        _TDH_IN_TYPE inType,
        _TDH_OUT_TYPE outType) noexcept;

private:

    TRACE_EVENT_INFO const* m_pTraceEventInfo;
    EVENT_RECORD const* m_pEventRecord;
    BYTE const* m_pbDataEnd;

    BYTE const* m_pbDataNext;
    BYTE const* m_pbCooked;
    USHORT m_cbCooked;
    USHORT m_cbRaw; // Relative to m_pbDataNext
    USHORT m_cookedInType;
    USHORT m_cbElement;  // 0 if item is complex.
    StackEntry m_stackTop;

    EtwEnumeratorState m_state;
    SubState m_subState;
    UCHAR m_cbPointerFallback; // Pointer size to use if event doesn't specify a size.

    LSTATUS m_lastError;
    EtwTimestampFormat m_timestampFormat;
    int m_timeZoneBiasMinutes;
    unsigned m_ticksToMilliseconds; // Number of milliseconds per tick.
    EtwEnumeratorCallbacks& m_enumeratorCallbacks;

    // Assume most events have fewer than 32 properties.
    EtwInternal::Buffer<USHORT, 32> m_integerValues;

    // Assume most events have fewer than 4 levels of nested structures.
    EtwInternal::Buffer<StackEntry, 4> m_stack;

    // Assume the FormatValue services will be used. Inline a 32-char buffer.
    EtwInternal::Buffer<EtwWCHAR, 32> m_stringBuffer;

    // FormatCurrentEvent might not be used, and its results are usually too
    // large to reasonably allocate inline. Always heap-allocate, but avoid
    // invalid parameter errors from _vsnwprintf by inlining a tiny buffer.
    EtwInternal::Buffer<EtwWCHAR, sizeof(void*)/sizeof(EtwWCHAR)> m_stringBuffer2;

    // TDH buffers are too large to allocate inline. Always heap-allocate.
    EtwInternal::Buffer<BYTE> m_teiBuffer;
    EtwInternal::Buffer<BYTE> m_mapBuffer;
};

/*
The current state of an EtwEnumerator.
Generally refers to the category of the item at which the enumerator is
currently positioned.
*/
enum EtwEnumeratorState
    : UCHAR
{
    EtwEnumeratorState_None,           // After construction, Clear, or failed StartEvent.
    EtwEnumeratorState_Error,          // After an error from a MoveNext operation.
    EtwEnumeratorState_AfterLastItem,  // Positioned after the last item in the event.
    // MoveNext() is an invalid operation for all states above this line.
    // MoveNext() is a valid operation for all states below this line.
    EtwEnumeratorState_BeforeFirstItem,// Positioned before the first item in the event.
    // GetItemInfo() is an invalid operation for all states above this line.
    // GetItemInfo() is a valid operation for all states below this line.
    EtwEnumeratorState_Value,          // Positioned at an item with data (a field or an array element).
    EtwEnumeratorState_ArrayBegin,     // Positioned before the first item in an array.
    EtwEnumeratorState_StructBegin,    // Positioned before the first item in a struct.
    EtwEnumeratorState_ArrayEnd,       // Positioned after the last item in an array.
    EtwEnumeratorState_StructEnd,      // Positioned after the last item in a struct.
};

/*
Options for use when formatting an item as a JSON string with
FormatCurrentItemAsJsonAndMoveNextSibling.
*/
enum EtwJsonItemFlags
    : unsigned
{
    EtwJsonItemFlags_None = 0,
    EtwJsonItemFlags_Name = 0x1,  // Include "FieldName": prefix.
    EtwJsonItemFlags_Space = 0x2, // Add a space after ':' and ',' characters.
};

/*
Flags controlling the metadata to be included in the suffix of a JSON event
string.
*/
enum EtwJsonSuffixFlags
    : unsigned
{
    EtwJsonSuffixFlags_None = 0,         // disable the "meta" section.
    EtwJsonSuffixFlags_provider = 0x1,   // provider name (or GUID if no name).
    EtwJsonSuffixFlags_event = 0x2,      // event name (or IDvVERSION if no name).
    EtwJsonSuffixFlags_time = 0x4,       // timestamp (e.g. "2018-04-01T00:23:38.6676037Z").
    EtwJsonSuffixFlags_cpu = 0x8,        // physical CPU number (decimal integer).
    EtwJsonSuffixFlags_pid = 0x10,       // process ID (decimal integer).
    EtwJsonSuffixFlags_tid = 0x20,       // thread ID (decimal integer).
    EtwJsonSuffixFlags_id = 0x40,        // event ID (decimal integer).
    EtwJsonSuffixFlags_version = 0x80,   // event version (decimal integer, omitted if 0).
    EtwJsonSuffixFlags_channel = 0x100,  // channel (name or decimal integer, omitted if 0).
    EtwJsonSuffixFlags_level = 0x200,    // level (name or decimal integer, omitted if 0).
    EtwJsonSuffixFlags_opcode = 0x400,   // opcode (name or decimal integer, omitted if 0).
    EtwJsonSuffixFlags_task = 0x800,     // task (name or decimal integer, omitted if 0).
    EtwJsonSuffixFlags_keywords = 0x1000,// keywords (name or decimal integer, omitted if 0).
    EtwJsonSuffixFlags_tags = 0x2000,    // event tags (hexadecimal, omitted if 0).
    EtwJsonSuffixFlags_activity = 0x4000,// activity ID (GUID string, omitted if 0).
    EtwJsonSuffixFlags_relatedActivity = 0x8000,// related activity ID (GUID string, omitted if not set).
    EtwJsonSuffixFlags_ktime = 0x10000,  // elapsed kernel time in milliseconds (omitted if unavailable).
    EtwJsonSuffixFlags_utime = 0x20000,  // elapsed user time in milliseconds (omitted if unavailable).
    EtwJsonSuffixFlags_ptime = 0x40000,  // elapsed processor time in CPU ticks (omitted if unavailable).
    EtwJsonSuffixFlags_attribs = 0x80000,// escaped event attributes (omitted if unavailable).
    EtwJsonSuffixFlags_Default = 0xff3f, // Include provider..relatedActivity, except id & version.
    EtwJsonSuffixFlags_All = ~0u
};

/*
General type of an event.
*/
enum EtwEventCategory
    : unsigned
{
    // Invalid event category.
    EtwEventCategory_Error = 0,

    // Event was generated using a TraceMessage-style API. These events
    // generally use TMF-based WPP decoding.
    // Note that TdhGetEventInformation() returns incomplete information for
    // TraceMessage events, and EtwEnumerator will not work correctly with the
    // incomplete information. StartEvent() and StartEventWithTraceEventInfo()
    // will return an error if traceEventInfo.DecodingSource == WPP.
    EtwEventCategory_TmfWpp,

    // Event was generated using a TraceEvent-style API. These events generally
    // use MOF/WBEM decoding. These events are sometimes called "Classic ETW".
    EtwEventCategory_Wbem,

    // Event was generated using an EventWrite-style API but does not include
    // TraceLogging metadata. These events generally use manifest decoding.
    // EventWrite-style events are sometimes called "Crimson ETW".
    EtwEventCategory_Manifest,

    // Event was generated using an EventWrite-style API and includes
    // TraceLogging metadata. These events use TraceLogging decoding.
    // EventWrite-style events are sometimes called "Crimson ETW".
    EtwEventCategory_TraceLogging,

    // Invalid event category.
    EtwEventCategory_Max
};

/*
Specifies timestamp formatting.
*/
enum EtwTimestampFormat
    : unsigned
{
    /*
    No timestamp format type has been set.
    Invalid type - valid format types are greater than this value.
    */
    EtwTimestampFormat_None = 0,

    /*
    Uses Internet Date/Time (rfc3339) format, "2009-06-15T13:45:30.1234567".
    */
    EtwTimestampFormat_Internet,

    /*
    Uses "tracefmt" (traditional WPP) format, "06/15/2009-13:45:30.1234567".
    */
    EtwTimestampFormat_Wpp,

    /*
    Invalid type - valid timestamp format types are less than this value.
    */
    EtwTimestampFormat_Max,

    /*
    If this flag is set, timestamps known to be UTC are converted to local
    time before formatting.
    */
    EtwTimestampFormat_Local = 0x100,

    /*
    By default, SYSTEMTIME is formatted with 3 digits of subsecond precision,
    and FILETIME is formatted with 7 digits of subsecond precision.
    If this flag is set, both SYSTEMTIME and FILETIME timestamps will use 3
    digits of subsecond precision.
    */
    EtwTimestampFormat_LowPrecision = 0x200,

    /*
    By default, timestamps with a known time zone are formatted with a time
    zone suffix. UTC timestamps use "Z" and local timestamps use "+NN:NN",
    e.g. "-08:00", or "+03:30". If this flag is set, no suffix will be used.
    */
    EtwTimestampFormat_NoTimeZoneSuffix = 0x400,

    /*
    By default, TDH_INTYPE_FILETIME and TDH_INTYPE_SYSTEMTIME are assumed to
    be "unspecified time zone" unless they are TDH_OUTTYPE_DATETIME_UTC.
    Set this flag to treat TDH_INTYPE_FILETIME as UTC. (Even with this flag,
    TDH_INTYPE_SYSTEMTIME is still treated as unspecified time zone.)
    */
    EtwTimestampFormat_AssumeFileTimeUTC = 0x800,

    /*
    Default: Internet format, not converted to local time, full precision,
    with "Z" suffix if timestamp is known to be UTC.
    */
    EtwTimestampFormat_Default = EtwTimestampFormat_Internet,

    EtwTimestampFormat_FlagMask = 0xf00,
    EtwTimestampFormat_TypeMask = 0x0ff,
};

/*
Receives information about the event currently being processed.
Note that all of the string fields may be null if the event does not have an
assigned value for the specified field.
*/
struct EtwEventInfo
{
    /*
    The name of the event, or null if the event does not have an assigned name.
    */
    EtwPCWSTR Name;

    union {
        struct {
            /*
            The tags applied to the current event, or 0 if no tags are set.
            */
            UINT32 Tags : 28;
        };
        UINT32 Reserved_Tags;
    };

    /*
    The event's BinaryXml data, or 0/null if event does not have BinaryXml.
    */
    UINT32 BinaryXmlSize;
    _Field_size_bytes_(BinaryXmlSize) void const* BinaryXml;

    GUID const* DecodeGuid;
    GUID const* ControlGuid;

    /*
    The event's task's EventGuid. May be null.
    */
    GUID const* EventGuid;

    EtwPCWSTR ProviderName;
    EtwPCWSTR LevelName;
    EtwPCWSTR ChannelName;
    EtwPCWSTR KeywordsName;
    EtwPCWSTR TaskName;
    EtwPCWSTR OpcodeName;
    EtwPCWSTR EventMessage;
    EtwPCWSTR ProviderMessage;
    EtwPCWSTR EventAttributes;
    EtwPCWSTR WbemActivityIdName;
    EtwPCWSTR WbemRelatedActivityIdName;
};

/*
Receives information about an event attribute.
*/
struct EtwAttributeInfo
{
    EtwPCWSTR Name;
    EtwPCWSTR Value;
};

/*
Receives information about the remaining event payload, i.e. the data that has
not yet been decoded.
*/
struct EtwRawDataPosition
{
    USHORT DataSize;
    _Field_size_bytes_(DataSize) void const* Data;
};

/*
Receives information about the item on which the enumerator is currently
positioned. The meaning of some fields in EtwItemInfo depends on the type of
the current item (i.e. the current State() of the enumerator).
*/
struct EtwItemInfo
{
    /*
    The name of the current item, or "" if item has no name (rare).
    */
    EtwPCWSTR Name;

    union {
        struct {
            /*
            The tags applied to the current field, or 0 if no tags are set.
            */
            UINT32 Tags : 28;

            /*
            If the current item is BeginArray or EndArray, or if the current
            item is an array element, IsArray == 1. Otherwise, IsArray == 0.
            */
            UINT32 IsArray : 1;
        };
        UINT32 Reserved_Tags;
    };

    /*
    The current field's intype.

    The InType here is the canonical intype, which may be different from the
    raw intype. For example, there are several different raw intypes
    representing variations of UNICODESTRING, but all of these raw intypes
    map to the canonical intype for TDH_INTYPE_UNICODESTRING. Since
    EtwEnumerator handles finding the string's data and size, the user does
    not usually need to care about which specific kind of UNICODESTRING was
    used in the raw event.

    If you need access to the original (raw) intype, use GetRawItemInfo.

    Canonical InType         Raw InType(s)
    -------------------------------------------------
    TDH_INTYPE_NULL          TDH_INTYPE_NULL
    TDH_INTYPE_UNICODESTRING TDH_INTYPE_UNICODESTRING
                             TDH_INTYPE_MANIFEST_COUNTEDSTRING
                             TDH_INTYPE_COUNTEDSTRING
                             TDH_INTYPE_REVERSEDCOUNTEDSTRING
                             TDH_INTYPE_NONNULLTERMINATEDSTRING
    TDH_INTYPE_ANSISTRING    TDH_INTYPE_ANSISTRING
                             TDH_INTYPE_MANIFEST_COUNTEDANSISTRING
                             TDH_INTYPE_COUNTEDANSISTRING
                             TDH_INTYPE_REVERSEDCOUNTEDANSISTRING
                             TDH_INTYPE_NONNULLTERMINATEDANSISTRING
    TDH_INTYPE_INT8          TDH_INTYPE_INT8
    TDH_INTYPE_UINT8         TDH_INTYPE_UINT8
    TDH_INTYPE_INT16         TDH_INTYPE_INT16
    TDH_INTYPE_UINT16        TDH_INTYPE_UINT16
    TDH_INTYPE_INT32         TDH_INTYPE_INT32
    TDH_INTYPE_UINT32        TDH_INTYPE_UINT32
    TDH_INTYPE_INT64         TDH_INTYPE_INT64
    TDH_INTYPE_UINT64        TDH_INTYPE_UINT64
    TDH_INTYPE_FLOAT         TDH_INTYPE_FLOAT
    TDH_INTYPE_DOUBLE        TDH_INTYPE_DOUBLE
    TDH_INTYPE_BOOLEAN       TDH_INTYPE_BOOLEAN
    TDH_INTYPE_BINARY        TDH_INTYPE_BINARY
                             TDH_INTYPE_MANIFEST_COUNTEDBINARY
                             TDH_INTYPE_HEXDUMP
    TDH_INTYPE_GUID          TDH_INTYPE_GUID
    TDH_INTYPE_POINTER       TDH_INTYPE_POINTER
    TDH_INTYPE_FILETIME      TDH_INTYPE_FILETIME
    TDH_INTYPE_SYSTEMTIME    TDH_INTYPE_SYSTEMTIME
    TDH_INTYPE_SID           TDH_INTYPE_SID
                             TDH_INTYPE_WBEMSID
    TDH_INTYPE_HEXINT32      TDH_INTYPE_HEXINT32
    TDH_INTYPE_HEXINT64      TDH_INTYPE_HEXINT64
    TDH_INTYPE_UNICODECHAR   TDH_INTYPE_UNICODECHAR
    TDH_INTYPE_ANSICHAR      TDH_INTYPE_ANSICHAR
    TDH_INTYPE_SIZET         TDH_INTYPE_SIZET
    */
    _TDH_IN_TYPE  InType  : 16;

    /*
    The current field's outtype.
    */
    _TDH_OUT_TYPE OutType : 16;

    /*
    If IsArray is true, contains the array index. Otherwise 0. Specifically:
    - For BeginArray state: ArrayIndex == 0.
    - For EndArray state: ArrayIndex == ArrayCount.
    - For array elements: the element's array index.
    - Otherwise: 0.
    */
    USHORT ArrayIndex;

    /*
    If IsArray is true, contains the array length. Otherwise 1.
    */
    USHORT ArrayCount;

    /*
    If the current item's data type has a fixed size (e.g. INT8, INT32, GUID),
    ElementSize is the size of a single item of this type (e.g. 1 for INT8, 4
    for INT32, 16 for GUID).
    If the current item's data type has a variable size (e.g. string, binary,
    sid, struct), ElementSize is 0.
    */
    USHORT ElementSize;

    /*
    For Value state: DataSize = the size of the current item's Data.
    For BeginArray state: DataSize = ArrayCount * ElementSize.
    For other states: DataSize = 0.

    Data points into the buffer of pEventRecord. The pointer may be unaligned
    and will become invalid when pEventRecord becomes invalid.

    For some data types, the Data field excludes a portion of the raw encoded
    data:
    - For nul-terminated strings, Data does not include the nul termination.
    - For counted string, Data does not include the count.
    - For counted binary, Data does not include the count.
    - For WBEMSID, Data points at the SID (does not include the TOKEN_USER).
    */
    USHORT DataSize;
    _Field_size_bytes_(DataSize) void const* Data;

    /*
    If the value is an enumeration or bitfield with a map, MapName contains
    the name of the map for use with TdhGetEventMapInformation(). Otherwise,
    MapName is null.
    */
    EtwPCWSTR MapName;
};

/*
Receives technical details about the item on which the enumerator is currently
positioned. The meaning of some fields in EtwRawItemInfo depends on the type
of the current item (i.e. the current State() of the enumerator).
*/
struct EtwRawItemInfo
{
    union {
        struct {
            /*
            EtwItemInfo::InType returns a canonical intype value, i.e. several
            different variations of utf-16le strings are reduced to the single
            type TDH_INTYPE_UNICODESTRING. EtwRawItemInfo::RawInType returns
            the original (raw) intype of the event.
            */
            _TDH_IN_TYPE RawInType : 16;

            /*
            The property flags set for the current item.
            */
            PROPERTY_FLAGS Flags  : 16;
        };
        UINT32 Reserved_RawInType;
    };

    /*
    EtwItemInfo::Data sometimes skips certain parts of the raw event data such
    as the raw size prefix or the string's nul termination.
    EtwRawItemInfo::RawData presents the full binary field value, including
    size prefix and nul termination.
    */
    UINT32 RawDataSize;
    _Field_size_bytes_(RawDataSize) void const* RawData;

    /*
    Pointer to the field's custom schema information, or null if the field
    does not have custom schema information. Custom schema information is
    rarely used, so this will almost always be null.

    Custom schema information is decoding information for use by another tool.
    Some fields contain encoded data that can be processed by another tool,
    e.g. binary data packed using a serialization system such as Protocol
    Buffers or Bond. These fields will be encoded using a normal ETW intype
    (e.g. BINARY), but can contain schema information to be passed to the
    deserialization tool. If such as system is in use, the CustomSchema
    pointer will point at the schema data. The schema data is a structure
    laid out as follows:

    UINT16 Protocol; // User-defined protocol identifier 5..31
    UINT16 Length;
    BYTE SchemaData[Length]; // Decoding information for the decoding tool.
    */
    void const* CustomSchema;
};

/*
Receives a pointer to a counted string and a length.
Note that ETW supports strings with embedded nul characters. The string
referenced by this structure will not be nul-terminated and may contain
embedded nul characters. Use DataLength instead of assuming the string is
nul-terminated.
*/
struct EtwStringView
{
    _Field_size_(DataLength) EtwWCHAR const* Data;
    UINT32 DataLength;
};

/*
Receives a pointer to a nul-terminated string and a length. The nul
termination character is not included in the length, i.e. the nul termination
will always be at position Data[DataLength].
*/
struct EtwStringViewZ
{
    _Field_size_(DataLength + 1) EtwPCWSTR Data;
    UINT32 DataLength;
};

/*
EtwEnumerator passes an instance of EtwStringBuilder to the methods of
EtwEnumeratorCallbacks.
*/
class EtwStringBuilder
{
    // EtwStringBuilder is constructed only by EtwEnumerator.
    friend class EtwEnumerator;
    explicit EtwStringBuilder(EtwInternal::Buffer<EtwWCHAR>& buffer) noexcept;

public:

    EtwStringBuilder(EtwStringBuilder const&) = delete;
    EtwStringBuilder& operator=(EtwStringBuilder const&) = delete;

    // Appends the specified character to the string builder.
    LSTATUS __stdcall
    AppendChar(
        EtwWCHAR value) noexcept;

    // Appends the specified nul-terminated string to the string builder.
    LSTATUS __stdcall
    AppendWide(
        _In_z_ EtwPCWSTR szValue) noexcept;

    // Appends the specified swprintf-formatted string to the string builder.
    LSTATUS __stdcall
    AppendPrintf(
        _Printf_format_string_ EtwPCWSTR szFormat,
        ...) noexcept;

private:

    EtwInternal::Buffer<EtwWCHAR>& m_buffer;
};

/*
EtwEnumeratorCallbacks is an abstract base class that provides customization
points for EtwEnumerator behavior. If the default behavior of EtwEnumerator
does not meet your needs, you can provide a customized EtwEnumeratorCallbacks
object.

Implementing this class is optional. A default-constructed EtwEnumerator will
use TdhGetEventInformation() and TdhGetEventMapInformation() as needed to load
decoding information, will use FormatMessage() to look up error codes, and
will perform simple enum formatting.
*/
class DECLSPEC_NOVTABLE EtwEnumeratorCallbacks // abstract
{
protected:

    // This class is abstract.
    constexpr EtwEnumeratorCallbacks() noexcept = default;

public:

    // Copy construction is not allowed on the abstract base class.
    EtwEnumeratorCallbacks(EtwEnumeratorCallbacks const&) = delete;

    // Copy assignment is not allowed on the abstract base class.
    EtwEnumeratorCallbacks& operator=(EtwEnumeratorCallbacks const&) = delete;

    /*
    EtwEnumerator passes a value from this enumeration to FormatResultCodeValue().
    */
    enum ResultCodeDomain : int
    {
        ResultCodeDomainNone, // invalid value, never passed to FormatResultCodeValue().
        ResultCodeDomainWIN32,
        ResultCodeDomainHRESULT,
        ResultCodeDomainNTSTATUS,
        ResultCodeDomainMax // invalid value, never passed to FormatResultCodeValue().
    };

    /*
    EtwEnumerator passes a value from this enumeration to
    FormatResultCodeValue() and FormatMapValue().
    */
    enum UnderlyingType : int
    {
        UnderlyingTypeNone, // invalid value
        UnderlyingTypeHexadecimal,
        UnderlyingTypeUnsigned,
        UnderlyingTypeMax // invalid value
    };

    /*
    This method is invoked by EtwEnumerator::PreviewEvent() after
    PreviewEvent() has updated its metadata based on the event.

    The default implementation of this method does nothing and returns
    ERROR_SUCCESS (causing PreviewEvent() to succeed).

    If this method returns a status other than ERROR_SUCCESS then EtwEnumerator
    will return EtwEventCategory_Error to the caller of PreviewEvent().
    */
    virtual LSTATUS __stdcall OnPreviewEvent(
        _In_ EVENT_RECORD const* pEventRecord,
        EtwEventCategory eventCategory) noexcept;

    /*
    This method is invoked by EtwEnumerator::StartEvent().

    The default implementation of this method calls TdhGetEventInformation.

    If this method returns ERROR_INSUFFICIENT_BUFFER then EtwEnumerator will
    retry with a buffer at least as large as the new value of *pcbBuffer.

    If this method returns any other error then EtwEnumerator will return the
    specified error to the caller of StartEvent().

    This method should be implemented with behavior similar to that of
    TdhGetEventInformation(). In particular, if *pcbBuffer is too small, it
    should set *pcbBuffer to the required buffer size and then return
    ERROR_INSUFFICIENT_BUFFER.
    */
    virtual LSTATUS __stdcall GetEventInformation(
        _In_ EVENT_RECORD const* pEvent,
        _In_ ULONG cTdhContext,
        _In_reads_opt_(cTdhContext) TDH_CONTEXT const* pTdhContext,
        _Out_writes_bytes_opt_(*pcbBuffer) TRACE_EVENT_INFO* pBuffer,
        _Inout_ ULONG* pcbBuffer) noexcept;

    /*
    This method is invoked by EtwEnumerator when formatting a value with a map
    (i.e. an enumeration value).

    The default implementation of this method calls TdhGetEventMapInformation.

    If this method returns ERROR_NOT_FOUND then EtwEnumerator will format the
    value as an integer instead of formatting the value as an enumeration.

    If this method returns ERROR_INSUFFICIENT_BUFFER then EtwEnumerator will
    retry with a buffer at least as large as the new value of *pcbBuffer.

    If this method returns any other error then EtwEnumerator will return the
    specified error to the caller of the EtwEnumerator format method.

    This method should be implemented with behavior similar to that of
    TdhGetEventMapInformation(). In particular, if *pcbBuffer is too small, it
    should set *pcbBuffer to the required buffer size and then return
    ERROR_INSUFFICIENT_BUFFER.
    */
    virtual LSTATUS __stdcall GetEventMapInformation(
        _In_ EVENT_RECORD const* pEvent,
        _In_ EtwPCWSTR pMapName,
        _Out_writes_bytes_opt_(*pcbBuffer) EVENT_MAP_INFO* pBuffer,
        _Inout_ ULONG *pcbBuffer) noexcept;

    /*
    This method is invoked by EtwEnumerator::FormatCurrentEvent() and
    EtwEnumerator::FormatCurrentEventWithMessage() when formatting an event
    message containing a "%%n" parameter string.

    The default implementation of this method does nothing and returns
    ERROR_MR_MID_NOT_FOUND (causing FormatCurrentEventWithMessage() to fail
    and causing FormatCurrentEvent() to fall back to formatting without the
    EventMessage).

    If this method returns an error, it will cause event message formatting to
    fail. Note that if event message formatting fails with error
    ERROR_MR_MID_NOT_FOUND, FormatCurrentMessage() will retry formatting as if
    the event did not have an EventMessage instead of returning the
    ERROR_MR_MID_NOT_FOUND error to the caller.
    */
    virtual LSTATUS __stdcall GetParameterMessage(
        _In_ EVENT_RECORD const* pEvent,
        ULONG messageId,
        EtwStringBuilder& parameterMessageBuilder) noexcept;

    /*
    This method will be called when EtwEnumerator attempts to format a field
    containing a result code, e.g. when the OUTTYPE is NTSTATUS, HRESULT, or
    WIN32ERROR.

    The default implementation looks up the result code using FormatMessage.
    If FormatMessage succeeds, the method generates a message something like:
    "HResult Error 0x80070002: The system cannot find the file specified.".
    If FormatMessage fails, the method generates a message something like:
    "Unknown HResult Error code: 0x80070002".

    If this method returns ERROR_NOT_FOUND then EtwEnumerator will format the
    value as an integer instead of formatting the value as a result code.

    If this method returns any other error then EtwEnumerator will return the
    specified error to the caller of the EtwEnumerator format method.
    */
    virtual LSTATUS __stdcall FormatResultCodeValue(
        ResultCodeDomain domain,
        UnderlyingType valueType,
        ULONG value,
        EtwStringBuilder& resultCodeBuilder) noexcept;

    /*
    This method will be called when EtwEnumerator attempts to format an
    integer value with an associated map name (i.e. an enumeration value).

    The default implementation formats value maps with no decoration, returning
    ERROR_NOT_FOUND if no match for the value was found in the value map. It
    formats bitmaps with items separated by " | ", potentially followed by a
    hexadecimal value with any unrecognized bits, or returning ERROR_NOT_FOUND
    if no bits were recognized.

    If this method returns ERROR_NOT_FOUND then EtwEnumerator will format the
    value as an integer.

    If this method returns any other error then EtwEnumerator will return the
    specified error to the caller of the EtwEnumerator format method.
    */
    virtual LSTATUS __stdcall FormatMapValue(
        _In_ EVENT_MAP_INFO const* pMapInfo,
        UnderlyingType valueType,
        ULONG value,
        EtwStringBuilder& mapBuilder) noexcept;
};

#pragma warning(pop)
