use std::sync::atomic::{AtomicPtr, Ordering};
use std::time::Duration;

use flume::select::Selector;
use windows::Win32::System::Diagnostics::Etw::EVENT_RECORD;

use crate::error::*;
use crate::event_record::EventRecord;
use crate::processtrace::*;

struct FlumeChannelConsumer {
    tx: flume::Sender<AtomicPtr<EVENT_RECORD>>,
    callback_rx: flume::Receiver<windows::core::HRESULT>,
    session_closed_tx: flume::Sender<windows::core::HRESULT>,
}

impl EventConsumer for FlumeChannelConsumer {
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
            // The receiver was closed without telling us.
            // This can happen when the receiver is an iterator or async operation, and it never tried to get the next value.
            return Err(E_UNEXPECTED.into());
        }

        Ok(())
    }

    fn complete(&self, err: windows::core::Error) {
        let _ = self.session_closed_tx.send(err.code());
    }
}

/// Synchronously receives ETW events from a [`ProcessTraceHandle`] thread.
///
/// Events borrow the waiter mutably, and the ETW callback thread stays blocked until
/// that borrow ends. Holding an event across the next call to `next()` is a compile error:
///
/// ```compile_fail
/// # fn f(waiter: &mut etw_helpers::event_consumer_flume::EtwEventWaiter) {
/// let first = waiter.next();
/// let second = waiter.next(); // error[E0499]: cannot borrow `*waiter` as mutable more than once
/// drop(first);
/// # }
/// ```
///
/// The ETW thread cannot exit while an event is lent, so drain the waiter or drop it
/// before joining the [`ProcessTraceThread`].
pub struct EtwEventWaiter {
    rx: flume::Receiver<AtomicPtr<EVENT_RECORD>>,
    callback_tx: flume::Sender<windows::core::HRESULT>,
    session_closed_rx: flume::Receiver<windows::core::HRESULT>,
    consumer: Option<FlumeChannelConsumer>,
    // True while the ETW callback is blocked waiting for us to release an event returned by `next()`
    lent: bool,
}

impl Default for EtwEventWaiter {
    fn default() -> Self {
        let (tx, rx) = flume::bounded::<AtomicPtr<EVENT_RECORD>>(0);
        let (callback_tx, callback_rx) = flume::bounded::<windows::core::HRESULT>(1);
        let (session_closed_tx, session_closed_rx) = flume::bounded::<windows::core::HRESULT>(1);

        EtwEventWaiter {
            rx,
            callback_tx,
            session_closed_rx,
            consumer: Some(FlumeChannelConsumer {
                tx,
                callback_rx,
                session_closed_tx,
            }),
            lent: false,
        }
    }
}

impl EtwEventWaiter {
    /// Returns the [`EventConsumer`] to pass to [`ProcessTraceHandle`].
    ///
    /// # Panics
    ///
    /// Panics if called more than once.
    pub fn get_consumer(&mut self) -> impl EventConsumer + use<> {
        self.consumer.take().unwrap()
    }

    /// Blocks until the next event arrives. Returns `None` once ProcessTrace has completed.
    ///
    /// The returned event borrows `self`, so it must be dropped before calling `next()` again.
    // Can't be `Iterator::next`: iterator items can't borrow from the iterator.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<EventRecord<'_>> {
        // Requiring `&mut self` guarantees any previously returned event is no longer referenced.
        self.release_current().ok()?;

        let ptr = Selector::new()
            .recv(&self.rx, |ptr| ptr.ok())
            .recv(&self.session_closed_rx, |_| None)
            .wait()?;
        self.lent = true;

