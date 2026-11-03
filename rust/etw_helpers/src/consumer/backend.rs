//! The public event consumer types, generic over a backend tag that selects how events are
//! handed over from the ETW thread. The backends' own types are implementation details.

use core::future::Future;
use core::time::Duration;

use crate::consumer::event_record::EventRecord;
#[cfg(any(feature = "futures", feature = "flume-async"))]
use crate::consumer::event_waiter::sealed::NextAsync;
use crate::consumer::event_waiter::sealed::{
    Connect, Next, NextOwned, NextOwnedAsync, Wait, WaitAsync,
};
use crate::consumer::event_waiter::{EventOrTimeout, OwnedIter, WithTimeout};
use crate::consumer::processtrace::EventProcessor;

/// The implementation of each backend: the types its consumers are built on. Public but
/// unnameable outside the crate, so [`Backend`] and [`StreamBackend`] are sealed.
pub(crate) mod imp {
    use crate::consumer::event_waiter::sealed::Connect;

    pub trait Backend: 'static {
        // Lends each event, and the ETW callback blocks until it is released.
        type Lent: Connect + Default;
        // Copies each event into a queue of up to `event_buffer_count` events.
        type Owned: Connect;

        fn new_owned(event_buffer_count: usize) -> Self::Owned;
    }

    pub trait StreamBackend: Backend {
        type Stream: Connect + Default;
    }
}

/// A backend for the event consumers: how events are handed over from the ETW thread. Pass one
/// as the type parameter of [`EtwEventWaiter`], [`EtwEventWaiterOwned`] or
/// `EtwEventAsyncStream`; which consumer traits they implement depends on it.
///
/// | Backend | Feature | Lent waiter | Owned waiter |
/// |---|---|---|---|
/// | [`Raw`] (default) | always | [`EventIterator`](crate::consumer::EventIterator), [`EventWaiter`](crate::consumer::EventWaiter) | the same, plus [`IntoIterator`] |
/// | `Crossbeam` | `crossbeam` | the same as `Raw` | the same as `Raw` |
/// | `Flume` | `flume` | the same as `Raw`; with `flume-async`, also [`EventWaiterAsync`](crate::consumer::EventWaiterAsync) | the same as `Raw`; with `flume-async`, also `EventWaiterAsync`, [`EventIteratorAsync`](crate::consumer::EventIteratorAsync) and `IntoEventStream` |
/// | `Futures` | `futures` | `EventWaiterAsync`, and `EventWaiter` (blocking) | `EventWaiterAsync`, `EventIteratorAsync`, `IntoEventStream`, and blocking `EventWaiter`, `EventIterator` and `IntoIterator` |
///
/// This trait is sealed.
pub trait Backend: imp::Backend {}

/// A [`Backend`] with a lending async stream, `EtwEventAsyncStream`: `Futures`, and `Flume`
/// with the `flume-async` feature. This trait is sealed.
pub trait StreamBackend: Backend + imp::StreamBackend {}

/// The default backend: no channel crate, just Win32 SRW locks and condition variables. It only
/// needs `core` and `alloc` (no `std`), and is always available.
pub enum Raw {}

impl imp::Backend for Raw {
    type Lent = crate::consumer::raw::LentWaiter;
    type Owned = crate::consumer::raw::OwnedWaiter;

    fn new_owned(event_buffer_count: usize) -> Self::Owned {
        crate::consumer::raw::OwnedWaiter::new(event_buffer_count)
    }
}
impl Backend for Raw {}

/// A backend built on [crossbeam-channel](https://crates.io/crates/crossbeam-channel). With the
/// `crossbeam` feature.
#[cfg(feature = "crossbeam")]
pub enum Crossbeam {}

#[cfg(feature = "crossbeam")]
impl imp::Backend for Crossbeam {
    type Lent = crate::consumer::crossbeam::LentWaiter;
    type Owned = crate::consumer::crossbeam::OwnedWaiter;

    fn new_owned(event_buffer_count: usize) -> Self::Owned {
        crate::consumer::crossbeam::OwnedWaiter::new(event_buffer_count)
    }
}
#[cfg(feature = "crossbeam")]
impl Backend for Crossbeam {}

