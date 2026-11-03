use std::{
    sync::{
        Mutex, PoisonError,
        atomic::{AtomicPtr, Ordering},
    },
    time::{Duration, Instant},
};

use futures::{
    FutureExt, SinkExt, StreamExt,
    future::{self, Either},
};
use futures_timer::Delay;
// Instrumented by hotpath when the `hotpath` feature is enabled; plain std/futures types otherwise.
use crate::profiling::wrap::futures_channel::mpsc;
use crate::profiling::wrap::std::sync::mpsc::{Receiver, SyncSender};
use windows_sys::Win32::System::Diagnostics::Etw::EVENT_RECORD;

use crate::consumer::event_record::{EventRecord, EventRecordOwned};
use crate::consumer::event_waiter::sealed::{Connect, NextOwned, NextOwnedAsync, Wait, WaitAsync};
use crate::consumer::event_waiter::{EventOrTimeout, call_or_cancel};
use crate::consumer::processtrace::*;
use crate::error::*;

mod stream;
pub use stream::LentStream;

// Processor of an `LentWaiter`: lends each event and blocks until it is handled.
struct LentProcessor {
    // Unbounded is fine: the callback waits for each event to be handled, so at most one is queued.
    tx: mpsc::UnboundedSender<AtomicPtr<EVENT_RECORD>>,
    // One reply per event, reused for every event. Only the ETW thread receives; the
    // Mutex (never contended) just makes the processor `Sync`.
    reply_rx: Mutex<Receiver<windows_sys::core::HRESULT>>,
}

impl EventProcessor for LentProcessor {
    unsafe fn on_event_raw(&self, evt: *mut EVENT_RECORD) -> Result<(), crate::Error> {
        if self.tx.unbounded_send(AtomicPtr::new(evt)).is_err() {
            // The waiter was dropped
            return Err(E_CANCELLED.into_error());
        }

        // `evt` is only valid until this callback returns, so wait for the waiter to finish with it.
        let reply_rx = self.reply_rx.lock().unwrap_or_else(PoisonError::into_inner);
        // Spin briefly first (see `lock::spin`): the waiter usually replies within a microsecond,
        // and blocking costs a wake on each side.
        let reply = crate::consumer::lock::spin(|| match reply_rx.try_recv() {
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            received => Some(received.map_err(|_| std::sync::mpsc::RecvError)),
        })
        .unwrap_or_else(|| reply_rx.recv());
        match reply {
            Ok(hr) if hr == S_OK => Ok(()),
            Ok(hr) => Err(hr.into_error()),
            // The waiter was dropped without handling the event
            Err(_) => Err(E_CANCELLED.into_error()),
        }
    }
}

// Processor of an `OwnedWaiter`: copies each event into a bounded queue.
struct OwnedProcessor {
    // Only the ETW thread uses this; the Mutex (never contended) just makes the processor `Sync`.
    tx: Mutex<mpsc::Sender<EventRecordOwned>>,
}

impl EventProcessor for OwnedProcessor {
    unsafe fn on_event_raw(&self, evt: *mut EVENT_RECORD) -> Result<(), crate::Error> {
        let evt = EventRecordOwned::from_raw(unsafe { &*evt });
        let mut tx = self.tx.lock().unwrap_or_else(PoisonError::into_inner);
        // Blocks while the queue is full. Fails once the waiter has been dropped.
        match tx.try_send(evt) {
            Ok(()) => {}
            Err(e) if e.is_full() => futures::executor::block_on(tx.send(e.into_inner()))
                .map_err(|_| E_CANCELLED.into_error())?,
            Err(_) => return Err(E_CANCELLED.into_error()),
        }
        Ok(())
    }
}

// Replies to the blocked ETW callback when dropped, so it is released even if the
// event handler panics (with `E_CANCELLED`, which stops the trace).
struct Reply<'a> {
    tx: &'a SyncSender<windows_sys::core::HRESULT>,
    hr: windows_sys::core::HRESULT,
}

impl Drop for Reply<'_> {
    fn drop(&mut self) {
        // Never full: the callback takes each reply before delivering the next event.
        let _ = self.tx.try_send(self.hr);
    }
}

// Polls `fut` with exponential backoff (at most ~63 spin hints in total) and gives up if it
// is still pending, mirroring the spin phase of crossbeam's `Backoff`.
fn spin_poll<F: Future + Unpin>(fut: &mut F) -> Option<F::Output> {
    const SPIN_LIMIT: u32 = 6;
    for step in 0..=SPIN_LIMIT {
        if let Some(output) = (&mut *fut).now_or_never() {
            return Some(output);
        }
        for _ in 0..(1u32 << step) {
            std::hint::spin_loop();
        }
    }
    None
}

