//! A basic example of reading events from a log file with the blocking consumers:
//! first lending each event straight from ETW's buffers, then iterating over owned copies with a
//! `for` loop. See `basic_async.rs` for the async consumers.
//!
//! ```powershell
//! cargo run --example basic_sync
//! ```
//!
//! Reads `sample.etl` from the current directory; any ETW log file works, such as one recorded
//! with Windows Performance Recorder (`wpr`). If the file doesn't exist, opening it fails and the
//! example exits with that error.

// `EtwEventWaiter` uses the default backend, `Raw`, which needs no channel crate. To use another
// one, name it, e.g. `EtwEventWaiter::<Crossbeam>::new()` (with the `crossbeam` feature, and
// `use etw_helpers::consumer::{Crossbeam, NewEventWaiter}`).
use etw_helpers::consumer::EtwEventWaiter;
// The consumer APIs are trait methods, so import the traits: `EventConsumer` provides
// `from_file()`, and `EventIterator` provides `next()`.
use etw_helpers::consumer::{EventConsumer, EventIterator, EventRecord};

const FILE_NAME: &str = "sample.etl";

fn main() -> Result<(), etw_helpers::Error> {
    read_lent_events()?;
    read_owned_events()?;
    Ok(())
}

/// Lent events are read straight from ETW's buffers, without copying. ETW waits while you handle
/// each event, so this is the most efficient way to read events one at a time.
fn read_lent_events() -> Result<(), etw_helpers::Error> {
    // Open the log file and start reading it on a background thread. The events go to an
    // `EtwEventWaiter`, which lends each event to its caller. The returned trace owns the
    // background thread, and has the same methods as the `EtwEventWaiter`.
    let mut waiter = EventConsumer::from_file(FILE_NAME, EtwEventWaiter::new())?;

    let mut count = 0;
    let mut kept = Vec::new();
    // `next()` blocks until the next event arrives, and returns `None` once the whole file has
    // been read.
    while let Some(evt) = waiter.next() {
        count += 1;
        if count <= 3 {
            print_event("lent", &evt);
        }

        // The event borrows `waiter`, so it can't be kept past this loop iteration (the compiler
        // enforces this). To keep an event, copy it with `into_owned()`.
        if kept.len() < 10 {
            kept.push(evt.into_owned());
        }
    }
    println!(
        "Read {count} lent events, and kept copies of {}",
        kept.len()
    );

    // Stop the trace and wait for the background thread to exit. This reports any error that
    // stopped ETW from reading the whole file. (Dropping `waiter` also stops the trace, but
    // without waiting or reporting errors.)
    waiter.stop_and_wait()
}

/// Owned events are copied out of ETW's buffers into a queue, so ETW can keep reading the file
/// while you handle earlier events, and the events can be kept without copying them again.
fn read_owned_events() -> Result<(), etw_helpers::Error> {
    // An owned-events waiter queues copies of up to 1024 events. ETW only waits for you when the
    // queue is full.
    let consumer = EtwEventWaiter::new_owned_events(1024);

    // Open the log file and start reading it on a background thread, as above.
    let waiter = EventConsumer::from_file(FILE_NAME, consumer)?;

    let mut count = 0;
    let mut largest: Option<EventRecord<'static>> = None;
    // A trace of owned events converts into a standard iterator, so it works with `for` loops and
    // iterator combinators. The loop ends once the whole file has been read, and the trace stops
    // when the loop is done with it.
    for evt in waiter {
        count += 1;

        // Owned events don't borrow anything, so they can be kept as they are.
        if largest
            .as_ref()
            .is_none_or(|largest| evt.get_user_data().len() > largest.get_user_data().len())
        {
            largest = Some(evt);
        }
    }
    println!("Read {count} owned events");
    if let Some(evt) = &largest {
        print_event("largest", evt);
    }

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
