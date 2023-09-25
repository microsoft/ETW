// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License.

/*
Sample for decoding ETW events using TdhFormatProperty.

This sample demonstrates the following:

- How to process events from ETL files using OpenTrace and ProcessTrace.
- How to format non-WPP events using EtwEnumerator.
- How to format WPP events using TdhGetProperty.
*/

#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN 1 // Exclude rarely-used APIs from <windows.h>
#endif

#include <windows.h>
#include <tdh.h>
#include <vector>

#include <wchar.h> // wprintf

#include <EtwEnumerator.h>

#pragma comment(lib, "tdh.lib") // Link against TDH.dll

/*
Decodes event data using EtwEnumerator and TdhGetProperty.
Prints event information to stdout.
*/
class DecoderContext
{
    EtwEnumerator m_enumerator;
    TDH_CONTEXT m_tdhContext[1]; // May contain TDH_CONTEXT_WPP_TMFSEARCHPATH.
    BYTE m_tdhContextCount;  // 1 if a TMF search path is present.
    std::vector<wchar_t> m_propertyBuffer; // Buffer for the string returned by TdhGetProperty.

public:

    /*
    Initialize the decoder context.

    - Configures the EtwEnumerator.
    - Sets up the TDH_CONTEXT array that will be used for decoding WPP.
    */
    explicit
    DecoderContext(_In_opt_ PCWSTR szTmfSearchPath)
        : m_enumerator() // Default-constructed enumerator uses default callbacks.
        , m_tdhContext()
        , m_tdhContextCount()
        , m_propertyBuffer()
    {
        // Configure the EtwEnumerator as desired.
        // This generates results similar to "tracefmt -sortableTime -utc".
        m_enumerator.SetTimestampFormat(static_cast<EtwTimestampFormat>(
            EtwTimestampFormat_Internet |
            EtwTimestampFormat_LowPrecision |
            EtwTimestampFormat_NoTimeZoneSuffix));

        // If a TMF search path was provided, set up the TDH_CONTEXT for it.
        if (szTmfSearchPath != nullptr)
        {
            m_tdhContext->ParameterValue = reinterpret_cast<UINT_PTR>(szTmfSearchPath);
            m_tdhContext->ParameterType = TDH_CONTEXT_WPP_TMFSEARCHPATH;
            m_tdhContext->ParameterSize = 0;
            m_tdhContextCount = 1;
        }
    }

    /*
    Decode and print the data for an event.
    */
    void
    PrintEventRecord(_In_ EVENT_RECORD* pEventRecord) noexcept
    {
        auto eventCategory = m_enumerator.PreviewEvent(pEventRecord);
        switch (eventCategory)
        {
        case EtwEventCategory_TmfWpp:

            PrintWppEvent(pEventRecord); // EtwEnumerator does not handle WPP events.
            break;

        case EtwEventCategory_Wbem:

            if (pEventRecord->EventHeader.EventDescriptor.Opcode == EVENT_TRACE_TYPE_INFO &&
                pEventRecord->EventHeader.ProviderId == EventTraceGuid)
            {
                /*
                The first event in every ETL file contains the data from the file header.
                This is the same data as was returned in the EVENT_TRACE_LOGFILEW by
                OpenTrace. Since we've already seen this information, we'll skip this
                event.
                */
                break;
            }

            __fallthrough;

        case EtwEventCategory_Manifest:
        case EtwEventCategory_TraceLogging:

            if (!m_enumerator.StartEvent(pEventRecord))
            {
                // Usually because we were unable to decode event.
                wprintf(L"[StartEvent error %u]\n", m_enumerator.LastError());
            }
            else
            {
                // Use EtwEnumerator to format the message.
                EtwStringViewZ formattedEvent;
                m_enumerator.FormatCurrentEvent(
                    L"[%9]%8.%3::%4 [%1]", // Traditional tracefmt message prefix.
                    EtwJsonSuffixFlags_Default,
                    &formattedEvent);
                wprintf(L"%ls\n", formattedEvent.Data);
            }
            break;
        }
    }

private:

    void PrintWppEvent(_In_ EVENT_RECORD* pEventRecord) noexcept
    {
        /*
        TDH supports a set of known properties for WPP events:
        - "Version": UINT32 (usually 0)
        - "TraceGuid": GUID
        - "GuidName": UNICODESTRING (module name)
        - "GuidTypeName": UNICODESTRING (source file name and line number)
        - "ThreadId": UINT32
        - "SystemTime": SYSTEMTIME
        - "UserTime": UINT32
        - "KernelTime": UINT32
        - "SequenceNum": UINT32
        - "ProcessId": UINT32
        - "CpuNumber": UINT32
        - "Indent": UINT32
        - "FlagsName": UNICODESTRING
        - "LevelName": UNICODESTRING
        - "FunctionName": UNICODESTRING
        - "ComponentName": UNICODESTRING
        - "SubComponentName": UNICODESTRING
        - "FormattedString": UNICODESTRING
        - "RawSystemTime": FILETIME
        - "ProviderGuid": GUID (usually 0)
        */

        SYSTEMTIME st = {};
        FileTimeToSystemTime((FILETIME*)&pEventRecord->EventHeader.TimeStamp, &st);
            
        // Print out the WPP event in a traditional format:
        // [CPU]PID.TID::TIME [Provider]Message
        wprintf(L"[%u]%04X.%04X::%04u-%02u-%02uT%02u:%02u:%02u.%03u [",
            pEventRecord->BufferContext.ProcessorNumber,
            pEventRecord->EventHeader.ProcessId,
            pEventRecord->EventHeader.ThreadId,
            st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond, st.wMilliseconds);
        PrintWppStringProperty(pEventRecord, L"GuidName"); // Module name (WPP's "CurrentDir" variable)
        wprintf(L"]");
        PrintWppStringProperty(pEventRecord, L"FormattedString");
        wprintf(L"\n");
    }

    /*
    Print the value of the given UNICODESTRING property.
    */
    void PrintWppStringProperty(
        _In_ EVENT_RECORD* pEventRecord,
        _In_z_ PCWSTR szPropertyName) noexcept
    {
        PROPERTY_DATA_DESCRIPTOR pdd = { reinterpret_cast<UINT_PTR>(szPropertyName) };

        ULONG status;
        ULONG cb = 0;
        status = TdhGetPropertySize(
            pEventRecord,
            m_tdhContextCount,
            m_tdhContextCount ? m_tdhContext : nullptr,
            1,
            &pdd,
            &cb);
        if (status == ERROR_SUCCESS)
        {
            try
            {
                if (m_propertyBuffer.size() < cb / 2)
                {
                    m_propertyBuffer.resize(cb / 2);
                }

                status = TdhGetProperty(
                    pEventRecord,
                    m_tdhContextCount,
                    m_tdhContextCount ? m_tdhContext : nullptr,
                    1,
                    &pdd,
                    cb,
                    reinterpret_cast<BYTE*>(m_propertyBuffer.data()));
            }
            catch (std::bad_alloc const&)
            {
                status = ERROR_OUTOFMEMORY;
            }
            catch (...)
            {
                status = ERROR_ASSERTION_FAILURE;
            }
        }

        if (status != ERROR_SUCCESS)
        {
            wprintf(L"[TdhGetProperty(%ls) error %u]", szPropertyName, status);
        }
        else
        {
            // Print the FormattedString property data (nul-terminated
            // wchar_t string).
            wprintf(L"%ls", m_propertyBuffer.data());
        }
    }
};

/*
Parses and stores the command line options.
*/
struct DecoderSettings
{
    std::vector<PCWSTR> etlFiles;
    std::vector<PCWSTR> manFiles;
    std::vector<PCWSTR> binFiles;
    PCWSTR szTmfSearchPath;
    bool showUsage;

    DecoderSettings(
        int argc,
        _In_count_(argc) PWSTR argv[])
        : szTmfSearchPath()
        , showUsage()
    {
        for (int i = 1; i < argc; i += 1)
        {
            PCWSTR szArg = argv[i];
            if (szArg[0] != L'/' && szArg[0] != L'-')
            {
                etlFiles.push_back(szArg);
            }
            else if (szArg[1] == L'\0' ||
                (szArg[2] != L'\0' && szArg[2] != L':' && szArg[2] != L'='))
            {
                // Options should be /X, /X:Value, or /X=Value
                wprintf(L"ERROR: Incorrectly-formatted option: %ls\n", szArg);
                showUsage = true;
            }
            else
            {
                PCWSTR szArgValue = &szArg[3];
                switch (szArg[1])
                {
                case L'?':
                case L'h':
                case L'H':
                    showUsage = true;
                    break;

                case L'B':
                case L'b':
                    binFiles.push_back(szArgValue);
                    break;

                case L'M':
                case L'm':
                    manFiles.push_back(szArgValue);
                    break;

                case L'T':
                case L't':
                    if (szTmfSearchPath == nullptr)
                    {
                        szTmfSearchPath = szArgValue;
                    }
                    else
                    {
                        wprintf(L"ERROR: TMF search path already set: %ls\n", szArg);
                        showUsage = true;
                    }
                    break;

                default:
                    wprintf(L"ERROR: Unrecognized option: %ls\n", szArg);
                    showUsage = true;
                    break;
                }
            }
        }

        if (!showUsage && etlFiles.empty())
        {
            wprintf(L"ERROR: No ETL files specified.\n");
            showUsage = true;
        }
    }
};

/*
Helper class to automatically close TRACEHANDLEs.
*/
class TraceHandles
{
public:

    ~TraceHandles()
    {
        CloseHandles();
    }

