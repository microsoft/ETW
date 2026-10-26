use std::{
    ffi::c_void, mem::ManuallyDrop, panic::{catch_unwind, AssertUnwindSafe}, sync::{atomic::{AtomicBool, AtomicU64, Ordering}, Arc}, thread::JoinHandle
};

use rsevents::Awaitable;
use windows::{
    core::PWSTR,
    Win32::{Foundation::GetLastError, System::Diagnostics::Etw::*},
};

#[repr(C)]
struct EventTraceLogFile {
    props: EVENT_TRACE_LOGFILEW,
    name: [u16; 1024],
}

struct SharedThreadData {
    // When true, all callbacks from ProcessTrace will return immediately without calling into the Consumer
    stop_callbacks: AtomicBool,
    // When set, ProcessTrace has returned
    stopped: rsevents::ManualResetEvent,
    // The lifetime of this session handle will be at least as long as the call to ProcessTrace
    session_handle: AtomicU64,
}

impl SharedThreadData {
    fn close_trace(&self, block_on_thread_exit: bool) {
        unsafe {
            self.stop_callbacks.store(true, Ordering::Release);
            let _ = CloseTrace(PROCESSTRACE_HANDLE{Value: self.session_handle.swap(0xDEADBEEFDEADBEEF, Ordering::Release)});

            // Close trace is not synchronous for real-time sessions.
            // All events still in the buffers will be delivered before
            // ProcessTrace returns.
            if block_on_thread_exit {
                let _ = self.stopped.try_wait();
            }
        }
    }
}

struct CallbackThreadData<C>
where
    C: EventConsumer + 'static,
{
    consumer: C,
    shared_data: ManuallyDrop<Arc<SharedThreadData>>
}

impl<C> CallbackThreadData<C>
where
    C: EventConsumer + 'static,
{
    unsafe fn inner_callback(
        &self,
        event_record: *mut EVENT_RECORD,
    ) -> Result<(), windows::core::Error> {
        let stop = self.shared_data.stop_callbacks.load(Ordering::Acquire);
        if stop {
            Ok(())
        } else {
            unsafe { <C as EventConsumer>::on_event_raw(&self.consumer, event_record) }
        }
    }

    fn process_trace_complete(&self, err: windows::core::Error) {
        <C as EventConsumer>::complete(&self.consumer, err);
        self.shared_data.stopped.set();
    }
}

/// A wrapper around the Win32 OpenTrace function.
/// Opens an ETW session, either for an existing real-time session or
/// for a file. Does not start processing events until [`process_trace`](Self::process_trace) is called.
pub struct ProcessTraceHandle<C>
where
    C: EventConsumer + 'static,
{
    // When the thread is started, this will be moved to the thread's ownership
    callback_data: Box<CallbackThreadData<C>>,
    // Shared between the callback thread and the owning handle
    shared_data: Arc<SharedThreadData>,
}

