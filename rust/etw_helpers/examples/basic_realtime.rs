//! A basic example of reading events from a real-time session with the blocking consumers,
//! using timeouts so the program stays responsive while the session is quiet. See
//! `basic_sync.rs` for reading log files.
//!
//! ```powershell
//! cargo run --example basic_realtime
//! ```
//!
//! Reads events from a running real-time session named `SampleSession`, which this example
//! doesn't create. For example, start one with
//! `logman start SampleSession -p Microsoft-Windows-Kernel-Process -rt -ets`, and stop it with
//! `logman stop SampleSession -ets`. If the session doesn't exist, ETW reports an error once the
//! first trace starts (`ERROR_WMI_INSTANCE_NOT_FOUND`), and the example exits with it. Reading a
//! real-time session requires administrator rights or membership in the "Performance Log Users" group.

use std::time::{Duration, Instant};

// `EtwEventWaiter` uses the default backend, `Raw`; see `basic_sync.rs` for choosing another.
use etw_helpers::consumer::EtwEventWaiter;
// The consumer APIs are trait methods, so import the traits: `EventConsumer` provides
// `from_session()`, `EventIterator` provides `next()`, and `EventWaiter` provides
// `wait_for_single_event_timeout()`.
use etw_helpers::consumer::{
    EventConsumer, EventIterator, EventOrTimeout, EventRecord, EventWaiter,
};

const SESSION_NAME: &str = "SampleSession";

// How long to wait for each event before doing something else.
const TIMEOUT: Duration = Duration::from_secs(1);

fn main() -> Result<(), etw_helpers::Error> {
    read_lent_events()?;
    read_owned_events()?;
    handle_events_with_a_closure()?;
    Ok(())
}

/// Lent events are read straight from ETW's buffers, without copying. A waiter created with
/// `new_with_timeout` stops waiting after the timeout, so `next()` can't block forever on a
/// session that receives no events.
fn read_lent_events() -> Result<(), etw_helpers::Error> {
    // Connect to the running session and start processing its events on a background thread.
    // A real-time session has no end, so `next()` would normally block until another event
    // arrives; with a timeout, it returns `EventOrTimeout::Timeout` after a second instead.
    let mut waiter =
        EventConsumer::from_session(SESSION_NAME, EtwEventWaiter::new_with_timeout(TIMEOUT))?;

    let start = Instant::now();
    let mut count = 0;
    let mut stopping = false;
    // `next()` returns `None` once the trace has stopped.
    while let Some(evt) = waiter.next() {
        match evt {
            EventOrTimeout::Event(evt) => {
                count += 1;
                if count <= 3 {
                    print_event("lent", &evt);
                }
            }
            // No event arrived within the timeout. The session is still running, so we can do
            // other work here and then wait again.
            EventOrTimeout::Timeout => println!("No events in the last second"),
        }

        // Stop reading after 10 seconds. A real-time trace still delivers the events ETW has
        // already buffered, so keep calling `next()` until it returns `None`.
        if !stopping && start.elapsed() > Duration::from_secs(10) {
            waiter.stop();
            stopping = true;
        }
    }
    println!("Read {count} lent events");

    // Wait for the background thread to exit, and report any error that stopped it.
    waiter.stop_and_wait()
}

/// Owned events are copied into a queue, so ETW can keep delivering events while you handle
/// earlier ones. A trace of owned events converts into a standard iterator, and with a timeout
/// it yields `EventOrTimeout::Timeout` while the session is quiet.
fn read_owned_events() -> Result<(), etw_helpers::Error> {
    // Queue copies of up to 1024 events, and give up waiting for each one after a second.
    let consumer = EtwEventWaiter::new_owned_events_with_timeout(1024, TIMEOUT);
    let trace = EventConsumer::from_session(SESSION_NAME, consumer)?;

    let mut count = 0;
    let mut quiet_seconds = 0;
    for evt in trace {
        match evt {
            // Owned events don't borrow anything, so they could be kept as they are.
            EventOrTimeout::Event(evt) => {
                count += 1;
                quiet_seconds = 0;
                if count <= 3 {
                    print_event("owned", &evt);
                }
            }
            EventOrTimeout::Timeout => quiet_seconds += 1,
        }

        // Stop once the session has been quiet for 5 seconds. Leaving the loop drops the
        // iterator, which stops the trace.
        if quiet_seconds == 5 {
            println!("No events for 5 seconds");
            break;
        }
    }
    println!("Read {count} owned events");

    Ok(())
}

/// Instead of `next()`, you can handle one event at a time with a closure. Passing a timeout to
/// `wait_for_single_event_timeout` works with any waiter (and overrides the timeout of one created
/// with `new_with_timeout`): the closure receives `EventOrTimeout::Timeout` if no event arrives in
/// time, and returning `false` from it stops the trace.
fn handle_events_with_a_closure() -> Result<(), etw_helpers::Error> {
    let mut waiter = EventConsumer::from_session(SESSION_NAME, EtwEventWaiter::new())?;

    let start = Instant::now();
    let mut count = 0;
    loop {
        let result = waiter.wait_for_single_event_timeout(TIMEOUT, |evt| {
            match evt {
                EventOrTimeout::Event(evt) => {
                    count += 1;
                    if count <= 3 {
                        print_event("closure", &evt);
                    }
                }
                EventOrTimeout::Timeout => println!("Still waiting for events"),
            }
            // Keep going for 10 seconds; after that, returning `false` stops the trace.
            start.elapsed() < Duration::from_secs(10)
        });

        // A timeout isn't an error: waiting only fails once the trace has stopped.
        if result.is_err() {
            break;
        }
    }
    println!("Handled {count} events with a closure");

    // The closure stopped the trace, so ProcessTrace's result isn't interesting here.
    let _ = waiter.stop_and_wait();
    Ok(())
}

/// Prints a summary of an event: its provider, ID and payload size.
fn print_event(label: &str, evt: &EventRecord) {
    let header = evt.get_event_header();
    // windows-sys GUIDs don't implement `Display`, so format the provider ID by hand.
    let id = header.ProviderId;
    let d = id.data4;
    let provider = format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        id.data1, id.data2, id.data3, d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]
    );
    println!(
        "{label}: provider {provider}, event ID {}, {} bytes of payload",
        header.EventDescriptor.Id,
        evt.get_user_data().len()
    );
}
