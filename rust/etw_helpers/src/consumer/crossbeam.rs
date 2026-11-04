use std::sync::atomic::{AtomicPtr, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{RecvTimeoutError, TryRecvError};
// Instrumented by hotpath when the `hotpath` feature is enabled; plain crossbeam types otherwise.
use crate::profiling::wrap::crossbeam_channel::{Receiver, Sender};
use windows_sys::Win32::System::Diagnostics::Etw::EVENT_RECORD;

use crate::consumer::event_record::{EventRecord, EventRecordOwned};
use crate::consumer::event_waiter::sealed::{Connect, Next, NextOwned, Wait};
use crate::consumer::event_waiter::{EventOrTimeout, call_or_cancel};
use crate::consumer::processtrace::*;
use crate::error::*;

// Processor of an `LentWaiter`: lends each event and blocks until it is released.
struct LentProcessor {
    tx: Sender<AtomicPtr<EVENT_RECORD>>,
    callback_rx: Receiver<windows_sys::core::HRESULT>,
}

impl EventProcessor for LentProcessor {
    fn on_event_raw(&self, evt: &EVENT_RECORD) -> Result<(), crate::Error> {
        // The waiter only reads through the pointer.
        if self
            .tx
            .send(AtomicPtr::new(core::ptr::from_ref(evt).cast_mut()))
            .is_err()
        {
            // The waiter was dropped.
            return Err(E_CANCELLED.into_error());
        }

        match recv_spin(&self.callback_rx, None) {
            Ok(hr) if hr == S_OK => Ok(()),
            Ok(hr) => Err(hr.into_error()),
            // The waiter was dropped without releasing the event.
            Err(_) => Err(E_CANCELLED.into_error()),
        }
    }
}

// Processor of an `OwnedWaiter`: copies each event into a bounded queue.
struct OwnedProcessor {
    tx: Sender<EventRecordOwned>,
}

impl EventProcessor for OwnedProcessor {
    fn on_event_raw(&self, evt: &EVENT_RECORD) -> Result<(), crate::Error> {
        let evt = EventRecordOwned::from_raw(evt);
        // Blocks while the queue is full. Fails once the waiter has been dropped.
        self.tx.send(evt).map_err(|_| E_CANCELLED.into_error())
    }
}

// Event channels disconnect once ProcessTrace has completed, when the processor holding the
// sender is dropped. (crossbeam's `recv_timeout` treats a deadline that overflows `Instant` as no
// deadline.)
fn recv_timeout<T>(rx: &Receiver<T>, timeout: Option<Duration>) -> Result<T, RecvTimeoutError> {
    match timeout {
        None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        Some(timeout) => rx.recv_timeout(timeout),
    }
}

// `recv_timeout`, but first spins briefly (see `lock::spin`): the other side of a lent event's
// handoff usually responds within a microsecond, and blocking costs a wake on each side.
fn recv_spin<T>(rx: &Receiver<T>, timeout: Option<Duration>) -> Result<T, RecvTimeoutError> {
    let spun = crate::consumer::lock::spin(|| match rx.try_recv() {
        Ok(msg) => Some(Ok(msg)),
        Err(TryRecvError::Empty) => None,
        Err(TryRecvError::Disconnected) => Some(Err(RecvTimeoutError::Disconnected)),
    });
    spun.unwrap_or_else(|| recv_timeout(rx, timeout))
}

// The time left until `deadline`, or `None` (no limit) without one.
fn remaining(deadline: Option<Instant>) -> Option<Duration> {
    deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()))
}

// A deadline `timeout` from now; `None` without a timeout, or if it overflows `Instant`.
fn deadline(timeout: Option<Duration>) -> Option<Instant> {
    timeout.and_then(|timeout| Instant::now().checked_add(timeout))
}

// `EtwEventWaiter<Crossbeam>`: lends each event straight from the ETW buffer; the ETW callback blocks
// until it is released.
pub struct LentWaiter {
    rx: Receiver<AtomicPtr<EVENT_RECORD>>,
    callback_tx: Sender<windows_sys::core::HRESULT>,
    // True while the ETW callback is blocked waiting for us to release an event returned by `next()`
    lent: bool,
    // Set once a closure returns `false` (or panics) on a timeout; any later event is rejected.
    cancelled: bool,
    // Until the trace starts.
    processor: Option<LentProcessor>,
}