impl<C> ProcessTraceHandle<C>
where
    C: EventConsumer + 'static,
{
    /// Opens the running real-time session `session_name`, delivering its events to `consumer`.
    ///
    /// # Panics
    ///
    /// Panics if `session_name` is empty or 1024 or more UTF-16 code units long.
    pub fn from_session(session_name: &str, consumer: C) -> Result<Self, windows::core::Error> {
        Self::create(session_name, false, consumer)
    }

    /// Opens the `.etl` log file `file_name`, delivering its events to `consumer`.
    ///
    /// # Panics
    ///
    /// Panics if `file_name` is empty or 1024 or more UTF-16 code units long.
    pub fn from_file(file_name: &str, consumer: C) -> Result<Self, windows::core::Error> {
        Self::create(file_name, true, consumer)
    }

    fn create(name: &str, is_file: bool, consumer: C) -> Result<Self, windows::core::Error> {
        unsafe {
            let shared_data = Arc::new(SharedThreadData {
                stop_callbacks: AtomicBool::new(false),
                stopped: rsevents::ManualResetEvent::new(rsevents::EventState::Unset),
                session_handle: AtomicU64::new(0) // Will be set below
            });

            let callback_data = Box::new(CallbackThreadData {
                consumer,
                shared_data: ManuallyDrop::new(shared_data.clone())
            });

            let mut log = {
                if name.is_empty() {
                    panic!()
                }

                let mut props: EventTraceLogFile = core::mem::zeroed();
                props.props.Anonymous1.ProcessTraceMode = if is_file {
                    PROCESS_TRACE_MODE_EVENT_RECORD
                } else {
                    PROCESS_TRACE_MODE_EVENT_RECORD | PROCESS_TRACE_MODE_REAL_TIME
                };
                props.props.Anonymous2.EventRecordCallback = Some(Self::event_record_callback);

                let name_utf16 = name.encode_utf16();

                let mut i = 0;
                for c in name_utf16 {
                    props.name[i] = c;
                    i += 1;

                    if i >= 1024 {
                        panic!();
                    }
                }

                props.name[i] = 0; // nul-terminate

                if is_file {
                    props.props.LogFileName = PWSTR::from_raw(props.name.as_mut_ptr());
                } else {
                    props.props.LoggerName = PWSTR::from_raw(props.name.as_mut_ptr());
                }

                // Cast through usize so tools can (potentially, in the future) track and understand
                // that the pointer we get back in the callback matches the exposed provenance.
                let x = &*callback_data;
                let y = core::ptr::from_ref(x);
                let z = y.expose_provenance();
                props.props.Context = z
                    as *const c_void as *mut c_void;

                props
            };

            let hndl = OpenTraceW(&mut log.props);
            if hndl.Value == u64::MAX {
                let err = GetLastError();
                Err(err.into())
            } else {
                // Ordering::Relaxed, because we haven't spawned any threads yet
                shared_data.session_handle.store(hndl.Value, Ordering::Relaxed);
                Ok(ProcessTraceHandle { callback_data, shared_data })
            }
        }
    }

    // This function is a member of ProcessTraceHandle because it needs to know type `C`
    unsafe extern "system" fn event_record_callback(event_record: *mut EVENT_RECORD) {
        let ctx: *mut CallbackThreadData<C> = core::ptr::with_exposed_provenance_mut(unsafe { (*event_record).UserContext as usize });
        if !ctx.is_null() {
            // It's not safe to let a panic cross back into C code.
            // Use AssertUnwindSafe because we will always abort in the event of a panic.
            let err = catch_unwind(AssertUnwindSafe(|| {
                let ctx = unsafe { &*ctx };
                let result = unsafe { ctx.inner_callback(event_record) };
                if result.is_err() {
                    ctx.shared_data.stop_callbacks.store(true, Ordering::Release);
                    let _ = unsafe { CloseTrace(PROCESSTRACE_HANDLE{Value: ctx.shared_data.session_handle.load(Ordering::Acquire)}) };
                }
            }));
            if err.is_err() {
                std::process::abort();
            }
        }
    }

    /// Starts a thread that runs ProcessTrace, calling the consumer for each event on that thread.
    ///
    /// The returned [`ProcessTraceThread`] stops the trace when dropped.
    pub fn process_trace(self) -> Result<ProcessTraceThread, windows::core::Error> {
        let thread = spawn_process_trace_thread(self.callback_data);

        Ok(ProcessTraceThread {
            thread: Some(thread),
            inner: self.shared_data.clone(),
        })
    }
}

fn spawn_process_trace_thread<C>(
    mut callback_data: Box<CallbackThreadData<C>>
) -> JoinHandle<Result<(), windows::core::Error>>
where
    C: EventConsumer + 'static,
{
    let shared_data = unsafe { ManuallyDrop::take(&mut callback_data.shared_data) };
    // callback_data.shared_data is now moved and must not be used beyond this point
    let data = Box::into_pin(callback_data);
    let handles = [PROCESSTRACE_HANDLE{Value: shared_data.session_handle.load(Ordering::Acquire)}];
    unsafe {
        std::thread::spawn(move || {
            let err = ProcessTrace(&handles, None, None);

            data.process_trace_complete(err.into());

            if err.is_err() {
                Err(windows::core::Error::from(err))
            } else {
                Ok(())
            }
        })
    }
}

pub struct ProcessTraceThread
{
    thread: Option<JoinHandle<Result<(), windows::core::Error>>>,
    inner: Arc<SharedThreadData>,
}

impl ProcessTraceThread
{
    /// Closes the trace and blocks until the ProcessTrace thread exits.
    ///
    /// The thread cannot exit while the consumer's callback is blocked, so release or drop any
    /// event waiter first. ProcessTrace's own result is discarded; use
    /// [`stop_and_get_thread`](Self::stop_and_get_thread) to observe it.
    ///
    /// Returns `E_UNEXPECTED` if the trace was already stopped or the thread panicked.
    pub fn stop_and_wait(&mut self) -> Result<(), windows::core::Error> {
        match self.stop_and_get_thread() {
            Ok(t) => {
                match t.join() {
                    Ok(_) => Ok(()),
                    Err(_) => Err(crate::error::E_UNEXPECTED.into())
                }
            },
            Err(e) => Err(e)
        }
    }

