use core::future::Future;
use core::time::Duration;

#[cfg(any(feature = "futures", feature = "flume-async"))]
use crate::consumer::event_waiter::IntoEventStream;
use crate::consumer::event_waiter::{
    EventIterator, EventIteratorAsync, EventOrTimeout, EventWaiter, EventWaiterAsync,
};
use crate::consumer::processtrace::ProcessTraceThread;
use crate::error::{E_TRACE_FINISHED, E_UNEXPECTED};

/// A trace being processed on a background thread, delivering its events to the consumer `C`.
///
/// Created by [`EventConsumer::from_file`](crate::consumer::EventConsumer::from_file) or
/// [`EventConsumer::from_session`](crate::consumer::EventConsumer::from_session). It implements the same
/// consumer traits as `C` ([`EventWaiter`], [`EventWaiterAsync`], [`EventIterator`],
/// [`EventIteratorAsync`], [`IntoIterator`] and `IntoEventStream`), forwarding to `C`.
///
/// Dropping it drops the consumer (releasing any lent event) and then stops the trace, without
/// waiting for the background thread. Call [`stop_and_wait`](Self::stop_and_wait) to wait for it
/// and get ProcessTrace's result.
pub struct EventTrace<C> {
    // Declared first, so it's dropped first: the trace can't stop while an event is lent.
    consumer: C,
    thread: ProcessTraceThread,
}

impl<C> EventTrace<C> {
    pub(crate) fn new(consumer: C, thread: ProcessTraceThread) -> Self {
        EventTrace { consumer, thread }
    }

    /// Stops the trace without waiting for the background thread. A log file's remaining events
    /// aren't delivered (except one ETW may already be delivering); for a real-time session, events
    /// already buffered still are. Either way, the consumer then reports the end of the trace as
    /// usual. Does nothing if the trace has already stopped.
    pub fn stop(&mut self) {
        self.thread.stop();
    }

    // Waits up to `timeout` for ProcessTrace to return, and returns its result.
    #[cfg(test)]
    pub(crate) fn wait_finished(&self, timeout: Duration) -> Option<windows_sys::core::HRESULT> {
        self.thread.wait_result(timeout)
    }
    /// Drops the consumer (releasing any lent event), stops the trace, and waits for the
    /// background thread to exit.
    ///
    /// Returns ProcessTrace's result: `Ok` once a log file has been read to the end, or the error
    /// that stopped ProcessTrace. Returns `E_UNEXPECTED` if the background thread panicked.
    pub fn stop_and_wait(self) -> Result<(), crate::Error> {
        let EventTrace {
            consumer,
            mut thread,
        } = self;
        drop(consumer);
        thread
            .stop_and_get_thread()?
            .join()
            .unwrap_or_else(|_| Err(crate::Error::from_hresult(E_UNEXPECTED)))
    }
}

// Replaces the consumer's report that ProcessTrace has completed with ProcessTrace's result,
// which was recorded before the consumer could see the trace end.
fn trace_result(
    thread: &ProcessTraceThread,
    result: Result<(), crate::Error>,
) -> Result<(), crate::Error> {
    result.map_err(|err| {
        if err.code() == E_TRACE_FINISHED {
            crate::Error::from_hresult(thread.result().unwrap_or(E_UNEXPECTED))
        } else {
            err
        }
    })
}

// Calls a `wait_for_single_event*` closure, stopping the trace when it returns `false` or panics.
//
// An owned-event processor may already be idle, with no callback left to notice that the waiter
// cancelled, and a lent waiter's closure may have received a timeout rather than an event to
// reject, so the trace is stopped here. When a lent waiter rejects an event this is redundant
// but harmless: cancelling is idempotent.
fn cancel_on_false<T>(thread: &ProcessTraceThread, f: impl FnOnce(T) -> bool, arg: T) -> bool {
    let mut guard = CancelGuard {
        thread,
        armed: true,
    };
    let keep_going = f(arg);
    guard.armed = !keep_going;
    keep_going
}

// Stops the trace when dropped while armed: when the closure returned `false`, or unwound.
struct CancelGuard<'a> {
    thread: &'a ProcessTraceThread,
    armed: bool,
}

impl Drop for CancelGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.thread.cancel();
        }
    }
}

/// Once ProcessTrace has completed, waiting fails with ProcessTrace's result (code `S_OK` once a
/// log file has been read to the end).
impl<C: EventWaiter> EventWaiter for EventTrace<C> {
    type Event<'a> = C::Event<'a>;

