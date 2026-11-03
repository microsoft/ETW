//! A basic example of reading events from a log file with the async consumers: first with a
//! lending async stream, then with a standard `futures::Stream` of owned events. See
//! `basic_sync.rs` for the blocking consumers.
//!
//! ```powershell
//! cargo run --example basic_async
//! ```
//!
//! Reads `sample.etl` from the current directory; any ETW log file works, such as one recorded
//! with Windows Performance Recorder (`wpr`). If the file doesn't exist, opening it fails and the
//! example exits with that error.

// `EtwEventAsyncStream` uses the default async backend, `Futures` (with the `futures` feature).
// For async owned events, the waiter names the `Futures` backend: the default backend, `Raw`, is
// blocking only.
use etw_helpers::consumer::{EtwEventAsyncStream, EtwEventWaiter, EventRecord, Futures};
// The consumer APIs are trait methods, so import the traits. The prelude has them all, including
// `EventConsumer` (`from_file()`), `EventIteratorAsync` (the lending stream's `next()`),
// `IntoEventStream` (`into_stream()`) and `NewEventWaiter` (constructors for a named backend).
use etw_helpers::consumer::prelude::*;
// `StreamExt` provides the standard stream combinators, such as `fold`.
use futures::StreamExt;

const FILE_NAME: &str = "sample.etl";

fn main() -> Result<(), etw_helpers::Error> {
    // The async consumers work with any executor. This example uses the simple one from the
    // `futures` crate, which runs the future to completion on the current thread.
    futures::executor::block_on(async {
        read_lending_stream().await?;
        read_owned_stream().await
    })
}

/// A lending stream reads events straight from ETW's buffers, without copying, and waits for
/// each one asynchronously. ETW waits while you handle each event.
async fn read_lending_stream() -> Result<(), etw_helpers::Error> {
    // Open the log file and start reading it on a background thread. The events go to an
    // `EtwEventAsyncStream`, which lends each event to its caller. The returned trace owns the
    // background thread, and has the same methods as the `EtwEventAsyncStream`.
    let mut stream = EventConsumer::from_file(FILE_NAME, EtwEventAsyncStream::new())?;

    let mut count = 0;
    // `next().await` waits for the next event, and returns `None` once the whole file has been
    // read. Each event borrows `stream`, so it can't be kept past this loop iteration (copy it
    // with `into_owned()` to keep it). That's also why this isn't a `futures::Stream`, whose items
    // can't borrow from the stream.
    while let Some(evt) = stream.next().await {
        count += 1;
        if count <= 3 {
            print_event("lent", &evt);
        }
    }
    println!("Read {count} lent events");

    // Stop the trace and wait for the background thread to exit. This reports any error that
    // stopped ETW from reading the whole file; ETW has already finished, so it doesn't block for
    // long. (Dropping `stream` also stops the trace, but without waiting or reporting errors.)
    stream.stop_and_wait()
}

/// Owned events are copied out of ETW's buffers into a queue, so they can be delivered as a
/// standard `futures::Stream`.
async fn read_owned_stream() -> Result<(), etw_helpers::Error> {
    // An owned-events waiter queues copies of up to 1024 events. ETW only waits for you when the
    // queue is full.
    let consumer = EtwEventWaiter::<Futures>::new_owned_events(1024);

    // Open the log file and start reading it on a background thread, as above.
    let waiter = EventConsumer::from_file(FILE_NAME, consumer)?;

    // `into_stream()` converts the trace into a `futures::Stream` of owned events, which ends once
    // the whole file has been read; the trace stops when the stream is dropped. Here, `fold`
    // counts the events and their payload bytes.
    let (count, payload_bytes) = waiter
        .into_stream()
        .fold((0, 0), |(count, bytes), evt| async move {
            (count + 1, bytes + evt.get_user_data().len())
        })
        .await;
    println!("Read {count} owned events with {payload_bytes} bytes of payload in total");

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
