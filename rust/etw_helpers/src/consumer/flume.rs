use std::sync::atomic::{AtomicPtr, Ordering};
use std::time::{Duration, Instant};

use flume::{RecvTimeoutError, TryRecvError};
// Instrumented by hotpath when the `hotpath` feature is enabled; plain flume types otherwise.
use crate::profiling::wrap::flume::{Receiver, Sender};
use windows_sys::Win32::System::Diagnostics::Etw::EVENT_RECORD;

use crate::consumer::event_record::{EventRecord, EventRecordOwned};
use crate::consumer::event_waiter::sealed::{Connect, Next, NextOwned, Wait};
#[cfg(feature = "flume-async")]
use crate::consumer::event_waiter::sealed::{NextOwnedAsync, WaitAsync};
use crate::consumer::event_waiter::{EventOrTimeout, call_or_cancel};
use crate::consumer::processtrace::*;
use crate::error::*;

#[cfg(feature = "flume-async")]
mod stream;
#[cfg(feature = "flume-async")]
pub use stream::LentStream;

// Processor of an `LentWaiter` (or flume `LentStream`): lends each event and blocks
// until it is released.
pub(crate) struct LentProcessor {
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
// sender is dropped.
fn recv_timeout<T>(rx: &Receiver<T>, timeout: Option<Duration>) -> Result<T, RecvTimeoutError> {
    // flume panics on a deadline that overflows `Instant`; such a timeout never expires anyway.
    match timeout.filter(|&timeout| Instant::now().checked_add(timeout).is_some()) {
        None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        Some(timeout) => rx.recv_timeout(timeout),
    }
}

// Tries to receive for a moment (see `lock::spin`) without blocking: the other side of a lent
// event's handoff usually responds within a microsecond, and blocking costs a wake on each side.
// `None` if nothing arrived.
fn try_recv_spin<T>(rx: &Receiver<T>) -> Option<Result<T, RecvTimeoutError>> {
    crate::consumer::lock::spin(|| match rx.try_recv() {
        Ok(msg) => Some(Ok(msg)),
        Err(TryRecvError::Empty) => None,
        Err(TryRecvError::Disconnected) => Some(Err(RecvTimeoutError::Disconnected)),
    })
}

// `recv_timeout`, after spinning briefly with `try_recv_spin`.
fn recv_spin<T>(rx: &Receiver<T>, timeout: Option<Duration>) -> Result<T, RecvTimeoutError> {
    try_recv_spin(rx).unwrap_or_else(|| recv_timeout(rx, timeout))
}

// The time left until `deadline`, or `None` (no limit) without one.
fn remaining(deadline: Option<Instant>) -> Option<Duration> {
    deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()))
}

// A deadline `timeout` from now; `None` without a timeout, or if it overflows `Instant`.
fn deadline(timeout: Option<Duration>) -> Option<Instant> {
    timeout.and_then(|timeout| Instant::now().checked_add(timeout))
}

// Async version of `recv_timeout`. Dropping the future before it completes is safe: flume leaves
// a message that hasn't been received in the channel.
#[cfg(feature = "flume-async")]
async fn recv_async_timeout<T>(
    rx: &Receiver<T>,
    timeout: Option<Duration>,
) -> Result<T, RecvTimeoutError> {
    match with_timeout(rx.recv_async(), timeout).await {
        Some(received) => received.map_err(|_| RecvTimeoutError::Disconnected),
        None => Err(RecvTimeoutError::Timeout),
    }
}

