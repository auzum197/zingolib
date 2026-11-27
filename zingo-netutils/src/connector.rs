//! The default TCP connector. It finds the address of a host by DNS over HTTPS.

use std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use futures::future::BoxFuture;
use hickory_resolver::{
    TokioResolver,
    config::{NameServerConfig, ResolverConfig},
    net::runtime::TokioRuntimeProvider,
};
use tokio::{net::TcpStream, sync::OnceCell};
use wallet_api::{Connection, Connector, TransportInfo};

/// A blocked server must not delay the first connection for long.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);

/// A DNS over HTTPS server.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DohServer {
    server_name: Arc<str>,
    addresses: Vec<IpAddr>,
}

impl DohServer {
    /// A server with the TLS name `server_name` at `addresses`.
    pub fn new(
        server_name: impl Into<Arc<str>>,
        addresses: impl IntoIterator<Item = IpAddr>,
    ) -> Self {
        Self {
            server_name: server_name.into(),
            addresses: addresses.into_iter().collect(),
        }
    }

    /// The public resolver of Quad9.
    pub fn quad9() -> Self {
        Self::new(
            "dns.quad9.net",
            [
                IpAddr::V4(Ipv4Addr::new(9, 9, 9, 9)),
                IpAddr::V4(Ipv4Addr::new(149, 112, 112, 112)),
            ],
        )
    }
}

/// The method that a [`TcpConnector`] uses to find the address of a host.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Resolver {
    /// DNS over HTTPS. If the lookup fails, the resolver of the operating system is used,
    /// and the network can read that lookup.
    DohOrSystem(DohServer),
    /// DNS over HTTPS only. A lookup that fails is an error.
    Doh(DohServer),
    /// The resolver of the operating system. The network can read each lookup.
    System,
}

/// A TCP connector.
///
/// The default is [`Resolver::DohOrSystem`] with [`DohServer::quad9`]. It works with a
/// private host name and in a network that blocks DNS over HTTPS. Select [`Resolver::Doh`]
/// if a lookup must never go to the resolver of the operating system.
///
/// The TLS handshake of the connection still shows the host name to the network.
#[derive(Debug, Clone)]
pub struct TcpConnector {
    resolver: Resolver,
    doh: Arc<OnceCell<TokioResolver>>,
}

impl Default for TcpConnector {
    fn default() -> Self {
        Self::new(Resolver::DohOrSystem(DohServer::quad9()))
    }
}

impl TcpConnector {
    /// A connector that finds addresses with `resolver`.
    pub fn new(resolver: Resolver) -> Self {
        Self {
            resolver,
            doh: Arc::default(),
        }
    }

    async fn addresses(&self, host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
        if let Ok(address) = host.parse::<IpAddr>() {
            return Ok(vec![SocketAddr::new(address, port)]);
        }
        // RFC 6761: a name under `localhost` is the loopback address and has no lookup.
        if host == "localhost" || host.ends_with(".localhost") {
            return Ok(vec![
                SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port),
                SocketAddr::new(Ipv6Addr::LOCALHOST.into(), port),
            ]);
        }
        resolve(
            &self.resolver,
            async |server| self.doh_lookup(server, host, port).await,
            async || system_lookup(host, port).await,
        )
        .await
    }

    async fn doh_lookup(
        &self,
        server: &DohServer,
        host: &str,
        port: u16,
    ) -> io::Result<Vec<SocketAddr>> {
        let resolver = self
            .doh
            .get_or_try_init(|| async {
                let name_servers = server
                    .addresses
                    .iter()
                    .map(|address| {
                        NameServerConfig::https(*address, server.server_name.clone(), None)
                    })
                    .collect();
                let mut builder = TokioResolver::builder_with_config(
                    ResolverConfig::from_parts(None, Vec::new(), name_servers),
                    TokioRuntimeProvider::default(),
                );
                builder.options_mut().timeout = LOOKUP_TIMEOUT;
                builder.options_mut().attempts = 1;
                builder.build()
            })
            .await
            .map_err(|e| lookup_error(host, e))?;
        // The final dot makes the name absolute, so no search domain is added.
        let lookup = resolver
            .lookup_ip(format!("{}.", host.trim_end_matches('.')))
            .await
            .map_err(|e| lookup_error(host, e))?;
        let addresses: Vec<_> = lookup
            .iter()
            .map(|address| SocketAddr::new(address, port))
            .collect();
        if addresses.is_empty() {
            return Err(lookup_error(host, "no address"));
        }

        Ok(addresses)
    }
}

/// Runs the lookups that `resolver` permits, in their order.
async fn resolve(
    resolver: &Resolver,
    doh: impl AsyncFnOnce(&DohServer) -> io::Result<Vec<SocketAddr>>,
    system: impl AsyncFnOnce() -> io::Result<Vec<SocketAddr>>,
) -> io::Result<Vec<SocketAddr>> {
    match resolver {
        Resolver::System => system().await,
        Resolver::Doh(server) => doh(server).await,
        Resolver::DohOrSystem(server) => match doh(server).await {
            Ok(addresses) => Ok(addresses),
            Err(error) => {
                tracing::warn!("{error} The resolver of the operating system is used.");
                system().await
            }
        },
    }
}

