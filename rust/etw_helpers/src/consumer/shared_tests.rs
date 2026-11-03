//! Functional tests shared by the event consumer backends, generic over the public
//! [`EventConsumer`], [`EventWaiter`], [`EventWaiterAsync`], [`EventIterator`] and
//! [`EventIteratorAsync`] traits.
//!
//! Each backend's `tests` module instantiates them for its types with [`lent_waiter_tests!`],
//! [`owned_waiter_tests!`], [`async_lent_waiter_tests!`], [`async_owned_waiter_tests!`] or
//! [`stream_tests!`]. Session, provider and `.etl` file names include the backend name, so the
//! tests of different backends can run in parallel.

// The stream and async tests are unused without a stream or async backend.
#![cfg_attr(
    not(any(feature = "futures", feature = "flume-async")),
    allow(dead_code, unused_macros, unused_imports)
)]

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use windows_sys::core::GUID;

#[cfg(any(feature = "futures", feature = "flume-async"))]
use crate::consumer::IntoEventStream;
use crate::consumer::{
    EventConsumer, EventIterator, EventIteratorAsync, EventOrTimeout, EventProcessor, EventRecord,
    EventTrace, EventWaiter, EventWaiterAsync, ProcessTraceHandle,
};
use crate::controller::{ControlTraceHandle, FileMode, SessionBuilder};
use crate::error::S_OK;
use crate::test_util::{guid_eq, wait_until_enabled};

// Consumers created without a timeout, which deliver `EventRecord`s.
pub(crate) trait Waiter:
    EventConsumer + for<'a> EventWaiter<Event<'a> = EventRecord<'a>>
{
}
impl<T: EventConsumer + for<'a> EventWaiter<Event<'a> = EventRecord<'a>>> Waiter for T {}

pub(crate) trait AsyncWaiter:
    EventConsumer + for<'a> EventWaiterAsync<Event<'a> = EventRecord<'a>>
{
}
impl<T: EventConsumer + for<'a> EventWaiterAsync<Event<'a> = EventRecord<'a>>> AsyncWaiter for T {}

pub(crate) trait Iter:
    EventConsumer + 'static + for<'a> EventIterator<Item<'a> = EventRecord<'a>>
{
}
impl<T: EventConsumer + 'static + for<'a> EventIterator<Item<'a> = EventRecord<'a>>> Iter for T {}

pub(crate) trait AsyncIter:
    EventConsumer + 'static + for<'a> EventIteratorAsync<Item<'a> = EventRecord<'a>>
{
}
impl<T: EventConsumer + 'static + for<'a> EventIteratorAsync<Item<'a> = EventRecord<'a>>> AsyncIter
    for T
{
}

// Consumers created with `new_with_timeout`, which deliver `EventOrTimeout`s.
pub(crate) trait TimedWaiter:
    EventConsumer + for<'a> EventWaiter<Event<'a> = EventOrTimeout<'a>>
{
}
impl<T: EventConsumer + for<'a> EventWaiter<Event<'a> = EventOrTimeout<'a>>> TimedWaiter for T {}

pub(crate) trait TimedAsyncWaiter:
    EventConsumer + for<'a> EventWaiterAsync<Event<'a> = EventOrTimeout<'a>>
{
}
impl<T: EventConsumer + for<'a> EventWaiterAsync<Event<'a> = EventOrTimeout<'a>>> TimedAsyncWaiter
    for T
{
}

pub(crate) trait TimedIter:
    EventConsumer + 'static + for<'a> EventIterator<Item<'a> = EventOrTimeout<'a>>
{
}
impl<T: EventConsumer + 'static + for<'a> EventIterator<Item<'a> = EventOrTimeout<'a>>> TimedIter
    for T
{
}

pub(crate) trait TimedAsyncIter:
    EventConsumer + 'static + for<'a> EventIteratorAsync<Item<'a> = EventOrTimeout<'a>>
{
}
impl<T> TimedAsyncIter for T where
    T: EventConsumer + 'static + for<'a> EventIteratorAsync<Item<'a> = EventOrTimeout<'a>>
{
}

