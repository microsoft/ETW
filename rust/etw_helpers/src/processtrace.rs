use std::{
    ffi::c_void,
    mem::ManuallyDrop,
    ops::Deref,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::JoinHandle,
};

use rsevents::Awaitable;
use windows::{
    core::PCSTR,
    Win32::{Foundation::GetLastError, System::Diagnostics::Etw::*},
};

use crate::sessions::*;

struct ProcessTraceHandleWrapper(PROCESSTRACE_HANDLE);

impl Deref for ProcessTraceHandleWrapper {
    type Target = PROCESSTRACE_HANDLE;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

unsafe impl Send for ProcessTraceHandleWrapper {}
unsafe impl Sync for ProcessTraceHandleWrapper {}

struct InnerProcessTraceHandle<C>
where
    C: EventConsumer + Send + Sync + 'static,
{
    consumer: C,
    hndl: Mutex<Option<ProcessTraceHandleWrapper>>,
    stop_callbacks: AtomicBool,
    stopped: rsevents::ManualResetEvent,
}

impl<C> InnerProcessTraceHandle<C>
where
    C: EventConsumer + Send + Sync + 'static,
{
    unsafe fn inner_callback(
        &self,
        event_record: *mut EVENT_RECORD,
    ) -> Result<(), windows::core::Error> {
        let stop = self.stop_callbacks.load(Ordering::Acquire);
        if stop {
            Ok(())
        } else {
            <C as EventConsumer>::on_event_raw(&self.consumer, event_record)
        }
    }

    fn process_trace_complete(&self, err: windows::core::Error) {
        let res = <C as EventConsumer>::complete(&self.consumer, err);
        self.stopped.set();
        res
    }

    fn close_trace(&self, block_on_thread_exit: bool) {
        unsafe {
            let mut guard = self.hndl.lock().unwrap();
            if guard.is_some() {
                self.stop_callbacks.store(true, Ordering::Release);
                CloseTrace(*guard.take().unwrap());
            }
            // Close trace is not synchronous for real-time sessions.
            // All events still in the buffers will be delivered before
            // ProcessTrace returns.
            if block_on_thread_exit {
                let _ = self.stopped.try_wait();
            }
        }
    }
}

impl<C> Drop for InnerProcessTraceHandle<C>
where
    C: EventConsumer + Send + Sync + 'static,
{
    fn drop(&mut self) {
        // TODO: We need to make sure ProcessTrace is actually finished,
        // or else we'll crash if another event callback comes in.
        self.close_trace(true)
    }
}

pub struct ProcessTraceHandle<C>
where
    C: EventConsumer + Send + Sync + 'static,
{
    inner: Arc<InnerProcessTraceHandle<C>>,
    ctx: ManuallyDrop<Arc<InnerProcessTraceHandle<C>>>,
}

impl<C> ProcessTraceHandle<C>
where
    C: EventConsumer + Send + Sync + 'static,
{
    #[allow(dead_code)]
    pub fn from_session(
        session_name: PCSTR,
        consumer: C,
    ) -> Result<ProcessTraceHandle<C>, windows::core::Error> {
        unsafe {
            let mut log = EventTraceLogFile::from_session(
                session_name,
                Some(ProcessTraceHandle::<C>::event_record_callback),
            );
            let inner = Arc::new(InnerProcessTraceHandle {
                consumer: consumer,
                hndl: Mutex::new(None),
                stop_callbacks: AtomicBool::new(false),
                stopped: rsevents::ManualResetEvent::new(rsevents::EventState::Unset),
            });

            let clone = ManuallyDrop::new(inner.clone());
            log = log.set_user_context(
                &*clone.as_ref() as *const InnerProcessTraceHandle<C> as *const c_void
            );

            let hndl = OpenTraceA(&mut log.props);
            if hndl.0 == u64::MAX {
                let err = GetLastError();
                Err(err.into())
            } else {
                *inner.hndl.lock().unwrap() = Some(ProcessTraceHandleWrapper(hndl));
                Ok(ProcessTraceHandle { inner, ctx: clone })
            }
        }
    }

    // pub fn from_file(file_name: &str) -> Result<ProcessTraceHandle, windows::core::Error> {
    //     unsafe {
    //         let name = PCSTR::from_raw(file_name.as_ptr());
    //         let mut log = EventTraceLogFile::from_file(name, Some(ProcessTraceHandle::event_record_callback));

    //         let hndl = OpenTraceA(&mut log.props);
    //         if hndl.0 == 0 {
    //             let err = GetLastError();
    //             Err(err.into())
    //         } else {
    //             Ok(ProcessTraceHandle{hndl: Box::pin(hndl)})
    //         }
    //     }
    // }

    #[allow(dead_code)]
    unsafe extern "system" fn event_record_callback(event_record: *mut EVENT_RECORD) {
        let ctx = (*event_record).UserContext as *mut InnerProcessTraceHandle<C>;
        if ctx != core::ptr::null_mut() {
            // It's not safe to let a panic cross back into C code.
            // Use AssertUnwindSafe because we will always abort in the event of a panic.
            let err = catch_unwind(AssertUnwindSafe(|| {
                let result = (*ctx).inner_callback(event_record);
                if result.is_err() {
                    (*ctx).close_trace(false);
                }
            }));
            if err.is_err() {
                std::process::abort();
            }
        }
    }

    #[allow(dead_code)]
    pub fn process_trace(self) -> Result<ProcessTraceThread<C>, windows::core::Error> {
        let thread = spawn_process_trace_thread(self.inner.clone(), self.ctx);

        Ok(ProcessTraceThread {
            thread: Some(thread),
            inner: self.inner,
        })
    }
}

fn spawn_process_trace_thread<C>(
    inner: Arc<InnerProcessTraceHandle<C>>,
    mut ctx: ManuallyDrop<Arc<InnerProcessTraceHandle<C>>>,
) -> JoinHandle<Result<(), windows::core::Error>>
where
    C: EventConsumer + Send + Sync + 'static,
{
    let handles = [***inner.hndl.lock().unwrap().as_ref().as_ref().unwrap()];
    unsafe {
        std::thread::spawn(move || {
            let err = ProcessTrace(&handles, None, None);
            inner.process_trace_complete(err.into());
            ManuallyDrop::drop(&mut ctx);
            if err.is_err() {
                Err(windows::core::Error::from(err))
            } else {
                Ok(())
            }
        })
    }
}

pub struct ProcessTraceThread<C>
where
    C: EventConsumer + Send + Sync + 'static,
{
    thread: Option<JoinHandle<Result<(), windows::core::Error>>>,
    inner: Arc<InnerProcessTraceHandle<C>>,
}

impl<C> ProcessTraceThread<C>
where
    C: EventConsumer + Send + Sync,
{
    #[allow(dead_code)]
    pub fn stop_and_wait(&mut self) -> Result<(), windows::core::Error> {
        let thread = self.stop_and_get_thread();
        thread.join().unwrap()
    }

    #[allow(dead_code)]
    pub fn stop_and_get_thread(&mut self) -> JoinHandle<Result<(), windows::core::Error>> {
        self.inner.close_trace(false);
        self.thread.take().unwrap()
    }
}

pub trait EventConsumer {
    unsafe fn on_event_raw(&self, evt: *mut EVENT_RECORD) -> Result<(), windows::core::Error>;

    fn complete(&self, _err: windows::core::Error) {}
}
