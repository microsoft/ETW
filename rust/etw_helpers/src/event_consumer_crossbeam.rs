use std::sync::atomic::{AtomicPtr, Ordering};

use crossbeam_channel::select;
use windows::Win32::System::Diagnostics::Etw::EVENT_RECORD;

use crate::error::*;
use crate::event_record::EventRecord;
use crate::processtrace::*;

struct CrossbeamChannelConsumer {
    tx: crossbeam_channel::Sender<AtomicPtr<EVENT_RECORD>>,
    callback_rx: crossbeam_channel::Receiver<windows::core::HRESULT>,
    session_closed_tx: crossbeam_channel::Sender<windows::core::HRESULT>,
}

impl EventConsumer for CrossbeamChannelConsumer {
    unsafe fn on_event_raw(&self, evt: *mut EVENT_RECORD) -> Result<(), windows::core::Error> {
        let send_result = self.tx.send(AtomicPtr::new(evt));
        if send_result.is_err() {
            // The only error send returns is for disconnected channels
            return Err(E_UNEXPECTED.into());
        }

        let result = self.callback_rx.recv();
        if let Ok(hr) = result {
            if hr != S_OK {
                return Err(hr.into());
            }
        } else {
            return Err(E_UNEXPECTED.into());
        }

        Ok(())
    }

    fn complete(&self, err: windows::core::Error) {
        let _ = self.session_closed_tx.send(err.code());
    }
}

pub struct EtwEventWaiter {
    rx: crossbeam_channel::Receiver<AtomicPtr<EVENT_RECORD>>,
    callback_tx: crossbeam_channel::Sender<windows::core::HRESULT>,
    session_closed_rx: crossbeam_channel::Receiver<windows::core::HRESULT>,
    consumer: Option<CrossbeamChannelConsumer>,
}

impl Default for EtwEventWaiter {
    fn default() -> Self {
        let (tx, rx) = crossbeam_channel::bounded::<AtomicPtr<EVENT_RECORD>>(0);
        let (callback_tx, callback_rx) = crossbeam_channel::bounded::<windows::core::HRESULT>(1);
        let (session_closed_tx, session_closed_rx) =
            crossbeam_channel::bounded::<windows::core::HRESULT>(1);

        EtwEventWaiter {
            rx,
            callback_tx,
            session_closed_rx,
            consumer: Some(CrossbeamChannelConsumer {
                tx,
                callback_rx,
                session_closed_tx,
            }),
        }
    }
}

impl EtwEventWaiter {
    pub fn get_consumer(&mut self) -> impl EventConsumer {
        self.consumer.take().unwrap()
    }

    pub fn expect_event<F>(&self, f: F) -> Result<(), windows::core::Error>
    where
        F: FnOnce(EventRecord) -> bool + Send + Sync,
    {
        let should_continue;
        select! {
            recv(self.rx) -> ptr => {
                let next_event_record = EventRecord::new(ptr.unwrap().load(Ordering::Acquire));

                should_continue = f(next_event_record);
                let hr = if !should_continue {
                    E_CANCELLED
                } else {
                    S_OK
                };

                let send_result = self.callback_tx.send(hr);
                if send_result.is_err() {
                    return Err(E_UNEXPECTED.into());
                }
            },
            recv(self.session_closed_rx) -> hr => {
                return Err(hr.unwrap().into());
            }
        }

        Ok::<(), windows::core::Error>(())
    }
}

pub struct EtwEventIter {
    rx: crossbeam_channel::IntoIter<AtomicPtr<EVENT_RECORD>>,
    callback_tx: crossbeam_channel::Sender<windows::core::HRESULT>,
    next: bool,
}

impl Iterator for EtwEventIter {
    type Item = *const EVENT_RECORD;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next {
            let _ = self.callback_tx.send(S_OK);
            self.next = false;
        }

        let evt = self.rx.next();
        evt.map(|evt| {
            let evt = evt.load(Ordering::Acquire);
            self.next = true;

            evt as *const EVENT_RECORD
        })
    }
}

impl IntoIterator for EtwEventWaiter {
    type IntoIter = EtwEventIter;
    type Item = *const EVENT_RECORD;

    fn into_iter(self) -> Self::IntoIter {
        EtwEventIter {
            rx: self.rx.into_iter(),
            callback_tx: self.callback_tx,
            next: false,
        }
    }
}

#[cfg(test)]
#[allow(non_upper_case_globals)]
mod tests {
    use crate::{FileMode, SessionBuilder};
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
        const test_name: &str = "EtwConsumer-Rust-Tests-ConsumeEvent-Crossbeam";

