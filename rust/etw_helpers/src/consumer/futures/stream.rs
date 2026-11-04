use std::{
    future::poll_fn,
    sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError},
    task::{Poll, Waker},
    time::Duration,
};

use windows_sys::Win32::System::Diagnostics::Etw::EVENT_RECORD;

use crate::consumer::event_record::EventRecord;
use crate::consumer::event_waiter::EventOrTimeout;
use crate::consumer::event_waiter::sealed::{Connect, NextAsync};
use crate::consumer::processtrace::*;
use crate::error::*;

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

impl EventProcessor for EtwEventStreamConsumer {
    fn on_event_raw(&self, evt: &EVENT_RECORD) -> Result<(), crate::Error> {
        let waker = {
            let mut state = self.shared.lock();
            if state.stream_dropped {
                return Err(E_CANCELLED.into_error());
            }
            state.slot = Slot::Ready(core::ptr::from_ref(evt));
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

    fn complete(&self, _err: crate::Error) {
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

// `EtwEventAsyncStream<Futures>`: a lending async stream over a `Mutex` + `Condvar` slot. The ETW
// callback blocks until the stream releases the event (or is dropped).
pub struct LentStream {
    shared: Arc<Shared>,
    consumer: Option<EtwEventStreamConsumer>,
}

impl Default for LentStream {
    fn default() -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                slot: Slot::Empty,
                waker: None,
                stream_dropped: false,
            }),
            released: Condvar::new(),
        });
        LentStream {
            consumer: Some(EtwEventStreamConsumer {
                shared: shared.clone(),
            }),
            shared,
        }
    }
}

impl Connect for LentStream {
    fn take_processor(&mut self) -> impl EventProcessor + 'static + use<> {
        self.consumer.take().expect("the trace was already started")
    }
}

impl NextAsync for LentStream {
    #[cfg_attr(
        feature = "hotpath",
        hotpath::measure(impl_type = "EtwEventAsyncStream<Futures>", future = true)
    )]
    async fn next_timeout_async(
        &mut self,
        timeout: Option<Duration>,
    ) -> Option<EventOrTimeout<'_>> {
        // Requiring `&mut self` guarantees any previously returned event is no longer referenced.
        self.release_current();

        // If this times out, the stream is left without a lent event, and a waker that is
        // replaced by the next call.
        let received = super::with_timeout(
            poll_fn(|cx| {
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
            }),
            timeout,
        )
        .await;
        let ptr = match received {
            Some(Some(ptr)) => ptr,
            Some(None) => return None,
            None => return Some(EventOrTimeout::Timeout),
        };

        // SAFETY: the ETW callback that owns `ptr` is blocked until the slot leaves `Lent`,
        // which only happens in `release_current` (requires `&mut self`, ending this borrow)
        // or in `Drop`.
        Some(EventOrTimeout::Event(EventRecord::from_ref(unsafe {
            &*ptr
        })))
    }
}

impl LentStream {
    fn release_current(&mut self) {
        let mut state = self.shared.lock();
        if let Slot::Lent = state.slot {
            state.slot = Slot::Empty;
            drop(state);
            self.shared.released.notify_one();
        }
    }
}

impl Drop for LentStream {
    fn drop(&mut self) {
        self.shared.lock().stream_dropped = true;
        self.shared.released.notify_one();
    }
}

#[cfg(test)]
mod tests {
    crate::consumer::shared_tests::stream_tests!(crate::consumer::Futures, "FuturesStream");
}