/// Adapts a closure for a timed wait, failing the test if the wait times out.
fn no_timeout<F>(f: F) -> impl FnOnce(EventOrTimeout<'_>) -> bool
where
    F: FnOnce(EventRecord<'_>) -> bool,
{
    move |evt| match evt {
        EventOrTimeout::Event(evt) => f(evt),
        EventOrTimeout::Timeout => panic!("timed out waiting for an event"),
    }
}

/// `wait_for_single_event_timeout` with a timeout (failing the test if it times out),
/// `wait_for_single_event` without.
trait WaitExt: for<'a> EventWaiter<Event<'a> = EventRecord<'a>> {
    fn wait(
        &mut self,
        timeout: Option<Duration>,
        f: impl FnOnce(EventRecord) -> bool,
    ) -> Result<(), crate::Error> {
        match timeout {
            Some(timeout) => self.wait_for_single_event_timeout(timeout, no_timeout(f)),
            None => self.wait_for_single_event(f),
        }
    }
}

impl<W: for<'a> EventWaiter<Event<'a> = EventRecord<'a>>> WaitExt for W {}

/// Generates the tests for the lent waiter of a backend (`EtwEventWaiter<$tag>`), plus any extra shared
/// tests named after the backend (e.g. `iterate` for [`EventIterator`] waiters), and after
/// `timed:` any extra tests of the waiter created with `new_with_timeout` (e.g. `timed_next`
/// for [`EventIterator`] waiters).
macro_rules! lent_waiter_tests {
    ($tag:ty, $backend:literal $(, $extra:ident)* $(; timed: $($timed_extra:ident),*)?) => {
        $crate::consumer::shared_tests::generate_tests!(
            <$crate::consumer::EtwEventWaiter<$tag>>::default,
            $backend,
            wait_for_single_event_timeout,
            consume_event,
            panicking_handler_releases_event,
            stop_file_trace,
            $($extra,)*
        );
        $crate::consumer::shared_tests::generate_tests!(
            <$crate::consumer::EtwEventWaiter<$tag> as $crate::consumer::NewEventWaiter>::new_with_timeout,
            $backend,
            timed_wait,
            $($($timed_extra,)*)?
        );
    };
}
pub(crate) use lent_waiter_tests;

/// Generates the tests for the owned waiter of a backend (`EtwEventWaiterOwned<$tag>`).
macro_rules! owned_waiter_tests {
    ($tag:ty, $backend:literal) => {
        #[test]
        fn owned_wait_for_single_event_timeout() -> Result<(), $crate::Error> {
            // A different backend name keeps its session separate from the lent waiter's test.
            $crate::consumer::shared_tests::wait_for_single_event_timeout(
                concat!($backend, "Owned"),
                || <$crate::consumer::EtwEventWaiter<$tag> as $crate::consumer::NewEventWaiter>::new_owned_events(16),
            )
        }

        $crate::consumer::shared_tests::generate_tests!(
            <$crate::consumer::EtwEventWaiter<$tag> as $crate::consumer::NewEventWaiter>::new_owned_events,
            $backend,
            owned_events_are_queued,
            owned_events_cancel,
            owned_events_cancel_when_idle,
            owned_panicking_handler_stops_trace,
            owned_panicking_handler_discards_queue,
            iterate_owned,
            into_iter,
        );

        // A different backend name keeps their sessions separate from the lent waiter's tests.
        #[test]
        fn owned_timed_wait() -> Result<(), $crate::Error> {
            $crate::consumer::shared_tests::timed_wait(concat!($backend, "Owned"), |timeout| {
                <$crate::consumer::EtwEventWaiter<$tag> as $crate::consumer::NewEventWaiter>::new_owned_events_with_timeout(16, timeout)
            })
        }

        #[test]
        fn owned_timed_next() -> Result<(), $crate::Error> {
            $crate::consumer::shared_tests::timed_next(concat!($backend, "Owned"), |timeout| {
                <$crate::consumer::EtwEventWaiter<$tag> as $crate::consumer::NewEventWaiter>::new_owned_events_with_timeout(16, timeout)
            })
        }

        #[test]
        fn timed_into_iter() -> Result<(), $crate::Error> {
            $crate::consumer::shared_tests::timed_into_iter($backend, |timeout| {
                <$crate::consumer::EtwEventWaiter<$tag> as $crate::consumer::NewEventWaiter>::new_owned_events_with_timeout(16, timeout)
            })
        }
    };
}
pub(crate) use owned_waiter_tests;

/// Generates the [`EventWaiterAsync`] tests for the lent waiter of a backend.
macro_rules! async_lent_waiter_tests {
    ($tag:ty, $backend:literal) => {
        #[tokio::test]
        async fn wait_for_single_event_async_timeout() -> Result<(), $crate::Error> {
            $crate::consumer::shared_tests::wait_for_single_event_async_timeout(
                $backend,
                <$crate::consumer::EtwEventWaiter<$tag>>::default,
            )
            .await
        }

        #[tokio::test]
        async fn consume_event_async() -> Result<(), $crate::Error> {
            $crate::consumer::shared_tests::consume_event_async($backend, <$crate::consumer::EtwEventWaiter<$tag>>::default).await
        }

        #[tokio::test]
        async fn timed_wait_async() -> Result<(), $crate::Error> {
            $crate::consumer::shared_tests::timed_wait_async($backend, <$crate::consumer::EtwEventWaiter<$tag> as $crate::consumer::NewEventWaiter>::new_with_timeout)
                .await
        }
    };
}
pub(crate) use async_lent_waiter_tests;

/// Generates the [`EventWaiterAsync`], [`EventIteratorAsync`] and `into_stream` tests for the
/// owned waiter of a backend.
macro_rules! async_owned_waiter_tests {
    ($tag:ty, $backend:literal) => {
        #[tokio::test]
        async fn owned_events_async() -> Result<(), $crate::Error> {
            $crate::consumer::shared_tests::owned_events_async($backend, <$crate::consumer::EtwEventWaiter<$tag> as $crate::consumer::NewEventWaiter>::new_owned_events).await
        }

        #[tokio::test]
        async fn owned_events_cancel_when_idle_async() -> Result<(), $crate::Error> {
            $crate::consumer::shared_tests::owned_events_cancel_when_idle_async(
                $backend,
                <$crate::consumer::EtwEventWaiter<$tag> as $crate::consumer::NewEventWaiter>::new_owned_events,
            )
            .await
        }

        #[tokio::test]
        async fn iterate_owned_async() -> Result<(), $crate::Error> {
            $crate::consumer::shared_tests::iterate_owned_async($backend, <$crate::consumer::EtwEventWaiter<$tag> as $crate::consumer::NewEventWaiter>::new_owned_events).await
        }

        #[tokio::test]
        async fn into_stream() -> Result<(), $crate::Error> {
            $crate::consumer::shared_tests::into_stream($backend, <$crate::consumer::EtwEventWaiter<$tag> as $crate::consumer::NewEventWaiter>::new_owned_events).await
        }

        #[tokio::test]
        async fn owned_timed_wait_async() -> Result<(), $crate::Error> {
            $crate::consumer::shared_tests::timed_wait_async(
                concat!($backend, "Owned"),
                |timeout| <$crate::consumer::EtwEventWaiter<$tag> as $crate::consumer::NewEventWaiter>::new_owned_events_with_timeout(16, timeout),
            )
            .await
        }

        #[tokio::test]
        async fn timed_next_async() -> Result<(), $crate::Error> {
            $crate::consumer::shared_tests::timed_next_async(
                concat!($backend, "Owned"),
                |timeout| <$crate::consumer::EtwEventWaiter<$tag> as $crate::consumer::NewEventWaiter>::new_owned_events_with_timeout(16, timeout),
            )
            .await
        }

        #[tokio::test]
        async fn timed_into_stream() -> Result<(), $crate::Error> {
            $crate::consumer::shared_tests::timed_into_stream($backend, |timeout| {
                <$crate::consumer::EtwEventWaiter<$tag> as $crate::consumer::NewEventWaiter>::new_owned_events_with_timeout(16, timeout)
            })
            .await
        }
    };
}
pub(crate) use async_owned_waiter_tests;

/// Generates the tests for the lending async stream of a backend (`EtwEventAsyncStream<$tag>`).
macro_rules! stream_tests {
    ($tag:ty, $backend:literal) => {
        // Compile-time check: `next()` futures are `Send`, so streams work on multi-threaded
        // executors.
        #[allow(dead_code)]
        fn next_is_send(stream: &mut $crate::consumer::EtwEventAsyncStream<$tag>) -> impl Send + '_ {
            $crate::consumer::EventIteratorAsync::next(stream)
        }

        #[tokio::test]
        async fn stream_events() -> Result<(), $crate::Error> {
            $crate::consumer::shared_tests::stream_events($backend, <$crate::consumer::EtwEventAsyncStream<$tag>>::default).await
        }

        #[tokio::test]
        async fn stream_ends_when_trace_completes() -> Result<(), $crate::Error> {
            $crate::consumer::shared_tests::stream_ends_when_trace_completes(
                $backend,
                <$crate::consumer::EtwEventAsyncStream<$tag>>::default,
            )
            .await
        }

        #[tokio::test]
        async fn timed_next_async() -> Result<(), $crate::Error> {
            $crate::consumer::shared_tests::timed_next_async($backend, <$crate::consumer::EtwEventAsyncStream<$tag> as $crate::consumer::NewEventAsyncStream>::new_with_timeout)
                .await
        }
    };
}
pub(crate) use stream_tests;