// Returns `None` if `fut` doesn't complete within `timeout`. The timer is only armed if `fut`
// isn't ready when first polled, since creating it wakes the shared timer thread.
#[cfg(feature = "flume-async")]
async fn with_timeout<F: std::future::Future>(
    fut: F,
    timeout: Option<Duration>,
) -> Option<F::Output> {
    use std::future::Future;
    use std::task::Poll;

    let mut fut = std::pin::pin!(fut);
    // A deadline that overflows `Instant` never expires.
    let Some(deadline) = timeout.and_then(|timeout| Instant::now().checked_add(timeout)) else {
        return Some(fut.await);
    };
    let mut delay = None;
    std::future::poll_fn(|cx| {
        if let Poll::Ready(output) = fut.as_mut().poll(cx) {
            return Poll::Ready(Some(output));
        }
        let delay = delay.get_or_insert_with(|| {
            futures_timer::Delay::new(deadline.saturating_duration_since(Instant::now()))
        });
        match std::pin::Pin::new(delay).poll(cx) {
            Poll::Ready(()) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    })
    .await
}

// The receiving side of a `LentProcessor`, shared by `LentWaiter` and the flume
// `LentStream`.
pub(crate) struct LentEvents {
    rx: Receiver<AtomicPtr<EVENT_RECORD>>,
    callback_tx: Sender<windows_sys::core::HRESULT>,
    // True while the ETW callback is blocked waiting for us to release an event returned by `next()`
    lent: bool,
    // Set once a closure returns `false` (or panics) on a timeout; any later event is rejected.
    cancelled: bool,
}

impl LentEvents {
    // Creates the channels connecting a lending processor to the receiving side.
    pub(crate) fn new() -> (Self, LentProcessor) {
        let (tx, rx) = crate::profiling::channel!(
            flume::bounded::<AtomicPtr<EVENT_RECORD>>(0),
            label = "flume_events"
        );
        let (callback_tx, callback_rx) = crate::profiling::channel!(
            flume::bounded::<windows_sys::core::HRESULT>(1),
            label = "flume_event_released"
        );
        let events = LentEvents {
            rx,
            callback_tx,
            lent: false,
            cancelled: false,
        };
        (events, LentProcessor { tx, callback_rx })
    }

    // Releases the previous event, then waits for the next one, until ProcessTrace has completed
    // or `timeout` passes. Once cancelled, rejects events instead: ETW may already have been
    // delivering one.
    fn receive(
        &mut self,
        timeout: Option<Duration>,
    ) -> Result<AtomicPtr<EVENT_RECORD>, RecvTimeoutError> {
        // Requiring `&mut self` guarantees any previously returned event is no longer referenced.
        self.release_current();
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

    // Async version of `receive`. Dropping the future before it completes is safe: flume leaves
    // an event that hasn't been received with the blocked sender.
    #[cfg(feature = "flume-async")]
    async fn receive_async(
        &mut self,
        timeout: Option<Duration>,
    ) -> Result<AtomicPtr<EVENT_RECORD>, RecvTimeoutError> {
        self.release_current();
        if !self.cancelled {
            if let Some(received) = try_recv_spin(&self.rx) {
                return received;
            }
            return recv_async_timeout(&self.rx, timeout).await;
        }
        let deadline = deadline(timeout);
        loop {
            let ptr = recv_async_timeout(&self.rx, remaining(deadline)).await?;
            if !self.cancelled {
                return Ok(ptr);
            }
            let _ = self.callback_tx.send(E_CANCELLED);
        }
    }

    fn next_timeout(&mut self, timeout: Option<Duration>) -> Option<EventOrTimeout<'_>> {
        let received = self.receive(timeout);
        self.lend(received)
    }

    #[cfg(feature = "flume-async")]
    pub(crate) async fn next_timeout_async(
        &mut self,
        timeout: Option<Duration>,
    ) -> Option<EventOrTimeout<'_>> {
        let received = self.receive_async(timeout).await;
        self.lend(received)
    }

    fn lend(
        &mut self,
        received: Result<AtomicPtr<EVENT_RECORD>, RecvTimeoutError>,
    ) -> Option<EventOrTimeout<'_>> {
        let ptr = match received {
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

    fn release_current(&mut self) {
        if self.lent {
            self.lent = false;
            // Never blocks: the callback takes each reply before delivering the next event.
            let _ = self.callback_tx.send(S_OK);
        }
    }

    fn wait<F>(&mut self, timeout: Option<Duration>, f: F) -> Result<(), crate::Error>
    where
        F: FnOnce(EventOrTimeout) -> bool,
    {
        let received = self.receive(timeout);
        self.handle_event(received, f)
    }

    #[cfg(feature = "flume-async")]
    async fn wait_async<F>(&mut self, timeout: Option<Duration>, f: F) -> Result<(), crate::Error>
    where
        F: FnOnce(EventOrTimeout) -> bool,
    {
        let received = self.receive_async(timeout).await;
        self.handle_event(received, f)
    }

    fn handle_event<F>(
        &mut self,
        received: Result<AtomicPtr<EVENT_RECORD>, RecvTimeoutError>,
        f: F,
    ) -> Result<(), crate::Error>
    where
        F: FnOnce(EventOrTimeout) -> bool,
    {
        let ptr = match received {
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

// `EtwEventWaiter<Flume>`: lends each event straight from the ETW buffer; the ETW callback blocks until
// it is released.
pub struct LentWaiter {
    events: LentEvents,
    // Until the trace starts.
    processor: Option<LentProcessor>,
}

impl Default for LentWaiter {
    fn default() -> Self {
        let (events, processor) = LentEvents::new();
        LentWaiter {
            events,
            processor: Some(processor),
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
        hotpath::measure(impl_type = "EtwEventWaiter<Flume>")
    )]
    fn next_timeout(&mut self, timeout: Option<Duration>) -> Option<EventOrTimeout<'_>> {
        self.events.next_timeout(timeout)
    }
}

impl Wait for LentWaiter {
    #[cfg_attr(
        feature = "hotpath",
        hotpath::measure(impl_type = "EtwEventWaiter<Flume>")
    )]
    fn wait<F>(&mut self, timeout: Option<Duration>, f: F) -> Result<(), crate::Error>
    where
        F: FnOnce(EventOrTimeout<'_>) -> bool,
    {
        self.events.wait(timeout, f)
    }
}

#[cfg(feature = "flume-async")]
impl WaitAsync for LentWaiter {
    #[cfg_attr(
        feature = "hotpath",
        hotpath::measure(impl_type = "EtwEventWaiter<Flume>", future = true)
    )]
    async fn wait_async<F>(&mut self, timeout: Option<Duration>, f: F) -> Result<(), crate::Error>
    where
        F: FnOnce(EventOrTimeout<'_>) -> bool,
    {
        self.events.wait_async(timeout, f).await
    }
}

// `EtwEventWaiterOwned<Flume>`: the ETW callback copies each event into a bounded queue, and only blocks
// while it's full. Returning `false` from a closure discards the queue.
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
            flume::bounded::<EventRecordOwned>(event_buffer_count),
            label = "flume_owned_events"
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

    // Waits for the next queued event, until ProcessTrace has completed or `timeout` passes. Once
    // cancelled, discards events instead.
    fn receive(&self, timeout: Option<Duration>) -> Result<EventRecordOwned, RecvTimeoutError> {
        if !self.cancelled {
            return recv_timeout(&self.rx, timeout);
        }
        let deadline = deadline(timeout);
        loop {
            recv_timeout(&self.rx, remaining(deadline))?;
        }
    }

    // Async version of `receive`.
    #[cfg(feature = "flume-async")]
    async fn receive_async(
        &self,
        timeout: Option<Duration>,
    ) -> Result<EventRecordOwned, RecvTimeoutError> {
        if !self.cancelled {
            return recv_async_timeout(&self.rx, timeout).await;
        }
        match with_timeout(
            async { while self.rx.recv_async().await.is_ok() {} },
            timeout,
        )
        .await
        {
            Some(()) => Err(RecvTimeoutError::Disconnected),
            None => Err(RecvTimeoutError::Timeout),
        }
    }

    fn handle_event<F>(
        &mut self,
        received: Result<EventRecordOwned, RecvTimeoutError>,
        f: F,
    ) -> Result<(), crate::Error>
    where
        F: FnOnce(EventOrTimeout) -> bool,
    {
        let Some(evt) = owned_event(received) else {
            return Err(E_TRACE_FINISHED.into_error());
        };
        call_or_cancel(self, f, evt, Self::cancel);
        Ok(())
    }
}

