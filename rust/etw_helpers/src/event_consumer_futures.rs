use std::{
    cell::RefCell,
    sync::atomic::{AtomicPtr, Ordering},
    time::Duration,
};

use futures::{
    channel::{mpsc, oneshot},
    future::{self, Either},
    StreamExt,
};
use futures_timer::Delay;
use windows::Win32::System::Diagnostics::Etw::EVENT_RECORD;

use crate::{event_record::EventRecord, error::*};
use crate::processtrace::*;

enum Message {
    /// The ETW callback is blocked until the HRESULT is sent or the sender is dropped.
    Event(AtomicPtr<EVENT_RECORD>, oneshot::Sender<windows::core::HRESULT>),
    /// ProcessTrace has returned with this result.
    Complete(windows::core::HRESULT),
}

struct FuturesChannelConsumer {
    // Unbounded is fine: the callback waits for each event to be handled, so at most one is queued.
    tx: mpsc::UnboundedSender<Message>,
}

impl EventConsumer for FuturesChannelConsumer {
    unsafe fn on_event_raw(&self, evt: *mut EVENT_RECORD) -> Result<(), windows::core::Error> {
        let (done_tx, done_rx) = oneshot::channel();
        if self
            .tx
            .unbounded_send(Message::Event(AtomicPtr::new(evt), done_tx))
            .is_err()
        {
            // The waiter was dropped
            return Err(E_CANCELLED.into());
        }

        // `evt` is only valid until this callback returns, so wait for the consumer to finish with it.
        match futures::executor::block_on(done_rx) {
            Ok(hr) if hr == S_OK => Ok(()),
            Ok(hr) => Err(hr.into()),
            // The event was dropped without being handled (e.g. the waiter was dropped)
            Err(oneshot::Canceled) => Err(E_CANCELLED.into()),
        }
    }

    fn complete(&self, err: windows::core::Error) {
        let _ = self.tx.unbounded_send(Message::Complete(err.code()));
    }
}

pub struct EtwEventAsyncWaiter {
    rx: RefCell<mpsc::UnboundedReceiver<Message>>,
    consumer: Option<FuturesChannelConsumer>,
}

impl Default for EtwEventAsyncWaiter {
    fn default() -> Self {
        let (tx, rx) = mpsc::unbounded::<Message>();
        EtwEventAsyncWaiter {
            rx: RefCell::new(rx),
            consumer: Some(FuturesChannelConsumer { tx }),
        }
    }
}

impl EtwEventAsyncWaiter {
    /// Returns the [`EventConsumer`] to pass to [`ProcessTraceHandle`].
    ///
    /// # Panics
    ///
    /// Panics if called more than once.
    pub fn get_consumer(&mut self) -> impl EventConsumer + use<> {
        self.consumer.take().unwrap()
    }

    /// Blocking version of [`expect_event_async`](Self::expect_event_async).
    pub fn expect_event<F>(&self, f: F) -> Result<(), windows::core::Error>
    where
        F: Fn(EventRecord) -> bool + Send + Sync,
    {
        futures::executor::block_on(self.expect_event_async(f))
    }

    /// Like [`expect_event`](Self::expect_event), but gives up if no event arrives within `timeout`.
    ///
    /// On timeout, returns an error with code `HRESULT_FROM_WIN32(ERROR_TIMEOUT)`
    /// (`windows::Win32::Foundation::ERROR_TIMEOUT.to_hresult()`), and `f` is not called.
    pub fn expect_event_timeout<F>(&self, timeout: Duration, f: F) -> Result<(), windows::core::Error>
    where
        F: Fn(EventRecord) -> bool + Send + Sync,
    {
        futures::executor::block_on(self.expect_event_async_timeout(timeout, f))
    }

    /// Waits for the next event and passes it to `f`. Returning `false` from `f` stops the trace.
    ///
    /// Returns an error once ProcessTrace has completed.
    pub async fn expect_event_async<F>(&self, f: F) -> Result<(), windows::core::Error>
    where
        F: Fn(EventRecord) -> bool + Send + Sync,
    {
        self.expect_event_inner(None, f).await
    }