/// Generates a `#[test]` calling each named shared test with the backend name and a
/// constructor for the waiter.
macro_rules! generate_tests {
    ($new:expr, $backend:literal, $($test:ident),* $(,)?) => {
        $(
            #[test]
            fn $test() -> Result<(), $crate::Error> {
                $crate::consumer::shared_tests::$test($backend, $new)
            }
        )*
    };
}
pub(crate) use generate_tests;
const WAIT: Duration = Duration::from_secs(10);

/// A real-time session (backed by a file) with a registered TraceLogging provider enabled.
pub(crate) struct RealtimeSession {
    pub(crate) name: String,
    pub(crate) provider_guid: GUID,
    provider: Pin<Box<tracelogging_dynamic::Provider>>,
    _session: ControlTraceHandle,
}

impl RealtimeSession {
    pub(crate) fn start(test: &str, backend: &str) -> Result<Self, crate::Error> {
        let name = format!("EtwConsumer-Rust-Tests-{test}-{backend}");
        let provider = register_provider(test, backend);
        let provider_guid = GUID::from_u128(provider.id().to_u128());

        let session = SessionBuilder::new_file_mode(
            &name,
            &crate::test_util::etl_path(&format!("{test}-{backend}.etl")),
            FileMode::Sequential,
        )
        .realtime_event_delivery()
        .start(true)?;
        session.enable_provider(&provider_guid, 0xFF)?;

        Ok(RealtimeSession {
            name,
            provider_guid,
            provider,
            _session: session,
        })
    }

    /// Writes one event per keyword, once the session has enabled the provider.
    pub(crate) fn write(&self, keywords: &[u64]) {
        wait_until_enabled(&self.provider);

        let mut eb = tracelogging_dynamic::EventBuilder::new();
        for &keyword in keywords {
            eb.reset("test event", tracelogging::Level::LogAlways, keyword, 0);
            assert_eq!(eb.write(&self.provider, None, None), 0);
        }
    }
}

fn register_provider(test: &str, backend: &str) -> Pin<Box<tracelogging_dynamic::Provider>> {
    let provider = Box::pin(tracelogging_dynamic::Provider::new(
        &format!("{test}_{backend}_test"),
        &tracelogging_dynamic::Provider::options(),
    ));
    unsafe {
        provider.as_ref().register();
    }
    provider
}

/// Writes events with keywords `1..=count` to a new log file and returns its name and the
/// events' provider ID.
pub(crate) fn write_events_to_file(
    test: &str,
    backend: &str,
    count: u64,
) -> Result<(String, GUID), crate::Error> {
    let file_name = crate::test_util::etl_path(&format!("{test}-{backend}.etl"));
    let provider = register_provider(test, backend);
    let provider_guid = GUID::from_u128(provider.id().to_u128());

    let session = SessionBuilder::new_file_mode(
        &format!("EtwConsumer-Rust-Tests-{test}-{backend}"),
        &file_name,
        FileMode::Sequential,
    )
    .start(true)?;
    session.enable_provider(&provider_guid, 0xFF)?;
    wait_until_enabled(&provider);

    let mut eb = tracelogging_dynamic::EventBuilder::new();
    for keyword in 1..=count {
        eb.reset("test event", tracelogging::Level::LogAlways, keyword, 0);
        assert_eq!(eb.write(&provider, None, None), 0);
    }

    Ok((file_name, provider_guid)) // Dropping `session` stops it, flushing and closing the file
}

struct CallbackObserver<P> {
    processor: P,
    returned: std::sync::mpsc::SyncSender<()>,
}

impl<P: EventProcessor> EventProcessor for CallbackObserver<P> {
    unsafe fn on_event_raw(
        &self,
        evt: *mut windows_sys::Win32::System::Diagnostics::Etw::EVENT_RECORD,
    ) -> Result<(), crate::Error> {
        let result = unsafe { self.processor.on_event_raw(evt) };
        let _ = self.returned.try_send(());
        result
    }

    fn complete(&self, err: crate::Error) {
        self.processor.complete(err);
    }
}

fn idle_owned_trace<W: EventConsumer>(
    test: &str,
    backend: &str,
    new: impl Fn(usize) -> W,
) -> Result<(RealtimeSession, EventTrace<W>), crate::Error> {
    let session = RealtimeSession::start(test, backend)?;
    let mut consumer = new(16);
    let (returned, rx) = std::sync::mpsc::sync_channel(1);
    let thread = ProcessTraceHandle::from_session(
        &session.name,
        CallbackObserver {
            processor: consumer.take_processor(),
            returned,
        },
    )?
    .process_trace()?;
    let trace = EventTrace::new(consumer, thread);
    session.write(&[1]);
    // Cancel only after the processor returned, with no pending or future events to notice it.
    rx.recv_timeout(WAIT).expect("callback did not return");
    Ok((session, trace))
}

