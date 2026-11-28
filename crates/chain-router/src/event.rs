//! Events for the requests that a [`crate::SourceSet`] sends to its sources.

use std::time::Duration;

use crate::{SourceError, SourceId};

/// The type of a [`crate::Request`], without its arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
#[allow(missing_docs)]
pub enum RequestKind {
    Info,
    Tip,
    Block,
    Blocks,
    Nullifiers,
    TreeState,
    SubtreeRoots,
    Transaction,
    Transactions,
    AddressTransactions,
    Mempool,
    Broadcast,
}

/// One request to one source, at its end.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RequestEvent {
    /// The source of the response.
    pub source: SourceId,
    /// The type of the request.
    pub kind: RequestKind,
    /// Time from the start of the request to the response. For a stream that fails later,
    /// time from the start of the request to that failure.
    pub duration: Duration,
    /// The failure, if the request failed.
    pub result: Result<(), SourceError>,
}

/// Receives one event for each request to a source.
///
/// A closure that takes a `&RequestEvent` is a receiver.
pub trait EventReceiver: Send + Sync {
    /// A request to a source ended.
    fn on_event(&self, event: &RequestEvent);
}

impl<F: Fn(&RequestEvent) + Send + Sync> EventReceiver for F {
    fn on_event(&self, event: &RequestEvent) {
        self(event);
    }
}