        let mut options = tracelogging_dynamic::Provider::options();
        let options = options.callback(
            provider_enabled_callback,
            &consume_event_enabled_event as *const rsevents::ManualResetEvent as usize,
        );

        let provider = Box::pin(tracelogging_dynamic::Provider::new(
            "crossbeam_consume_event_test",
            &options,
        ));
        unsafe {
            provider.as_ref().register();
        }
        let provider_guid = windows::core::GUID::from_u128(provider.id().to_u128());
        let mut eb = tracelogging_dynamic::EventBuilder::new();

        let h = SessionBuilder::new_file_mode(
            "EtwConsumer-Rust-Tests-ConsumeEvent-Crossbeam",
            "cbce.etl",
            FileMode::Sequential,
        )
        .realtime_event_delivery()
        .start(true)?;

        h.enable_provider(&provider_guid)?;

        let mut consumer = EtwEventWaiter::default();
        let event_consumer = consumer.get_consumer();

        let trace = ProcessTraceHandle::from_session(test_name, event_consumer)?;

        consume_event_enabled_event.wait();

        assert!(provider.enabled(tracelogging::Level::LogAlways, 3));

        eb.reset("test event", tracelogging::Level::LogAlways, 1, 0);
        assert_eq!(eb.write(&provider, None, None), 0);

        eb.reset("test event", tracelogging::Level::LogAlways, 2, 0);
        assert_eq!(eb.write(&provider, None, None), 0);
        // Log a third event to make sure we can quit properly
        assert_eq!(eb.write(&provider, None, None), 0);

        let mut thread = trace.process_trace()?;

        assert!(consumer
            .expect_event(|evt| {
                let event_header = evt.get_event_header();
                assert_eq!(event_header.ProviderId, provider_guid);
                assert_eq!(event_header.EventDescriptor.Keyword, 1u64);

                true
            })
            .is_ok());

        assert!(consumer
            .expect_event(|evt| {
                let event_header = evt.get_event_header();
                assert_eq!(event_header.ProviderId, provider_guid);
                assert_eq!(event_header.EventDescriptor.Keyword, 2u64);

                false
            })
            .is_ok());

        let _ = thread.stop_and_wait(); // We don't care about what ProcessTrace returned

        Ok(())
    }

    static iterator_event_enabled_event: rsevents::ManualResetEvent =
        rsevents::ManualResetEvent::new(rsevents::EventState::Unset);

    #[test]
    fn iterate() -> Result<(), windows::core::Error> {
        const test_name: &str = "EtwConsumer-Rust-Tests-Iterator-Crossbeam";

        let mut options = tracelogging_dynamic::Provider::options();
        let options = options.callback(
            provider_enabled_callback,
            &iterator_event_enabled_event as *const rsevents::ManualResetEvent as usize,
        );

        let provider = Box::pin(tracelogging_dynamic::Provider::new(
            "iterate_event_test",
            &options,
        ));
        unsafe {
            provider.as_ref().register();
        }
        let provider_guid = windows::core::GUID::from_u128(provider.id().to_u128());
        let mut eb = tracelogging_dynamic::EventBuilder::new();

        let h = SessionBuilder::new_file_mode(
            "EtwConsumer-Rust-Tests-Iterator-Crossbeam",
            "cbi.etl",
            FileMode::Sequential,
        )
        .realtime_event_delivery()
        .start(true)?;

        h.enable_provider(&provider_guid)?;

        let mut consumer = EtwEventWaiter::default();
        let event_consumer = consumer.get_consumer();

        let trace = ProcessTraceHandle::from_session(test_name, event_consumer)?;

        iterator_event_enabled_event.wait();

        assert!(provider.enabled(tracelogging::Level::LogAlways, 3));

        eb.reset("test event", tracelogging::Level::LogAlways, 1, 0);
        assert_eq!(eb.write(&provider, None, None), 0);
        assert_eq!(eb.write(&provider, None, None), 0);
        assert_eq!(eb.write(&provider, None, None), 0);
        assert_eq!(eb.write(&provider, None, None), 0);
        assert_eq!(eb.write(&provider, None, None), 0);

        let mut thread = trace.process_trace()?;

        let mut count = 0;
        for _evt in consumer {
            count += 1;

            if count == 5 {
                break;
            }
        }

        let _ = thread.stop_and_wait(); // We don't care about what ProcessTrace returned

        Ok(())
    }
}
