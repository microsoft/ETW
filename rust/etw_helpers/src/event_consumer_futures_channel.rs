use std::{
    cell::RefCell,
    sync::{
        atomic::{AtomicPtr, Ordering},
        Mutex,
    },
};

use futures::{SinkExt, StreamExt};
use windows::Win32::System::Diagnostics::Etw::EVENT_RECORD;

use crate::event_record::EventRecord;
use crate::processtrace::*;

#[allow(overflowing_literals)]
static E_CANCELLED: windows::core::HRESULT = windows::core::HRESULT(0x800704c7);

struct FuturesChannelConsumer {
    tx: Mutex<futures::channel::mpsc::Sender<AtomicPtr<EVENT_RECORD>>>,
}

impl EventConsumer for FuturesChannelConsumer {
    unsafe fn on_event_raw(&self, evt: *mut EVENT_RECORD) -> Result<(), windows::core::Error> {
        futures::executor::block_on(async {
            let send_result = self.tx.lock().unwrap().send(AtomicPtr::new(evt)).await;
            if send_result.is_err() {
                if send_result.expect_err("err").is_disconnected() {
                    return Err(E_CANCELLED.into());
                } else {
                    panic!();
                }
            }
            Ok(())
        })
    }

    fn complete(&self, _err: windows::core::Error) {
        futures::executor::block_on(async {
            let _ = self.tx.lock().unwrap().close();
        });
    }
}

#[allow(dead_code)]
pub struct EtwEventAsyncWaiter {
    rx: RefCell<futures::channel::mpsc::Receiver<AtomicPtr<EVENT_RECORD>>>,
    consumer: Option<FuturesChannelConsumer>,
}

impl EtwEventAsyncWaiter {
    #[allow(dead_code)]
    pub fn new() -> EtwEventAsyncWaiter {
        let (tx, rx) = futures::channel::mpsc::channel::<AtomicPtr<EVENT_RECORD>>(0);
        EtwEventAsyncWaiter {
            rx: RefCell::new(rx),
            consumer: Some(FuturesChannelConsumer { tx: Mutex::new(tx) }),
        }
    }

    #[allow(dead_code)]
    pub fn get_consumer(&mut self) -> impl EventConsumer {
        self.consumer.take().unwrap()
    }

    #[allow(dead_code)]
    pub fn expect_event<F>(&self, f: F) -> Result<(), windows::core::Error>
    where
        F: Fn(EventRecord) -> bool + Send + Sync,
    {
        futures::executor::block_on(self.expect_event_async(f))
    }

    #[allow(dead_code)]
    pub async fn expect_event_async<F>(&self, f: F) -> Result<(), windows::core::Error>
    where
        F: Fn(EventRecord) -> bool + Send + Sync,
    {
        let next_event = self.rx.borrow_mut().next().await;
        let next_event_record = EventRecord::new(next_event.unwrap().load(Ordering::Acquire));

        let should_continue = f(next_event_record);
        if !should_continue {
            self.rx.borrow_mut().close();
        }

        return Ok::<(), windows::core::Error>(());
    }
}

#[cfg(test)]
#[allow(non_upper_case_globals)]
mod tests {
    use crate::EtwSession;
    use futures::TryFutureExt;
    use rsevents::Awaitable;
    use std::ffi::c_void;
    use tracelogging::{Guid, Level};

    use super::*;

    fn provider_enabled_callback(
        _source_id: &Guid,
        _event_control_code: u32,
        _level: Level,
        _match_any_keyword: u64,
        _match_all_keyword: u64,
        _filter_data: usize,
        callback_context: usize,
    ) {
        unsafe {
            let ctx = &*(callback_context as *const c_void as *const rsevents::ManualResetEvent);
            ctx.set();
        }
    }

    static consume_event_enabled_event: rsevents::ManualResetEvent =
        rsevents::ManualResetEvent::new(rsevents::EventState::Unset);

    #[test]
    fn consume_event() -> Result<(), windows::core::Error> {
        const sz_test_name: windows::core::PCSTR =
            windows::s!("EtwConsumer-Rust-Tests-ConsumeEvent-Futures");

        let mut options = tracelogging_dynamic::Provider::options();
        let options = options.callback(
            provider_enabled_callback,
            &consume_event_enabled_event as *const rsevents::ManualResetEvent as usize,
        );

        let provider = Box::pin(tracelogging_dynamic::Provider::new(
            "futures_consume_event_test",
            &options,
        ));
        unsafe {
            provider.as_ref().register();
        }
        let provider_guid = windows::core::GUID::from_u128(provider.id().to_u128());
        let mut eb = tracelogging_dynamic::EventBuilder::new();

        let h = EtwSession::get_or_start_etw_session(sz_test_name, true)?;
        h.enable_provider(&provider_guid)?;

        let mut consumer = EtwEventAsyncWaiter::new();
        let event_consumer = consumer.get_consumer();

        let trace = ProcessTraceHandle::from_session(sz_test_name, event_consumer)?;

        consume_event_enabled_event.wait();

        assert!(provider.enabled(tracelogging::Level::LogAlways, 3));

        eb.reset("test event", tracelogging::Level::LogAlways, 1, 0);
        assert_eq!(eb.write(&provider, None, None), 0);

        let fut = consumer.expect_event_async(|evt| {
            let event_header = evt.get_event_header();
            assert_eq!(event_header.ProviderId, provider_guid);
            assert_eq!(event_header.EventDescriptor.Keyword, 1u64);

            true
        });

        let mut thread = trace.process_trace()?;

        let fut2 = consumer.expect_event_async(|evt| {
            let event_header = evt.get_event_header();
            assert_eq!(event_header.ProviderId, provider_guid);
            assert_eq!(event_header.EventDescriptor.Keyword, 2u64);

            false
        });

        let fut3 = fut.and_then(|_| fut2);

        eb.reset("test event", tracelogging::Level::LogAlways, 2, 0);
        assert_eq!(eb.write(&provider, None, None), 0);
        // Log a third event to make sure we can quit properly
        assert_eq!(eb.write(&provider, None, None), 0);

        let result = futures::executor::block_on(fut3);

        let _ = thread.stop_and_wait(); // We don't care about what ProcessTrace returned

        result
    }
}
