use std::{
    sync::{atomic, Condvar, Mutex},
    time::Duration,
};

use windows::Win32::System::Diagnostics::Etw::EVENT_RECORD;

use crate::processtrace::*;

pub struct EtwEventAsyncWaiterWithTimeout<'a> {
    ready_for_next_event: atomic::AtomicBool,
    next_event_consumer_set: Condvar,
    waiter: Mutex<Option<Box<dyn Fn(&EVENT_RECORD) -> bool + Send + Sync + 'a>>>,

    event_callback_completed: Condvar,
    waiter2: Mutex<bool>,
}

impl<'a> EventConsumer for EtwEventAsyncWaiterWithTimeout<'a> {
    fn on_event(&self, evt: &EVENT_RECORD) -> Result<(), windows::core::Error> {
        let mut guard;
        loop {
            let event_consumer_ready = self.ready_for_next_event.compare_exchange(
                true,
                false,
                atomic::Ordering::Acquire,
                atomic::Ordering::Relaxed,
            );
            if event_consumer_ready.is_err() {
                guard = self.waiter.lock().unwrap();
                if guard.is_none() {
                    let result = self
                        .next_event_consumer_set
                        .wait_timeout(guard, Duration::new(10, 0))
                        .unwrap();
                    if result.1.timed_out() {
                        return Err(windows::core::HRESULT(-2147023436i32).into());
                    // HRESULT_FROM_WIN32(ERROR_TIMEOUT)
                    } else {
                        guard = result.0;
                        break;
                    }
                }
            } else {
                guard = self.waiter.lock().unwrap();
                break;
            }
        }

        if let Some(f) = guard.take() {
            let should_continue = f(evt);
            *self.waiter2.lock().unwrap() = true;
            self.event_callback_completed.notify_one();
            if !should_continue {
                return Err(windows::core::HRESULT(-2147023673).into()); // HRESULT_FROM_WIN32(ERROR_CANCELLED)
            } else {
                return Ok(());
            }
        }

        Ok(())
    }
}

impl<'a> EtwEventAsyncWaiterWithTimeout<'a> {
    #[allow(dead_code)]
    pub fn new() -> EtwEventAsyncWaiterWithTimeout<'a> {
        EtwEventAsyncWaiterWithTimeout {
            ready_for_next_event: atomic::AtomicBool::new(false),
            next_event_consumer_set: Condvar::new(),
            waiter: Mutex::new(None),
            event_callback_completed: Condvar::new(),
            waiter2: Mutex::new(false),
        }
    }

    #[allow(dead_code)]
    pub async fn expect_event<F>(&self, f: F) -> Result<(), windows::core::Error>
    where
        F: Fn(&EVENT_RECORD) -> bool + Send + Sync + 'a,
    {
        {
            let mut guard = self.waiter.lock().unwrap();
            *guard = Some(Box::new(f));
        }

        let ready = self.ready_for_next_event.compare_exchange(
            false,
            true,
            atomic::Ordering::Acquire,
            atomic::Ordering::Relaxed,
        );
        if ready.is_err() {
            panic!("Cannot await more than one call to expect_event at once");
        } else {
        }
        self.next_event_consumer_set.notify_one();

        {
            let guard = self.waiter2.lock().unwrap();
            // If *guard == true then the condition variable was notified before we were ready to wait on it,
            // so skip the wait so we don't deadlock.
            if *guard == false {
                let mut result = self
                    .event_callback_completed
                    .wait_timeout(guard, Duration::new(10, 0))
                    .unwrap();
                if result.1.timed_out() {
                    return Err::<(), windows::core::Error>(
                        windows::core::HRESULT(-2147023436i32).into(),
                    ); // HRESULT_FROM_WIN32(ERROR_TIMEOUT)
                }
                *result.0 = false;
            }
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
            windows::s!("EtwConsumer-Rust-Tests-ConsumeEvent-Raw");

        let mut options = tracelogging_dynamic::Provider::options();
        let options = options.callback(
            provider_enabled_callback,
            &consume_event_enabled_event as *const rsevents::ManualResetEvent as usize,
        );

        let provider = Box::pin(tracelogging_dynamic::Provider::new(
            "consume_event_test",
            &options,
        ));
        unsafe {
            provider.as_ref().register();
        }
        let provider_guid = windows::core::GUID::from_u128(provider.id().to_u128());
        let mut eb = tracelogging_dynamic::EventBuilder::new();

        let h = EtwSession::get_or_start_etw_session(sz_test_name, true)?;
        h.enable_provider(&provider_guid)?;

        let consumer = EtwEventAsyncWaiterWithTimeout::new();

        let trace = ProcessTraceHandle::from_session(sz_test_name, consumer)?;

        consume_event_enabled_event.wait();

        eb.reset("test event", tracelogging::Level::LogAlways, 1, 0);
        eb.write(&provider, None, None);

        let fut = consumer.expect_event(|evt: &EVENT_RECORD| {
            if evt.EventHeader.ProviderId == provider_guid {
                println!(
                    "Found event from provider! {}",
                    evt.EventHeader.EventDescriptor.Keyword
                );
                true
            } else {
                false
            }
        });

        let mut thread = trace.process_trace()?;

        let fut2 = consumer.expect_event(|_evt| {
            println!("yay second event");
            false
        });

        eb.reset("test event", tracelogging::Level::LogAlways, 2, 0);
        eb.write(&provider, None, None);
        // Log a third event to make sure we can quit properly
        eb.write(&provider, None, None);

        let fut3 = fut.and_then(|_| fut2);

        let result = futures::executor::block_on(fut3);

        let _ = thread.stop_and_wait(); // We don't care about what ProcessTrace returned

        result
    }
}