    /// Closes the trace without waiting and returns the ProcessTrace thread, whose result is
    /// ProcessTrace's result. For real-time sessions, events already buffered are still
    /// delivered before the thread exits.
    ///
    /// Returns `E_UNEXPECTED` if the trace was already stopped.
    pub fn stop_and_get_thread(&mut self) -> Result<JoinHandle<Result<(), windows::core::Error>>, windows::core::Error> {
        if let Some(t) = self.thread.take() {
            self.inner.close_trace(false);
            Ok(t)
        }
        else {
            Err(crate::error::E_UNEXPECTED.into())
        }
    }
}

impl Drop for ProcessTraceThread {
    fn drop(&mut self) {
        if let Some(_t) = self.thread.take() {
            self.inner.close_trace(false);
        }
    }
}

pub trait EventConsumer : Send + Sync {
    /// Called on the ProcessTrace thread for each event.
    ///
    /// # Safety
    ///
    /// `evt` must point to a valid `EVENT_RECORD` that remains valid until this call returns.
    /// Implementations must not let `evt`, or anything borrowed from it, outlive the call.
    unsafe fn on_event_raw(&self, evt: *mut EVENT_RECORD) -> Result<(), windows::core::Error>;

    /// Called once on the ProcessTrace thread after ProcessTrace returns, with its result.
    /// No more events will be delivered. The default does nothing.
    fn complete(&self, _err: windows::core::Error) {}
}

#[cfg(test)]
#[allow(non_upper_case_globals)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    use crate::{FileMode, SessionBuilder};

    use super::*;

    struct CountingConsumer {
        provider_id: windows::core::GUID,
        count: Arc<AtomicUsize>,
        done: Arc<rsevents::ManualResetEvent>,
    }

    impl EventConsumer for CountingConsumer {
        unsafe fn on_event_raw(&self, evt: *mut EVENT_RECORD) -> Result<(), windows::core::Error> {
            if unsafe { (*evt).EventHeader.ProviderId } == self.provider_id {
                self.count.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        }

        fn complete(&self, _err: windows::core::Error) {
            self.done.set();
        }
    }

    #[test]
    fn from_file() -> Result<(), windows::core::Error> {
        const test_name: &str = "EtwConsumer-Rust-Tests-FromFile";
        const file_name: &str = "ff.etl";

        let provider = Box::pin(tracelogging_dynamic::Provider::new(
            "from_file_test",
            &tracelogging_dynamic::Provider::options(),
        ));
        unsafe {
            provider.as_ref().register();
        }
        let provider_id = windows::core::GUID::from_u128(provider.id().to_u128());

        {
            let h = SessionBuilder::new_file_mode(test_name, file_name, FileMode::Sequential)
                .start(true)?;
            h.enable_provider(&provider_id, 0xFF)?;

            let deadline = Instant::now() + Duration::from_secs(10);
            while !provider.enabled(tracelogging::Level::LogAlways, 0) {
                assert!(Instant::now() < deadline, "provider was never enabled");
                std::thread::sleep(Duration::from_millis(10));
            }

            let mut eb = tracelogging_dynamic::EventBuilder::new();
            eb.reset("test event", tracelogging::Level::LogAlways, 1, 0);
            for _ in 0..3 {
                assert_eq!(eb.write(&provider, None, None), 0);
            }
        } // Dropping the handle stops the session, flushing and closing the file

        let count = Arc::new(AtomicUsize::new(0));
        let done = Arc::new(rsevents::ManualResetEvent::new(rsevents::EventState::Unset));
        let trace = ProcessTraceHandle::from_file(
            file_name,
            CountingConsumer {
                provider_id,
                count: count.clone(),
                done: done.clone(),
            },
        )?;
        let mut thread = trace.process_trace()?;

        // ProcessTrace returns on its own at the end of the file
        assert!(done.wait_for(Duration::from_secs(10)), "ProcessTrace did not finish");
        let _ = thread.stop_and_wait();

        assert_eq!(count.load(Ordering::Relaxed), 3);

        Ok(())
    }
}