use std::{
    future::poll_fn,
    sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError},
    task::{Poll, Waker},
};

use windows::Win32::System::Diagnostics::Etw::EVENT_RECORD;

use crate::error::*;
use crate::event_record::EventRecord;
use crate::processtrace::*;

enum Slot {
    /// No event pending. The ETW thread is free to deliver the next one.
    Empty,
    /// The ETW callback has published an event and is blocked until it is released.
    Ready(*const EVENT_RECORD),
    /// The event has been lent to the caller of `next()`; the ETW callback is still blocked.
    Lent,
    /// ProcessTrace has returned; no more events will be delivered.
    Complete,
}

struct State {
    slot: Slot,
    waker: Option<Waker>,
    stream_dropped: bool,
}

// SAFETY: the raw pointer in `Slot` is only dereferenced by the stream while the
// ETW callback thread that owns it is blocked waiting for the slot to be released.
unsafe impl Send for State {}

struct Shared {
    state: Mutex<State>,
    released: Condvar,
}

impl Shared {
    // Never let a poisoned lock turn into a panic on the ETW callback thread (which aborts).
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

struct EtwEventStreamConsumer {
    shared: Arc<Shared>,
}

impl EventConsumer for EtwEventStreamConsumer {
    unsafe fn on_event_raw(&self, evt: *mut EVENT_RECORD) -> Result<(), windows::core::Error> {
        let waker = {
            let mut state = self.shared.lock();
            if state.stream_dropped {
                return Err(E_CANCELLED.into());
            }
            state.slot = Slot::Ready(evt);
            state.waker.take()
        };
        if let Some(w) = waker {
            w.wake();
        }

        // `evt` is only valid until this callback returns, so block until the
        // stream has released it (or the stream has been dropped).
        let state = self.shared.lock();
        let mut state = self
            .shared
            .released
            .wait_while(state, |s| {
                matches!(s.slot, Slot::Ready(_) | Slot::Lent) && !s.stream_dropped
            })
            .unwrap_or_else(PoisonError::into_inner);
        state.slot = Slot::Empty;

        Ok(())
    }

    fn complete(&self, _err: windows::core::Error) {
        let waker = {
            let mut state = self.shared.lock();
            state.slot = Slot::Complete;
            state.waker.take()
        };
        if let Some(w) = waker {
            w.wake();
        }
    }
}

/// A lending async stream of ETW events.
///
/// Each event returned by [`next`](Self::next) borrows the stream mutably, and the
/// ETW callback thread stays blocked until that borrow ends. Holding an event
/// across the next call to `next()` is therefore a compile error:
///
/// ```compile_fail
/// # async fn f(stream: &mut etw_helpers::event_consumer_async::EtwEventAsyncStream) {
/// let first = stream.next().await;
/// let second = stream.next().await; // error[E0499]: cannot borrow `*stream` as mutable more than once
/// drop(first);
/// # }
/// ```
pub struct EtwEventAsyncStream {
    shared: Arc<Shared>,
    consumer: Option<EtwEventStreamConsumer>,
}

impl Default for EtwEventAsyncStream {
    fn default() -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                slot: Slot::Empty,
                waker: None,
                stream_dropped: false,
            }),
            released: Condvar::new(),
        });
        EtwEventAsyncStream {
            consumer: Some(EtwEventStreamConsumer {
                shared: shared.clone(),
            }),
            shared,
        }
    }
}

impl EtwEventAsyncStream {
    /// Returns the [`EventConsumer`] to pass to [`ProcessTraceHandle`].
    ///
    /// # Panics
    ///
    /// Panics if called more than once; the stream supports a single producer.
    pub fn get_consumer(&mut self) -> impl EventConsumer + use<> {
        self.consumer.take().unwrap()
    }