impl Default for LentWaiter {
    fn default() -> Self {
        let (tx, rx) = crate::profiling::channel!(
            crossbeam_channel::bounded::<AtomicPtr<EVENT_RECORD>>(0),
            label = "crossbeam_events"
        );
        let (callback_tx, callback_rx) = crate::profiling::channel!(
            crossbeam_channel::bounded::<windows_sys::core::HRESULT>(1),
            label = "crossbeam_event_released"
        );
        LentWaiter {
            rx,
            callback_tx,
            lent: false,
            cancelled: false,
            processor: Some(LentProcessor { tx, callback_rx }),
        }
    }
}

impl LentWaiter {
    // Waits for the next event, until ProcessTrace has completed or `timeout` passes. Once
    // cancelled, rejects events instead: ETW may already have been delivering one.
    fn receive(
        &mut self,
        timeout: Option<Duration>,
    ) -> Result<AtomicPtr<EVENT_RECORD>, RecvTimeoutError> {
        if !self.cancelled {
            return recv_spin(&self.rx, timeout);
        }
        let deadline = deadline(timeout);
        loop {
            let ptr = recv_timeout(&self.rx, remaining(deadline))?;
            if !self.cancelled {
                return Ok(ptr);
            }
            let _ = self.callback_tx.send(E_CANCELLED);
        }
    }

    fn release_current(&mut self) {
        if self.lent {
            self.lent = false;
            // Never blocks: the callback takes each reply before delivering the next event.
            let _ = self.callback_tx.send(S_OK);
        }
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
        hotpath::measure(impl_type = "EtwEventWaiter<Crossbeam>")
    )]
    fn next_timeout(&mut self, timeout: Option<Duration>) -> Option<EventOrTimeout<'_>> {
        // Requiring `&mut self` guarantees any previously returned event is no longer referenced.
        self.release_current();

        let ptr = match self.receive(timeout) {
            Ok(ptr) => ptr,
            Err(RecvTimeoutError::Timeout) => return Some(EventOrTimeout::Timeout),
            Err(RecvTimeoutError::Disconnected) => return None,
        };
        self.lent = true;

        // SAFETY: the ETW callback that owns this event is blocked until we send on
        // `callback_tx`, which only happens in `release_current` (requires `&mut self`,
        // ending this borrow) or when `self` is dropped.
        Some(EventOrTimeout::Event(EventRecord::from_ref(unsafe {
            &*ptr.load(Ordering::Acquire)
        })))
    }
}

impl Wait for LentWaiter {
    #[cfg_attr(
        feature = "hotpath",
        hotpath::measure(impl_type = "EtwEventWaiter<Crossbeam>")
    )]
    fn wait<F>(&mut self, timeout: Option<Duration>, f: F) -> Result<(), crate::Error>
    where
        F: FnOnce(EventOrTimeout<'_>) -> bool,
    {
        self.release_current();

        let ptr = match self.receive(timeout) {
            Ok(ptr) => ptr,
            Err(RecvTimeoutError::Timeout) => {
                call_or_cancel(self, f, EventOrTimeout::Timeout, |waiter| {
                    waiter.cancelled = true;
                });
                return Ok(());
            }
            Err(RecvTimeoutError::Disconnected) => return Err(E_TRACE_FINISHED.into_error()),
        };

        // Created before `f` runs so the callback is released even if `f` panics.
        let reply = Reply {
            tx: &self.callback_tx,
        };

        // SAFETY: the ETW callback that owns this event is blocked until `reply` sends,
        // which can only happen after `f` has returned (or unwound).
        let evt: &EVENT_RECORD = unsafe { &*ptr.load(Ordering::Acquire) };
        let hr = if f(EventOrTimeout::Event(EventRecord::from_ref(evt))) {
            S_OK
        } else {
            E_CANCELLED
        };

        reply.send(hr)
    }
}

// Replies to the blocked ETW callback. If dropped without `send` (the event handler panicked),
// it replies `E_CANCELLED`, which releases the callback and stops the trace.
struct Reply<'a> {
    tx: &'a Sender<windows_sys::core::HRESULT>,
}

