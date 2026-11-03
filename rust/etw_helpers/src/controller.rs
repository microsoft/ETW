//! Controlling the trace sessions that collect events from providers.
//!
//! Start, stop and configure ETW trace sessions, which collect the events that providers write,
//! with [`SessionBuilder`]; attach to running ones with [`get_existing_session`]. Read the events
//! with [`crate::consumer`].

mod session;

pub use session::*;