// Awaits `fut`, returning `None` once `timeout` has passed. If it times out, `fut` is dropped;
// a pending `next()` is dropped without taking a message.
pub(crate) async fn with_timeout<F: Future + Unpin>(
    mut fut: F,
    timeout: Option<Duration>,
) -> Option<F::Output> {
    // The ETW thread usually delivers the next event within about a microsecond, so first
    // spin briefly, like crossbeam's blocking receive. Going pending instead costs a task
    // wakeup (with `block_on`, a thread park and unpark), and arming the timeout wakes
    // futures-timer's thread (again when it is dropped); both cost more than the handoff.
    if let Some(output) = spin_poll(&mut fut) {
        return Some(output);
    }
    let Some(timeout) = timeout else {
        return Some(fut.await);
    };
    match future::select(fut, Delay::new(timeout)).await {
        Either::Left((output, _)) => Some(output),
        Either::Right(_) => None,
    }
}

// The time left until `deadline`, or `None` (no limit) without one.
fn remaining(deadline: Option<Instant>) -> Option<Duration> {
    deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()))
}

// A deadline `timeout` from now; `None` without a timeout, or if it overflows `Instant`.
fn deadline(timeout: Option<Duration>) -> Option<Instant> {
    timeout.and_then(|timeout| Instant::now().checked_add(timeout))
}

// A receive that `with_timeout` gave up on (`None`), or one that found the channel closed
// (`Some(None)`), as what an owned waiter returns.
fn owned_event(received: Option<Option<EventRecordOwned>>) -> Option<EventOrTimeout<'static>> {
    match received {
        Some(Some(evt)) => Some(EventOrTimeout::Event(EventRecord::Owned(evt))),
        None => Some(EventOrTimeout::Timeout),
        // ProcessTrace has completed and every queued event has been received.
        Some(None) => None,
    }
}

// `EtwEventWaiter<Futures>`: lends each event straight from the ETW buffer to an async (or blocking)
// wait; the ETW callback blocks until the closure returns.
pub struct LentWaiter {
    rx: mpsc::UnboundedReceiver<AtomicPtr<EVENT_RECORD>>,
    reply_tx: SyncSender<windows_sys::core::HRESULT>,
    // Set once a closure returns `false` (or panics) on a timeout; any later event is rejected.
    cancelled: bool,
    // Until the trace starts.
    processor: Option<LentProcessor>,
}

impl Default for LentWaiter {
    fn default() -> Self {
        let (tx, rx) = crate::profiling::channel!(
            futures::channel::mpsc::unbounded::<AtomicPtr<EVENT_RECORD>>(),
            label = "futures_events"
        );
        let (reply_tx, reply_rx) = crate::profiling::channel!(
            std::sync::mpsc::sync_channel::<windows_sys::core::HRESULT>(1),
            capacity = 1,
            label = "futures_event_released"
        );
        LentWaiter {
            rx,
            reply_tx,
            cancelled: false,
            processor: Some(LentProcessor {
                tx,
                reply_rx: Mutex::new(reply_rx),
            }),
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

impl WaitAsync for LentWaiter {
    #[cfg_attr(
        feature = "hotpath",
        hotpath::measure(impl_type = "EtwEventWaiter<Futures>", future = true)
    )]
    async fn wait_async<F>(&mut self, timeout: Option<Duration>, f: F) -> Result<(), crate::Error>
    where
        F: FnOnce(EventOrTimeout<'_>) -> bool,
    {
        // Only a cancelled waiter rejects events and waits again, so only it needs a deadline.
        let deadline = if self.cancelled {
            deadline(timeout)
        } else {
            None
        };
        let ptr = loop {
            let wait = if self.cancelled {
                remaining(deadline)
            } else {
                timeout
            };
            match with_timeout(self.rx.next(), wait).await {
                // Once cancelled, reject events: ETW may already have been delivering one.
                Some(Some(_)) if self.cancelled => {
                    let _ = self.reply_tx.try_send(E_CANCELLED);
                }
                Some(Some(ptr)) => break ptr,
                Some(None) => return Err(E_TRACE_FINISHED.into_error()),
                None => {
                    call_or_cancel(self, f, EventOrTimeout::Timeout, |waiter| {
                        waiter.cancelled = true;
                    });
                    return Ok(());
                }
            }
        };

        // Created before `f` runs so the callback is released even if `f` panics.
        let mut reply = Reply {
            tx: &self.reply_tx,
            hr: E_CANCELLED,
        };

        // SAFETY: the ETW callback that owns this event is blocked until `reply` is
        // dropped, which can only happen after `f` has returned (or unwound).
        let evt: &EVENT_RECORD = unsafe { &*ptr.load(Ordering::Acquire) };

        if f(EventOrTimeout::Event(EventRecord::from_ref(evt))) {
            reply.hr = S_OK;
        }
        Ok(())
    }
}

impl Wait for LentWaiter {
    fn wait<F>(&mut self, timeout: Option<Duration>, f: F) -> Result<(), crate::Error>
    where
        F: FnOnce(EventOrTimeout<'_>) -> bool,
    {
        futures::executor::block_on(self.wait_async(timeout, f))
    }
}

// `EtwEventWaiterOwned<Futures>`: the ETW callback copies each event into a bounded futures channel, and
// only blocks while it's full. Returning `false` from a closure discards the queue.
pub struct OwnedWaiter {
    rx: mpsc::Receiver<EventRecordOwned>,
    // Set once a closure returns `false` (or panics); the queue is then discarded.
    cancelled: bool,
    // Until the trace starts.
    processor: Option<OwnedProcessor>,
}

impl OwnedWaiter {
    /// Creates a waiter that queues up to `event_buffer_count` events. The queue always has room
    /// for at least one event (futures channels reserve a slot for each sender), so 0 behaves
    /// like 1.
    pub fn new(event_buffer_count: usize) -> Self {
        // A futures channel holds `buffer` events plus one per sender.
        let buffer = event_buffer_count.saturating_sub(1);
        let (tx, rx) = crate::profiling::channel!(
            futures::channel::mpsc::channel::<EventRecordOwned>(buffer),
            capacity = buffer,
            label = "futures_owned_events"
        );
        OwnedWaiter {
            rx,
            cancelled: false,
            processor: Some(OwnedProcessor { tx: Mutex::new(tx) }),
        }
    }

    // Discards the events already queued (releasing a processor blocked on a full queue), and
    // every later one. `EventTrace` has already stopped the trace (see `cancel_on_false`).
    fn cancel(&mut self) {
        self.cancelled = true;
        while self.rx.try_recv().is_ok() {}
    }

    // Waits for the next queued event (`Some(Some(_))`), until ProcessTrace has completed
    // (`Some(None)`) or `timeout` passes (`None`). Once cancelled, discards events instead.
    async fn receive(&mut self, timeout: Option<Duration>) -> Option<Option<EventRecordOwned>> {
        if !self.cancelled {
            return with_timeout(self.rx.next(), timeout).await;
        }
        let rx = &mut self.rx;
        let drained = std::pin::pin!(async move { while rx.next().await.is_some() {} });
        with_timeout(drained, timeout).await.map(|()| None)
    }
}

impl Connect for OwnedWaiter {
    fn take_processor(&mut self) -> impl EventProcessor + 'static + use<> {
        self.processor
            .take()
            .expect("the trace was already started")
    }
}

impl NextOwnedAsync for OwnedWaiter {
    #[cfg_attr(
        feature = "hotpath",
        hotpath::measure(impl_type = "EtwEventWaiterOwned<Futures>", future = true)
    )]
    async fn next_owned_async(
        &mut self,
        timeout: Option<Duration>,
    ) -> Option<EventOrTimeout<'static>> {
        owned_event(self.receive(timeout).await)
    }
}