    /// Waits for the next event. Returns `None` once ProcessTrace has completed.
    ///
    /// The returned event borrows `self`, so it must be dropped before calling `next()` again.
    pub async fn next(&mut self) -> Option<EventRecord<'_>> {
        // Requiring `&mut self` guarantees any previously returned event is no longer referenced.
        self.release_current();

        let ptr = poll_fn(|cx| {
            let mut state = self.shared.lock();
            match state.slot {
                Slot::Ready(p) => {
                    state.slot = Slot::Lent;
                    Poll::Ready(Some(p))
                }
                Slot::Complete => Poll::Ready(None),
                Slot::Empty => {
                    match &mut state.waker {
                        Some(w) if w.will_wake(cx.waker()) => {}
                        w => *w = Some(cx.waker().clone()),
                    }
                    Poll::Pending
                }
                Slot::Lent => unreachable!("previous event was released above"),
            }
        })
        .await?;

        // SAFETY: the ETW callback that owns `ptr` is blocked until the slot leaves `Lent`,
        // which only happens in `release_current` (requires `&mut self`, ending this borrow)
        // or in `Drop`.
        Some(EventRecord::from_ref(unsafe { &*ptr }))
    }

    fn release_current(&mut self) {
        let mut state = self.shared.lock();
        if let Slot::Lent = state.slot {
            state.slot = Slot::Empty;
            drop(state);
            self.shared.released.notify_one();
        }
    }
}

impl Drop for EtwEventAsyncStream {
    fn drop(&mut self) {
        self.shared.lock().stream_dropped = true;
        self.shared.released.notify_one();
    }
}

#[cfg(test)]
#[allow(non_upper_case_globals)]
mod tests {
    use crate::{FileMode, SessionBuilder};

    use super::*;

    #[tokio::test]
    async fn stream_events() -> Result<(), windows::core::Error> {
        const test_name: &str = "EtwConsumer-Rust-Tests-StreamEvent";

        let provider = Box::pin(tracelogging_dynamic::Provider::new(
            "stream_event_test",
            &tracelogging_dynamic::Provider::options(),
        ));
        unsafe {
            provider.as_ref().register();
        }
        let provider_guid = windows::core::GUID::from_u128(provider.id().to_u128());
        let mut eb = tracelogging_dynamic::EventBuilder::new();

        let h = SessionBuilder::new_file_mode(test_name, "strm.etl", FileMode::Sequential)
            .realtime_event_delivery()
            .start(true)?;

        h.enable_provider(&provider_guid, 0xFF)?;

        let mut etw_event_stream = EtwEventAsyncStream::default();
        let event_consumer = etw_event_stream.get_consumer();

        let trace = ProcessTraceHandle::from_session(test_name, event_consumer)?;

        let mut thread = trace.process_trace()?;

        eb.write(&provider, None, None);
        eb.write(&provider, None, None);
        eb.write(&provider, None, None);

        let mut process_trace_thread = None;
        let mut kept = Vec::new();
        while let Some(evt) = etw_event_stream.next().await {
            assert_eq!(evt.get_event_header().ProviderId, provider_guid);
            let ext_count = evt.get_extended_data_items().count();

            let serialized = EventRecord::from_serialized(evt.serialize())?;
            assert_eq!(serialized.get_event_header().ProviderId, provider_guid);
            assert_eq!(serialized.get_user_data(), evt.get_user_data());
            assert_eq!(serialized.get_extended_data_items().count(), ext_count);

            kept.push((ext_count, evt.into_owned()));

            if kept.len() == 3 {
                process_trace_thread = Some(thread.stop_and_get_thread());
            }
        }

        let _ = process_trace_thread.expect("x")?.join(); // We don't care about what ProcessTrace returned

        assert_eq!(kept.len(), 3);
        for (ext_count, evt) in kept {
            assert!(ext_count > 0, "TraceLogging events carry schema extended data");
            assert_eq!(evt.get_event_header().ProviderId, provider_guid);
            assert_eq!(evt.get_extended_data_items().count(), ext_count);
        }

        Ok(())
    }
}