pub(crate) fn wait_for_single_event_timeout<W: Waiter>(
    backend: &str,
    new: impl Fn() -> W,
) -> Result<(), crate::Error> {
    // Waits 50ms, which must time out; `f` then returns `keep_going`.
    fn assert_times_out(waiter: &mut impl EventWaiter, keep_going: bool) {
        let start = std::time::Instant::now();
        let mut timed_out = false;
        waiter
            .wait_for_single_event_timeout(Duration::from_millis(50), |evt| {
                assert!(matches!(evt, EventOrTimeout::Timeout), "no event expected");
                timed_out = true;
                keep_going
            })
            .expect("a timeout isn't an error");
        assert!(timed_out);
        assert!(start.elapsed() >= Duration::from_millis(50));
    }

    // Before the trace starts, the waiter holds its own processor, so no event can arrive.
    assert_times_out(&mut new(), true);

    // A real-time session that nothing logs to yet.
    let session = RealtimeSession::start("Timeout", backend)?;
    let mut waiter = EventConsumer::from_session(&session.name, new())?;
    assert_times_out(&mut waiter, true);

    // The trace is still running after a timeout, so waiting again receives later events.
    session.write(&[1]);
    waiter.wait(Some(WAIT), |evt| {
        assert!(guid_eq(
            &evt.get_event_header().ProviderId,
            &session.provider_guid
        ));
        true
    })?;

    // Returning `false` on a timeout stops the trace, like returning it for an event.
    assert_times_out(&mut waiter, false);
    session.write(&[2]);
    waiter
        .wait(Some(WAIT), |_| panic!("trace was cancelled"))
        .unwrap_err();
    let _ = waiter.stop_and_wait();

    Ok(())
}
pub(crate) fn consume_event<W: Waiter>(
    backend: &str,
    new: impl Fn() -> W,
) -> Result<(), crate::Error> {
    let session = RealtimeSession::start("ConsumeEvent", backend)?;
    let provider_guid = session.provider_guid;

    let mut waiter = EventConsumer::from_session(&session.name, new())?;
    // Log a third event to make sure we can quit properly
    session.write(&[1, 2, 2]);

    waiter.wait(Some(WAIT), |evt| {
        // Give the ETW thread a chance to reuse the event's buffers if the callback has already returned.
        std::thread::sleep(Duration::from_millis(200));

        let event_header = evt.get_event_header();
        assert!(guid_eq(&event_header.ProviderId, &provider_guid));
        assert_eq!(event_header.EventDescriptor.Keyword, 1u64);
        true
    })?;

    waiter.wait(None, |evt| {
        let event_header = evt.get_event_header();
        assert!(guid_eq(&event_header.ProviderId, &provider_guid));
        assert_eq!(event_header.EventDescriptor.Keyword, 2u64);
        false
    })?;

    // The closure above returned false, which cancels the trace.
    assert!(waiter.wait(None, |_| true).is_err());

    let _ = waiter.stop_and_wait(); // We don't care about what ProcessTrace returned

    Ok(())
}

pub(crate) fn panicking_handler_releases_event<W: Waiter>(
    backend: &str,
    new: impl Fn() -> W,
) -> Result<(), crate::Error> {
    let session = RealtimeSession::start("PanicRelease", backend)?;

    let mut waiter = EventConsumer::from_session(&session.name, new())?;
    session.write(&[1, 1]);

    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        waiter.wait(Some(WAIT), |_| panic!("handler panicked"))
    }));
    assert!(panicked.is_err());

    // The panic released the ETW callback with E_CANCELLED, so the trace completes rather
    // than staying blocked (which would surface here as a timeout).
    waiter.wait(Some(WAIT), |_| true).unwrap_err();

    let _ = waiter.stop_and_wait();

    Ok(())
}

pub(crate) fn stop_file_trace<W: Waiter>(
    backend: &str,
    new: impl Fn() -> W,
) -> Result<(), crate::Error> {
    let (file_name, provider_guid) = write_events_to_file("StopFile", backend, 200)?;

    let mut waiter = EventConsumer::from_file(&file_name, new())?;
    // Handle one event, then stop while ETW still has most of the file to read.
    waiter.wait(Some(WAIT), |_| true)?;
    waiter.stop();

    // Stopping a log file trace stops delivering its events (except one already being
    // delivered), rather than reading the rest of the file.
    let mut after_stop = 0;
    while waiter
        .wait(Some(WAIT), |evt| {
            if guid_eq(&evt.get_event_header().ProviderId, &provider_guid) {
                after_stop += 1;
            }
            true
        })
        .is_ok()
    {}
    assert!(
        after_stop <= 1,
        "{after_stop} events were delivered after stopping"
    );

    let _ = waiter.stop_and_wait();
    Ok(())
}

pub(crate) fn iterate<W: Iter>(backend: &str, new: impl Fn() -> W) -> Result<(), crate::Error> {
    let session = RealtimeSession::start("Iterate", backend)?;

    let mut waiter = EventConsumer::from_session(&session.name, new())?;
    session.write(&[1; 5]);

    let mut count = 0;
    while let Some(evt) = waiter.next() {
        assert!(guid_eq(
            &evt.get_event_header().ProviderId,
            &session.provider_guid
        ));
        count += 1;

        if count == 1 {
            waiter.stop();
        }
    }
    // Stopping delivers the events ETW had already handed to the consumer, and then ends the
    // iteration. ETW buffers events per CPU, so the others may not have been handed over yet.
    assert!((1..=5).contains(&count), "received {count} events");

    let _ = waiter.stop_and_wait(); // We don't care about what ProcessTrace returned

    Ok(())
}

