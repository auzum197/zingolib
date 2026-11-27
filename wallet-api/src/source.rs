//! The chain source trait and the description of a source.

use futures::future::BoxFuture;

use crate::{Request, Response, TransportInfo};

/// Position of a source in its [`crate::SourceSet`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceId(pub usize);

/// Whether the wallet accepts the data of a source without a check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Integrity {
    /// Results are accepted without a check.
    Assumed,
    /// Results need a check or a quorum.
    Unverified,
}

/// The requests that a source supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Capabilities {
    /// Support for [`Request::SubtreeRoots`].
    pub subtree_roots: bool,
    /// Support for [`Request::Nullifiers`].
    pub nullifier_ranges: bool,
    /// Support for [`Request::AddressTransactions`].
    pub address_queries: bool,
    /// Support for [`Request::Mempool`].
    pub mempool: bool,
    /// Support for [`Request::Broadcast`].
    pub broadcast: bool,
}

impl Capabilities {
    /// Support for every request.
    pub const fn all() -> Self {
        Self {
            subtree_roots: true,
            nullifier_ranges: true,
            address_queries: true,
            mempool: true,
            broadcast: true,
        }
    }

    /// Whether the source supports `request`.
    pub fn supports(&self, request: &Request) -> bool {
        match request {
            Request::SubtreeRoots { .. } => self.subtree_roots,
            Request::Nullifiers(_) => self.nullifier_ranges,
            Request::AddressTransactions { .. } => self.address_queries,
            Request::Mempool => self.mempool,
            Request::Broadcast(_) => self.broadcast,
            _ => true,
        }
    }
}

/// Static description of a source.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SourceInfo {
    /// Name for logs and display.
    pub name: String,
    /// Integrity of its data.
    pub integrity: Integrity,
    /// Transport properties.
    pub transport: TransportInfo,
    /// Supported requests.
    pub capabilities: Capabilities,
}

impl SourceInfo {
    /// A source with all capabilities.
    pub fn new(name: impl Into<String>, integrity: Integrity, transport: TransportInfo) -> Self {
        Self {
            name: name.into(),
            integrity,
            transport,
            capabilities: Capabilities::all(),
        }
    }
}

/// Why a request failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SourceError {
    /// No response in time.
    #[error("request timed out. {0}")]
    Timeout(String),
    /// The source could not be reached.
    #[error("source unavailable. {0}")]
    Unavailable(String),
    /// The source does not support the request.
    #[error("request not supported. {0}")]
    Unsupported(String),
    /// The request is invalid for the source.
    #[error("request rejected. {0}")]
    Rejected(String),
    /// A stream stopped before its end.
    #[error("stream interrupted. {0}")]
    Interrupted(String),
    /// Too few sources returned the same response.
    #[error("sources disagree")]
    Disagreement,
    /// No configured source supports the request.
    #[error("no source can serve this request")]
    NoSource,
    /// The response kind does not match the request.
    #[error("source returned a response of the wrong kind")]
    UnexpectedResponse,
    /// The response has a wrong length or structure.
    #[error("invalid data from the source. {0}")]
    InvalidData(String),
    /// Data from the source failed a check.
    #[error("data from the source failed verification")]
    FailedVerification,
    /// Any other failure.
    #[error("{0}")]
    Other(String),
}

/// One provider of chain data, such as an indexer or a full node.
pub trait ChainSource: Send + Sync {
    /// Description of this source.
    fn info(&self) -> &SourceInfo;

    /// Executes one request.
    fn execute(&self, request: Request) -> BoxFuture<'_, Result<Response, SourceError>>;
}
