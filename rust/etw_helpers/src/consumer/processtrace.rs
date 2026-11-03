// The ProcessTrace thread still uses `std`, so this module links it even when the rest of the
// crate is `no_std`.
extern crate std;

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::ffi::c_void;
use core::mem::ManuallyDrop;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;

use windows_sys::Win32::{
    Foundation::{ERROR_SUCCESS, GetLastError},
    System::Diagnostics::Etw::*,
};

use crate::consumer::lock::Lock;

#[repr(C)]
struct EventTraceLogFile {
    props: EVENT_TRACE_LOGFILEW,
    name: [u16; 1024],
}

struct SharedThreadData {
    // When true, all callbacks from ProcessTrace will return immediately without calling into the processor
    stop_callbacks: AtomicBool,
    // The lifetime of this session handle will be at least as long as the call to ProcessTrace.
    // `CLOSED_HANDLE` once the trace has been closed.
    session_handle: AtomicU64,
    // ProcessTrace's result, once it has returned. Notified when it's set.
    result: Lock<Option<windows_sys::core::HRESULT>>,
    // Reading a log file rather than a real-time session.
    is_file: bool,
}

const CLOSED_HANDLE: u64 = 0xDEADBEEFDEADBEEF;

impl SharedThreadData {
    // Closing a real-time trace isn't synchronous: events still in the buffers are delivered
    // before ProcessTrace returns. Join the ProcessTrace thread to wait for that. Closes the
    // handle only once, however often it's called.
    fn close_trace(&self) {
        let handle = self.session_handle.swap(CLOSED_HANDLE, Ordering::AcqRel);
        if handle != CLOSED_HANDLE {
            let _ = unsafe { CloseTrace(PROCESSTRACE_HANDLE { Value: handle }) };
        }
    }

    fn cancel_trace(&self) {
        self.stop_callbacks.store(true, Ordering::Release);
        self.close_trace();
    }

    // Stops the trace when asked to. A real-time trace is only closed, so the events ETW already
    // handed over are still delivered. Closing doesn't stop ProcessTrace from reading the rest of
    // a log file, though, so a file trace is cancelled instead.
    fn stop_trace(&self) {
        if self.is_file {
            self.cancel_trace();
        } else {
            self.close_trace();
        }
    }
}

struct CallbackThreadData<C>
where
    C: EventProcessor + 'static,
{
    processor: C,
    shared_data: ManuallyDrop<Arc<SharedThreadData>>,
}

impl<C> CallbackThreadData<C>
where
    C: EventProcessor + 'static,
{
    // Per-event time on the ETW thread, including how long the processor holds the event.
    #[cfg_attr(
        feature = "hotpath",
        hotpath::measure(impl_type = "CallbackThreadData")
    )]
    unsafe fn inner_callback(&self, event_record: *mut EVENT_RECORD) -> Result<(), crate::Error> {
        let stop = self.shared_data.stop_callbacks.load(Ordering::Acquire);
        if stop {
            Ok(())
        } else {
            unsafe { <C as EventProcessor>::on_event_raw(&self.processor, event_record) }
        }
    }

    fn process_trace_complete(&self, err: crate::Error) {
        <C as EventProcessor>::complete(&self.processor, err);
    }
}

/// A wrapper around the Win32 OpenTrace function.
/// Opens an ETW session, either for an existing real-time session or
/// for a file. Does not start processing events until [`process_trace`](Self::process_trace) is called.
pub struct ProcessTraceHandle<C>
where
    C: EventProcessor + 'static,
{
    // When the thread is started, this will be moved to the thread's ownership
    callback_data: Box<CallbackThreadData<C>>,
    // Shared between the callback thread and the owning handle
    shared_data: Arc<SharedThreadData>,
}