        // SAFETY: the ETW callback that owns this event is blocked until we send on
        // `callback_tx`, which only happens in `release_current` (requires `&mut self`,
        // ending this borrow) or when `self` is dropped.
        Some(EventRecord::from_ref(unsafe { &*ptr.load(Ordering::Acquire) }))
    }

    fn release_current(&mut self) -> Result<(), windows::core::Error> {
        if self.lent {
            self.lent = false;
            if self.callback_tx.send(S_OK).is_err() {
                return Err(E_UNEXPECTED.into());
            }
        }
        Ok(())
    }

    /// Waits for the next event and passes it to `f`. Returning `false` from `f` stops the trace.
    ///
    /// Returns an error once ProcessTrace has completed.
    pub fn expect_event<F>(&mut self, f: F) -> Result<(), windows::core::Error>
    where
        F: FnMut(EventRecord) -> bool + Send + Sync,
    {
        self.expect_event_inner(None, f)
    }

    /// Like [`expect_event`](Self::expect_event), but gives up if no event arrives within `timeout`.
    ///
    /// On timeout, returns an error with code `HRESULT_FROM_WIN32(ERROR_TIMEOUT)`
    /// (`windows::Win32::Foundation::ERROR_TIMEOUT.to_hresult()`), and `f` is not called.
    pub fn expect_event_timeout<F>(&mut self, timeout: Duration, f: F) -> Result<(), windows::core::Error>
    where
        F: FnMut(EventRecord) -> bool + Send + Sync,
    {
        self.expect_event_inner(Some(timeout), f)
    }

    fn expect_event_inner<F>(&mut self, timeout: Option<Duration>, mut f: F) -> Result<(), windows::core::Error>
    where
        F: FnMut(EventRecord) -> bool + Send + Sync,
    {
        self.release_current()?;

        let selector = Selector::new()
            .recv(&self.rx, |ptr| {
                let Ok(ptr) = ptr else {
                    return Err(self.closed_error());
                };
                let evt_ptr: *const EVENT_RECORD = ptr.load(Ordering::Acquire);
                let evt: &EVENT_RECORD = unsafe { &*evt_ptr };
                let next_event_record: EventRecord = EventRecord::from_ref(evt);

                let should_continue = f(next_event_record);
                let hr = if !should_continue { E_CANCELLED } else { S_OK };

                let send_result = self.callback_tx.send(hr);
                if send_result.is_err() {
                    Err(E_UNEXPECTED.into())
                } else {
                    Ok(())
                }
            })
            .recv(&self.session_closed_rx, |hr| {
                Err(hr.unwrap_or(E_UNEXPECTED).into())
            });

        match timeout {
            None => selector.wait(),
            Some(timeout) => selector
                .wait_timeout(timeout)
                .unwrap_or_else(|_| Err(E_TIMEOUT.into())),
        }
    }

    // The ETW thread has exited; report how ProcessTrace completed if we haven't already.
    fn closed_error(&self) -> windows::core::Error {
        self.session_closed_rx.try_recv().unwrap_or(E_UNEXPECTED).into()
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

    #[test]
    fn expect_event_timeout() {
        // The consumer is never given to ProcessTrace, so the channels stay open but no event arrives.
        let mut waiter = EtwEventWaiter::default();
        let start = std::time::Instant::now();
        let err = waiter
            .expect_event_timeout(Duration::from_millis(50), |_| panic!("no event expected"))
            .unwrap_err();
        assert_eq!(err.code(), windows::Win32::Foundation::ERROR_TIMEOUT.to_hresult());
        assert!(start.elapsed() >= Duration::from_millis(50));
    }

    static consume_event_enabled_event: rsevents::ManualResetEvent =
        rsevents::ManualResetEvent::new(rsevents::EventState::Unset);

    #[test]
    fn consume_event() -> Result<(), windows::core::Error> {
        const test_name: &str = "EtwConsumer-Rust-Tests-ConsumeEvent-Flume";

        let mut options = tracelogging_dynamic::Provider::options();
        let options = options.callback(
            provider_enabled_callback,
            &consume_event_enabled_event as *const rsevents::ManualResetEvent as usize,
        );

        let provider = Box::pin(tracelogging_dynamic::Provider::new(
            "flume_consume_event_test",
            options,
        ));
        unsafe {
            provider.as_ref().register();
        }
        let provider_guid = windows::core::GUID::from_u128(provider.id().to_u128());
        let mut eb = tracelogging_dynamic::EventBuilder::new();

        let h = SessionBuilder::new_file_mode(
            "EtwConsumer-Rust-Tests-ConsumeEvent-Flume",
            "fce.etl",
            FileMode::Sequential,
        )
        .realtime_event_delivery()
        .start(true)?;

        h.enable_provider(&provider_guid, 0xFF)?;

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
            .expect_event_timeout(Duration::from_secs(10), |evt| {
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

        // The closure above returned false, which cancels the trace.
        assert!(consumer.expect_event(|_| true).is_err());

        let _ = thread.stop_and_wait(); // We don't care about what ProcessTrace returned

        Ok(())
    }

    static iterator_event_enabled_event: rsevents::ManualResetEvent =
        rsevents::ManualResetEvent::new(rsevents::EventState::Unset);

    #[test]
    fn iterate() -> Result<(), windows::core::Error> {
        const test_name: &str = "EtwConsumer-Rust-Tests-Iterator-Flume";

        let mut options = tracelogging_dynamic::Provider::options();
        let options = options.callback(
            provider_enabled_callback,
            &iterator_event_enabled_event as *const rsevents::ManualResetEvent as usize,
        );

        let provider = Box::pin(tracelogging_dynamic::Provider::new(
            "iterate_event_test",
            options,
        ));
        unsafe {
            provider.as_ref().register();
        }
        let provider_guid = windows::core::GUID::from_u128(provider.id().to_u128());
        let mut eb = tracelogging_dynamic::EventBuilder::new();

        let h = SessionBuilder::new_file_mode(
            "EtwConsumer-Rust-Tests-Iterator-Flume",
            "fi.etl",
            FileMode::Sequential,
        )
        .realtime_event_delivery()
        .start(true)?;

        h.enable_provider(&provider_guid, 0xFF)?;

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
        while let Some(evt) = consumer.next() {
            assert_eq!(evt.get_event_header().ProviderId, provider_guid);
            count += 1;

            if count == 5 {
                break;
            }
        }
        assert_eq!(count, 5);

        // Release the last event so ProcessTrace can return
        drop(consumer);
        let _ = thread.stop_and_wait(); // We don't care about what ProcessTrace returned

        Ok(())
    }
}
