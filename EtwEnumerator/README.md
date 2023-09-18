# EtwEnumerator

C++ library for decoding and formatting ETW events.

`EtwEnumerator` helps decode and format ETW events.

- Encapsulates tricky decoding rules behind a simple interface.
- Supports high performance event processing.
- Presents each field with its name, type, and value (binary data) in the
  order the field appears in the event's payload.
- Provides helper methods to format individual field values as strings.
- Provides helper methods to format the entire event as a string, either using
  the event's message string (if it has one) or using JSON. Uses formatting
  rules similar to the Windows SDK `tracefmt` tool.
- Supports structured data, i.e. arrays and nested structures.
- Exposes the information from
  [TRACE_EVENT_INFO](https://learn.microsoft.com/windows/win32/api/tdh/ns-tdh-trace_event_info)
  in a user-friendly manner, e.g. provider name, event name, control GUID, decode GUID. Developer should
  not need to use `TRACE_EVENT_INFO` directly when using this class.
- Exposes the information from
  [EVENT_PROPERTY_INFO](https://learn.microsoft.com/windows/win32/api/tdh/ns-tdh-event_property_info)
  in a user-friendly manner, e.g. field name, field types, field decoding. Developer should not need to
  use `EVENT_PROPERTY_INFO` directly when using this class.
- Non-goal: Does not decode TMF-based WPP events.
- Non-goal: Does not attempt to expose information easily accessible from the
  [EVENT_RECORD](https://learn.microsoft.com/windows/win32/api/evntcons/ns-evntcons-event_record)
  (e.g. does not expose `EVENT_DESCRIPTOR` or `ActivityId`).
  Developer should access these values from the `EVENT_RECORD` as needed.

General usage:

1. Developer's code initializes a callback context object that contains the
   objects needed for decoding an event. The context object should include an
   `EtwEnumerator` object.
2. Developer's code invokes
   [OpenTrace](https://learn.microsoft.com/windows/win32/api/evntrace/nf-evntrace-opentracew)
   with:
   - either an ETL filename in `EVENT_TRACE_LOGFILE.LogFileName`,
     or an ETW realtime session name in `EVENT_TRACE_LOGFILE.LoggerName`,
   - a pointer to the developer's
     [EventRecordCallback](https://learn.microsoft.com/windows/win32/api/evntrace/nc-evntrace-pevent_record_callback)
     function in `EVENT_TRACE_LOGFILE.EventRecordCallback`,
   - a pointer to the developer's callback context in
     `EVENT_TRACE_LOGFILE.Context`,
   - the correct flags in `EVENT_TRACE_LOGFILE.ProcessTraceMode` (must include
     `PROCESS_TRACE_MODE_EVENT_RECORD`; if processing a realtime session, must
     include `PROCESS_TRACE_MODE_REAL_TIME`; if using the FormatCurrentEvent
     methods, must NOT include `PROCESS_TRACE_MODE_RAW_TIMESTAMP`).
   OpenTrace returns a `TRACEHANDLE`.
2. Developer's code invokes
   [ProcessTrace](https://learn.microsoft.com/windows/win32/api/evntrace/nf-evntrace-processtrace)
   with the `TRACEHANDLE`. `ProcessTrace` invokes the developer's `EventRecordCallback`
   function once for each `pEventRecord` in the trace.
3. The developer's `EventRecordCallback` implementation uses the context's
   `EtwEnumerator` object to obtain information about the event.
   a. Get the pointer to the developer's callback context from
      `pEventRecord->UserContext`, and use the `etwEnumerator` from the context.
   b. Call `etwEnumerator.PreviewEvent` to update the `etwEnumerator`'s metadata
      and to determine the event category.
   c. If the category is `TmfWpp`, use `TdhGetProperty` or `TdhGetWppProperty` to
      decode the event instead of using `EtwEnumerator`. `EtwEnumerator` cannot
      decode TMF-based WPP.
   d. If the category is `Wbem` and `pEventRecord->EventHeader.ProviderId` equals
      `EventTraceGuid`, consider skipping the event. `Wbem` EventTrace events
      contain trace metadata, not normal event data, and are not usually shown
      in decoded traces.
   e. Call `etwEnumerator.StartEvent()` to look up decoding information and
      initialize the enumeration. (Or use `StartEventWithTraceEventInfo()` if
      you want to look up the decoding information yourself.)
   f. Use `etwEnumerator.GetEventInfo()` as needed to access event properties
      like provider name and event name.
   g. If you want to access individual field values, use the `MoveNext()`,
      `State()`, and `GetItemInfo()` methods to enumerate the event items.
      - Call `etwEnumerator.MoveNext()` to move to the next item in the event.
        Stop enumerating when `MoveNext()` returns false.
      - Call `etwEnumerator.State()` to determine the kind of item at the
        enumerator's current position: start/end of array, start/end of
        structure, or individual field value.
      - Call `etwEnumerator.GetItemInfo()` to get information about each item
        such as the name and type.
      - Handle the item as necessary. Optionally use `FormatCurrentValue()` to
        convert the value into a string.
   h. Use `FormatCurrentEvent` (or other `Format` methods) as needed to generate a
      string representation of the event data.
   i. Handle the end of event. Optionally use `GetRawDataPosition()` to
      determine whether any undecoded data is present in the event payload.

Example usage pattern, assuming the caller has received a `pEventRecord` from
`ProcessTrace` (i.e. this example would be the developer's `EventRecordCallback`
function):

```cpp
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
```
