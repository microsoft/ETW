use core::future::Future;
use core::time::Duration;

use crate::consumer::event_record::EventRecord;
use crate::consumer::event_trace::EventTrace;
use crate::consumer::processtrace::{EventProcessor, ProcessTraceHandle};

/// Traits the consumers in this crate implement, from which the public consumer traits are
/// derived. Public but unnameable outside the crate, so the public traits that build on them
/// are sealed.
pub(crate) mod sealed {
    use core::future::Future;
    use core::time::Duration;

    use crate::consumer::event_waiter::EventOrTimeout;
    use crate::consumer::processtrace::EventProcessor;

    /// Connects a consumer to a trace.
    pub trait Connect {
        /// Takes the processor that delivers events to `self`. Called once, when
        /// [`EventConsumer::from_file`](crate::consumer::EventConsumer::from_file) or
        /// [`from_session`](crate::consumer::EventConsumer::from_session) starts the trace.
        fn take_processor(&mut self) -> impl EventProcessor + 'static + use<Self>;
    }

    /// Waits for the next event and passes it to `f`. If `timeout` passes first, passes
    /// [`EventOrTimeout::Timeout`] to `f` instead and returns `Ok`; without a timeout, `f` only
    /// ever receives an event. Either way, returning `false` from `f` cancels the waiter (and
    /// `EventTrace` stops the trace): later waits discard events until ProcessTrace completes.
    /// Implementing it implements [`EventWaiter`](crate::consumer::EventWaiter), and
    /// [`WithTimeout<Self>`](crate::consumer::WithTimeout) a timed one.
    ///
    /// Once ProcessTrace has completed (and, with owned events, every queued event has been
    /// returned), fails with `E_TRACE_FINISHED`, which `EventTrace` replaces with ProcessTrace's
    /// result.
    pub trait Wait {
        fn wait<F>(&mut self, timeout: Option<Duration>, f: F) -> Result<(), crate::Error>
        where
            F: FnOnce(EventOrTimeout<'_>) -> bool;
    }

    /// Async version of [`Wait`]. Implementing it implements
    /// [`EventWaiterAsync`](crate::consumer::EventWaiterAsync).
    pub trait WaitAsync {
        fn wait_async<F>(
            &mut self,
            timeout: Option<Duration>,
            f: F,
        ) -> impl Future<Output = Result<(), crate::Error>>
        where
            F: FnOnce(EventOrTimeout<'_>) -> bool;
    }

    /// Returns the next event, [`EventOrTimeout::Timeout`] if `timeout` passes first, or `None`
    /// once there are no more events. Implementing it implements
    /// [`EventIterator`](crate::consumer::EventIterator).
    pub trait Next {
        fn next_timeout(&mut self, timeout: Option<Duration>) -> Option<EventOrTimeout<'_>>;
    }

    /// Async version of [`Next`]. Implementing it implements
    /// [`EventIteratorAsync`](crate::consumer::EventIteratorAsync).
    pub trait NextAsync {
        fn next_timeout_async(
            &mut self,
            timeout: Option<Duration>,
        ) -> impl Future<Output = Option<EventOrTimeout<'_>>>;
    }

    /// [`Next`] for owned events: returns `None` once ProcessTrace has completed and every queued
    /// event has been returned (or the trace was cancelled). Implementing it implements [`Next`],
    /// and lets the consumer implement [`IntoIterator`] with
    /// [`OwnedIter`](crate::consumer::OwnedIter).
    pub trait NextOwned {
        fn next_owned(&mut self, timeout: Option<Duration>) -> Option<EventOrTimeout<'static>>;
    }

    /// Async version of [`NextOwned`]. Implementing it implements [`NextAsync`] and
    /// `IntoEventStream`.
    pub trait NextOwnedAsync {
        fn next_owned_async(
            &mut self,
            timeout: Option<Duration>,
        ) -> impl Future<Output = Option<EventOrTimeout<'static>>>;
    }

