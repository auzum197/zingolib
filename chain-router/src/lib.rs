//! Traits and types for replaceable chain sources, transports and source routing.

#![warn(missing_docs)]

pub mod chain;
mod event;
mod request;
mod route;
mod set;
mod source;
mod transport;

pub use futures::future::BoxFuture;

pub use event::{EventReceiver, RequestEvent, RequestKind};
pub use request::{Request, Response, ResponseStream, WalletIdentifier};
pub use route::{Failover, Quorum, Route, Router, SourceHealth};
pub use set::{Fetched, IntegrityBasis, RouteInfo, SourceSet};
pub use source::{Capabilities, ChainSource, Integrity, SourceError, SourceId, SourceInfo};
pub use transport::{
    Bandwidth, Connection, Connector, NetworkExposure, SourceExposure, TransportInfo,
};
