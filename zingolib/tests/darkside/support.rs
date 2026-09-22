//! darksidewalletd's staging operations, on the mock indexer.
//!
//! darksidewalletd served recorded blocks and let a test stage, apply
//! and replace them. The mock mines the recorded blocks' transactions,
//! replaces blocks with [`MockChain::reorg_to`], and, like
//! darksidewalletd, puts submitted transactions in the next block it
//! mines.

use std::num::NonZeroU32;

use zingo_common_components::protocol::ActivationHeights;
use zingolib::config::WalletConfig;
use zingolib::lightclient::LightClient;
use zingolib::testutils::chain_generics::conduct_chain::ConductChain;
use zingolib::testutils::default_test_wallet_settings;
use zingolib::testutils::mock_indexer::{MockChain, MockNet};
use zingolib::testutils::scenarios::{ClientBuilder, pre_ironwood_activation_heights};

use crate::constants::{
    ABANDON_TO_DARKSIDE_SAP_10_000_000_ZAT, DARKSIDE_SEED, GENESIS_BLOCK,
    TRANSACTION_INCOMING_100TAZ,
};

/// The height of the first block of the recorded reorg datasets.
pub const DATASET_START: u32 = 202;

/// The activation schedule of the darkside chains: the recorded
/// transactions predate NU6.3.
pub fn activation_heights() -> ActivationHeights {
    pre_ironwood_activation_heights()
}

/// Launches an empty mock chain on the darkside schedule.
pub async fn launch() -> MockNet {
    MockNet::launch_with(MockChain::with_activation_heights(activation_heights())).await
}

/// Reads a recorded dataset: one hex-encoded block per line.
pub fn read_dataset(path: &str) -> Vec<Vec<u8>> {
    std::fs::read_to_string(format!("{}/{path}", env!("CARGO_MANIFEST_DIR")))
        .unwrap()
        .lines()
        .map(|line| hex::decode(line).unwrap())
        .collect()
}

/// Replaces every block from [`DATASET_START`] on with the recorded
/// `dataset`, applied up to `tip`, as darksidewalletd's reset, stage and
/// apply did. The blocks below are empty.
pub async fn stage_dataset(net: &MockNet, dataset: &[Vec<u8>], tip: u32) {
    let mut chain = net.chain.write().await;
    let base = DATASET_START - 1;
    if chain.tip() > base {
        chain.reorg_to(base);
    }
    let missing = base - chain.tip();
    chain.mine_empty_blocks(missing);
    for block in &dataset[..(tip - base) as usize] {
        chain.mine_recorded_block(block);
    }
}

/// Mines the next blocks of `dataset` up to `tip`, as darksidewalletd's
/// apply of staged blocks did.
pub async fn apply_dataset(net: &MockNet, dataset: &[Vec<u8>], tip: u32) {
    let mut chain = net.chain.write().await;
    let from = (chain.tip() + 1 - DATASET_START) as usize;
    let to = (tip + 1 - DATASET_START) as usize;
    for block in &dataset[from..to] {
        chain.mine_recorded_block(block);
    }
}

/// Takes the one transaction a client submitted out of the mempool.
/// darksidewalletd held submitted transactions aside as "incoming", out of
/// the mempool and of any block, until a test staged them.
pub async fn take_incoming_transaction(net: &MockNet) -> Vec<u8> {
    let mut incoming = net.chain.write().await.take_mempool();
    assert_eq!(incoming.len(), 1, "one transaction was submitted");
    incoming.remove(0)
}

/// Resets the chain to the three blocks of `prepare_darksidewalletd`: the
/// recorded genesis block and two more, the first of them carrying the
/// wallet's 100 TAZ receipt when `include_startup_funds` is set.
pub async fn prepare_darksidewalletd(net: &MockNet, include_startup_funds: bool) {
    let mut chain = net.chain.write().await;
    if chain.tip() > 0 {
        chain.reorg_to(0);
    }
    chain.mine_recorded_block(&hex::decode(GENESIS_BLOCK).unwrap());
    let funds = include_startup_funds
        .then(|| hex::decode(TRANSACTION_INCOMING_100TAZ).unwrap())
        .into_iter()
        .collect();
    chain.mine_block(funds);
    chain.mine_empty_blocks(1);
}

/// Builds a client of `net` for `mnemonic_phrase` with `birthday`.
pub async fn build_client(
    client_builder: &mut ClientBuilder,
    mnemonic_phrase: &str,
    birthday: u32,
) -> LightClient {
    client_builder
        .build_client(
            WalletConfig::MnemonicPhrase {
                mnemonic_phrase: mnemonic_phrase.to_string(),
                no_of_accounts: NonZeroU32::try_from(1).expect("hard-coded integer"),
                birthday,
                wallet_settings: default_test_wallet_settings(),
            },
            true,
            activation_heights(),
        )
        .await
}

/// The darkside chain-generics environment: a chain funded by the recorded
/// abandon-art to darkside-seed transaction, mined when the chain grows.
pub struct DarksideEnvironment {
    net: MockNet,
    client_builder: ClientBuilder,
}

impl ConductChain for DarksideEnvironment {
    async fn setup() -> Self {
        let net = launch().await;
        net.chain.write().await.mine_empty_blocks(1);
        let client_builder = ClientBuilder::new(net.indexer_uri(), tempfile::tempdir().unwrap());
        DarksideEnvironment {
            net,
            client_builder,
        }
    }

    fn lightserver_uri(&self) -> Option<http::Uri> {
        Some(self.net.indexer_uri())
    }

    /// Mines the funding block at once. darksidewalletd staged it without
    /// applying it, which left the faucet unfunded at the first send.
    async fn create_faucet(&mut self) -> LightClient {
        self.net.chain.write().await.mine_block(vec![
            hex::decode(ABANDON_TO_DARKSIDE_SAP_10_000_000_ZAT).unwrap(),
        ]);
        build_client(&mut self.client_builder, DARKSIDE_SEED, 1).await
    }

    async fn zingo_config(&mut self) -> zingolib::config::ClientConfig {
        self.client_builder.make_unique_data_dir_and_create_config(
            activation_heights(),
            WalletConfig::NewSeed {
                no_of_accounts: 1.try_into().unwrap(),
                chain_height: 1,
                wallet_settings: default_test_wallet_settings(),
            },
        )
    }

    async fn increase_chain_height(&mut self) {
        self.net.chain.write().await.mine_mempool();
    }

    fn confirmation_patience_blocks(&self) -> usize {
        1
    }
}
