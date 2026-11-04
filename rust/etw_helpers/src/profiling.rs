//! Profiling with [hotpath-rs](https://hotpath.rs). The hotpath dependency is only included with the
//! `hotpath` feature; without it, these fall back to the plain channel types and constructors.
//! Functions are instrumented with `#[cfg_attr(feature = "hotpath", hotpath::measure(...))]`.

/// `hotpath::channel!` with the `hotpath` feature; otherwise returns the channel unchanged.
#[cfg(feature = "hotpath")]
#[allow(unused_macros)] // Without a channel backend feature
macro_rules! channel {
    ($($args:tt)*) => {
        hotpath::channel!($($args)*)
    };
}

/// `hotpath::channel!` with the `hotpath` feature; otherwise returns the channel unchanged.
#[cfg(not(feature = "hotpath"))]
#[allow(unused_macros)] // Without a channel backend feature
macro_rules! channel {
    ($channel:expr $(, $($rest:tt)*)?) => {
        $channel
    };
}

#[allow(unused_imports)] // Without a channel backend feature
pub(crate) use channel;

/// Channel endpoints matching what [`channel!`] returns: hotpath's instrumented wrappers with
/// the `hotpath` feature, the plain types otherwise. Mirrors `hotpath::wrap`.
pub(crate) mod wrap {
    #[cfg(feature = "futures")]
    pub(crate) mod std {
        pub(crate) mod sync {
            pub(crate) mod mpsc {
                #[cfg(not(feature = "hotpath"))]
                pub(crate) use ::std::sync::mpsc::{Receiver, SyncSender};
                #[cfg(feature = "hotpath")]
                pub(crate) use hotpath::wrap::std::sync::mpsc::{Receiver, SyncSender};
            }
        }
    }

    #[cfg(feature = "crossbeam")]
    pub(crate) mod crossbeam_channel {
        #[cfg(not(feature = "hotpath"))]
        pub(crate) use ::crossbeam_channel::{Receiver, Sender};
        #[cfg(feature = "hotpath")]
        pub(crate) use hotpath::wrap::crossbeam_channel::{Receiver, Sender};
    }

    #[cfg(feature = "flume")]
    pub(crate) mod flume {
        #[cfg(not(feature = "hotpath"))]
        pub(crate) use ::flume::{Receiver, Sender};
        #[cfg(feature = "hotpath")]
        pub(crate) use hotpath::wrap::flume::{Receiver, Sender};
    }

    #[cfg(feature = "futures")]
    pub(crate) mod futures_channel {
        #[cfg(not(feature = "hotpath"))]
        pub(crate) use ::futures::channel::mpsc;
        #[cfg(feature = "hotpath")]
        pub(crate) use hotpath::wrap::futures_channel::mpsc;
    }
}