    /// Like [`expect_event_async`](Self::expect_event_async), but gives up if no event arrives within `timeout`.
    ///
    /// On timeout, returns an error with code `HRESULT_FROM_WIN32(ERROR_TIMEOUT)`
    /// (`windows::Win32::Foundation::ERROR_TIMEOUT.to_hresult()`), and `f` is not called.
    pub async fn expect_event_async_timeout<F>(&self, timeout: Duration, f: F) -> Result<(), windows::core::Error>
    where
        F: Fn(EventRecord) -> bool + Send + Sync,
    {
        self.expect_event_inner(Some(Delay::new(timeout)), f).await
    }

    // A ref to rx is only held in this single function
    #[allow(clippy::await_holding_refcell_ref)]
    async fn expect_event_inner<F>(&self, timeout: Option<Delay>, f: F) -> Result<(), windows::core::Error>
    where
        F: Fn(EventRecord) -> bool + Send + Sync,
    {
        let next = {
            let mut rx = self.rx.borrow_mut();
            match timeout {
                None => rx.next().await,
                // If the delay wins, the pending `next()` is dropped without taking a message.
                Some(delay) => match future::select(rx.next(), delay).await {
                    Either::Left((next, _)) => next,
                    Either::Right(_) => return Err(E_TIMEOUT.into()),
                },
            }
        };
        match next {
            Some(Message::Event(ptr, done)) => {
                // SAFETY: the ETW callback that owns this event is blocked until `done` is
                // sent or dropped, which can only happen after `f` has returned (or unwound).
                let evt: &EVENT_RECORD = unsafe { &*ptr.load(Ordering::Acquire) };

                let should_continue = f(EventRecord::from_ref(evt));
                let _ = done.send(if should_continue { S_OK } else { E_CANCELLED });
                Ok(())
            }
            Some(Message::Complete(hr)) => Err(hr.into()),
            None => Err(E_UNEXPECTED.into()),
        }
    }
}

#[cfg(test)]
#[allow(non_upper_case_globals)]
mod tests {
    use crate::{FileMode, SessionBuilder};
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

    #[test]
    fn expect_event_timeout() {
        // The consumer is never given to ProcessTrace, so the channels stay open but no event arrives.
        let waiter = EtwEventAsyncWaiter::default();
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
        const test_name: &str = "EtwConsumer-Rust-Tests-ConsumeEvent-Futures";

        let mut options = tracelogging_dynamic::Provider::options();
        let options = options.callback(
            provider_enabled_callback,
            &consume_event_enabled_event as *const rsevents::ManualResetEvent as usize,
        );

        let provider = Box::pin(tracelogging_dynamic::Provider::new(
            "futures_consume_event_test",
            options,
        ));
        unsafe {
            provider.as_ref().register();
        }
        let provider_guid = windows::core::GUID::from_u128(provider.id().to_u128());
        let mut eb = tracelogging_dynamic::EventBuilder::new();

        let h = SessionBuilder::new_file_mode(
            "EtwConsumer-Rust-Tests-ConsumeEvent-Futures",
            "futcb.etl",
            FileMode::Sequential,
        )
        .realtime_event_delivery()
        .start(true)?;

        h.enable_provider(&provider_guid, 0xFF)?;

        let mut consumer = EtwEventAsyncWaiter::default();
        let event_consumer = consumer.get_consumer();

        let trace = ProcessTraceHandle::from_session(test_name, event_consumer)?;

        consume_event_enabled_event.wait();

        assert!(provider.enabled(tracelogging::Level::LogAlways, 3));

        eb.reset("test event", tracelogging::Level::LogAlways, 1, 0);
        assert_eq!(eb.write(&provider, None, None), 0);

        let fut = consumer.expect_event_async_timeout(Duration::from_secs(10), |evt| {
            // Give the ETW thread a chance to reuse the event's buffers if the callback has already returned.
            std::thread::sleep(std::time::Duration::from_millis(200));

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

        // The second closure returned false, which cancels the trace.
        assert!(consumer.expect_event(|_| true).is_err());

        let _ = thread.stop_and_wait(); // We don't care about what ProcessTrace returned

        result
    }
}