    impl<T: NextOwned> Next for T {
        fn next_timeout(&mut self, timeout: Option<Duration>) -> Option<EventOrTimeout<'_>> {
            self.next_owned(timeout)
        }
    }

    impl<T: NextOwnedAsync> NextAsync for T {
        // Awaiting lets the `'static` event shorten to the borrow of `self`.
        async fn next_timeout_async(
            &mut self,
            timeout: Option<Duration>,
        ) -> Option<EventOrTimeout<'_>> {
            self.next_owned_async(timeout).await
        }
    }
}

/// An event consumer: one of this crate's waiters or streams, which receive events from a trace.
///
/// Start consuming a trace with [`from_file`](Self::from_file) or
/// [`from_session`](Self::from_session), which return an [`EventTrace`] that runs the trace on a
/// background thread and has the same consumer APIs as the consumer:
///
/// ```no_run
/// use etw_helpers::consumer::{EtwEventWaiter, EventConsumer};
///
/// let waiter = EventConsumer::from_file("sample.etl", EtwEventWaiter::new_owned_events(1024))?;
/// for evt in waiter {
///     println!("event {}", evt.get_event_header().EventDescriptor.Id);
/// }
/// # Ok::<(), etw_helpers::Error>(())
/// ```
///
/// This trait is sealed. To handle raw events yourself, implement
/// [`EventProcessor`](crate::consumer::EventProcessor) and use [`ProcessTraceHandle`] instead.
pub trait EventConsumer: sealed::Connect + Sized {
    /// Opens the log file `file_name` and starts reading it on a background thread, delivering
    /// its events to `consumer`.
    fn from_file(file_name: &str, mut consumer: Self) -> Result<EventTrace<Self>, crate::Error> {
        let thread =
            ProcessTraceHandle::from_file(file_name, consumer.take_processor())?.process_trace()?;
        Ok(EventTrace::new(consumer, thread))
    }

    /// Opens the running real-time session `session_name` and starts processing it on a
    /// background thread, delivering its events to `consumer`.
    fn from_session(
        session_name: &str,
        mut consumer: Self,
    ) -> Result<EventTrace<Self>, crate::Error> {
        let thread = ProcessTraceHandle::from_session(session_name, consumer.take_processor())?
            .process_trace()?;
        Ok(EventTrace::new(consumer, thread))
    }
}

impl<T: sealed::Connect> EventConsumer for T {}

// Calls a `wait_for_single_event*` closure, then `cancel(waiter)` if it returned `false` or
// unwound, so a panicking closure cancels the waiter just like one that returns `false`.
pub(crate) fn call_or_cancel<W, T>(
    waiter: &mut W,
    f: impl FnOnce(T) -> bool,
    arg: T,
    cancel: impl FnOnce(&mut W),
) {
    struct Guard<'a, W, C: FnOnce(&mut W)> {
        waiter: &'a mut W,
        cancel: Option<C>,
    }

    impl<W, C: FnOnce(&mut W)> Drop for Guard<'_, W, C> {
        fn drop(&mut self) {
            if let Some(cancel) = self.cancel.take() {
                cancel(self.waiter);
            }
        }
    }

    let mut guard = Guard {
        waiter,
        cancel: Some(cancel),
    };
    if f(arg) {
        guard.cancel = None;
    }
}
/// What a timed wait's closure receives, and what a consumer created with `new_with_timeout`
/// returns: the next event, or [`Timeout`](Self::Timeout) if none arrived in time.
pub enum EventOrTimeout<'a> {
    /// The next event.
    Event(EventRecord<'a>),
    /// No event arrived within the timeout. The trace is still running: wait again for later
    /// events.
    Timeout,
}

