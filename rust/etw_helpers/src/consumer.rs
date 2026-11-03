//! Reading events from log files and real-time sessions.
//!
//! Consume a trace with one of the event consumers: open a log file or real-time session with
//! [`EventConsumer::from_file`] or [`EventConsumer::from_session`], then read events through the
//! returned [`EventTrace`] with the consumer traits ([`EventIterator`], [`EventWaiter`], and their
//! async versions). Import them all with the [`prelude`].
//!
//! The consumers are [`EtwEventWaiter`] (lends each event), [`EtwEventWaiterOwned`] (queues copies
//! of events) and `EtwEventAsyncStream` (a lending async stream). Their type parameter selects a
//! [`Backend`]: [`Raw`], the default, is always available, and `Crossbeam`, `Flume` and `Futures` are
//! enabled by the features of the same names.
//!
//! To handle raw events on the ProcessTrace thread yourself, implement [`EventProcessor`] and pass
//! it to [`ProcessTraceHandle`].

mod backend;
mod event_record;
mod event_trace;
mod event_waiter;
mod lock;
mod processtrace;
#[cfg(test)]
mod shared_tests;

// The backends behind the `Backend` tags. Their types are implementation details.
#[cfg(feature = "crossbeam")]
mod crossbeam;
#[cfg(feature = "flume")]
mod flume;
#[cfg(feature = "futures")]
mod futures;
mod raw;

/// The consumer traits, whose methods the consumers' APIs are, and the constructor traits:
/// `use etw_helpers::consumer::prelude::*;`.
pub mod prelude {
    pub use super::{
        EventConsumer, EventIterator, EventIteratorAsync, EventWaiter, EventWaiterAsync,
        NewEventWaiter,
    };
    #[cfg(any(feature = "futures", feature = "flume-async"))]
    pub use super::{IntoEventStream, NewEventAsyncStream};
}

#[cfg(feature = "crossbeam")]
pub use backend::Crossbeam;
#[cfg(feature = "flume")]
pub use backend::Flume;
#[cfg(feature = "futures")]
pub use backend::Futures;
pub use backend::{
    Backend, EtwEventWaiter, EtwEventWaiterOwned, NewEventWaiter, Raw, StreamBackend,
};
#[cfg(any(feature = "futures", feature = "flume-async"))]
pub use backend::{DefaultStreamBackend, EtwEventAsyncStream, NewEventAsyncStream};
pub use event_record::*;
pub use event_trace::{EventTrace, EventTraceIntoIter};
pub use event_waiter::*;
pub use processtrace::*;