pub(crate) fn owned_events_are_queued<W: Waiter>(
    backend: &str,
    new: impl Fn(usize) -> W,
) -> Result<(), crate::Error> {
    let (file_name, provider_guid) = write_events_to_file("Owned", backend, 3)?;

    let mut waiter = EventConsumer::from_file(&file_name, new(16))?;

    // Nothing has been received yet, but the callback queued every event instead of
    // blocking on the first one, so ProcessTrace reached the end of the file.
    assert_eq!(waiter.wait_finished(WAIT), Some(S_OK));

    // Events queued before ProcessTrace completed are still delivered.
    let mut keywords = Vec::new();
    let err = loop {
        let received = waiter.wait(Some(WAIT), |evt| {
            assert!(matches!(evt, EventRecord::Owned(_)));
            let header = evt.get_event_header();
            if guid_eq(&header.ProviderId, &provider_guid) {
                keywords.push(header.EventDescriptor.Keyword);
            }
            true
        });
        if let Err(err) = received {
            break err;
        }
    };
    assert_eq!(keywords, [1, 2, 3]);
    // Once the queue is drained, waiting reports how ProcessTrace completed.
    assert_eq!(err.code(), S_OK);

    let _ = waiter.stop_and_wait();

    Ok(())
}

pub(crate) fn owned_events_cancel<W: Waiter>(
    backend: &str,
    new: impl Fn(usize) -> W,
) -> Result<(), crate::Error> {
    // A real-time trace only completes on its own if cancelling stopped it.
    let session = RealtimeSession::start("OwnedCancel", backend)?;
    let provider_guid = session.provider_guid;

    // With room for one event, the callback blocks on a full queue until we cancel.
    let mut waiter = EventConsumer::from_session(&session.name, new(1))?;
    session.write(&[1, 1, 1]);

    let ours = AtomicBool::new(false);
    while !ours.load(Ordering::Relaxed) {
        waiter.wait(Some(WAIT), |evt| {
            assert!(matches!(evt, EventRecord::Owned(_)));
            let is_ours = guid_eq(&evt.get_event_header().ProviderId, &provider_guid);
            ours.store(is_ours, Ordering::Relaxed);
            !is_ours // Cancel on our first event
        })?;
    }

    // The blocked callback was released and the trace completed, rather than continuing
    // (which would surface here as a timeout); the remaining events were discarded.
    waiter.wait(Some(WAIT), |_| true).unwrap_err();

    let _ = waiter.stop_and_wait();

    Ok(())
}

pub(crate) fn owned_events_cancel_when_idle<W: Waiter>(
    backend: &str,
    new: impl Fn(usize) -> W,
) -> Result<(), crate::Error> {
    for (variant, timeout) in [("Untimed", None), ("Timed", Some(WAIT))] {
        let (session, mut waiter) =
            idle_owned_trace("OwnedCancelIdle", &format!("{backend}{variant}"), &new)?;
        waiter.wait(timeout, |evt| {
            assert!(matches!(evt, EventRecord::Owned(_)));
            assert!(guid_eq(
                &evt.get_event_header().ProviderId,
                &session.provider_guid
            ));
            false
        })?;

        waiter
            .wait(Some(WAIT), |_| panic!("trace was cancelled"))
            .unwrap_err();
        let _ = waiter.stop_and_wait();
    }
    Ok(())
}

pub(crate) fn owned_panicking_handler_stops_trace<W: Waiter>(
    backend: &str,
    new: impl Fn(usize) -> W,
) -> Result<(), crate::Error> {
    let (_session, mut waiter) = idle_owned_trace("OwnedPanic", backend, &new)?;

    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        waiter.wait(Some(WAIT), |_| panic!("handler panicked"))
    }));
    assert!(panicked.is_err());

    // The panic stopped the trace, like returning `false`, rather than leaving the idle
    // real-time trace running (which would surface here as a timeout).
    waiter.wait(Some(WAIT), |_| true).unwrap_err();
    let _ = waiter.stop_and_wait();

    Ok(())
}

pub(crate) fn owned_panicking_handler_discards_queue<W: Waiter>(
    backend: &str,
    new: impl Fn(usize) -> W,
) -> Result<(), crate::Error> {
    let (file_name, _) = write_events_to_file("OwnedPanicQueue", backend, 3)?;

    let mut waiter = EventConsumer::from_file(&file_name, new(16))?;
    // Every event is queued before any is handled.
    assert_eq!(waiter.wait_finished(WAIT), Some(S_OK));

    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        waiter.wait(Some(WAIT), |_| panic!("handler panicked"))
    }));
    assert!(panicked.is_err());

    // Like returning `false`, the panic discarded the events still queued.
    waiter
        .wait(Some(WAIT), |_| panic!("queued events were discarded"))
        .unwrap_err();
    let _ = waiter.stop_and_wait();

    Ok(())
}

pub(crate) fn iterate_owned<W: Iter>(
    backend: &str,
    new: impl Fn(usize) -> W,
) -> Result<(), crate::Error> {
    let (file_name, provider_guid) = write_events_to_file("IterateOwned", backend, 3)?;

    let mut waiter = EventConsumer::from_file(&file_name, new(16))?;

    // `next()` returns every event, then `None` once ProcessTrace has completed.
    let mut keywords = Vec::new();
    while let Some(evt) = waiter.next() {
        assert!(matches!(evt, EventRecord::Owned(_)));
        let header = evt.get_event_header();
        if guid_eq(&header.ProviderId, &provider_guid) {
            keywords.push(header.EventDescriptor.Keyword);
        }
    }
    assert_eq!(keywords, [1, 2, 3]);

    // ProcessTrace read the whole file.
    waiter.stop_and_wait()?;

    Ok(())
}

pub(crate) fn into_iter<W>(backend: &str, new: impl Fn(usize) -> W) -> Result<(), crate::Error>
where
    W: EventConsumer + IntoIterator<Item = EventRecord<'static>>,
{
    let (file_name, provider_guid) = write_events_to_file("IntoIter", backend, 3)?;

    // A queue of one event: the iterator keeps pulling events while ProcessTrace runs.
    let waiter = EventConsumer::from_file(&file_name, new(1))?;

    // The iterator yields every event and ends once ProcessTrace has completed; the events
    // outlive the waiter. It owns the trace, which stops when it's dropped.
    let events: Vec<EventRecord<'static>> = waiter.into_iter().collect();
    let keywords: Vec<u64> = events
        .iter()
        .map(EventRecord::get_event_header)
        .filter(|header| guid_eq(&header.ProviderId, &provider_guid))
        .map(|header| header.EventDescriptor.Keyword)
        .collect();
    assert_eq!(keywords, [1, 2, 3]);

    Ok(())
}