impl<C> ProcessTraceHandle<C>
where
    C: EventProcessor + 'static,
{
    /// Opens the running real-time session `session_name`, delivering its events to `processor`.
    ///
    /// # Panics
    ///
    /// Panics if `session_name` is empty or 1024 or more UTF-16 code units long.
    pub fn from_session(session_name: &str, processor: C) -> Result<Self, crate::Error> {
        Self::create(session_name, false, processor)
    }

    /// Opens the `.etl` log file `file_name`, delivering its events to `processor`.
    ///
    /// # Panics
    ///
    /// Panics if `file_name` is empty or 1024 or more UTF-16 code units long.
    pub fn from_file(file_name: &str, processor: C) -> Result<Self, crate::Error> {
        Self::create(file_name, true, processor)
    }

    fn create(name: &str, is_file: bool, processor: C) -> Result<Self, crate::Error> {
        unsafe {
            let shared_data = Arc::new(SharedThreadData {
                stop_callbacks: AtomicBool::new(false),
                session_handle: AtomicU64::new(0), // Will be set below
                result: Lock::new(None),
                is_file,
            });

            let callback_data = Box::new(CallbackThreadData {
                processor,
                shared_data: ManuallyDrop::new(shared_data.clone()),
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
                    props.props.LogFileName = props.name.as_mut_ptr();
                } else {
                    props.props.LoggerName = props.name.as_mut_ptr();
                }

                // Cast through usize so tools can (potentially, in the future) track and understand
                // that the pointer we get back in the callback matches the exposed provenance.
                let x = &*callback_data;
                let y = core::ptr::from_ref(x);
                let z = y.expose_provenance();
                props.props.Context = z as *const c_void as *mut c_void;

                props
            };

            let hndl = OpenTraceW(&mut log.props);
            if hndl.Value == u64::MAX {
                Err(crate::Error::from_win32(GetLastError()))
            } else {
                // Ordering::Relaxed, because we haven't spawned any threads yet
                shared_data
                    .session_handle
                    .store(hndl.Value, Ordering::Relaxed);
                Ok(ProcessTraceHandle {
                    callback_data,
                    shared_data,
                })
            }
        }
    }

    // This function is a member of ProcessTraceHandle because it needs to know type `C`
    unsafe extern "system" fn event_record_callback(event_record: *mut EVENT_RECORD) {
        let ctx: *mut CallbackThreadData<C> =
            core::ptr::with_exposed_provenance_mut(unsafe { (*event_record).UserContext as usize });
        // A panic can't unwind into ETW: unwinding out of an `extern "system"` function aborts
        // the process.
        if !ctx.is_null() {
            let ctx = unsafe { &*ctx };
            let result = unsafe { ctx.inner_callback(event_record) };
            if result.is_err() {
                ctx.shared_data.cancel_trace();
            }
        }
    }

    /// Starts a thread that runs ProcessTrace, calling the processor for each event on that thread.
    ///
    /// The returned [`ProcessTraceThread`] stops the trace when dropped.
    pub fn process_trace(self) -> Result<ProcessTraceThread, crate::Error> {
        let thread = spawn_process_trace_thread(self.callback_data);

        Ok(ProcessTraceThread {
            thread: Some(thread),
            inner: self.shared_data.clone(),
        })
    }
}

fn spawn_process_trace_thread<C>(
    mut callback_data: Box<CallbackThreadData<C>>,
) -> JoinHandle<Result<(), crate::Error>>
where
    C: EventProcessor + 'static,
{
    let shared_data = unsafe { ManuallyDrop::take(&mut callback_data.shared_data) };
    // callback_data.shared_data is now moved and must not be used beyond this point
    let data = Box::into_pin(callback_data);
    let handles = [PROCESSTRACE_HANDLE {
        Value: shared_data.session_handle.load(Ordering::Acquire),
    }];
    unsafe {
        std::thread::spawn(move || {
            let err = ProcessTrace(
                handles.as_ptr(),
                handles.len() as u32,
                core::ptr::null(),
                core::ptr::null(),
            );

            // Recorded before the processor is dropped, so it's available once a consumer sees
            // its channel disconnect.
            *shared_data.result.lock() = Some(crate::Error::from_win32(err).code());
            shared_data.result.notify_all();
            data.process_trace_complete(crate::Error::from_win32(err));

            if err != ERROR_SUCCESS {
                Err(crate::Error::from_win32(err))
            } else {
                Ok(())
            }
        })
    }
}

pub struct ProcessTraceThread {
    thread: Option<JoinHandle<Result<(), crate::Error>>>,
    inner: Arc<SharedThreadData>,
}

impl ProcessTraceThread {
    // Stops the trace without waiting for the thread; see `SharedThreadData::stop_trace`.
    pub(crate) fn stop(&self) {
        self.inner.stop_trace();
    }

    // Closes the trace, and stops calling the processor for events still being delivered.
    pub(crate) fn cancel(&self) {
        self.inner.cancel_trace();
    }

    // ProcessTrace's result, once it has returned.
    pub(crate) fn result(&self) -> Option<windows_sys::core::HRESULT> {
        *self.inner.result.lock()
    }

    // Waits up to `timeout` for ProcessTrace to return, and returns its result.
    #[cfg(test)]
    pub(crate) fn wait_result(
        &self,
        timeout: core::time::Duration,
    ) -> Option<windows_sys::core::HRESULT> {
        let mut result = self.inner.result.lock();
        result.wait_while(crate::consumer::lock::Deadline::after(Some(timeout)), |r| {
            r.is_none()
        });
        *result
    }