    void CloseHandles()
    {
        while (!handles.empty())
        {
            CloseTrace(handles.back());
            handles.pop_back();
        }
    }

    ULONG OpenTraceW(
        _Inout_ EVENT_TRACE_LOGFILEW* pLogFile)
    {
        ULONG status;

        handles.reserve(handles.size() + 1);
        TRACEHANDLE handle = ::OpenTraceW(pLogFile);
        if (handle == INVALID_PROCESSTRACE_HANDLE)
        {
            status = GetLastError();
        }
        else
        {
            handles.push_back(handle);
            status = 0;
        }

        return status;
    }

    ULONG ProcessTrace(
        _In_opt_ FILETIME* pStartTime,
        _In_opt_ FILETIME* pEndTime)
    {
        return ::ProcessTrace(
            handles.data(),
            static_cast<ULONG>(handles.size()),
            pStartTime,
            pEndTime);
    }

private:

    std::vector<TRACEHANDLE> handles;
};

/*
This function will be used as the EventRecordCallback function in EVENT_TRACE_LOGFILE.
It expects that the EVENT_TRACE_LOGFILE's Context pointer is set to a DecoderContext.
*/
static void WINAPI
EventRecordCallback(_In_ EVENT_RECORD* pEventRecord) noexcept
{
    // We expect that the EVENT_TRACE_LOGFILE.Context pointer was set with a
    // pointer to a DecoderContext. ProcessTrace will put the Context value
    // into EVENT_RECORD.UserContext.
    DecoderContext* pContext = static_cast<DecoderContext*>(pEventRecord->UserContext);

    // The actual decoding work is done in PrintEventRecord.
    pContext->PrintEventRecord(pEventRecord);
}

int __cdecl wmain(int argc, _In_count_(argc) PWSTR argv[])
{
    int exitCode;

    try
    {
        DecoderSettings settings(argc, argv);
        TraceHandles handles;
        if (settings.showUsage)
        {
            wprintf(LR"(
Usage:

  EtwEnumeratorDecode [options] filename1.etl (filename2.etl...)

Options:

  -m:ManifestFile.man  Load decoding data from a manifest with TdhLoadManifest.
  -b:ResourceFile.dll  Load decoding data from a DLL with
                       TdhLoadManifestFromBinary.
  -t:TmfSearchPath     Set the TMF search path to use for WPP events.
)");
            exitCode = 1;
            goto Done;
        }

        DecoderContext context(settings.szTmfSearchPath);

        for (size_t i = 0; i != settings.manFiles.size(); i += 1)
        {
            exitCode = TdhLoadManifest(const_cast<PWSTR>(settings.manFiles[i]));
            if (exitCode != 0)
            {
                wprintf(L"ERROR: TdhLoadManifest error %u for manifest: %ls\n",
                    exitCode,
                    settings.manFiles[i]);
                goto Done;
            }
        }

        for (size_t i = 0; i != settings.binFiles.size(); i += 1)
        {
            exitCode = TdhLoadManifestFromBinary(const_cast<PWSTR>(settings.binFiles[i]));
            if (exitCode != 0)
            {
                wprintf(L"ERROR: TdhLoadManifestFromBinary error %u for binary: %ls\n",
                    exitCode,
                    settings.binFiles[i]);
                goto Done;
            }
        }

        for (size_t i = 0; i != settings.etlFiles.size(); i += 1)
        {
            EVENT_TRACE_LOGFILEW logFile = { const_cast<PWSTR>(settings.etlFiles[i]) };
            logFile.ProcessTraceMode = PROCESS_TRACE_MODE_EVENT_RECORD;
            logFile.EventRecordCallback = &EventRecordCallback;
            logFile.Context = &context;

            exitCode = handles.OpenTraceW(&logFile);
            if (exitCode != 0)
            {
                wprintf(L"ERROR: OpenTraceW error %u for file: %ls\n",
                    exitCode,
                    settings.etlFiles[i]);
                goto Done;
            }

            wprintf(L"Opened: %ls\n", logFile.LogFileName);

            // Optionally print information read from the log file.
            // For example, show information about lost buffers and events.

            if (logFile.LogfileHeader.BuffersLost != 0)
            {
                wprintf(L"  **BuffersLost = %lu\n", logFile.LogfileHeader.BuffersLost);
            }

            if (logFile.LogfileHeader.EventsLost != 0)
            {
                wprintf(L"  **EventsLost = %lu\n", logFile.LogfileHeader.EventsLost);
            }
        }

        exitCode = handles.ProcessTrace(nullptr, nullptr);
        if (exitCode != 0)
        {
            wprintf(L"ERROR: ProcessTrace error %u\n",
                exitCode);
            goto Done;
        }
    }
    catch (std::exception const& ex)
    {
        wprintf(L"\nERROR: %hs\n", ex.what());
        exitCode = 1;
    }

Done:

    return exitCode;
}