/// A backend built on [flume](https://crates.io/crates/flume). With the `flume` feature; the
/// `flume-async` feature adds async APIs and `EtwEventAsyncStream<Flume>`.
#[cfg(feature = "flume")]
pub enum Flume {}

#[cfg(feature = "flume")]
impl imp::Backend for Flume {
    type Lent = crate::consumer::flume::LentWaiter;
    type Owned = crate::consumer::flume::OwnedWaiter;

    fn new_owned(event_buffer_count: usize) -> Self::Owned {
        crate::consumer::flume::OwnedWaiter::new(event_buffer_count)
    }
}
#[cfg(feature = "flume")]
impl Backend for Flume {}

#[cfg(feature = "flume-async")]
impl imp::StreamBackend for Flume {
    type Stream = crate::consumer::flume::LentStream;
}
#[cfg(feature = "flume-async")]
impl StreamBackend for Flume {}

/// An async backend that works with any executor, built on the
/// [futures](https://crates.io/crates/futures) channels. Its waiters also have blocking APIs. With
/// the `futures` feature.
///
/// Its owned waiters' queues always have room for at least one event (futures channels reserve a
/// slot for each sender), so an `event_buffer_count` of 0 behaves like 1.
#[cfg(feature = "futures")]
pub enum Futures {}

#[cfg(feature = "futures")]
impl imp::Backend for Futures {
    type Lent = crate::consumer::futures::LentWaiter;
    type Owned = crate::consumer::futures::OwnedWaiter;

    fn new_owned(event_buffer_count: usize) -> Self::Owned {
        crate::consumer::futures::OwnedWaiter::new(event_buffer_count)
    }
}
#[cfg(feature = "futures")]
impl Backend for Futures {}

#[cfg(feature = "futures")]
impl imp::StreamBackend for Futures {
    type Stream = crate::consumer::futures::LentStream;
}
#[cfg(feature = "futures")]
impl StreamBackend for Futures {}

/// Receives ETW events from a trace, lending each one straight from the ETW buffer, with no
/// copies. ETW waits until you move on to the next event. Start the trace with
/// [`EventConsumer::from_file`](crate::consumer::EventConsumer::from_file) or
/// [`from_session`](crate::consumer::EventConsumer::from_session).
///
/// `B` selects the [`Backend`], [`Raw`] by default:
///
/// ```no_run
/// use etw_helpers::consumer::prelude::*;
/// use etw_helpers::consumer::EtwEventWaiter;
///
/// let mut waiter = EventConsumer::from_file("sample.etl", EtwEventWaiter::new())?;
/// while let Some(evt) = waiter.next() {
///     println!("event {}", evt.get_event_header().EventDescriptor.Id);
/// }
/// # Ok::<(), etw_helpers::Error>(())
/// ```
///
/// To choose another backend, name it: `EtwEventWaiter::<Crossbeam>::new()`. This uses
/// [`NewEventWaiter`], so import it (or the [`prelude`](crate::consumer::prelude)). Which consumer
/// traits the waiter implements depends on the backend (see [`Backend`]). To let ProcessTrace run
/// ahead of a slow consumer, use [`new_owned_events`](NewEventWaiter::new_owned_events).
///
/// Events borrow the waiter mutably, and the ETW callback thread stays blocked until that borrow
/// ends. Holding an event across the next call to `next()` is a compile error:
///
/// ```compile_fail
/// # use etw_helpers::consumer::{EtwEventWaiter, EventIterator};
/// # fn f(waiter: &mut EtwEventWaiter) {
/// let first = waiter.next();
/// let second = waiter.next(); // error[E0499]: cannot borrow `*waiter` as mutable more than once
/// drop(first);
/// # }
/// ```
pub struct EtwEventWaiter<B: Backend = Raw> {
    inner: <B as imp::Backend>::Lent,
}