    /// Stops the trace and blocks until the ProcessTrace thread exits.
    ///
    /// A log file's remaining events aren't delivered. For a real-time session, the events ETW has
    /// already handed to the consumer are still delivered to the processor before the thread
    /// exits, so a processor must not block
    /// indefinitely on them (for example, it should fail once whatever it hands events to is gone).
    /// The thread cannot exit while the processor's callback is blocked, so release or drop any
    /// event waiter first. ProcessTrace's own result is discarded; use
    /// [`stop_and_get_thread`](Self::stop_and_get_thread) to observe it.
    ///
    /// Returns `E_UNEXPECTED` if the trace was already stopped or the thread panicked.
    pub fn stop_and_wait(&mut self) -> Result<(), crate::Error> {
        match self.stop_and_get_thread() {
            Ok(t) => match t.join() {
                Ok(_) => Ok(()),
                Err(_) => Err(crate::Error::from_hresult(crate::error::E_UNEXPECTED)),
            },
            Err(e) => Err(e),
        }
    }

    /// Stops the trace without waiting and returns the ProcessTrace thread, whose result is
    /// ProcessTrace's result. A log file's remaining events aren't delivered. For real-time
    /// sessions, the events ETW has already handed to the consumer are still delivered to the
    /// processor before the thread exits.
    ///
    /// Returns `E_UNEXPECTED` if the trace was already stopped.
    pub fn stop_and_get_thread(
        &mut self,
    ) -> Result<JoinHandle<Result<(), crate::Error>>, crate::Error> {
        if let Some(t) = self.thread.take() {
            self.inner.stop_trace();
            Ok(t)
        } else {
            Err(crate::Error::from_hresult(crate::error::E_UNEXPECTED))
        }
    }
}

/// Dropping it closes the trace without waiting, and stops delivering events to the processor.
impl Drop for ProcessTraceThread {
    fn drop(&mut self) {
        if let Some(_t) = self.thread.take() {
            self.inner.cancel_trace();
        }
    }
}

/// Handles the events that ProcessTrace delivers, on the ProcessTrace thread.
///
/// Pass one to [`ProcessTraceHandle::from_session`] or [`ProcessTraceHandle::from_file`] to
/// handle raw `EVENT_RECORD`s directly on the ProcessTrace thread. For the event consumers in
/// this crate, use [`EventConsumer::from_file`](crate::consumer::EventConsumer::from_file) or
/// [`from_session`](crate::consumer::EventConsumer::from_session) instead, which connect them for you.
pub trait EventProcessor: Send + Sync {
    /// Called on the ProcessTrace thread for each event.
    ///
    /// # Safety
    ///
    /// `evt` must point to a valid `EVENT_RECORD` that remains valid until this call returns.
    /// Implementations must not let `evt`, or anything borrowed from it, outlive the call.
    unsafe fn on_event_raw(&self, evt: *mut EVENT_RECORD) -> Result<(), crate::Error>;

    /// Called once on the ProcessTrace thread after ProcessTrace returns, with its result.
    /// No more events will be delivered. The default does nothing.
    fn complete(&self, _err: crate::Error) {}
}

#[cfg(test)]
#[allow(non_upper_case_globals)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

    use crate::controller::{FileMode, SessionBuilder};

    use super::*;

    struct CountingConsumer {
        provider_id: windows_sys::core::GUID,
        count: Arc<AtomicUsize>,
        done: std::sync::mpsc::SyncSender<()>,
    }

    impl EventProcessor for CountingConsumer {
        unsafe fn on_event_raw(&self, evt: *mut EVENT_RECORD) -> Result<(), crate::Error> {
            if crate::test_util::guid_eq(
                unsafe { &(*evt).EventHeader.ProviderId },
                &self.provider_id,
            ) {
                self.count.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        }

        fn complete(&self, _err: crate::Error) {
            let _ = self.done.try_send(());
        }
    }

    #[test]
    fn from_file() -> Result<(), crate::Error> {
        const test_name: &str = "EtwConsumer-Rust-Tests-FromFile";
        let file_name = &crate::test_util::etl_path("ff.etl");

        let provider = Box::pin(tracelogging_dynamic::Provider::new(
            "from_file_test",
            &tracelogging_dynamic::Provider::options(),
        ));
        unsafe {
            provider.as_ref().register();
        }
        let provider_id = windows_sys::core::GUID::from_u128(provider.id().to_u128());

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
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
        let trace = ProcessTraceHandle::from_file(
            file_name,
            CountingConsumer {
                provider_id,
                count: count.clone(),
                done: done_tx,
            },
        )?;
        let mut thread = trace.process_trace()?;

        // ProcessTrace returns on its own at the end of the file
        assert!(
            done_rx.recv_timeout(Duration::from_secs(10)).is_ok(),
            "ProcessTrace did not finish"
        );
        let _ = thread.stop_and_wait();

        assert_eq!(count.load(Ordering::Relaxed), 3);

        Ok(())
    }
}
