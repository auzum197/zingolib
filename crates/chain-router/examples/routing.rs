//! Failover across public lightwalletd servers, with txid lookups limited to two of them.
//!
//!     cargo run -p chain-router --example routing

use std::{error::Error, sync::Arc};

use chain_router::{AllowedIdentifiers, ChainSource, Failover, RequestEvent, SourceSet};
use lightwallet_grpc_client::GrpcIndexer;

const SERVERS: [&str; 5] = [
    "https://zec.rocks:443",
    "https://lwd.zcashexplorer.app:9067",
    "https://zec.alexxiy.top:9067",
    "https://lwd.z0n.jp:443",
    "https://l.ombie.cash:443",
];

const TXID_SERVERS: [&str; 2] = [
    "https://zec-node.cakewallet.com:443",
    "https://zec.0xrpc.io:443",
];

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut indexers: Vec<Arc<dyn ChainSource>> = Vec::new();
    for uri in SERVERS {
        indexers.push(Arc::new(GrpcIndexer::new(uri.parse()?).await?));
    }
    for uri in TXID_SERVERS {
        let indexer = GrpcIndexer::new(uri.parse()?).await?;
        indexers.push(Arc::new(
            indexer.allow_identifiers(AllowedIdentifiers::all()),
        ));
    }

    let names: Vec<_> = indexers.iter().map(|i| i.info().name.clone()).collect();
    let sources = SourceSet::new(indexers, Arc::new(Failover::default())).with_events(
        move |event: &RequestEvent| {
            let outcome = if event.result.is_ok() { "ok" } else { "failed" };
            println!("{:?} {} {outcome}", event.kind, names[event.source.0]);
        },
    );

    let tip = sources.tip().await?;

    let block = sources.block(tip.value.height).await?;
    assert!(block.source.0 < SERVERS.len());

    let txid = block.value.transactions[0].txid;
    let transaction = sources.transaction(txid).await?;
    assert!(transaction.source.0 >= SERVERS.len());

    Ok(())
}
