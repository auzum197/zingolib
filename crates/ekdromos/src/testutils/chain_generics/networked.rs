//! implementation of conduct chain for live chains

use http::Uri;

use zcash_protocol::consensus::BlockHeight;

use lightwallet_grpc_client::Indexer as _;

use super::conduct_chain::ConductChain;
use crate::{
    config::DEFAULT_INDEXER_URI_TESTNET,
    lightclient::{DEFAULT_REQUEST_TIMEOUT, LightClient},
};

/// this is essentially a placeholder.
/// allows using existing `ChainGeneric` functions with `TestNet` wallets
pub struct NetworkedTestEnvironment {
    indexer_uri: Uri,
    latest_known_server_height: Option<BlockHeight>,
}

impl NetworkedTestEnvironment {
    async fn update_server_height(&mut self) {
        let mut indexer =
            lightwallet_grpc_client::GrpcIndexer::new(self.lightserver_uri().unwrap())
                .await
                .unwrap();
        let latest = indexer
            .get_latest_block(DEFAULT_REQUEST_TIMEOUT)
            .await
            .unwrap()
            .height as u32;
        self.latest_known_server_height = Some(BlockHeight::from(latest));
        crate::testutils::timestamped_test_log(
            format!("Networked Test Chain is now at height {latest}").as_str(),
        );
    }
}

impl ConductChain for NetworkedTestEnvironment {
    async fn setup() -> Self {
        Self {
            indexer_uri: <Uri as std::str::FromStr>::from_str(DEFAULT_INDEXER_URI_TESTNET).unwrap(),
            latest_known_server_height: None,
        }
    }

    async fn create_faucet(&mut self) -> LightClient {
        unimplemented!()
    }

    fn client_target(
        &mut self,
    ) -> (
        crate::lightclient::store::FileStore,
        crate::config::ChainType,
        http::Uri,
    ) {
        unimplemented!()
    }

    async fn increase_chain_height(&mut self) {
        let before_height = self.latest_known_server_height;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            self.update_server_height().await;
            if self.latest_known_server_height != before_height {
                break;
            }
        }
    }

    fn lightserver_uri(&self) -> Option<Uri> {
        Some(self.indexer_uri.clone())
    }

    fn confirmation_patience_blocks(&self) -> usize {
        10
    }
}