impl Reply<'_> {
    fn send(self, hr: windows_sys::core::HRESULT) -> Result<(), crate::Error> {
        let reply = std::mem::ManuallyDrop::new(self);
        // Never blocks: the callback takes each reply before delivering the next event.
        reply.tx.send(hr).map_err(|_| E_UNEXPECTED.into_error())
    }
}

impl Drop for Reply<'_> {
    fn drop(&mut self) {
        let _ = self.tx.send(E_CANCELLED);
    }
}

// `EtwEventWaiterOwned<Crossbeam>`: the ETW callback copies each event into a bounded queue, and only
// blocks while it's full. Returning `false` from a closure discards the queue.
pub struct OwnedWaiter {
    rx: Receiver<EventRecordOwned>,
    // Set once a closure returns `false` (or panics); the queue is then discarded.
    cancelled: bool,
    // Until the trace starts.
    processor: Option<OwnedProcessor>,
}

impl OwnedWaiter {
    /// Creates a waiter that queues up to `event_buffer_count` events. With 0, each event is
    /// handed over directly, but the ETW callback still doesn't wait for it to be handled.
    pub fn new(event_buffer_count: usize) -> Self {
        let (tx, rx) = crate::profiling::channel!(
            crossbeam_channel::bounded::<EventRecordOwned>(event_buffer_count),
            label = "crossbeam_owned_events"
        );
        OwnedWaiter {
            rx,
            cancelled: false,
            processor: Some(OwnedProcessor { tx }),
        }
    }

    // Discards the events already queued (releasing a processor blocked on a full queue), and
    // every later one. `EventTrace` has already stopped the trace (see `cancel_on_false`).
    fn cancel(&mut self) {
        self.cancelled = true;
        while self.rx.try_recv().is_ok() {}
    }

    // After cancelling: discards events until ProcessTrace has completed (`Disconnected`), or
    // `timeout` passes (`Timeout`).
    fn drain(&self, timeout: Option<Duration>) -> RecvTimeoutError {
        let deadline = deadline(timeout);
        loop {
            if let Err(err) = recv_timeout(&self.rx, remaining(deadline)) {
                return err;
            }
        }
    }

    // Waits for the next queued event, until ProcessTrace has completed or `timeout` passes.
    fn receive(&self, timeout: Option<Duration>) -> Result<EventRecordOwned, RecvTimeoutError> {
        if self.cancelled {
            return Err(self.drain(timeout));
        }
        recv_timeout(&self.rx, timeout)
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
        hotpath::measure(impl_type = "EtwEventWaiterOwned<Crossbeam>")
    )]
    fn next_owned(&mut self, timeout: Option<Duration>) -> Option<EventOrTimeout<'static>> {
        match self.receive(timeout) {
            Ok(evt) => Some(EventOrTimeout::Event(EventRecord::Owned(evt))),
            Err(RecvTimeoutError::Timeout) => Some(EventOrTimeout::Timeout),
            // ProcessTrace has completed and every queued event has been received.
            Err(RecvTimeoutError::Disconnected) => None,
        }
    }
}

impl Wait for OwnedWaiter {
    #[cfg_attr(
        feature = "hotpath",
        hotpath::measure(impl_type = "EtwEventWaiterOwned<Crossbeam>")
    )]
    fn wait<F>(&mut self, timeout: Option<Duration>, f: F) -> Result<(), crate::Error>
    where
        F: FnOnce(EventOrTimeout<'_>) -> bool,
    {
        let evt = match self.receive(timeout) {
            Ok(evt) => EventOrTimeout::Event(EventRecord::Owned(evt)),
            Err(RecvTimeoutError::Timeout) => EventOrTimeout::Timeout,
            // ProcessTrace has completed and every queued event has been received.
            Err(RecvTimeoutError::Disconnected) => return Err(E_TRACE_FINISHED.into_error()),
        };

        call_or_cancel(self, f, evt, Self::cancel);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    crate::consumer::shared_tests::lent_waiter_tests!(
        crate::consumer::Crossbeam,
        "Crossbeam",
        iterate;
        timed: timed_next
    );
    crate::consumer::shared_tests::owned_waiter_tests!(crate::consumer::Crossbeam, "Crossbeam");
}