// What an owned waiter returns for a receive. `None` once ProcessTrace has completed and every
// queued event has been received.
fn owned_event(
    received: Result<EventRecordOwned, RecvTimeoutError>,
) -> Option<EventOrTimeout<'static>> {
    match received {
        Ok(evt) => Some(EventOrTimeout::Event(EventRecord::Owned(evt))),
        Err(RecvTimeoutError::Timeout) => Some(EventOrTimeout::Timeout),
        Err(RecvTimeoutError::Disconnected) => None,
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
        hotpath::measure(impl_type = "EtwEventWaiterOwned<Flume>")
    )]
    fn next_owned(&mut self, timeout: Option<Duration>) -> Option<EventOrTimeout<'static>> {
        owned_event(self.receive(timeout))
    }
}

impl Wait for OwnedWaiter {
    #[cfg_attr(
        feature = "hotpath",
        hotpath::measure(impl_type = "EtwEventWaiterOwned<Flume>")
    )]
    fn wait<F>(&mut self, timeout: Option<Duration>, f: F) -> Result<(), crate::Error>
    where
        F: FnOnce(EventOrTimeout<'_>) -> bool,
    {
        let received = self.receive(timeout);
        self.handle_event(received, f)
    }
}

#[cfg(feature = "flume-async")]
impl NextOwnedAsync for OwnedWaiter {
    #[cfg_attr(
        feature = "hotpath",
        hotpath::measure(impl_type = "EtwEventWaiterOwned<Flume>", future = true)
    )]
    async fn next_owned_async(
        &mut self,
        timeout: Option<Duration>,
    ) -> Option<EventOrTimeout<'static>> {
        owned_event(self.receive_async(timeout).await)
    }
}

