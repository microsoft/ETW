//! An event consumer with no channel crate dependency: the ETW thread and the waiter
//! synchronize through Win32 SRW locks and condition variables.
//!
//! Only `core`, `alloc` and Win32 APIs are used here (no `std`), with a view to supporting
//! `no_std` environments that have `alloc`.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::time::Duration;

use windows_sys::Win32::System::Diagnostics::Etw::EVENT_RECORD;

use crate::consumer::event_record::{EventRecord, EventRecordOwned};
use crate::consumer::event_waiter::sealed::{Connect, Next, NextOwned, Wait};
use crate::consumer::event_waiter::{EventOrTimeout, call_or_cancel};
use crate::consumer::lock::{Deadline, Lock};
use crate::consumer::processtrace::*;
use crate::error::*;

enum Slot {
    Empty,
    // The ETW callback has published this event and is blocked until it is released.
    Ready(*const EVENT_RECORD),
    // The event has been handed to the waiter; the ETW callback is still blocked.
    Lent,
    // The waiter is done with the event; the ETW callback returns this result.
    Released(windows_sys::core::HRESULT),
}

struct LentState {
    slot: Slot,
    // ProcessTrace has completed (the processor was dropped).
    finished: bool,
    waiter_dropped: bool,
}

// SAFETY: the event pointer in `Slot` is only dereferenced by the waiter while the ETW
// callback that owns it is blocked.
unsafe impl Send for LentState {}

// Processor of an `LentWaiter`: lends each event and blocks until it is released.
struct LentProcessor {
    shared: Arc<Lock<LentState>>,
}

impl EventProcessor for LentProcessor {
    fn on_event_raw(&self, evt: &EVENT_RECORD) -> Result<(), crate::Error> {
        let mut state = self.shared.lock();
        if state.waiter_dropped {
            return Err(E_CANCELLED.into_error());
        }
        state.slot = Slot::Ready(core::ptr::from_ref(evt));
        self.shared.notify_all();

        // `evt` is only valid until this callback returns, so wait for the waiter to release it.
        state.wait_while(Deadline::NEVER, |s| {
            matches!(s.slot, Slot::Ready(_) | Slot::Lent) && !s.waiter_dropped
        });
        match core::mem::replace(&mut state.slot, Slot::Empty) {
            Slot::Released(hr) if hr == S_OK => Ok(()),
            Slot::Released(hr) => Err(hr.into_error()),
            // The waiter was dropped without releasing the event.
            _ => Err(E_CANCELLED.into_error()),
        }
    }
}

impl Drop for LentProcessor {
    fn drop(&mut self) {
        self.shared.lock().finished = true;
        self.shared.notify_all();
    }
}

enum Received<T> {
    Event(T),
    Finished,
    Timeout,
}

// `EtwEventWaiter<Raw>`: lends each event straight from the ETW buffer, synchronizing through a `Lock`
// slot; the ETW callback blocks until the event is released.
pub struct LentWaiter {
    shared: Arc<Lock<LentState>>,
    // Set once a closure returns `false` (or panics) on a timeout; any later event is rejected.
    cancelled: bool,
    // Until the trace starts.
    processor: Option<LentProcessor>,
}

impl Default for LentWaiter {
    fn default() -> Self {
        let shared = Arc::new(Lock::new(LentState {
            slot: Slot::Empty,
            finished: false,
            waiter_dropped: false,
        }));
        LentWaiter {
            shared: shared.clone(),
            cancelled: false,
            processor: Some(LentProcessor { shared }),
        }
    }
}

impl LentWaiter {
    fn release_current(&mut self) {
        let mut state = self.shared.lock();
        if let Slot::Lent = state.slot {
            state.slot = Slot::Released(S_OK);
            self.shared.notify_all();
        }
    }

    // Waits for the next event, until ProcessTrace has completed or `deadline` passes. Once
    // cancelled, rejects events instead: ETW may already have been delivering one.
    fn receive(&mut self, deadline: Deadline) -> Received<*const EVENT_RECORD> {
        let mut state = self.shared.lock();
        loop {
            state.wait_while(deadline, |s| {
                !matches!(s.slot, Slot::Ready(_)) && !s.finished
            });
            match state.slot {
                Slot::Ready(_) if self.cancelled => {
                    state.slot = Slot::Released(E_CANCELLED);
                    self.shared.notify_all();
                }
                Slot::Ready(evt) => {
                    state.slot = Slot::Lent;
                    return Received::Event(evt);
                }
                _ if state.finished => return Received::Finished,
                _ => return Received::Timeout,
            }
        }
    }
}

