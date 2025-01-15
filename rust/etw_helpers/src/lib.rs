#[cfg(any(feature = "crossbeam"))]
pub mod event_consumer_crossbeam;
#[cfg(any(feature = "flume"))]
pub mod event_consumer_flume;
#[cfg(any(feature = "futures"))]
mod event_consumer_futures;
#[cfg(any(feature = "futures"))]
mod event_consumer_futures_stream;
//mod event_consumer_raw;
mod error;
mod event_record;
mod processtrace;
mod sessions;

/// If only one of the event consumer features are enabled,
/// this module will be enabled as well to provide a generic naming scheme for accessing it.
/// This allows for the selected feature to be switched without requiring code changes.
///
/// If multiple event consumer features are enabled,
/// they must be accessed via their individual module names.
#[cfg(any(all(feature = "crossbeam", not(feature = "flume")), all(feature = "flume", not(feature = "crossbeam"))))]
pub mod event_consumer {
    // For the event waiter, prefer the crossbeam implementation if the feature is enabled
    #[cfg(all(feature = "crossbeam", not(feature = "flume")))]
    pub use super::event_consumer_crossbeam::*;
    #[cfg(all(feature = "flume", not(feature = "crossbeam")))]
    pub use super::event_consumer_flume::*;
}

/// When the `futures` feature is enabled, this module will contain the async event consumers.
#[cfg(any(feature = "futures"))]
pub mod event_consumer_async {
    pub use super::event_consumer_futures::*;
    pub use super::event_consumer_futures_stream::*;
}

//pub use event_consumer_raw::*;
pub use event_record::*;
pub use processtrace::*;
pub use sessions::*;