// Adapts an untimed wait's closure to `sealed::Wait`, which only times out with a timeout.
fn untimed<F>(f: F) -> impl FnOnce(EventOrTimeout<'_>) -> bool
where
    F: FnOnce(EventRecord<'_>) -> bool,
{
    move |evt| match evt {
        EventOrTimeout::Event(evt) => f(evt),
        EventOrTimeout::Timeout => unreachable!("waited without a timeout"),
    }
}

// The event from an untimed `sealed::Next`, which only times out with a timeout.
fn untimed_event(evt: EventOrTimeout<'_>) -> EventRecord<'_> {
    match evt {
        EventOrTimeout::Event(evt) => evt,
        EventOrTimeout::Timeout => unreachable!("waited without a timeout"),
    }
}

/// Waits for events one at a time, passing each to a closure.
///
/// The closure receives an [`EventRecord<'_>`] (an [`EventOrTimeout<'_>`] for consumers created
/// with `new_with_timeout`) that can't escape it; the ETW callback thread stays blocked until
/// the closure returns (or unwinds), so the event remains valid. Use [`EventRecord::into_owned`]
/// to keep an event.
pub trait EventWaiter {
    /// What [`wait_for_single_event`](Self::wait_for_single_event)'s closure receives:
    /// [`EventRecord`], or [`EventOrTimeout`] for consumers created with `new_with_timeout`.
    type Event<'a>;

    /// Waits for the next event and passes it to `f`. For consumers created with
    /// `new_with_timeout`, passes [`EventOrTimeout::Timeout`] instead if no event arrives within
    /// their timeout. Returning `false` from `f` stops the trace.
    ///
    /// Returns an error once ProcessTrace has completed.
    fn wait_for_single_event<F>(&mut self, f: F) -> Result<(), crate::Error>
    where
        F: FnOnce(Self::Event<'_>) -> bool;

    /// Like [`wait_for_single_event`](Self::wait_for_single_event), but if no event arrives
    /// within `timeout` (which overrides any timeout given to `new_with_timeout`), passes
    /// [`EventOrTimeout::Timeout`] to `f` and returns `Ok`, so the caller can do other work and
    /// then wait again. Returning `false` from `f` stops the trace, whichever it received.
    ///
    /// Returns an error once ProcessTrace has completed.
    ///
    /// ```no_run
    /// use std::time::Duration;
    /// use etw_helpers::consumer::{EtwEventWaiter, EventConsumer, EventOrTimeout, EventWaiter};
    ///
    /// let mut waiter = EventConsumer::from_session("my-session", EtwEventWaiter::new_owned_events(1024))?;
    /// loop {
    ///     let result = waiter.wait_for_single_event_timeout(Duration::from_secs(1), |evt| {
    ///         match evt {
    ///             EventOrTimeout::Event(evt) => {
    ///                 println!("event {}", evt.get_event_header().EventDescriptor.Id);
    ///             }
    ///             EventOrTimeout::Timeout => println!("no events in the last second"),
    ///         }
    ///         true // keep going
    ///     });
    ///     if result.is_err() {
    ///         break; // ProcessTrace has completed
    ///     }
    /// }
    /// # Ok::<(), etw_helpers::Error>(())
    /// ```
    fn wait_for_single_event_timeout<F>(
        &mut self,
        timeout: Duration,
        f: F,
    ) -> Result<(), crate::Error>
    where
        F: FnOnce(EventOrTimeout<'_>) -> bool;
}

/// Async version of [`EventWaiter`]: waits for events one at a time without blocking the
/// executor, passing each to a closure. Timers don't depend on a particular executor.
///
/// Dropping a returned future before it completes doesn't lose an event; the next call
/// receives it instead.
pub trait EventWaiterAsync {
    /// What [`wait_for_single_event_async`](Self::wait_for_single_event_async)'s closure
    /// receives: [`EventRecord`], or [`EventOrTimeout`] for consumers created with
    /// `new_with_timeout`.
    type Event<'a>;

    /// Waits for the next event and passes it to `f`. For consumers created with
    /// `new_with_timeout`, passes [`EventOrTimeout::Timeout`] instead if no event arrives within
    /// their timeout. Returning `false` from `f` stops the trace.
    ///
    /// Returns an error once ProcessTrace has completed.
    fn wait_for_single_event_async<F>(
        &mut self,
        f: F,
    ) -> impl Future<Output = Result<(), crate::Error>>
    where
        F: FnOnce(Self::Event<'_>) -> bool;

    /// Like [`wait_for_single_event_async`](Self::wait_for_single_event_async), but if no event
    /// arrives within `timeout` (which overrides any timeout given to `new_with_timeout`), passes
    /// [`EventOrTimeout::Timeout`] to `f` and returns `Ok`, so the caller can do other work and
    /// then wait again. Returning `false` from `f` stops the trace, whichever it received.
    ///
    /// Returns an error once ProcessTrace has completed.
    fn wait_for_single_event_async_timeout<F>(
        &mut self,
        timeout: Duration,
        f: F,
    ) -> impl Future<Output = Result<(), crate::Error>>
    where
        F: FnOnce(EventOrTimeout<'_>) -> bool;
}

/// A lending iterator of events.
///
/// This can't be [`Iterator`]: an iterator's items can't borrow from the iterator, but each
/// event returned by [`next`](Self::next) borrows `self` mutably, and the ETW callback thread
/// stays blocked until that borrow ends. Holding an event across the next call to `next()` is
/// therefore a compile error. Use [`EventRecord::into_owned`] to keep an event.
pub trait EventIterator {
    /// What [`next`](Self::next) returns: [`EventRecord`], or [`EventOrTimeout`] for consumers
    /// created with `new_with_timeout`.
    type Item<'a>
    where
        Self: 'a;

    /// Blocks until the next event arrives (for consumers created with `new_with_timeout`, at
    /// most for their timeout, then returns [`EventOrTimeout::Timeout`]). Returns `None` once
    /// there are no more events.
    ///
    /// The returned event borrows `self`, so it must be dropped before calling `next()` again.
    fn next(&mut self) -> Option<Self::Item<'_>>;
}

/// A lending async stream of events: the async version of [`EventIterator`].
///
/// This is deliberately not a `futures::Stream`, whose items can't borrow from the stream.
pub trait EventIteratorAsync {
    /// What [`next`](Self::next) returns: [`EventRecord`], or [`EventOrTimeout`] for consumers
    /// created with `new_with_timeout`.
    type Item<'a>
    where
        Self: 'a;

    /// Waits for the next event (for consumers created with `new_with_timeout`, at most for
    /// their timeout, then returns [`EventOrTimeout::Timeout`]). Returns `None` once there are no
    /// more events.
    ///
    /// The returned event borrows `self`, so it must be dropped before calling `next()` again.
    fn next(&mut self) -> impl Future<Output = Option<Self::Item<'_>>>;
}

/// Converts a consumer of owned events into a [`futures::Stream`] of its events. With the
/// `futures` or `flume-async` feature.
#[cfg(any(feature = "futures", feature = "flume-async"))]
pub trait IntoEventStream {
    /// The stream's items: `EventRecord<'static>`, or `EventOrTimeout<'static>` for consumers
    /// created with `new_with_timeout`.
    type Item;

    /// Converts `self` into a stream of its events, which ends once ProcessTrace has completed and
    /// every queued event has been returned.
    fn into_stream(self) -> impl futures::Stream<Item = Self::Item>;
}

#[cfg(any(feature = "futures", feature = "flume-async"))]
impl<T: sealed::NextOwnedAsync> IntoEventStream for T {
    type Item = EventRecord<'static>;

    fn into_stream(self) -> impl futures::Stream<Item = EventRecord<'static>> {
        futures::stream::unfold(self, |mut consumer| async move {
            let evt = consumer.next_owned_async(None).await?;
            Some((untimed_event(evt), consumer))
        })
    }
}

/// An [`Iterator`] over the events of a consumer of owned events, returned by its
/// [`into_iter`](IntoIterator::into_iter). Blocks until each event arrives (for consumers
/// created with `new_with_timeout`, at most for their timeout, then yields
/// [`EventOrTimeout::Timeout`]), and ends once ProcessTrace has completed and every queued
/// event has been returned.
pub struct OwnedIter<C>(pub(crate) C);

impl<C: sealed::NextOwned> Iterator for OwnedIter<C> {
    type Item = EventRecord<'static>;

    fn next(&mut self) -> Option<EventRecord<'static>> {
        self.0.next_owned(None).map(untimed_event)
    }
}

impl<T: sealed::Wait> EventWaiter for T {
    type Event<'a> = EventRecord<'a>;

    fn wait_for_single_event<F>(&mut self, f: F) -> Result<(), crate::Error>
    where
        F: FnOnce(EventRecord<'_>) -> bool,
    {
        self.wait(None, untimed(f))
    }

    fn wait_for_single_event_timeout<F>(
        &mut self,
        timeout: Duration,
        f: F,
    ) -> Result<(), crate::Error>
    where
        F: FnOnce(EventOrTimeout<'_>) -> bool,
    {
        self.wait(Some(timeout), f)
    }
}

impl<T: sealed::WaitAsync> EventWaiterAsync for T {
    type Event<'a> = EventRecord<'a>;

    fn wait_for_single_event_async<F>(
        &mut self,
        f: F,
    ) -> impl Future<Output = Result<(), crate::Error>>
    where
        F: FnOnce(EventRecord<'_>) -> bool,
    {
        self.wait_async(None, untimed(f))
    }

    fn wait_for_single_event_async_timeout<F>(
        &mut self,
        timeout: Duration,
        f: F,
    ) -> impl Future<Output = Result<(), crate::Error>>
    where
        F: FnOnce(EventOrTimeout<'_>) -> bool,
    {
        self.wait_async(Some(timeout), f)
    }
}

impl<T: sealed::Next> EventIterator for T {
    type Item<'a>
        = EventRecord<'a>
    where
        T: 'a;

    fn next(&mut self) -> Option<EventRecord<'_>> {
        self.next_timeout(None).map(untimed_event)
    }
}

impl<T: sealed::NextAsync> EventIteratorAsync for T {
    type Item<'a>
        = EventRecord<'a>
    where
        T: 'a;

    async fn next(&mut self) -> Option<EventRecord<'_>> {
        self.next_timeout_async(None).await.map(untimed_event)
    }
}

/// A consumer created with `new_with_timeout`, which gives up waiting for an event after a
/// timeout. Use it like the consumer, except that it returns [`EventOrTimeout`] rather than
/// [`EventRecord`], so a blocking `next()` on a real-time session that receives no events
/// returns [`EventOrTimeout::Timeout`] instead of blocking until the session stops.
///
/// ```no_run
/// use std::time::Duration;
/// use etw_helpers::consumer::{EtwEventWaiter, EventConsumer, EventIterator, EventOrTimeout};
///
/// let consumer = EtwEventWaiter::new_with_timeout(Duration::from_secs(1));
/// let mut waiter = EventConsumer::from_session("my-session", consumer)?;
/// while let Some(evt) = waiter.next() {
///     match evt {
///         EventOrTimeout::Event(evt) => {
///             println!("event {}", evt.get_event_header().EventDescriptor.Id);
///         }
///         EventOrTimeout::Timeout => println!("no events in the last second"),
///     }
/// }
/// # Ok::<(), etw_helpers::Error>(())
/// ```
///
/// Timeouts passed to `wait_for_single_event_timeout` and
/// `wait_for_single_event_async_timeout` override this one.
///
/// Events are lent as before, so holding one across the next call to `next()` is still a
/// compile error:
///
/// ```compile_fail
/// # use etw_helpers::consumer::EventIterator;
/// # fn f(waiter: &mut etw_helpers::consumer::WithTimeout<etw_helpers::consumer::EtwEventWaiter>) {
/// let first = waiter.next();
/// let second = waiter.next(); // error[E0499]: cannot borrow `*waiter` as mutable more than once
/// drop(first);
/// # }
/// ```
pub struct WithTimeout<C> {
    consumer: C,
    timeout: Duration,
}

impl<C> WithTimeout<C> {
    pub(crate) fn new(consumer: C, timeout: Duration) -> Self {
        WithTimeout { consumer, timeout }
    }

    /// How long to wait for each event.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }
}

impl<C: sealed::Connect> sealed::Connect for WithTimeout<C> {
    fn take_processor(&mut self) -> impl EventProcessor + 'static + use<C> {
        self.consumer.take_processor()
    }
}

impl<C: sealed::Wait> EventWaiter for WithTimeout<C> {
    type Event<'a> = EventOrTimeout<'a>;

    fn wait_for_single_event<F>(&mut self, f: F) -> Result<(), crate::Error>
    where
        F: FnOnce(EventOrTimeout<'_>) -> bool,
    {
        self.consumer.wait(Some(self.timeout), f)
    }

    fn wait_for_single_event_timeout<F>(
        &mut self,
        timeout: Duration,
        f: F,
    ) -> Result<(), crate::Error>
    where
        F: FnOnce(EventOrTimeout<'_>) -> bool,
    {
        self.consumer.wait(Some(timeout), f)
    }
}

impl<C: sealed::WaitAsync> EventWaiterAsync for WithTimeout<C> {
    type Event<'a> = EventOrTimeout<'a>;

    fn wait_for_single_event_async<F>(
        &mut self,
        f: F,
    ) -> impl Future<Output = Result<(), crate::Error>>
    where
        F: FnOnce(EventOrTimeout<'_>) -> bool,
    {
        self.consumer.wait_async(Some(self.timeout), f)
    }

    fn wait_for_single_event_async_timeout<F>(
        &mut self,
        timeout: Duration,
        f: F,
    ) -> impl Future<Output = Result<(), crate::Error>>
    where
        F: FnOnce(EventOrTimeout<'_>) -> bool,
    {
        self.consumer.wait_async(Some(timeout), f)
    }
}

impl<C: sealed::Next> EventIterator for WithTimeout<C> {
    type Item<'a>
        = EventOrTimeout<'a>
    where
        C: 'a;

    fn next(&mut self) -> Option<EventOrTimeout<'_>> {
        self.consumer.next_timeout(Some(self.timeout))
    }
}

impl<C: sealed::NextAsync> EventIteratorAsync for WithTimeout<C> {
    type Item<'a>
        = EventOrTimeout<'a>
    where
        C: 'a;

    fn next(&mut self) -> impl Future<Output = Option<EventOrTimeout<'_>>> {
        self.consumer.next_timeout_async(Some(self.timeout))
    }
}

impl<C: sealed::NextOwned> IntoIterator for WithTimeout<C> {
    type Item = EventOrTimeout<'static>;
    type IntoIter = OwnedIter<Self>;

    fn into_iter(self) -> OwnedIter<Self> {
        OwnedIter(self)
    }
}

impl<C: sealed::NextOwned> Iterator for OwnedIter<WithTimeout<C>> {
    type Item = EventOrTimeout<'static>;

    fn next(&mut self) -> Option<EventOrTimeout<'static>> {
        self.0.consumer.next_owned(Some(self.0.timeout))
    }
}

#[cfg(any(feature = "futures", feature = "flume-async"))]
impl<C: sealed::NextOwnedAsync> IntoEventStream for WithTimeout<C> {
    type Item = EventOrTimeout<'static>;

    fn into_stream(self) -> impl futures::Stream<Item = EventOrTimeout<'static>> {
        futures::stream::unfold(self, |mut timed| async move {
            let evt = timed.consumer.next_owned_async(Some(timed.timeout)).await?;
            Some((evt, timed))
        })
    }
}
