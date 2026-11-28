//! Quorum reads across public lightwalletd servers.
//!
//!     cargo run -p chain-router --example quorum

use std::{error::Error, sync::Arc};

use chain_router::{ChainSource, IntegrityBasis, Quorum, SourceSet};
use lightwallet_grpc_client::GrpcIndexer;

const SERVERS: [&str; 3] = [
    "https://zec.rocks:443",
    "https://zec-node.cakewallet.com:443",
    "https://lwd.zcashexplorer.app:9067",
];

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut indexers: Vec<Arc<dyn ChainSource>> = Vec::new();
    for uri in SERVERS {
        indexers.push(Arc::new(GrpcIndexer::new(uri.parse()?).await?));
    }
    let sources = SourceSet::new(indexers, Arc::new(Quorum::new(2)));

    let tip = sources.tip().await?;
    let height = tip.value.height - 100;
    let tree_state = sources.tree_state(height).await?;
    assert_eq!(tree_state.basis, IntegrityBasis::Quorum(2));

    println!("tree state at {height} confirmed by 2 servers");
    Ok(())
}
