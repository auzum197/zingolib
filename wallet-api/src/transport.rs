//! Connection-based transports such as TCP and Tor, and their privacy properties.

use std::io;

use futures::future::BoxFuture;
use tokio::io::{AsyncRead, AsyncWrite};

/// The exposure and the cost of a transport.
///
/// The connector supplies these values. No code verifies them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct TransportInfo {
    /// Client data that is exposed to the source.
    pub exposed_to_source: SourceExposure,
    /// Traffic data that is exposed to an observer on the network.
    pub exposed_to_network: NetworkExposure,
    /// Data on this link has a price.
    pub metered: bool,
    /// How much data the link carries.
    pub bandwidth: Bandwidth,
}

impl TransportInfo {
    /// A direct link that hides nothing and has no cost.
    pub const fn direct() -> Self {
        Self {
            exposed_to_source: SourceExposure::Address,
            exposed_to_network: NetworkExposure {
                content: true,
                destination: true,
                traffic_pattern: true,
            },
            metered: false,
            bandwidth: Bandwidth::Normal,
        }
    }

    pub(crate) fn weakest(self, other: Self) -> Self {
        let low = [self.bandwidth, other.bandwidth].contains(&Bandwidth::Low);
        Self {
            exposed_to_source: self.exposed_to_source.min(other.exposed_to_source),
            exposed_to_network: NetworkExposure {
                content: self.exposed_to_network.content || other.exposed_to_network.content,
                destination: self.exposed_to_network.destination
                    || other.exposed_to_network.destination,
                traffic_pattern: self.exposed_to_network.traffic_pattern
                    || other.exposed_to_network.traffic_pattern,
            },
            metered: self.metered || other.metered,
            bandwidth: if low {
                Bandwidth::Low
            } else {
                Bandwidth::Normal
            },
        }
    }
}

/// Client data that is exposed to the source. Each level includes the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum SourceExposure {
    /// The network address of the client.
    Address,
    /// The fact that requests come from one client, without its address.
    Pseudonym,
    /// Nothing that connects one request to another.
    Nothing,
}

/// Traffic data that is exposed to an observer on the network. Each field is independent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct NetworkExposure {
    /// The content of requests and responses.
    pub content: bool,
    /// The source that the client connects to.
    pub destination: bool,
    /// The timing and sizes of the traffic.
    pub traffic_pattern: bool,
}

/// How much data a link carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Bandwidth {
    /// Sufficient for full block ranges.
    Normal,
    /// Each request must be small.
    Low,
}

/// A bidirectional byte stream.
pub trait Connection: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin> Connection for T {}

/// A connection-based transport such as TCP, Tor or Nym.
pub trait Connector: Send + Sync + 'static {
    /// Opens a stream to `host` and `port`.
    fn connect<'a>(
        &'a self,
        host: &'a str,
        port: u16,
    ) -> BoxFuture<'a, io::Result<Box<dyn Connection>>>;

    /// Properties of streams from this connector.
    fn info(&self) -> TransportInfo;
}