/// Receives copies of ETW events from a trace. The ETW callback copies each event into a queue,
/// so ProcessTrace only waits while the queue is full. Create one with
/// [`EtwEventWaiter::new_owned_events`](NewEventWaiter::new_owned_events), and start the trace with
/// [`EventConsumer::from_file`](crate::consumer::EventConsumer::from_file) or
/// [`from_session`](crate::consumer::EventConsumer::from_session).
///
/// Events are always [`EventRecord::Owned`]. Events queued before ProcessTrace completes are
/// still returned afterwards. Returning `false` from a `wait_for_single_event*` closure (or panicking
/// in it) stops the trace and discards any events still queued. Iterating the waiter by value ([`IntoIterator`])
/// yields its events as an ordinary [`Iterator`]; with an async backend, `IntoEventStream` yields
/// them as a `futures::Stream`.
pub struct EtwEventWaiterOwned<B: Backend = Raw> {
    inner: <B as imp::Backend>::Owned,
}

/// Creates an [`EtwEventWaiter`] for a [`Backend`]: `EtwEventWaiter::<Crossbeam>::new()`.
///
/// The default backend's waiter has the same constructors as inherent methods, so
/// `EtwEventWaiter::new()` needs no import.
pub trait NewEventWaiter: Connect + Sized {
    /// The type [`new_owned_events`](Self::new_owned_events) returns: an
    /// [`EtwEventWaiterOwned`] with the same backend.
    type Owned;

    /// Creates a waiter that lends each event.
    fn new() -> Self;

    /// Creates a waiter that queues copies of up to `event_buffer_count` events. With 0, each
    /// event is handed over directly, but the ETW callback still doesn't wait for it to be
    /// handled.
    fn new_owned_events(event_buffer_count: usize) -> Self::Owned;

    /// Like [`new`](Self::new), but the waiter gives up waiting for an event after `timeout`,
    /// returning [`EventOrTimeout::Timeout`].
    fn new_with_timeout(timeout: Duration) -> WithTimeout<Self> {
        WithTimeout::new(Self::new(), timeout)
    }

    /// Like [`new_owned_events`](Self::new_owned_events), but the waiter gives up waiting for an
    /// event after `timeout`, returning [`EventOrTimeout::Timeout`].
    fn new_owned_events_with_timeout(
        event_buffer_count: usize,
        timeout: Duration,
    ) -> WithTimeout<Self::Owned> {
        WithTimeout::new(Self::new_owned_events(event_buffer_count), timeout)
    }
}

impl<B: Backend> NewEventWaiter for EtwEventWaiter<B> {
    type Owned = EtwEventWaiterOwned<B>;

    fn new() -> Self {
        <Self as Default>::default()
    }

    fn new_owned_events(event_buffer_count: usize) -> EtwEventWaiterOwned<B> {
        EtwEventWaiterOwned {
            inner: B::new_owned(event_buffer_count),
        }
    }
}

/// The default backend's constructors, so they work without naming the backend or importing
/// [`NewEventWaiter`].
impl EtwEventWaiter {
    /// Creates a waiter that lends each event. See [`NewEventWaiter::new`].
    pub fn new() -> Self {
        <Self as NewEventWaiter>::new()
    }

    /// Creates a waiter that queues copies of up to `event_buffer_count` events. See
    /// [`NewEventWaiter::new_owned_events`].
    pub fn new_owned_events(event_buffer_count: usize) -> EtwEventWaiterOwned {
        <Self as NewEventWaiter>::new_owned_events(event_buffer_count)
    }

    /// Like [`new`](Self::new), with a timeout. See [`NewEventWaiter::new_with_timeout`].
    pub fn new_with_timeout(timeout: Duration) -> WithTimeout<Self> {
        <Self as NewEventWaiter>::new_with_timeout(timeout)
    }

    /// Like [`new_owned_events`](Self::new_owned_events), with a timeout. See
    /// [`NewEventWaiter::new_owned_events_with_timeout`].
    pub fn new_owned_events_with_timeout(
        event_buffer_count: usize,
        timeout: Duration,
    ) -> WithTimeout<EtwEventWaiterOwned> {
        <Self as NewEventWaiter>::new_owned_events_with_timeout(event_buffer_count, timeout)
    }
}