async fn system_lookup(host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
    Ok(tokio::net::lookup_host((host, port)).await?.collect())
}

fn lookup_error(host: &str, error: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("DNS over HTTPS lookup of {host} failed: {error}."))
}

impl Connector for TcpConnector {
    fn connect<'a>(
        &'a self,
        host: &'a str,
        port: u16,
    ) -> BoxFuture<'a, io::Result<Box<dyn Connection>>> {
        Box::pin(async move {
            let host = host.trim_start_matches('[').trim_end_matches(']');
            let mut error =
                io::Error::new(io::ErrorKind::NotFound, format!("{host} has no address"));
            for address in self.addresses(host, port).await? {
                match TcpStream::connect(address).await {
                    Ok(stream) => {
                        stream.set_nodelay(true)?;
                        return Ok(Box::new(stream) as Box<dyn Connection>);
                    }
                    Err(e) => error = e,
                }
            }

            Err(error)
        })
    }

    fn info(&self) -> TransportInfo {
        TransportInfo::direct()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use wallet_api::{ChainSource as _, Request};

    use super::*;
    use crate::GrpcIndexer;

    /// A resolver that no lookup can reach: no server listens on this loopback address.
    fn unreachable_doh() -> TcpConnector {
        TcpConnector::new(Resolver::Doh(DohServer::new(
            "dns.invalid",
            [IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))],
        )))
    }

    #[tokio::test]
    async fn address_and_localhost_need_no_lookup() {
        let connector = unreachable_doh();

        let literal = connector.addresses("192.0.2.7", 9067).await.unwrap();
        assert_eq!(literal, ["192.0.2.7:9067".parse().unwrap()]);

        let localhost = connector.addresses("localhost", 9067).await.unwrap();
        assert!(localhost.iter().all(|address| address.ip().is_loopback()));
        assert_eq!(localhost.len(), 2);
    }

    #[tokio::test]
    async fn doh_only_gives_an_error_for_a_failed_lookup() {
        let error = unreachable_doh()
            .addresses("example.com", 443)
            .await
            .unwrap_err()
            .to_string();

        assert!(
            error.contains("DNS over HTTPS lookup of example.com failed"),
            "{error}"
        );
    }

    const FROM_DOH: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), 443);
    const FROM_SYSTEM: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2)), 443);

    struct Lookups {
        addresses: io::Result<Vec<SocketAddr>>,
        doh_ran: bool,
        system_ran: bool,
    }

    /// Runs `resolver` with a fake for each lookup.
    async fn lookups(resolver: Resolver, doh_fails: bool) -> Lookups {
        let (doh_ran, system_ran) = (Cell::new(false), Cell::new(false));
        let addresses = resolve(
            &resolver,
            async |_| {
                doh_ran.set(true);
                if doh_fails {
                    Err(lookup_error("example.com", "blocked"))
                } else {
                    Ok(vec![FROM_DOH])
                }
            },
            async || {
                system_ran.set(true);
                Ok(vec![FROM_SYSTEM])
            },
        )
        .await;

        Lookups {
            addresses,
            doh_ran: doh_ran.get(),
            system_ran: system_ran.get(),
        }
    }

    fn default_resolver() -> Resolver {
        TcpConnector::default().resolver
    }

    #[tokio::test]
    async fn default_keeps_the_doh_addresses_and_does_no_system_lookup() {
        assert!(matches!(default_resolver(), Resolver::DohOrSystem(_)));

        let lookups = lookups(default_resolver(), false).await;
        assert_eq!(lookups.addresses.unwrap(), [FROM_DOH]);
        assert!(!lookups.system_ran);
    }

    #[tokio::test]
    async fn default_does_a_system_lookup_after_a_failed_doh_lookup() {
        let lookups = lookups(default_resolver(), true).await;

        assert!(lookups.doh_ran);
        assert_eq!(lookups.addresses.unwrap(), [FROM_SYSTEM]);
    }

    #[tokio::test]
    async fn doh_only_does_no_system_lookup_after_a_failed_doh_lookup() {
        let lookups = lookups(Resolver::Doh(DohServer::quad9()), true).await;

        assert!(lookups.addresses.is_err());
        assert!(!lookups.system_ran);
    }

    #[tokio::test]
    async fn system_only_does_no_doh_lookup() {
        let lookups = lookups(Resolver::System, false).await;

        assert_eq!(lookups.addresses.unwrap(), [FROM_SYSTEM]);
        assert!(!lookups.doh_ran);
    }

    #[tokio::test]
    async fn failed_lookup_reaches_the_caller_of_a_request() {
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
        let uri: http::Uri = "https://example.com:443".parse().unwrap();
        let indexer = GrpcIndexer::with_connector(uri, unreachable_doh()).unwrap();

        let error = indexer.execute(Request::Tip).await.unwrap_err().to_string();
        assert!(
            error.contains("DNS over HTTPS lookup of example.com failed"),
            "{error}"
        );
    }
}