pub(crate) async fn wait_for_single_event_async_timeout<W: AsyncWaiter>(
    backend: &str,
    new: impl Fn() -> W,
) -> Result<(), crate::Error> {
    // Waits 50ms, which must time out; `f` then returns `keep_going`.
    async fn assert_times_out(waiter: &mut impl EventWaiterAsync, keep_going: bool) {
        let start = std::time::Instant::now();
        let mut timed_out = false;
        waiter
            .wait_for_single_event_async_timeout(Duration::from_millis(50), |evt| {
                assert!(matches!(evt, EventOrTimeout::Timeout), "no event expected");
                timed_out = true;
                keep_going
            })
            .await
            .expect("a timeout isn't an error");
        assert!(timed_out);
        assert!(start.elapsed() >= Duration::from_millis(50));
    }

    // Before the trace starts, the waiter holds its own processor, so no event can arrive.
    assert_times_out(&mut new(), true).await;

    // A real-time session that nothing logs to yet.
    let session = RealtimeSession::start("TimeoutAsync", backend)?;
    let mut waiter = EventConsumer::from_session(&session.name, new())?;
    assert_times_out(&mut waiter, true).await;

    // The trace is still running after a timeout, so waiting again receives later events.
    session.write(&[1]);
    waiter
        .wait_for_single_event_async_timeout(
            WAIT,
            no_timeout(|evt| {
                assert!(guid_eq(
                    &evt.get_event_header().ProviderId,
                    &session.provider_guid
                ));
                true
            }),
        )
        .await?;

    // Returning `false` on a timeout stops the trace, like returning it for an event.
    assert_times_out(&mut waiter, false).await;
    session.write(&[2]);
    waiter
        .wait_for_single_event_async_timeout(WAIT, |_| panic!("trace was cancelled"))
        .await
        .unwrap_err();
    let _ = waiter.stop_and_wait();

    Ok(())
}
pub(crate) async fn consume_event_async<W: AsyncWaiter>(
    backend: &str,
    new: impl Fn() -> W,
) -> Result<(), crate::Error> {
    // A real-time trace only completes on its own if cancelling stopped it.
    let session = RealtimeSession::start("ConsumeEventAsync", backend)?;
    let provider_guid = session.provider_guid;

    let mut waiter = EventConsumer::from_session(&session.name, new())?;
    session.write(&[1, 2, 3]);

    waiter
        .wait_for_single_event_async_timeout(
            WAIT,
            no_timeout(|evt| {
                let event_header = evt.get_event_header();
                assert!(guid_eq(&event_header.ProviderId, &provider_guid));
                assert_eq!(event_header.EventDescriptor.Keyword, 1u64);
                true
            }),
        )
        .await?;

    waiter
        .wait_for_single_event_async(|evt| {
            let event_header = evt.get_event_header();
            assert!(guid_eq(&event_header.ProviderId, &provider_guid));
            assert_eq!(event_header.EventDescriptor.Keyword, 2u64);
            false
        })
        .await?;

    // The closure above returned false, which cancels the trace, so the third event is never
    // delivered (which would surface here as a timeout or a panic).
    waiter
        .wait_for_single_event_async_timeout(WAIT, |_| panic!("trace was cancelled"))
        .await
        .unwrap_err();

    let _ = waiter.stop_and_wait();

    Ok(())
}

pub(crate) async fn owned_events_async<W: AsyncWaiter>(
    backend: &str,
    new: impl Fn(usize) -> W,
) -> Result<(), crate::Error> {
    let (file_name, provider_guid) = write_events_to_file("OwnedAsync", backend, 3)?;

    let mut waiter = EventConsumer::from_file(&file_name, new(16))?;

    let mut keywords = Vec::new();
    let err = loop {
        let received = waiter
            .wait_for_single_event_async_timeout(
                WAIT,
                no_timeout(|evt| {
                    assert!(matches!(evt, EventRecord::Owned(_)));
                    let header = evt.get_event_header();
                    if guid_eq(&header.ProviderId, &provider_guid) {
                        keywords.push(header.EventDescriptor.Keyword);
                    }
                    true
                }),
            )
            .await;
        if let Err(err) = received {
            break err;
        }
    };
    assert_eq!(keywords, [1, 2, 3]);
    // Once the queue is drained, waiting reports how ProcessTrace completed.
    assert_eq!(err.code(), S_OK);

    waiter.stop_and_wait()?;

    Ok(())
}

pub(crate) async fn owned_events_cancel_when_idle_async<W: AsyncWaiter>(
    backend: &str,
    new: impl Fn(usize) -> W,
) -> Result<(), crate::Error> {
    for (variant, timed) in [("Untimed", false), ("Timed", true)] {
        let (session, mut waiter) =
            idle_owned_trace("OwnedCancelIdleAsync", &format!("{backend}{variant}"), &new)?;
        let cancel = |evt: EventRecord<'_>| {
            assert!(matches!(evt, EventRecord::Owned(_)));
            assert!(guid_eq(
                &evt.get_event_header().ProviderId,
                &session.provider_guid
            ));
            false
        };
        if timed {
            waiter
                .wait_for_single_event_async_timeout(WAIT, no_timeout(cancel))
                .await?;
        } else {
            waiter.wait_for_single_event_async(cancel).await?;
        }

        waiter
            .wait_for_single_event_async_timeout(WAIT, |_| panic!("trace was cancelled"))
            .await
            .unwrap_err();
        let _ = waiter.stop_and_wait();
    }
    Ok(())
}

pub(crate) async fn iterate_owned_async<W: AsyncIter>(
    backend: &str,
    new: impl Fn(usize) -> W,
) -> Result<(), crate::Error> {
    let (file_name, provider_guid) = write_events_to_file("IterateOwnedAsync", backend, 3)?;

    let mut waiter = EventConsumer::from_file(&file_name, new(16))?;

    // `next()` returns every event, then `None` once ProcessTrace has completed.
    let mut keywords = Vec::new();
    while let Some(evt) = waiter.next().await {
        assert!(matches!(evt, EventRecord::Owned(_)));
        let header = evt.get_event_header();
        if guid_eq(&header.ProviderId, &provider_guid) {
            keywords.push(header.EventDescriptor.Keyword);
        }
    }
    assert_eq!(keywords, [1, 2, 3]);

    // ProcessTrace read the whole file.
    waiter.stop_and_wait()?;

    Ok(())
}