impl Drop for LentWaiter {
    fn drop(&mut self) {
        // Releases a callback blocked on a lent event, which then stops the trace.
        self.shared.lock().waiter_dropped = true;
        self.shared.notify_all();
    }
}

impl Connect for LentWaiter {
    fn take_processor(&mut self) -> impl EventProcessor + 'static + use<> {
        self.processor
            .take()
            .expect("the trace was already started")
    }
}

impl Next for LentWaiter {
    #[cfg_attr(
        feature = "hotpath",
        hotpath::measure(impl_type = "EtwEventWaiter<Raw>")
    )]
    fn next_timeout(&mut self, timeout: Option<Duration>) -> Option<EventOrTimeout<'_>> {
        // Requiring `&mut self` guarantees any previously returned event is no longer referenced.
        self.release_current();

        let evt = match self.receive(Deadline::after(timeout)) {
            Received::Event(evt) => evt,
            Received::Timeout => return Some(EventOrTimeout::Timeout),
            Received::Finished => return None,
        };

        // SAFETY: the ETW callback that owns this event is blocked until it is released, which
        // only happens in `release_current` (requires `&mut self`, ending this borrow) or when
        // `self` is dropped.
        Some(EventOrTimeout::Event(EventRecord::from_ref(unsafe {
            &*evt
        })))
    }
}

impl Wait for LentWaiter {
    #[cfg_attr(
        feature = "hotpath",
        hotpath::measure(impl_type = "EtwEventWaiter<Raw>")
    )]
    fn wait<F>(&mut self, timeout: Option<Duration>, f: F) -> Result<(), crate::Error>
    where
        F: FnOnce(EventOrTimeout<'_>) -> bool,
    {
        self.release_current();

        let evt = match self.receive(Deadline::after(timeout)) {
            Received::Event(evt) => evt,
            Received::Timeout => {
                call_or_cancel(self, f, EventOrTimeout::Timeout, |waiter| {
                    waiter.cancelled = true;
                });
                return Ok(());
            }
            Received::Finished => return Err(E_TRACE_FINISHED.into_error()),
        };

        // Created before `f` runs so the callback is released even if `f` panics.
        let mut reply = Reply {
            shared: &self.shared,
            hr: E_CANCELLED,
        };

        // SAFETY: the ETW callback that owns this event is blocked until `reply` is dropped,
        // which can only happen after `f` has returned (or unwound).
        if f(EventOrTimeout::Event(EventRecord::from_ref(unsafe {
            &*evt
        }))) {
            reply.hr = S_OK;
        }

        Ok(())
    }
}

// Releases the lent event when dropped, so the ETW callback is released even if the event
// handler panics (with `E_CANCELLED`, which stops the trace).
struct Reply<'a> {
    shared: &'a Lock<LentState>,
    hr: windows_sys::core::HRESULT,
}

impl Drop for Reply<'_> {
    fn drop(&mut self) {
        self.shared.lock().slot = Slot::Released(self.hr);
        self.shared.notify_all();
    }
}

struct OwnedState {
    queue: VecDeque<EventRecordOwned>,
    capacity: usize,
    // ProcessTrace has completed (the processor was dropped).
    finished: bool,
    // The trace was cancelled (or the waiter dropped); the callback stops queueing events.
    cancelled: bool,
}

// Processor of an `OwnedWaiter`: copies each event into a bounded queue.
struct OwnedProcessor {
    shared: Arc<Lock<OwnedState>>,
}