    fn wait_for_single_event<F>(&mut self, f: F) -> Result<(), crate::Error>
    where
        F: FnOnce(C::Event<'_>) -> bool,
    {
        let thread = &self.thread;
        trace_result(
            thread,
            self.consumer
                .wait_for_single_event(move |evt| cancel_on_false(thread, f, evt)),
        )
    }

    fn wait_for_single_event_timeout<F>(
        &mut self,
        timeout: Duration,
        f: F,
    ) -> Result<(), crate::Error>
    where
        F: FnOnce(EventOrTimeout<'_>) -> bool,
    {
        let thread = &self.thread;
        trace_result(
            thread,
            self.consumer
                .wait_for_single_event_timeout(timeout, move |evt| cancel_on_false(thread, f, evt)),
        )
    }
}

/// Once ProcessTrace has completed, waiting fails with ProcessTrace's result (code `S_OK` once a
/// log file has been read to the end).
impl<C: EventWaiterAsync> EventWaiterAsync for EventTrace<C> {
    type Event<'a> = C::Event<'a>;

    fn wait_for_single_event_async<F>(
        &mut self,
        f: F,
    ) -> impl Future<Output = Result<(), crate::Error>>
    where
        F: FnOnce(C::Event<'_>) -> bool,
    {
        let thread = &self.thread;
        let waited = self
            .consumer
            .wait_for_single_event_async(move |evt| cancel_on_false(thread, f, evt));
        async move { trace_result(thread, waited.await) }
    }

    fn wait_for_single_event_async_timeout<F>(
        &mut self,
        timeout: Duration,
        f: F,
    ) -> impl Future<Output = Result<(), crate::Error>>
    where
        F: FnOnce(EventOrTimeout<'_>) -> bool,
    {
        let thread = &self.thread;
        let waited = self
            .consumer
            .wait_for_single_event_async_timeout(timeout, move |evt| {
                cancel_on_false(thread, f, evt)
            });
        async move { trace_result(thread, waited.await) }
    }
}

impl<C: EventIterator> EventIterator for EventTrace<C> {
    type Item<'a>
        = C::Item<'a>
    where
        C: 'a;

    fn next(&mut self) -> Option<C::Item<'_>> {
        EventIterator::next(&mut self.consumer)
    }
}

impl<C: EventIteratorAsync> EventIteratorAsync for EventTrace<C> {
    type Item<'a>
        = C::Item<'a>
    where
        C: 'a;

    fn next(&mut self) -> impl Future<Output = Option<C::Item<'_>>> {
        EventIteratorAsync::next(&mut self.consumer)
    }
}
impl<C: IntoIterator> IntoIterator for EventTrace<C> {
    type Item = C::Item;
    type IntoIter = EventTraceIntoIter<C::IntoIter>;

    fn into_iter(self) -> Self::IntoIter {
        EventTraceIntoIter {
            iter: self.consumer.into_iter(),
            _thread: self.thread,
        }
    }
}

/// An [`Iterator`] over the events of an [`EventTrace`], returned by its
/// [`into_iter`](IntoIterator::into_iter). Dropping it stops the trace.
pub struct EventTraceIntoIter<I> {
    // Declared first, so it's dropped before the trace is stopped.
    iter: I,
    _thread: ProcessTraceThread,
}

impl<I: Iterator> Iterator for EventTraceIntoIter<I> {
    type Item = I::Item;

    fn next(&mut self) -> Option<I::Item> {
        self.iter.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.iter.size_hint()
    }
}

#[cfg(any(feature = "futures", feature = "flume-async"))]
impl<C: IntoEventStream> IntoEventStream for EventTrace<C> {
    type Item = C::Item;

    /// The stream owns the trace, and dropping it stops the trace.
    fn into_stream(self) -> impl futures::Stream<Item = C::Item> {
        TraceStream {
            stream: Box::pin(self.consumer.into_stream()),
            _thread: self.thread,
        }
    }
}

#[cfg(any(feature = "futures", feature = "flume-async"))]
struct TraceStream<S> {
    // Declared first, so it's dropped before the trace is stopped.
    stream: core::pin::Pin<Box<S>>,
    _thread: ProcessTraceThread,
}

#[cfg(any(feature = "futures", feature = "flume-async"))]
impl<S: futures::Stream> futures::Stream for TraceStream<S> {
    type Item = S::Item;

    fn poll_next(
        self: core::pin::Pin<&mut Self>,
        cx: &mut core::task::Context<'_>,
    ) -> core::task::Poll<Option<S::Item>> {
        self.get_mut().stream.as_mut().poll_next(cx)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.stream.size_hint()
    }
}