#[cfg(any(feature = "futures", feature = "flume-async"))]
pub(crate) async fn into_stream<W: EventConsumer + IntoEventStream<Item = EventRecord<'static>>>(
    backend: &str,
    new: impl Fn(usize) -> W,
) -> Result<(), crate::Error> {
    use futures::StreamExt;

    let (file_name, provider_guid) = write_events_to_file("IntoStream", backend, 3)?;

    // A queue of one event: the stream keeps pulling events while ProcessTrace runs.
    let waiter = EventConsumer::from_file(&file_name, new(1))?;

    // The stream yields every event and ends once ProcessTrace has completed. It owns the
    // trace, which stops when it's dropped.
    let keywords: Vec<u64> = waiter
        .into_stream()
        .filter_map(|evt| async move {
            let header = evt.get_event_header();
            guid_eq(&header.ProviderId, &provider_guid).then_some(header.EventDescriptor.Keyword)
        })
        .collect()
        .await;
    assert_eq!(keywords, [1, 2, 3]);

    Ok(())
}

pub(crate) async fn stream_events<S: AsyncIter>(
    backend: &str,
    new: impl Fn() -> S,
) -> Result<(), crate::Error> {
    let session = RealtimeSession::start("StreamEvents", backend)?;
    let provider_guid = session.provider_guid;

    let mut stream = EventConsumer::from_session(&session.name, new())?;
    session.write(&[1, 1, 1]);

    let mut kept = Vec::new();
    while let Some(evt) = stream.next().await {
        if !guid_eq(&evt.get_event_header().ProviderId, &provider_guid) {
            continue;
        }
        let ext_count = evt.get_extended_data_items().count();

        let serialized = EventRecord::from_serialized(evt.serialize())?;
        assert!(guid_eq(
            &serialized.get_event_header().ProviderId,
            &provider_guid
        ));
        assert_eq!(serialized.get_user_data(), evt.get_user_data());
        assert_eq!(serialized.get_extended_data_items().count(), ext_count);

        kept.push((ext_count, evt.into_owned()));

        // Stopping the trace still delivers the events ETW had already handed to the consumer,
        // and then ends the stream.
        if kept.len() == 1 {
            stream.stop();
        }
    }

    let _ = stream.stop_and_wait(); // We don't care about what ProcessTrace returned

    // Owned copies outlive the trace. ETW buffers events per CPU, so events in buffers it hadn't
    // handed over yet when the trace stopped may be missing.
    assert!(
        (1..=3).contains(&kept.len()),
        "received {} events",
        kept.len()
    );
    for (ext_count, evt) in kept {
        assert!(
            ext_count > 0,
            "TraceLogging events carry schema extended data"
        );
        assert!(guid_eq(&evt.get_event_header().ProviderId, &provider_guid));
        assert_eq!(evt.get_extended_data_items().count(), ext_count);
    }

    Ok(())
}

pub(crate) async fn stream_ends_when_trace_completes<S: AsyncIter>(
    backend: &str,
    new: impl Fn() -> S,
) -> Result<(), crate::Error> {
    let (file_name, provider_guid) = write_events_to_file("StreamComplete", backend, 3)?;

    let mut stream = EventConsumer::from_file(&file_name, new())?;

    let mut keywords = Vec::new();
    while let Some(evt) = stream.next().await {
        let header = evt.get_event_header();
        if guid_eq(&header.ProviderId, &provider_guid) {
            keywords.push(header.EventDescriptor.Keyword);
        }
    }
    assert_eq!(keywords, [1, 2, 3]);

    // ProcessTrace read the whole file.
    stream.stop_and_wait()?;

    Ok(())
}

// Times a call, asserting it took at least `min` and (with generous slack) less than `WAIT`.
fn assert_took<T>(min: Duration, f: impl FnOnce() -> T) -> T {
    let start = std::time::Instant::now();
    let result = f();
    let elapsed = start.elapsed();
    assert!(elapsed >= min, "returned after {elapsed:?}");
    assert!(elapsed < WAIT, "returned after {elapsed:?}");
    result
}

const SHORT: Duration = Duration::from_millis(50);

// Whether `evt` is an event from `session`'s provider with `keyword`.
fn is_ours(evt: &EventRecord<'_>, session: &RealtimeSession, keyword: u64) -> bool {
    let header = evt.get_event_header();
    guid_eq(&header.ProviderId, &session.provider_guid) && header.EventDescriptor.Keyword == keyword
}

pub(crate) fn timed_wait<W: TimedWaiter>(
    backend: &str,
    new: impl Fn(Duration) -> W,
) -> Result<(), crate::Error> {
    // Before the trace starts, no event can arrive, so the waiter times out after its timeout.
    let mut timed_out = false;
    assert_took(SHORT, || {
        new(SHORT).wait_for_single_event(|evt| {
            timed_out = matches!(evt, EventOrTimeout::Timeout);
            true
        })
    })?;
    assert!(timed_out);

    let session = RealtimeSession::start("TimedWait", backend)?;
    let mut waiter = EventConsumer::from_session(&session.name, new(WAIT))?;

    // A timeout passed to the wait overrides the waiter's.
    let mut timed_out = false;
    assert_took(SHORT, || {
        waiter.wait_for_single_event_timeout(SHORT, |evt| {
            timed_out = matches!(evt, EventOrTimeout::Timeout);
            true
        })
    })?;
    assert!(timed_out);

    session.write(&[1]);
    let mut received = false;
    waiter.wait_for_single_event(|evt| {
        received = matches!(evt, EventOrTimeout::Event(evt) if is_ours(&evt, &session, 1));
        true
    })?;
    assert!(received);

    let _ = waiter.stop_and_wait();
    Ok(())
}