impl EventProcessor for OwnedProcessor {
    fn on_event_raw(&self, evt: &EVENT_RECORD) -> Result<(), crate::Error> {
        let evt = EventRecordOwned::from_raw(evt);

        let mut state = self.shared.lock();
        // Blocks while the queue is full.
        state.wait_while(Deadline::NEVER, |s| {
            s.queue.len() >= s.capacity.max(1) && !s.cancelled
        });
        if state.cancelled {
            return Err(E_CANCELLED.into_error());
        }
        state.queue.push_back(evt);
        self.shared.notify_all();

        if state.capacity == 0 {
            // No buffering: hand the event over directly, without waiting for it to be handled.
            state.wait_while(Deadline::NEVER, |s| !s.queue.is_empty() && !s.cancelled);
        }

        Ok(())
    }
}

impl Drop for OwnedProcessor {
    fn drop(&mut self) {
        self.shared.lock().finished = true;
        self.shared.notify_all();
    }
}

// `EtwEventWaiterOwned<Raw>`: the ETW callback copies each event into a `VecDeque` of up to the
// requested capacity, and only blocks while it's full (with capacity 0, until the event is taken).
pub struct OwnedWaiter {
    shared: Arc<Lock<OwnedState>>,
    // Until the trace starts.
    processor: Option<OwnedProcessor>,
}

impl OwnedWaiter {
    /// Creates a waiter that queues up to `event_buffer_count` events. With 0, each event is
    /// handed over directly, but the ETW callback still doesn't wait for it to be handled.
    pub fn new(event_buffer_count: usize) -> Self {
        let shared = Arc::new(Lock::new(OwnedState {
            queue: VecDeque::with_capacity(event_buffer_count.max(1)),
            capacity: event_buffer_count,
            finished: false,
            cancelled: false,
        }));
        OwnedWaiter {
            shared: shared.clone(),
            processor: Some(OwnedProcessor { shared }),
        }
    }

    // Waits for the next queued event, until ProcessTrace has completed or `deadline` passes.
    fn receive(&mut self, deadline: Deadline) -> Received<EventRecordOwned> {
        let mut state = self.shared.lock();
        state.wait_while(deadline, |s| s.queue.is_empty() && !s.finished);
        // Events queued before ProcessTrace completed are still delivered, unless cancelled.
        if let Some(evt) = state.queue.pop_front() {
            self.shared.notify_all(); // There is room in the queue again
            return Received::Event(evt);
        }
        if state.finished {
            Received::Finished
        } else {
            Received::Timeout
        }
    }

    // Stops queueing events, which fails the callback's pending or next event and so stops the
    // trace. Events still queued are discarded.
    fn cancel(&mut self) {
        let mut state = self.shared.lock();
        state.cancelled = true;
        state.queue.clear();
        self.shared.notify_all();
    }
}

impl Drop for OwnedWaiter {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl Connect for OwnedWaiter {
    fn take_processor(&mut self) -> impl EventProcessor + 'static + use<> {
        self.processor
            .take()
            .expect("the trace was already started")
    }
}

impl NextOwned for OwnedWaiter {
    #[cfg_attr(
        feature = "hotpath",
        hotpath::measure(impl_type = "EtwEventWaiterOwned<Raw>")
    )]
    fn next_owned(&mut self, timeout: Option<Duration>) -> Option<EventOrTimeout<'static>> {
        // Once cancelled, the queue stays empty, so this waits for ProcessTrace to complete.
        match self.receive(Deadline::after(timeout)) {
            Received::Event(evt) => Some(EventOrTimeout::Event(EventRecord::Owned(evt))),
            Received::Timeout => Some(EventOrTimeout::Timeout),
            Received::Finished => None,
        }
    }
}

impl Wait for OwnedWaiter {
    #[cfg_attr(
        feature = "hotpath",
        hotpath::measure(impl_type = "EtwEventWaiterOwned<Raw>")
    )]
    fn wait<F>(&mut self, timeout: Option<Duration>, f: F) -> Result<(), crate::Error>
    where
        F: FnOnce(EventOrTimeout<'_>) -> bool,
    {
        let Some(evt) = self.next_owned(timeout) else {
            return Err(E_TRACE_FINISHED.into_error());
        };

        call_or_cancel(self, f, evt, Self::cancel);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    crate::consumer::shared_tests::lent_waiter_tests!(
        crate::consumer::Raw,
        "Raw",
        iterate;
        timed: timed_next
    );
    crate::consumer::shared_tests::owned_waiter_tests!(crate::consumer::Raw, "Raw");
}
