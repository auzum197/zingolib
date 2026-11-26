//! Traits and types for replaceable chain sources, transports and source routing.

#![warn(missing_docs)]

pub mod chain;
mod request;
mod route;
mod set;
mod source;
mod transport;

pub use futures::future::BoxFuture;

pub use request::{Disclosure, Request, Response, ResponseStream};
pub use route::{Failover, Quorum, Route, Router};
pub use set::{Fetched, RouteInfo, SourceSet, TrustBasis};
pub use source::{Capabilities, ChainSource, SourceError, SourceId, SourceInfo, TrustLevel};
pub use transport::{
    Bandwidth, Connection, Connector, NetworkExposure, SourceExposure, TransportInfo,
};