impl NextOwned for OwnedWaiter {
    fn next_owned(&mut self, timeout: Option<Duration>) -> Option<EventOrTimeout<'static>> {
        futures::executor::block_on(self.next_owned_async(timeout))
    }
}

impl WaitAsync for OwnedWaiter {
    #[cfg_attr(
        feature = "hotpath",
        hotpath::measure(impl_type = "EtwEventWaiterOwned<Futures>", future = true)
    )]
    async fn wait_async<F>(&mut self, timeout: Option<Duration>, f: F) -> Result<(), crate::Error>
    where
        F: FnOnce(EventOrTimeout<'_>) -> bool,
    {
        let Some(evt) = owned_event(self.receive(timeout).await) else {
            return Err(E_TRACE_FINISHED.into_error());
        };

        call_or_cancel(self, f, evt, Self::cancel);

        Ok(())
    }
}

impl Wait for OwnedWaiter {
    fn wait<F>(&mut self, timeout: Option<Duration>, f: F) -> Result<(), crate::Error>
    where
        F: FnOnce(EventOrTimeout<'_>) -> bool,
    {
        futures::executor::block_on(self.wait_async(timeout, f))
    }
}

#[cfg(test)]
mod tests {
    crate::consumer::shared_tests::lent_waiter_tests!(crate::consumer::Futures, "Futures");
    crate::consumer::shared_tests::owned_waiter_tests!(crate::consumer::Futures, "Futures");
    crate::consumer::shared_tests::async_lent_waiter_tests!(crate::consumer::Futures, "Futures");
    crate::consumer::shared_tests::async_owned_waiter_tests!(crate::consumer::Futures, "Futures");

    // Compile-time check: the async waits and streams are `Send`, so they work on
    // multi-threaded executors.
    #[allow(dead_code)]
    fn async_is_send(
        waiter: &mut crate::consumer::EtwEventWaiter<crate::consumer::Futures>,
        owned: crate::consumer::EtwEventWaiterOwned<crate::consumer::Futures>,
    ) -> (impl Send + '_, impl Send) {
        (
            crate::consumer::EventWaiterAsync::wait_for_single_event_async(waiter, |_| true),
            crate::consumer::IntoEventStream::into_stream(owned),
        )
    }

    // Compile-time check: a trace and its stream are `Send`, too.
    #[allow(dead_code)]
    fn trace_is_send(
        trace: crate::consumer::EventTrace<
            crate::consumer::EtwEventWaiterOwned<crate::consumer::Futures>,
        >,
    ) -> impl Send {
        crate::consumer::IntoEventStream::into_stream(trace)
    }
}