impl<B: Backend> Default for EtwEventWaiter<B> {
    fn default() -> Self {
        EtwEventWaiter {
            inner: Default::default(),
        }
    }
}

impl<B: Backend> Connect for EtwEventWaiter<B> {
    fn take_processor(&mut self) -> impl EventProcessor + 'static + use<B> {
        self.inner.take_processor()
    }
}

impl<B: Backend> Wait for EtwEventWaiter<B>
where
    <B as imp::Backend>::Lent: Wait,
{
    fn wait<F>(&mut self, timeout: Option<Duration>, f: F) -> Result<(), crate::Error>
    where
        F: FnOnce(EventOrTimeout<'_>) -> bool,
    {
        self.inner.wait(timeout, f)
    }
}

impl<B: Backend> WaitAsync for EtwEventWaiter<B>
where
    <B as imp::Backend>::Lent: WaitAsync,
{
    fn wait_async<F>(
        &mut self,
        timeout: Option<Duration>,
        f: F,
    ) -> impl Future<Output = Result<(), crate::Error>>
    where
        F: FnOnce(EventOrTimeout<'_>) -> bool,
    {
        self.inner.wait_async(timeout, f)
    }
}

impl<B: Backend> Next for EtwEventWaiter<B>
where
    <B as imp::Backend>::Lent: Next,
{
    fn next_timeout(&mut self, timeout: Option<Duration>) -> Option<EventOrTimeout<'_>> {
        self.inner.next_timeout(timeout)
    }
}

impl<B: Backend> Connect for EtwEventWaiterOwned<B> {
    fn take_processor(&mut self) -> impl EventProcessor + 'static + use<B> {
        self.inner.take_processor()
    }
}

impl<B: Backend> Wait for EtwEventWaiterOwned<B>
where
    <B as imp::Backend>::Owned: Wait,
{
    fn wait<F>(&mut self, timeout: Option<Duration>, f: F) -> Result<(), crate::Error>
    where
        F: FnOnce(EventOrTimeout<'_>) -> bool,
    {
        self.inner.wait(timeout, f)
    }
}

impl<B: Backend> WaitAsync for EtwEventWaiterOwned<B>
where
    <B as imp::Backend>::Owned: WaitAsync,
{
    fn wait_async<F>(
        &mut self,
        timeout: Option<Duration>,
        f: F,
    ) -> impl Future<Output = Result<(), crate::Error>>
    where
        F: FnOnce(EventOrTimeout<'_>) -> bool,
    {
        self.inner.wait_async(timeout, f)
    }
}

impl<B: Backend> NextOwned for EtwEventWaiterOwned<B>
where
    <B as imp::Backend>::Owned: NextOwned,
{
    fn next_owned(&mut self, timeout: Option<Duration>) -> Option<EventOrTimeout<'static>> {
        self.inner.next_owned(timeout)
    }
}

impl<B: Backend> NextOwnedAsync for EtwEventWaiterOwned<B>
where
    <B as imp::Backend>::Owned: NextOwnedAsync,
{
    fn next_owned_async(
        &mut self,
        timeout: Option<Duration>,
    ) -> impl Future<Output = Option<EventOrTimeout<'static>>> {
        self.inner.next_owned_async(timeout)
    }
}

impl<B: Backend> IntoIterator for EtwEventWaiterOwned<B>
where
    <B as imp::Backend>::Owned: NextOwned,
{
    type Item = EventRecord<'static>;
    type IntoIter = OwnedIter<Self>;

    fn into_iter(self) -> OwnedIter<Self> {
        OwnedIter(self)
    }
}

/// The default [`StreamBackend`]: `Futures` with the `futures` feature, otherwise `Flume`.
#[cfg(feature = "futures")]
pub type DefaultStreamBackend = Futures;
/// The default [`StreamBackend`]: `Futures` with the `futures` feature, otherwise `Flume`.
#[cfg(all(feature = "flume-async", not(feature = "futures")))]
pub type DefaultStreamBackend = Flume;