#[cfg(feature = "flume-async")]
impl WaitAsync for OwnedWaiter {
    #[cfg_attr(
        feature = "hotpath",
        hotpath::measure(impl_type = "EtwEventWaiterOwned<Flume>", future = true)
    )]
    async fn wait_async<F>(&mut self, timeout: Option<Duration>, f: F) -> Result<(), crate::Error>
    where
        F: FnOnce(EventOrTimeout<'_>) -> bool,
    {
        let received = self.receive_async(timeout).await;
        self.handle_event(received, f)
    }
}

#[cfg(test)]
mod tests {
    crate::consumer::shared_tests::lent_waiter_tests!(
        crate::consumer::Flume,
        "Flume",
        iterate;
        timed: timed_next
    );
    crate::consumer::shared_tests::owned_waiter_tests!(crate::consumer::Flume, "Flume");
    #[cfg(feature = "flume-async")]
    crate::consumer::shared_tests::async_lent_waiter_tests!(crate::consumer::Flume, "Flume");
    #[cfg(feature = "flume-async")]
    crate::consumer::shared_tests::async_owned_waiter_tests!(crate::consumer::Flume, "Flume");

    // Compile-time check: the async waits and streams are `Send`, so they work on
    // multi-threaded executors.
    #[cfg(feature = "flume-async")]
    #[allow(dead_code)]
    fn async_is_send(
        waiter: &mut crate::consumer::EtwEventWaiter<crate::consumer::Flume>,
        owned: crate::consumer::EtwEventWaiterOwned<crate::consumer::Flume>,
    ) -> (impl Send + '_, impl Send) {
        (
            crate::consumer::EventWaiterAsync::wait_for_single_event_async(waiter, |_| true),
            crate::consumer::IntoEventStream::into_stream(owned),
        )
    }
}
