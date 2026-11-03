# Rust helpers for ETW (Event Tracing for Windows)

## Compared to other ETW event consumer crates

There are several other crates that provide ETW functionality, especially for consuming events. Those crates, however,
generally mirror the C API's callback pattern for receiving events, which can be cumbersome to use in Rust. To work around
ProcessTrace's threading model and event record lifetimes, they either invoke the callback on a separate thread, or copy the
entire event into a buffer so it can be sent to a different thread, or both. These are all valid approaches that work well,
but they are not idiomatic Rust.

This crate aims to provide a more idiomatic Rust interface for ETW by offering event consumers that can be iterated over
with a simple loop, or async waited on. The crate will handle the threading  and lifetime issues internally, and will
avoid making any copies of the event data unless explicitly requested.

## Features

The event consumers (`EtwEventWaiter`, `EtwEventWaiterOwned` and `EtwEventAsyncStream`) take a backend type parameter
that selects how events are handed over from the ETW thread: `EtwEventWaiter<Crossbeam>`, and so on. The backends'
own types are implementation details. The default, `Raw`, needs no channel crate and is always available; the others
are enabled by features, so you can match your existing dependencies or get async APIs.

| Feature | Enabled by default | Provides |
|---|---|---|
| `std` | yes | Everything that needs the standard library, which every feature below implies. Without it, the crate is `no_std` (it still needs `alloc`) and only the `Raw` backend is available. (The ProcessTrace thread still uses `std` internally for now.) |
| `crossbeam` | yes | The `Crossbeam` backend: blocking consumers built on [crossbeam-channel](https://crates.io/crates/crossbeam-channel) |
| `flume` | no | The `Flume` backend: the same API built on [flume](https://crates.io/crates/flume) |
| `flume-async` | no | `flume`, plus flume's async channels: `EventWaiterAsync` for the `Flume` waiters (and `EventIteratorAsync` and `into_stream()` for the owned one), and `EtwEventAsyncStream<Flume>` |
| `futures` | yes | The `Futures` backend: async consumers that work with any executor (with blocking versions too), and the default `EtwEventAsyncStream` |
| `hotpath` | no | Profiling instrumentation via [hotpath-rs](https://hotpath.rs); without this feature, hotpath isn't a dependency at all |
| `hotpath-alloc` | no | `hotpath`, plus per-function allocation tracking |

To use flume, enable its feature. Disable the default features as well if you don't also want crossbeam and futures:

```toml
[dependencies]
etw_helpers = { git = "https://github.com/microsoft/etw", default-features = false, features = ["flume"] }
```

`EtwEventWaiter::new()` uses the default backend. To choose one, name it, as in `EtwEventWaiter::<Crossbeam>::new()`;
that constructor comes from the `NewEventWaiter` trait, so import it (or `etw_helpers::consumer::prelude::*`, which has
every consumer trait). Switching backends then only changes the type parameter.

Everything else is available regardless of features. Session control is in `etw_helpers::controller`, and reading events
(including the traits below) in `etw_helpers::consumer`:

- **Session control**: start sessions with `SessionBuilder` (file or in-memory buffering mode, optionally with real-time
  delivery), attach to running ones with `get_existing_session`, and enable or disable providers. A
  `ControlTraceHandle` stops its session when dropped, unless you call `manual_stop()`.
- **Event processing**: `EventConsumer::from_file` / `from_session` consume a `.etl` file or a real-time session on a
  background thread, returning an `EventTrace` with the consumer's APIs. Underneath, `ProcessTraceHandle` passes events
  to any `EventProcessor`; implement one yourself to handle raw `EVENT_RECORD`s directly on the ETW thread.
- **Event records**: `EventRecord` exposes the header, payload and extended data items without copying. Call
  `into_owned()` to keep an event, or `serialize()` / `EventRecord::from_serialized()` to convert it to and from bytes in
  the `TDH_SERIALIZED_EVENT_RECORD` format used by `TdhEx1SerializeEventRecord`.
- **A dependency-free backend**: `Raw`, the default, has the same API as `Crossbeam` and `Flume`, but synchronizes
  through Win32 SRW locks and condition variables instead of a channel crate. It uses only `core`, `alloc` and Win32
  APIs, with a view to supporting `no_std` environments, and is the fastest backend for lent events.

Win32 types such as `EVENT_RECORD` and `GUID` come from [windows-sys](https://crates.io/crates/windows-sys). Errors are
`etw_helpers::Error`, which wraps an `HRESULT` (`code()`); Win32 error codes are converted with `HRESULT_FROM_WIN32`
(`etw_helpers::hresult_from_win32`). windows-sys GUIDs don't implement `PartialEq`, so compare their fields (`data1`
to `data4`) to check an event's `ProviderId`.

The consumers' APIs are trait methods, so import the traits (`use etw_helpers::consumer::prelude::*;` imports them all).
Which ones a consumer implements depends on its backend:

| Trait | Methods | Implemented by |
|---|---|---|
| `EventConsumer` | `from_file()` / `from_session()`, which start a trace and return an `EventTrace` | every consumer |
| `NewEventWaiter` / `NewEventAsyncStream` | `new()`, `new_owned_events()` and their `_with_timeout` versions, for a named backend | `EtwEventWaiter<B>` / `EtwEventAsyncStream<B>` |
| `EventIterator` | lending `next()` | the `Raw`, `Crossbeam` and `Flume` waiters; every owned waiter (blocking for `Futures`) |
| `EventIteratorAsync` | lending `async` `next()` | `EtwEventAsyncStream`; the `Futures` owned waiter, and the `Flume` one with `flume-async` |
| `EventWaiter` | `wait_for_single_event`, `wait_for_single_event_timeout` | every waiter (blocking for `Futures`) |
| `EventWaiterAsync` | `wait_for_single_event_async`, `wait_for_single_event_async_timeout` | the `Futures` waiters, and the `Flume` ones with `flume-async` |
| `IntoEventStream` | `into_stream()`, a `futures::Stream` of owned events | the `Futures` owned waiter, and the `Flume` one with `flume-async` |

An `EventTrace<C>` owns the background thread and implements each of these traits (and `IntoIterator`) that its
consumer `C` does. Dropping it stops the trace, and `stop()` stops it without dropping the consumer (a real-time trace still delivers its
buffered events); `stop_and_wait()` also waits for the thread and returns ProcessTrace's
result.

Each waiter comes in two kinds:

- `EtwEventWaiter::new()` lends each event straight from the ETW buffer, with no copies. ProcessTrace waits until you
  move on to the next event, and the borrow checker rejects holding an event across calls to `next()`.
- `EtwEventWaiter::new_owned_events(event_buffer_count)` returns an `EtwEventWaiterOwned`, which copies each event into a queue of up to
  `event_buffer_count` events, so ProcessTrace only waits when the queue is full. Its events are always
  `EventRecord::Owned`, so it also converts into a standard `Iterator` (`into_iter()`, or a `for` loop) and, for the
  async ones, a `futures::Stream` (`into_stream()`).

Every consumer can also be created with a timeout (`new_with_timeout(timeout)` and
`new_owned_events_with_timeout(event_buffer_count, timeout)`), which returns it wrapped in `WithTimeout<C>`. It's used the same way, but it gives up waiting for an event after
the timeout: `next()`, `into_iter()`, `into_stream()` and the `wait_for_single_event*` closures then get
`EventOrTimeout::Timeout` instead of `EventOrTimeout::Event(evt)`. That way, a real-time session that receives no events
can't block `next()` until something else stops the session. The traits' `Event`/`Item` types are `EventRecord` for
plain consumers and `EventOrTimeout` for timed ones.

## Usage

Starting or consuming ETW sessions requires administrator rights or membership in the "Performance Log Users" group.

For complete, commented programs, see `examples/basic_sync.rs` (lent events, and owned events in a `for` loop) and
`examples/basic_async.rs` (a lending async stream, and a `futures::Stream` of owned events), which read `sample.etl`, and
`examples/basic_realtime.rs` (reading a running real-time session named `SampleSession` with timeouts):

```powershell
cargo run --example basic_sync
cargo run --example basic_async
cargo run --example basic_realtime
```

### Reading events from a log file

```rust
use etw_helpers::consumer::prelude::*;
use etw_helpers::consumer::EtwEventWaiter;

fn main() -> Result<(), etw_helpers::Error> {
    // Reads the file on a background thread, delivering its events to the waiter.
    let mut waiter = EventConsumer::from_file("trace.etl", EtwEventWaiter::new())?;

    // next() returns None once the end of the file is reached.
    let mut kept = Vec::new();
    while let Some(evt) = waiter.next() {
        let header = evt.get_event_header();
        println!("event {} with {} bytes of payload", header.EventDescriptor.Id, evt.get_user_data().len());

        // Events borrow from the ETW buffer; copy the ones you want to keep.
        if header.EventDescriptor.Id == 1 {
            kept.push(evt.into_owned());
        }
    }
    println!("kept {} events", kept.len());

    // Waits for the background thread and reports any error that stopped it reading the file.
    waiter.stop_and_wait()
}
```

Instead of `next()`, you can handle one event at a time with a closure. Returning `false` stops the trace. With a
timeout (which overrides any timeout from `new_with_timeout`), the closure gets `EventOrTimeout::Timeout` if no event
arrives in time, and you can do other work before waiting again:

```rust
use etw_helpers::consumer::{EventOrTimeout, EventWaiter};

loop {
    let result = waiter.wait_for_single_event_timeout(Duration::from_secs(5), |evt| {
        match evt {
            EventOrTimeout::Event(evt) => println!("event {}", evt.get_event_header().EventDescriptor.Id),
            EventOrTimeout::Timeout => println!("no events for 5 seconds"),
        }
        true // keep going
    });
    if result.is_err() {
        break; // ProcessTrace has completed
    }
}
```

### Buffering events

If handling events can be slow, let ProcessTrace run ahead by queueing copies of the events:

```rust
use etw_helpers::consumer::Crossbeam;

// Any backend works; this one names Crossbeam.
let waiter = EventConsumer::from_file("trace.etl", EtwEventWaiter::<Crossbeam>::new_owned_events(1024))?;

// A standard iterator, which ends once ProcessTrace has completed.
for evt in waiter {
    // Always an owned copy, so it can be kept without calling into_owned()
}
```

### Async

With the `futures` feature, `EtwEventAsyncStream` offers the same lending `next()` as an `async fn`
(`EventIteratorAsync`), and `EtwEventWaiter<Futures>` offers the closure API (`EventWaiterAsync`, plus blocking versions
through `EventWaiter`). `EtwEventWaiter::<Futures>::new_owned_events(n)` queues copies, and `into_stream()`
(`IntoEventStream`) turns it into a `futures::Stream`.
Timeouts use [futures-timer](https://crates.io/crates/futures-timer), so they don't
depend on a particular executor.

```rust
use etw_helpers::consumer::prelude::*;
use etw_helpers::consumer::EtwEventAsyncStream;

let mut stream = EventConsumer::from_file("trace.etl", EtwEventAsyncStream::new())?;

while let Some(evt) = stream.next().await {
    println!("event {}", evt.get_event_header().EventDescriptor.Id);
}

stream.stop_and_wait()?;
```

With the `flume-async` feature, `EtwEventAsyncStream::<Flume>::new()` works the same way (it's the default stream when
`futures` is disabled), and the `Flume` waiters gain the same async APIs as the `Futures` ones.

### Consuming a real-time session

To receive events as they are logged, start a session with real-time delivery and use
`EventConsumer::from_session` instead of `from_file`. A real-time trace runs until it is stopped, so `next()` keeps
waiting for new events:

```rust
use etw_helpers::controller::{FileMode, SessionBuilder};
use windows_sys::core::GUID;

const SESSION_NAME: &str = "MyTraceSession";
let provider = GUID::from_u128(0x00000000_0000_0000_0000_000000000000); // Your provider's GUID

// Log to trace.etl and also deliver events in real time. Dropping `session` stops it.
let session = SessionBuilder::new_file_mode(SESSION_NAME, "trace.etl", FileMode::Sequential)
    .realtime_event_delivery()
    .start(true)?; // true: replace a running session with the same name
session.enable_provider(&provider, 0xFF)?; // 0xFF: all levels

let mut waiter = EventConsumer::from_session(SESSION_NAME, EtwEventWaiter::new())?;
```

### Profiling

The `profile_*` examples run a steady stream of events through each consumer and print a
[hotpath](https://hotpath.rs) report with function timings, channel throughput and latency, and per-thread CPU usage:

```powershell
cargo run --release --example profile_crossbeam --features hotpath
cargo run --release --example profile_flume --features flume,hotpath
cargo run --release --example profile_raw --features hotpath
cargo run --release --example profile_futures --features hotpath
cargo run --release --example profile_futures_stream --features hotpath
```

Each run lasts `PROFILE_SECONDS` (default 10) per phase at `PROFILE_EVENTS_PER_SECOND` (default 50000). Use
`--features hotpath-alloc` to include allocations.
