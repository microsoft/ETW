//! Profiling workload for the async stream: writes a steady stream of events to a real-time
//! ETW session and consumes them with `EtwEventAsyncStream<Futures>`. See `profile_futures` for
//! `EtwEventWaiter<Futures>`.
//!
//! ```powershell
//! cargo run --release --example profile_futures_stream --features hotpath
//! ```
//!
//! Runs for `PROFILE_SECONDS` (default 10) at `PROFILE_EVENTS_PER_SECOND` (default 50000),
//! long enough for hotpath's 250 ms thread CPU samples to average out (Windows accounts
//! thread CPU time in ~15.6 ms ticks).
//!
//! Needs rights to start ETW sessions (admin or Performance Log Users).

use std::pin::Pin;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use etw_helpers::consumer::{EtwEventAsyncStream, Futures};
use etw_helpers::consumer::{EventConsumer, EventIteratorAsync};
use etw_helpers::controller::{ControlTraceHandle, FileMode, SessionBuilder};

// Real-time delivery of the first events can take a couple of seconds (buffer flush timer).
const TIMEOUT: Duration = Duration::from_secs(10);

type Provider = Pin<Box<tracelogging_dynamic::Provider>>;

// windows-sys GUIDs don't implement PartialEq.
fn same_guid(a: &windows_sys::core::GUID, b: &windows_sys::core::GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

fn env_or(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn event_count() -> usize {
    (env_or("PROFILE_SECONDS", 10) * env_or("PROFILE_EVENTS_PER_SECOND", 50_000)) as usize
}

fn start_session(
    name: &str,
) -> Result<(ControlTraceHandle, Provider, std::path::PathBuf), etw_helpers::Error> {
    let target_dir = std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".to_string());
    let file_path = std::path::Path::new(&target_dir).join(format!("profile_{name}.etl"));

    let provider = Box::pin(tracelogging_dynamic::Provider::new(
        &format!("etw_helpers_profile_{name}"),
        &tracelogging_dynamic::Provider::options(),
    ));
    unsafe {
        provider.as_ref().register();
    }
    let provider_guid = windows_sys::core::GUID::from_u128(provider.id().to_u128());

    let session = SessionBuilder::new_file_mode(
        &format!("EtwConsumer-Rust-Profile-{name}"),
        &file_path.to_string_lossy(),
        FileMode::Sequential,
    )
    .buffer_size_kb(512)
    .buffer_counts(32, 64)
    .realtime_event_delivery()
    .start(true)?;
    session.enable_provider(&provider_guid, 0xFF)?;

    let deadline = Instant::now() + Duration::from_secs(10);
    while !provider.enabled(tracelogging::Level::LogAlways, 0) {
        assert!(Instant::now() < deadline, "provider was never enabled");
        std::thread::sleep(Duration::from_millis(10));
    }

    Ok((session, provider, file_path))
}

// Writes `count` events at a steady rate, then stops the session once the consumer reports
// it is done (or `TIMEOUT` later, since real-time delivery can drop events), which ends
// ProcessTrace and so unblocks a consumer still waiting for events.
fn spawn_writer(
    provider: Provider,
    session: ControlTraceHandle,
    count: usize,
    done_rx: mpsc::Receiver<()>,
) -> JoinHandle<()> {
    let rate = env_or("PROFILE_EVENTS_PER_SECOND", 50_000) as f64;
    std::thread::spawn(move || {
        let mut eb = tracelogging_dynamic::EventBuilder::new();
        let start = Instant::now();
        let mut written = 0;
        while written < count {
            // Catch up on every event due so far, so coarse sleeps don't lower the rate.
            let due = ((start.elapsed().as_secs_f64() * rate) as usize).min(count);
            for i in written..due {
                eb.reset("profile event", tracelogging::Level::LogAlways, 1, 0);
                eb.add_u32("index", i as u32, tracelogging::OutType::Default, 0);
                eb.add_str8(
                    "payload",
                    "the quick brown fox jumps over the lazy dog",
                    tracelogging::OutType::Default,
                    0,
                );
                eb.write(&provider, None, None);
            }
            written = due;
            std::thread::sleep(Duration::from_millis(1));
        }

        let _ = done_rx.recv_timeout(TIMEOUT);
        drop(session);
    })
}

async fn profile_stream() -> Result<usize, etw_helpers::Error> {
    let name = "FuturesStream";
    let count = event_count();
    let (session, provider, file_path) = start_session(name)?;
    let provider_guid = windows_sys::core::GUID::from_u128(provider.id().to_u128());

    let session_name = format!("EtwConsumer-Rust-Profile-{name}");
    let mut stream =
        EventConsumer::from_session(&session_name, EtwEventAsyncStream::<Futures>::new())?;

    let (done_tx, done_rx) = mpsc::channel();
    let writer = spawn_writer(provider, session, count, done_rx);

    // No per-event timeout: if events are dropped, the writer stops the session, which ends
    // the stream.
    let mut received = 0;
    while received < count {
        let Some(evt) = stream.next().await else {
            break;
        };
        if same_guid(&evt.get_event_header().ProviderId, &provider_guid) {
            received += 1;
            std::hint::black_box(evt.into_owned());
        }
    }

    let _ = done_tx.send(());
    // Releases the last lent event and stops the trace.
    let _ = stream.stop_and_wait();
    writer.join().expect("writer thread panicked");
    let _ = std::fs::remove_file(file_path);

    Ok(received)
}

#[cfg_attr(feature = "hotpath", hotpath::main)]
fn main() -> Result<(), etw_helpers::Error> {
    let count = event_count();
    println!(
        "received {} of {count} events",
        futures::executor::block_on(profile_stream())?
    );
    Ok(())
}
