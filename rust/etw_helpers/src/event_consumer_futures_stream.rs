use std::{
    marker::PhantomData,
    pin::Pin,
    sync::{
        atomic::{AtomicPtr, Ordering},
        Arc, Mutex, Condvar,
    },
};

use futures::{task, Stream};
use windows::Win32::System::Diagnostics::Etw::EVENT_RECORD;

use crate::event_record::EventRecord;
use crate::processtrace::*;
use crate::error::*;

struct EtwEventStreamInner<'a> {
    waker: Mutex<Option<task::Waker>>,
    next_event: Mutex<AtomicPtr<EVENT_RECORD>>,
    consumer_complete: Arc<Condvar>,
    _x: PhantomData<&'a bool>,
}

impl<'a> EventConsumer for EtwEventStreamConsumer<'a> {
    unsafe fn on_event_raw(&self, evt: *mut EVENT_RECORD) -> Result<(), windows::core::Error> {
        let mut guard = self.inner.next_event.lock().unwrap();
        if guard.load(Ordering::Acquire) != core::ptr::null_mut() {
            let res = self.inner.consumer_complete.wait(guard);
            if res.is_ok() {
                guard = res.unwrap();
            } else {
                return Err(E_UNEXPECTED.into())
            }
        }

        guard.store(evt, Ordering::Release);
        if let Some(w) = &*self.inner.waker.lock().unwrap() {
            w.wake_by_ref();
        }

        Ok(())
    }

    fn complete(&self, _err: windows::core::Error) {
        unsafe {
            let _ = self.on_event_raw(12345 as *mut EVENT_RECORD);
        }
    }
}

impl<'a> Stream for EtwEventStreamExt<'a> {
    type Item = EventRecord;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<Option<Self::Item>> {
        *self.inner.waker.lock().unwrap() = Some(cx.waker().clone());
        let guard = self.inner.next_event.lock().unwrap();
        let ptr = guard.load(Ordering::Acquire);
        if ptr == core::ptr::null_mut() {
            task::Poll::Pending
        } else if ptr == (12345 as *mut EVENT_RECORD) {
            task::Poll::Ready(None)
        } else {
            let evt = guard.load(Ordering::Acquire);
            guard.store(core::ptr::null_mut(), Ordering::Release);
            let result = task::Poll::Ready(Some(EventRecord::new(evt).to_owned()));
            self.inner.consumer_complete.notify_all();
            result
        }
    }
}

pub struct EtwEventStreamConsumer<'a> {
    inner: Arc<EtwEventStreamInner<'a>>,
}

pub struct EtwEventStreamExt<'a> {
    inner: Arc<EtwEventStreamInner<'a>>,
}

#[allow(dead_code)]
pub struct EtwEventAsyncStream<'a> {
    inner: Arc<EtwEventStreamInner<'a>>,
}

impl<'a> EtwEventAsyncStream<'a> {
    #[allow(dead_code)]
    pub fn new() -> EtwEventAsyncStream<'a> {
        EtwEventAsyncStream {
            inner: Arc::new(EtwEventStreamInner {
                waker: Mutex::new(None),
                next_event: Mutex::new(AtomicPtr::new(core::ptr::null_mut())),
                consumer_complete: Arc::default(),
                _x: PhantomData,
            }),
        }
    }

    #[allow(dead_code)]
    pub fn get_consumer(&self) -> impl EventConsumer + 'a {
        EtwEventStreamConsumer {
            inner: self.inner.clone(),
        }
    }

    #[allow(dead_code)]
    pub fn get_stream(&self) -> impl Stream + 'a {
        EtwEventStreamExt {
            inner: self.inner.clone(),
        }
    }
}

#[cfg(test)]
#[allow(non_upper_case_globals)]
mod tests {
    use crate::EtwSession;
    use futures::StreamExt;

    use super::*;

    #[tokio::test]
    async fn stream_events() -> Result<(), windows::core::Error> {
        const sz_test_name: windows::core::PCSTR =
            windows::s!("EtwConsumer-Rust-Tests-StreamEvent");

        let provider = Box::pin(tracelogging_dynamic::Provider::new(
            "stream_event_test",
            &tracelogging_dynamic::Provider::options(),
        ));
        unsafe {
            provider.as_ref().register();
        }
        let provider_guid = windows::core::GUID::from_u128(provider.id().to_u128());
        let mut eb = tracelogging_dynamic::EventBuilder::new();

        let h = EtwSession::get_or_start_etw_session(sz_test_name, true)?;
        h.enable_provider(&provider_guid)?;

        let etw_event_stream = EtwEventAsyncStream::new();
        let event_consumer = etw_event_stream.get_consumer();
        let event_stream = etw_event_stream.get_stream();

        let trace = ProcessTraceHandle::from_session(sz_test_name, event_consumer)?;

        let mut thread = trace.process_trace()?;

        let mut events = event_stream.enumerate().fuse();

        eb.write(&provider, None, None);
        eb.write(&provider, None, None);
        eb.write(&provider, None, None);

        let mut process_trace_thread = None;
        let mut count = 0;
        while let Some(_evt) = events.next().await {
            count += 1;
            println!("Yay! {count}");

            if count == 3 {
                process_trace_thread = Some(thread.stop_and_get_thread());
            }
        }

        let _ = process_trace_thread.expect("x").join(); // We don't care about what ProcessTrace returned

        Ok(())
    }
}
