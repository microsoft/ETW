use std::{
    ffi::c_void, mem::ManuallyDrop, panic::{catch_unwind, AssertUnwindSafe}, sync::{atomic::{AtomicBool, AtomicU64, Ordering}, Arc}, thread::JoinHandle
};

use rsevents::Awaitable;
use windows::{
    core::PSTR,
    Win32::{Foundation::GetLastError, System::Diagnostics::Etw::*},
};

#[repr(C)]
struct EventTraceLogFile {
    props: EVENT_TRACE_LOGFILEA,
    name: [u8; 1024],
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
            <C as EventConsumer>::on_event_raw(&self.consumer, event_record)
        }
    }

    fn process_trace_complete(&self, err: windows::core::Error) {
        <C as EventConsumer>::complete(&self.consumer, err);
        self.shared_data.stopped.set();
    }
}

/// A wrapper around the Win32 OpenTrace function.
/// Opens an ETW session, either for an existing real-time session or
/// for a file. Does not start processing events until [process_trace] is called.
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
    pub fn from_session(session_name: &str, consumer: C) -> Result<Self, windows::core::Error> {
        Self::create(session_name, false, consumer)
    }

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
                props.props.Anonymous1.ProcessTraceMode =
                    PROCESS_TRACE_MODE_EVENT_RECORD | PROCESS_TRACE_MODE_REAL_TIME;
                props.props.Anonymous2.EventRecordCallback = Some(Self::event_record_callback);

                let len = name.len();
                if len >= 1024 {
                    panic!()
                }

                core::ptr::copy_nonoverlapping(name.as_ptr(), props.name.as_mut_ptr(), len);
                props.name[len] = b'\0';

                if is_file {
                    props.props.LogFileName = PSTR::from_raw(props.name.as_mut_ptr());
                } else {
                    props.props.LoggerName = PSTR::from_raw(props.name.as_mut_ptr());
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

            let hndl = OpenTraceA(&mut log.props);
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
        let ctx: *mut CallbackThreadData<C> = core::ptr::with_exposed_provenance_mut((*event_record).UserContext as usize);
        if !ctx.is_null() {
            // It's not safe to let a panic cross back into C code.
            // Use AssertUnwindSafe because we will always abort in the event of a panic.
            let err = catch_unwind(AssertUnwindSafe(|| {
                let result = (*ctx).inner_callback(event_record);
                if result.is_err() {
                    (*ctx).shared_data.stop_callbacks.store(true, Ordering::Release);
                    let _ = CloseTrace(PROCESSTRACE_HANDLE{Value: (*ctx).shared_data.session_handle.load(Ordering::Acquire)});
                }
            }));
            if err.is_err() {
                std::process::abort();
            }
        }
    }

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
    unsafe fn on_event_raw(&self, evt: *mut EVENT_RECORD) -> Result<(), windows::core::Error>;

    fn complete(&self, _err: windows::core::Error) {}
}