pub(crate) fn timed_next<W: TimedIter>(
    backend: &str,
    new: impl Fn(Duration) -> W,
) -> Result<(), crate::Error> {
    // Before the trace starts, no event can arrive, so `next()` times out after the timeout.
    let mut waiter = new(SHORT);
    let evt = assert_took(SHORT, || {
        waiter
            .next()
            .map(|evt| matches!(evt, EventOrTimeout::Timeout))
    });
    assert_eq!(evt, Some(true));

    // A real-time session that receives no events doesn't block `next()` forever.
    let session = RealtimeSession::start("TimedNext", backend)?;
    let mut waiter = EventConsumer::from_session(&session.name, new(SHORT))?;
    let evt = assert_took(SHORT, || {
        waiter
            .next()
            .map(|evt| matches!(evt, EventOrTimeout::Timeout))
    });
    assert_eq!(evt, Some(true));

    // Later events still arrive; `next()` times out until they do.
    session.write(&[1]);
    let start = std::time::Instant::now();
    loop {
        match waiter.next().expect("the trace is still running") {
            EventOrTimeout::Event(evt) if is_ours(&evt, &session, 1) => break,
            _ => assert!(start.elapsed() < WAIT, "no event received"),
        }
    }

    // Once the trace stops, `next()` returns `None`.
    waiter.stop();
    while waiter.next().is_some() {
        assert!(start.elapsed() < WAIT, "the trace didn't stop");
    }

    let _ = waiter.stop_and_wait();
    Ok(())
}

pub(crate) fn timed_into_iter<W>(
    backend: &str,
    new: impl Fn(Duration) -> W,
) -> Result<(), crate::Error>
where
    W: EventConsumer + IntoIterator<Item = EventOrTimeout<'static>>,
{
    let session = RealtimeSession::start("TimedIntoIter", backend)?;
    let mut iter = EventConsumer::from_session(&session.name, new(SHORT))?.into_iter();

    // The iterator yields timeouts while there are no events, rather than blocking.
    let evt = assert_took(SHORT, || iter.next());
    assert!(matches!(evt, Some(EventOrTimeout::Timeout)));

    session.write(&[1]);
    let start = std::time::Instant::now();
    let found = iter.any(|evt| {
        assert!(start.elapsed() < WAIT, "no event received");
        matches!(evt, EventOrTimeout::Event(evt) if is_ours(&evt, &session, 1))
    });
    assert!(found);

    Ok(())
}

pub(crate) async fn timed_wait_async<W: TimedAsyncWaiter>(
    backend: &str,
    new: impl Fn(Duration) -> W,
) -> Result<(), crate::Error> {
    // Before the trace starts, no event can arrive, so the waiter times out after its timeout.
    let start = std::time::Instant::now();
    let mut timed_out = false;
    new(SHORT)
        .wait_for_single_event_async(|evt| {
            timed_out = matches!(evt, EventOrTimeout::Timeout);
            true
        })
        .await?;
    assert!(timed_out);
    assert!((SHORT..WAIT).contains(&start.elapsed()));

    let session = RealtimeSession::start("TimedWaitAsync", backend)?;
    let mut waiter = EventConsumer::from_session(&session.name, new(WAIT))?;

    // A timeout passed to the wait overrides the waiter's.
    let start = std::time::Instant::now();
    let mut timed_out = false;
    waiter
        .wait_for_single_event_async_timeout(SHORT, |evt| {
            timed_out = matches!(evt, EventOrTimeout::Timeout);
            true
        })
        .await?;
    assert!(timed_out);
    assert!((SHORT..WAIT).contains(&start.elapsed()));

    session.write(&[1]);
    let mut received = false;
    waiter
        .wait_for_single_event_async(|evt| {
            received = matches!(evt, EventOrTimeout::Event(evt) if is_ours(&evt, &session, 1));
            true
        })
        .await?;
    assert!(received);

    let _ = waiter.stop_and_wait();
    Ok(())
}

pub(crate) async fn timed_next_async<W: TimedAsyncIter>(
    backend: &str,
    new: impl Fn(Duration) -> W,
) -> Result<(), crate::Error> {
    // A real-time session that receives no events doesn't block `next()` forever.
    let session = RealtimeSession::start("TimedNextAsync", backend)?;
    let mut waiter = EventConsumer::from_session(&session.name, new(SHORT))?;
    let start = std::time::Instant::now();
    assert!(matches!(waiter.next().await, Some(EventOrTimeout::Timeout)));
    assert!((SHORT..WAIT).contains(&start.elapsed()));

    // Later events still arrive; `next()` times out until they do.
    session.write(&[1]);
    let start = std::time::Instant::now();
    loop {
        match waiter.next().await.expect("the trace is still running") {
            EventOrTimeout::Event(evt) if is_ours(&evt, &session, 1) => break,
            _ => assert!(start.elapsed() < WAIT, "no event received"),
        }
    }

    // Once the trace stops, `next()` returns `None`.
    waiter.stop();
    while waiter.next().await.is_some() {
        assert!(start.elapsed() < WAIT, "the trace didn't stop");
    }

    let _ = waiter.stop_and_wait();
    Ok(())
}

#[cfg(any(feature = "futures", feature = "flume-async"))]
pub(crate) async fn timed_into_stream<W>(
    backend: &str,
    new: impl Fn(Duration) -> W,
) -> Result<(), crate::Error>
where
    W: EventConsumer + IntoEventStream<Item = EventOrTimeout<'static>>,
{
    use futures::StreamExt;

    let session = RealtimeSession::start("TimedIntoStream", backend)?;
    let mut stream =
        std::pin::pin!(EventConsumer::from_session(&session.name, new(SHORT))?.into_stream());

    // The stream yields timeouts while there are no events, rather than waiting.
    let start = std::time::Instant::now();
    assert!(matches!(stream.next().await, Some(EventOrTimeout::Timeout)));
    assert!((SHORT..WAIT).contains(&start.elapsed()));

    session.write(&[1]);
    let start = std::time::Instant::now();
    while let Some(evt) = stream.next().await {
        if matches!(evt, EventOrTimeout::Event(evt) if is_ours(&evt, &session, 1)) {
            return Ok(());
        }
        assert!(start.elapsed() < WAIT, "no event received");
    }
    panic!("the stream ended without the event");
}