/// A lending async stream of ETW events. With the `futures` or `flume-async` feature. Start the
/// trace with [`EventConsumer::from_file`](crate::consumer::EventConsumer::from_file) or
/// [`from_session`](crate::consumer::EventConsumer::from_session), and read events with
/// [`EventIteratorAsync::next`](crate::consumer::EventIteratorAsync::next).
///
/// `B` selects the [`StreamBackend`]: by default `Futures` with the `futures` feature, otherwise
/// `Flume`. To choose one, name it: `EtwEventAsyncStream::<Flume>::new()`, which uses
/// [`NewEventAsyncStream`].
///
/// Each event borrows the stream mutably, and the ETW callback thread stays blocked until that
/// borrow ends. Holding an event across the next call to `next()` is therefore a compile error:
///
/// ```compile_fail
/// # use etw_helpers::consumer::{EtwEventAsyncStream, EventIteratorAsync};
/// # async fn f(stream: &mut EtwEventAsyncStream) {
/// let first = stream.next().await;
/// let second = stream.next().await; // error[E0499]: cannot borrow `*stream` as mutable more than once
/// drop(first);
/// # }
/// ```
///
/// Dropping the stream releases the current event and stops the trace.
#[cfg(any(feature = "futures", feature = "flume-async"))]
pub struct EtwEventAsyncStream<B: StreamBackend = DefaultStreamBackend> {
    inner: <B as imp::StreamBackend>::Stream,
}

/// Creates an [`EtwEventAsyncStream`] for a [`StreamBackend`]:
/// `EtwEventAsyncStream::<Flume>::new()`.
///
/// The default backend's stream has the same constructors as inherent methods, so
/// `EtwEventAsyncStream::new()` needs no import.
#[cfg(any(feature = "futures", feature = "flume-async"))]
pub trait NewEventAsyncStream: Connect + Sized {
    /// Creates a stream.
    fn new() -> Self;

    /// Like [`new`](Self::new), but the stream gives up waiting for an event after `timeout`,
    /// returning [`EventOrTimeout::Timeout`].
    fn new_with_timeout(timeout: Duration) -> WithTimeout<Self> {
        WithTimeout::new(Self::new(), timeout)
    }
}

#[cfg(any(feature = "futures", feature = "flume-async"))]
impl<B: StreamBackend> NewEventAsyncStream for EtwEventAsyncStream<B> {
    fn new() -> Self {
        <Self as Default>::default()
    }
}

/// The default backend's constructors, so they work without naming the backend or importing
/// [`NewEventAsyncStream`].
#[cfg(any(feature = "futures", feature = "flume-async"))]
impl EtwEventAsyncStream {
    /// Creates a stream. See [`NewEventAsyncStream::new`].
    pub fn new() -> Self {
        <Self as NewEventAsyncStream>::new()
    }

    /// Like [`new`](Self::new), with a timeout. See [`NewEventAsyncStream::new_with_timeout`].
    pub fn new_with_timeout(timeout: Duration) -> WithTimeout<Self> {
        <Self as NewEventAsyncStream>::new_with_timeout(timeout)
    }
}

#[cfg(any(feature = "futures", feature = "flume-async"))]
impl<B: StreamBackend> Default for EtwEventAsyncStream<B> {
    fn default() -> Self {
        EtwEventAsyncStream {
            inner: Default::default(),
        }
    }
}

#[cfg(any(feature = "futures", feature = "flume-async"))]
impl<B: StreamBackend> Connect for EtwEventAsyncStream<B> {
    fn take_processor(&mut self) -> impl EventProcessor + 'static + use<B> {
        self.inner.take_processor()
    }
}

#[cfg(any(feature = "futures", feature = "flume-async"))]
impl<B: StreamBackend> NextAsync for EtwEventAsyncStream<B>
where
    <B as imp::StreamBackend>::Stream: NextAsync,
{
    fn next_timeout_async(
        &mut self,
        timeout: Option<Duration>,
    ) -> impl Future<Output = Option<EventOrTimeout<'_>>> {
        self.inner.next_timeout_async(timeout)
    }
}
