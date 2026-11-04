use std::time::Duration;

use crate::consumer::event_waiter::EventOrTimeout;
use crate::consumer::event_waiter::sealed::{Connect, NextAsync};
use crate::consumer::flume::{LentEvents, LentProcessor};
use crate::consumer::processtrace::*;

// `EtwEventAsyncStream<Flume>`: a lending async stream that reuses the flume waiter's `LentEvents`,
// receiving with `recv_async`. Dropping it releases the current event and stops the trace.
pub struct LentStream {
    events: LentEvents,
    // Until the trace starts.
    processor: Option<LentProcessor>,
}

impl Default for LentStream {
    fn default() -> Self {
        let (events, processor) = LentEvents::new();
        LentStream {
            events,
            processor: Some(processor),
        }
    }
}

impl Connect for LentStream {
    fn take_processor(&mut self) -> impl EventProcessor + 'static + use<> {
        self.processor
            .take()
            .expect("the trace was already started")
    }
}

impl NextAsync for LentStream {
    #[cfg_attr(
        feature = "hotpath",
        hotpath::measure(impl_type = "EtwEventAsyncStream<Flume>", future = true)
    )]
    async fn next_timeout_async(
        &mut self,
        timeout: Option<Duration>,
    ) -> Option<EventOrTimeout<'_>> {
        self.events.next_timeout_async(timeout).await
    }
}

#[cfg(test)]
mod tests {
    crate::consumer::shared_tests::stream_tests!(crate::consumer::Flume, "FlumeStream");
}
