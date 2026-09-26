//! Wallet files carrying real scanned state (notes, witnesses, checkpointed shard trees,
//! nullifiers, spends, reorgs) put through save and load against the in-process mock chain.
//!
//! The mock chain draws randomness, so no check here pins a value. Each scenario asserts that
//! persistence is invisible: a wallet reloaded from its own file keeps behaving exactly like
//! the wallet that wrote it.
//!
//! Funding goes through shielded and transparent coinbase transactions, which the mock builds
//! without a prover. Sending needs the real Sapling parameters, so the send scenario is named
//! `slow` and CI skips it.

use std::io;
use std::num::NonZeroU32;
use std::panic::catch_unwind;
use std::sync::mpsc;
use std::time::Duration;

use byteorder::{LittleEndian, WriteBytesExt};
use zcash_encoding::{Optional, Vector};
use zcash_keys::address::UnifiedAddress;
use zcash_keys::keys::{Era, UnifiedFullViewingKey, UnifiedSpendingKey};
use zcash_protocol::value::Zatoshis;
use zip32::AccountId;

use zingolib::ActivationHeights;
use zingolib::config::{ClientConfig, DEFAULT_WALLET_NAME, WalletConfig};
use zingolib::lightclient::LightClient;
use zingolib::testutils::default_test_wallet_settings;
use zingolib::testutils::lightclient::from_inputs;
use zingolib::testutils::mock_indexer::{
    CoinbaseRecipient, MockChain, MockNet, faucet_funding_transaction_for,
    shielded_coinbase_transaction,
};
use zingolib::testutils::synthetic_wallet::SyntheticWalletBuilder;
use zingolib::wallet::encryption::{self, Argon2Params, EncryptionConfig};
use zingolib::wallet::keys::unified::ReceiverSelection;
use zingolib::wallet::summary::data::TransactionSummaries;
use zingolib::wallet::{
    LightWallet, PerformanceLevel, SyncConfig, TransparentAddressDiscovery,
    TransparentAddressDiscoveryScopes, WalletSettings,
};
use zingolib_common::serialization::ReadableWriteable;
use zingolib_file_format::{WalletFile, WalletFileRef};
use zingolib_price::PriceList;

const SEED: &str = zingo_test_vectors::seeds::HOSPITAL_MUSEUM_SEED;
/// Orchard coinbase lands in the Orchard pool below this height and in Ironwood from it on,
/// so one chain carries notes in all three shielded pools.
const IRONWOOD_HEIGHT: u32 = 6;
const PASSPHRASE: &str = "persistence is invisible";
const KDF: Argon2Params = Argon2Params {
    m_cost: 8,
    t_cost: 1,
    p_cost: 1,
};
/// The value of the last block `fund_pools` mines.
const LAST_FUNDING: u64 = 660_000;
const READ_TIMEOUT: Duration = Duration::from_secs(5);
const EDGE: usize = 256;
const STRIDE: usize = 61;

fn activation_heights() -> ActivationHeights {
    ActivationHeights::builder()
        .set_overwinter(Some(1))
        .set_sapling(Some(1))
        .set_blossom(Some(1))
        .set_heartwood(Some(1))
        .set_canopy(Some(1))
        .set_nu5(Some(1))
        .set_nu6(Some(1))
        .set_nu6_1(Some(1))
        .set_nu6_2(Some(1))
        .set_nu6_3(Some(IRONWOOD_HEIGHT))
        .set_nu7(None)
        .build()
}

async fn launch() -> MockNet {
    MockNet::launch_with(MockChain::with_activation_heights(activation_heights())).await
}

fn from_seed(accounts: u32, wallet_settings: WalletSettings) -> WalletConfig {
    WalletConfig::MnemonicPhrase {
        mnemonic_phrase: SEED.to_string(),
        no_of_accounts: NonZeroU32::new(accounts).expect("at least one account"),
        birthday: 1,
        wallet_settings,
    }
}

async fn open(
    net: &MockNet,
    wallet_config: WalletConfig,
    file: Option<&[u8]>,
    encryption: Option<EncryptionConfig>,
) -> LightClient {
    // The client reads its file once, on construction, and nothing here saves to disk, so the
    // directory can go as soon as the client exists.
    let dir = tempfile::tempdir().expect("a tempdir is creatable");
    if let Some(file) = file {
        std::fs::write(dir.path().join(DEFAULT_WALLET_NAME), file)
            .expect("the tempdir is writable");
    }
    let config = ClientConfig::builder()
        .set_chain_type(net.chain_type())
        .set_indexer_uri(net.indexer_uri())
        .set_wallet_dir(dir.path().to_path_buf())
        .set_wallet_config(wallet_config)
        .build();
    LightClient::new(config, false, encryption)
        .await
        .expect("a mock-net client builds")
}

async fn restore(net: &MockNet, file: &[u8], passphrase: Option<&str>) -> LightClient {
    let encryption = passphrase.map(|passphrase| EncryptionConfig::new(passphrase.to_string()));
    open(net, WalletConfig::Read, Some(file), encryption).await
}

async fn coinbase(net: &MockNet, recipient: CoinbaseRecipient, zats: u64) {
    let mut chain = net.chain.write().await;
    let transaction = shielded_coinbase_transaction(
        &chain.chain_type(),
        chain.next_height(),
        recipient,
        Zatoshis::const_from_u64(zats),
    );
    chain.mine_block(vec![transaction]);
}

async fn transparent_coinbase(net: &MockNet, taddr: &str, zats: u64) {
    net.chain
        .write()
        .await
        .mine_block_rewarding(taddr, Zatoshis::const_from_u64(zats), vec![]);
}

fn sapling(address: &UnifiedAddress) -> CoinbaseRecipient {
    CoinbaseRecipient::Sapling(
        *address
            .sapling()
            .expect("the address has a sapling receiver"),
    )
}

fn orchard(address: &UnifiedAddress) -> CoinbaseRecipient {
    CoinbaseRecipient::Orchard(
        *address
            .orchard()
            .expect("the address has an orchard receiver"),
    )
}

async fn sync(scenario: &str, client: &mut LightClient) {
    client
        .sync_and_await()
        .await
        .unwrap_or_else(|e| panic!("{scenario}: sync fails: {e}"));
}

/// Mines sapling and orchard notes below the Ironwood activation and ironwood notes above it,
/// syncing midway so the shard trees hold checkpoints from more than one sync.
async fn fund_pools(scenario: &str, net: &MockNet, client: &mut LightClient) -> UnifiedAddress {
    assert_eq!(
        net.chain_height().await,
        0,
        "{scenario}: pool funding starts on an empty chain"
    );
    let (_, address) = client
        .generate_unified_address(ReceiverSelection::all_shielded(), AccountId::ZERO)
        .await
        .expect("a shielded address generates");
    coinbase(net, sapling(&address), 110_000).await;
    coinbase(net, orchard(&address), 220_000).await;
    coinbase(net, sapling(&address), 330_000).await;
    sync(scenario, client).await;
    coinbase(net, orchard(&address), 440_000).await;
    let pre_ironwood = IRONWOOD_HEIGHT - 1 - net.chain_height().await;
    net.chain.write().await.mine_empty_blocks(pre_ironwood);
    coinbase(net, orchard(&address), 550_000).await;
    coinbase(net, orchard(&address), LAST_FUNDING).await;
    net.chain.write().await.mine_empty_blocks(2);
    sync(scenario, client).await;

    let balance = client
        .account_balance(AccountId::ZERO)
        .await
        .expect("the account has a balance");
    for (pool, total) in [
        ("sapling", balance.total_sapling_balance),
        ("orchard", balance.total_orchard_balance),
        ("ironwood", balance.total_ironwood_balance),
    ] {
        assert!(
            total.is_some_and(|total| total > Zatoshis::ZERO),
            "{scenario}: the {pool} pool is funded"
        );
    }
    address
}

fn encode(wallet: &LightWallet) -> Vec<u8> {
    let mut bytes = vec![];
    wallet
        .write(&mut bytes, &wallet.chain_type())
        .expect("a wallet encodes in memory");
    bytes
}

async fn plaintext(client: &LightClient) -> Vec<u8> {
    encode(&*client.wallet().read().await)
}

fn first_difference(a: &[u8], b: &[u8]) -> Option<usize> {
    a.iter()
        .zip(b)
        .position(|(x, y)| x != y)
        .or_else(|| (a.len() != b.len()).then(|| a.len().min(b.len())))
}

fn key_stores(wallet: &LightWallet) -> Vec<u8> {
    let mut bytes = vec![];
    for (account, keys) in &wallet.unified_key_store {
        bytes
            .write_u32::<LittleEndian>(u32::from(*account))
            .expect("in-memory writes succeed");
        keys.write(&mut bytes, wallet.chain_type())
            .expect("a key store encodes in memory");
    }
    bytes
}

/// Save is idempotent, reload is exact through both readers, and everything derived from the
/// file on load matches the wallet that wrote it. Returns the saved file.
async fn assert_round_trips(scenario: &str, client: &LightClient) -> Vec<u8> {
    let wallet = client.wallet().read().await;
    let chain_type = wallet.chain_type();
    let file = encode(&wallet);
    assert_eq!(
        first_difference(&file, &encode(&wallet)),
        None,
        "{scenario}: writing an unchanged wallet twice gives different bytes"
    );
    let expected = file.clone();

    let reloaded = LightWallet::read(file.as_slice(), chain_type)
        .unwrap_or_else(|e| panic!("{scenario}: LightWallet::read rejects a fresh save: {e}"));
    let rewritten = encode(&reloaded);
    assert_eq!(
        rewritten.len(),
        file.len(),
        "{scenario}: LightWallet::read then write changes the file length"
    );
    assert_eq!(
        first_difference(&rewritten, &expected),
        None,
        "{scenario}: LightWallet::read then write changes the file"
    );

    let any = WalletFile::read_any(file.as_slice())
        .unwrap_or_else(|e| panic!("{scenario}: WalletFile::read_any rejects a fresh save: {e}"));
    let mut rewritten = vec![];
    WalletFileRef::from(&any)
        .write(&mut rewritten, &any.chain_type)
        .expect("a wallet file encodes in memory");
    assert_eq!(
        first_difference(&rewritten, &expected),
        None,
        "{scenario}: WalletFile::read_any then write changes the file"
    );

    assert_eq!(
        reloaded.unified_addresses(),
        wallet.unified_addresses(),
        "{scenario}: unified addresses re-derive differently"
    );
    assert_eq!(
        reloaded.transparent_addresses(),
        wallet.transparent_addresses(),
        "{scenario}: transparent addresses re-derive differently"
    );
    assert_eq!(
        key_stores(&reloaded),
        key_stores(&wallet),
        "{scenario}: the key store re-encodes differently"
    );
    file
}

async fn assert_twins(scenario: &str, step: &str, original: &LightClient, restored: &LightClient) {
    assert_eq!(
        first_difference(&plaintext(original).await, &plaintext(restored).await),
        None,
        "{scenario}: the reloaded wallet's file diverges after {step}"
    );
    let accounts: Vec<AccountId> = original
        .wallet()
        .read()
        .await
        .unified_key_store
        .keys()
        .copied()
        .collect();
    for account in accounts {
        assert_eq!(
            original.account_balance(account).await.ok(),
            restored.account_balance(account).await.ok(),
            "{scenario}: account {account:?} balance diverges after {step}"
        );
    }
    assert_eq!(
        original.transaction_summaries(false).await.ok(),
        restored.transaction_summaries(false).await.ok(),
        "{scenario}: transaction summaries diverge after {step}"
    );
}

async fn sync_twins(scenario: &str, original: &mut LightClient, restored: &mut LightClient) {
    sync(&format!("{scenario} original"), original).await;
    sync(&format!("{scenario} reloaded"), restored).await;
}

/// Reorgs the top `depth` blocks away onto a longer branch that re-mines their transactions
/// one block higher.
///
/// The new branch keeps every note commitment at its tree position. A branch that puts a
/// different commitment where a scanned one was makes pepper-sync fail with "Inserted root
/// conflicts with existing root", with or without a reload, so it cannot be used here.
async fn remine_later(net: &MockNet, depth: u32) {
    let mut chain = net.chain.write().await;
    let tip = chain.tip();
    let evicted = chain.reorg_to(tip - depth);
    chain.mine_empty_blocks(1);
    chain.mine_block(evicted);
    chain.mine_empty_blocks(depth);
}

/// The differential check: the writer and a client built from its file keep scanning the same
/// chain through new funds, new addresses and a reorg, and never disagree.
async fn assert_reload_invisible(
    scenario: &str,
    net: &MockNet,
    original: &mut LightClient,
    restored: &mut LightClient,
) {
    assert_twins(scenario, "reload", original, restored).await;

    let (_, address) = original
        .generate_unified_address(ReceiverSelection::all_shielded(), AccountId::ZERO)
        .await
        .expect("a shielded address generates");
    let (_, twin_address) = restored
        .generate_unified_address(ReceiverSelection::all_shielded(), AccountId::ZERO)
        .await
        .expect("a shielded address generates");
    assert_eq!(
        address, twin_address,
        "{scenario}: the next unified address differs after reload"
    );
    assert_twins(scenario, "address generation", original, restored).await;

    coinbase(net, sapling(&address), 12_000).await;
    coinbase(net, orchard(&address), 23_000).await;
    sync_twins(scenario, original, restored).await;
    assert_twins(scenario, "funding", original, restored).await;

    remine_later(net, 2).await;
    sync_twins(scenario, original, restored).await;
    assert_twins(scenario, "a reorg", original, restored).await;

    coinbase(net, orchard(&address), 34_000).await;
    sync_twins(scenario, original, restored).await;
    assert_twins(scenario, "funding after the reorg", original, restored).await;

    net.chain.write().await.mine_empty_blocks(3);
    sync_twins(scenario, original, restored).await;
    assert_twins(scenario, "empty blocks", original, restored).await;
}

#[derive(Debug, Clone, Copy)]
enum Corruption {
    Flip(usize),
    Truncate(usize),
}

impl Corruption {
    fn apply(self, file: &[u8]) -> Vec<u8> {
        match self {
            Self::Flip(offset) => {
                let mut corrupt = file.to_vec();
                corrupt[offset] ^= 0xff;
                corrupt
            }
            Self::Truncate(len) => file[..len].to_vec(),
        }
    }
}

/// Every offset in the first and last `EDGE` bytes and every `STRIDE`th offset between them.
fn flips(len: usize) -> impl Iterator<Item = Corruption> {
    let head = 0..EDGE.min(len);
    let middle = (EDGE..len.saturating_sub(EDGE)).step_by(STRIDE);
    let tail = len.saturating_sub(EDGE).max(EDGE.min(len))..len;
    head.chain(middle).chain(tail).map(Corruption::Flip)
}

fn truncations(len: usize) -> impl Iterator<Item = Corruption> {
    (0..len).step_by(STRIDE).map(Corruption::Truncate)
}

/// Why reading `file` did not reject it, or `None` when it did.
fn unrejected(file: Vec<u8>, read: fn(&[u8]) -> io::Result<()>) -> Option<String> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let why = match catch_unwind(|| read(&file)) {
            Ok(Err(_)) => None,
            Ok(Ok(())) => Some("accepted".to_string()),
            Err(panic) => Some(format!(
                "panicked: {}",
                panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic
                        .downcast_ref::<&str>()
                        .map(|message| message.to_string()))
                    .unwrap_or_default()
            )),
        };
        sender.send(why).ok();
    });
    receiver
        .recv_timeout(READ_TIMEOUT)
        .unwrap_or_else(|_| Some(format!("no answer within {READ_TIMEOUT:?}")))
}

fn assert_rejected(
    scenario: &str,
    file: &[u8],
    corruptions: impl Iterator<Item = Corruption>,
    read: fn(&[u8]) -> io::Result<()>,
) {
    let failures: Vec<String> = corruptions
        .filter_map(|corruption| {
            unrejected(corruption.apply(file), read).map(|why| format!("{corruption:?}: {why}"))
        })
        .collect();
    assert!(
        failures.is_empty(),
        "{scenario}: {} corruptions of a {} byte file were not rejected:\n{}",
        failures.len(),
        file.len(),
        failures.join("\n")
    );
}

fn read_any(file: &[u8]) -> io::Result<()> {
    WalletFile::read_any(file).map(drop)
}

fn read_encrypted_any(file: &[u8]) -> io::Result<()> {
    WalletFile::read_encrypted_any(file, Some(PASSPHRASE.to_string())).map(drop)
}

async fn funded(
    scenario: &str,
    net: &MockNet,
    encryption: Option<EncryptionConfig>,
) -> LightClient {
    let mut client = open(
        net,
        from_seed(1, default_test_wallet_settings()),
        None,
        encryption,
    )
    .await;
    fund_pools(scenario, net, &mut client).await;
    client
}

#[tokio::test]
async fn synced() {
    let net = launch().await;
    let mut client = funded("synced", &net, None).await;

    let file = assert_round_trips("synced", &client).await;
    assert_rejected("synced", &file, truncations(file.len()), read_any);
    let mut restored = restore(&net, &file, None).await;
    assert_reload_invisible("synced", &net, &mut client, &mut restored).await;
}

/// The plaintext layout carries no checksum, so a flipped byte inside a value (seed entropy,
/// an amount, a memo) decodes to a different but well-formed wallet. Kept to run on demand
/// with `--ignored` until the layout gains an integrity check.

#[tokio::test]
async fn reorg() {
    let net = launch().await;
    let mut client = funded("reorg", &net, None).await;
    let status_of = |summaries: &TransactionSummaries| {
        summaries
            .0
            .iter()
            .find(|summary| summary.value == LAST_FUNDING)
            .map(|summary| summary.status)
    };
    let before = status_of(
        &client
            .transaction_summaries(false)
            .await
            .expect("summaries build"),
    );
    remine_later(&net, net.chain_height().await - IRONWOOD_HEIGHT).await;
    sync("reorg", &mut client).await;
    let after = status_of(
        &client
            .transaction_summaries(false)
            .await
            .expect("summaries build"),
    );
    assert!(
        before.is_some() && after.is_some() && before != after,
        "reorg: the re-mined receipt moves from {before:?}, found {after:?}"
    );

    let file = assert_round_trips("reorg", &client).await;
    let mut restored = restore(&net, &file, None).await;
    assert_reload_invisible("reorg", &net, &mut client, &mut restored).await;
}

#[tokio::test]
async fn multi() {
    let net = launch().await;
    let mut client = open(
        &net,
        from_seed(2, default_test_wallet_settings()),
        None,
        None,
    )
    .await;
    let second = AccountId::try_from(1).expect("1 is a valid account index");
    let mut shielded = vec![];
    for (receivers, account) in [
        (ReceiverSelection::all_shielded(), AccountId::ZERO),
        (ReceiverSelection::orchard_only(), AccountId::ZERO),
        (ReceiverSelection::sapling_only(), second),
        (ReceiverSelection::all_shielded(), second),
    ] {
        let (_, address) = client
            .generate_unified_address(receivers, account)
            .await
            .expect("a shielded address generates");
        shielded.push(address);
    }
    let mut taddrs = vec![];
    for account in [AccountId::ZERO, AccountId::ZERO, second] {
        let (id, _) = client
            .generate_transparent_address(account, false)
            .await
            .expect("a transparent address generates");
        let taddr = client.transparent_addresses().await.remove(&id);
        taddrs.push(taddr.expect("the new address is listed"));
    }
    {
        let mut wallet = client.wallet().write().await;
        for account in [AccountId::ZERO, second] {
            wallet
                .generate_refund_addresses(2, account)
                .expect("refund addresses generate");
        }
    }

    coinbase(&net, sapling(&shielded[0]), 110_000).await;
    coinbase(&net, orchard(&shielded[1]), 220_000).await;
    coinbase(&net, sapling(&shielded[2]), 330_000).await;
    transparent_coinbase(&net, &taddrs[1], 440_000).await;
    transparent_coinbase(&net, &taddrs[2], 550_000).await;
    coinbase(&net, orchard(&shielded[3]), 660_000).await;
    net.chain.write().await.mine_empty_blocks(2);
    sync("multi", &mut client).await;
    let first = client
        .account_balance(AccountId::ZERO)
        .await
        .expect("the account has a balance");
    let other = client
        .account_balance(second)
        .await
        .expect("the account has a balance");
    for (account, pool, total) in [
        (AccountId::ZERO, "sapling", first.total_sapling_balance),
        (AccountId::ZERO, "orchard", first.total_orchard_balance),
        (second, "sapling", other.total_sapling_balance),
        (second, "ironwood", other.total_ironwood_balance),
    ] {
        assert!(
            total.is_some_and(|total| total > Zatoshis::ZERO),
            "multi: account {account:?} is funded in {pool}"
        );
    }

    // Transparent coinbase stays out of the balance until it matures.
    let coins = client
        .transaction_summaries(false)
        .await
        .expect("multi: summaries build")
        .0
        .iter()
        .filter(|summary| !summary.transparent_coins.is_empty())
        .count();
    assert_eq!(
        coins, 2,
        "multi: both transparent coinbase receipts are found"
    );

    let file = assert_round_trips("multi", &client).await;
    let mut restored = restore(&net, &file, None).await;
    assert_reload_invisible("multi", &net, &mut client, &mut restored).await;
}

async fn seal(client: &LightClient) -> Vec<u8> {
    let mut wallet = client.wallet().write().await;
    wallet.save_required = true;
    wallet
        .save()
        .expect("an encrypted wallet saves")
        .expect("a save is due")
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// The envelope hides the secrets the plaintext holds, opens only with the passphrase, and
/// opens to the same wallet through both readers.
async fn assert_sealed(scenario: &str, client: &LightClient, plaintext: &[u8], envelope: &[u8]) {
    let wallet = client.wallet().read().await;
    let chain_type = wallet.chain_type();
    assert!(
        encryption::is_encrypted(envelope),
        "{scenario}: the save is an encrypted envelope"
    );

    let phrase = client.mnemonic_phrase().expect("the wallet has a seed");
    let entropy = bip0039::Mnemonic::<bip0039::English>::from_phrase(phrase)
        .expect("the wallet's phrase parses")
        .entropy()
        .to_vec();
    let keys = &wallet.unified_key_store[&AccountId::ZERO];
    let usk = UnifiedSpendingKey::try_from(keys)
        .expect("the wallet holds spending keys")
        .to_bytes(Era::Orchard);
    let ufvk = UnifiedFullViewingKey::try_from(keys)
        .expect("spending keys give viewing keys")
        .encode(&chain_type);
    for (name, secret) in [("seed entropy", &entropy), ("spending key", &usk)] {
        assert!(
            contains(plaintext, secret),
            "{scenario}: the plaintext should hold the {name}"
        );
    }
    for (name, secret) in [
        ("seed entropy", entropy.as_slice()),
        ("spending key", usk.as_slice()),
        ("viewing key", ufvk.as_bytes()),
    ] {
        assert!(
            !contains(envelope, secret),
            "{scenario}: the {name} appears in the envelope"
        );
    }

    let expected = plaintext.to_vec();
    let reloaded = LightWallet::read_encrypted(envelope, chain_type, Some(PASSPHRASE.to_string()))
        .unwrap_or_else(|e| panic!("{scenario}: the passphrase does not open the envelope: {e}"));
    assert!(
        reloaded.is_encrypted(),
        "{scenario}: a wallet opened from an envelope keeps encrypting"
    );
    assert_eq!(
        first_difference(&encode(&reloaded), &expected),
        None,
        "{scenario}: LightWallet::read_encrypted then write changes the file"
    );

    let (any, session) = WalletFile::read_encrypted_any(envelope, Some(PASSPHRASE.to_string()))
        .unwrap_or_else(|e| panic!("{scenario}: read_encrypted_any rejects the envelope: {e}"));
    assert!(
        session.is_some(),
        "{scenario}: read_encrypted_any returns the session that opened the envelope"
    );
    let mut rewritten = vec![];
    WalletFileRef::from(&any)
        .write(&mut rewritten, &any.chain_type)
        .expect("a wallet file encodes in memory");
    assert_eq!(
        first_difference(&rewritten, &expected),
        None,
        "{scenario}: WalletFile::read_encrypted_any then write changes the file"
    );

    for passphrase in [Some("not the passphrase".to_string()), None] {
        assert!(
            LightWallet::read_encrypted(envelope, chain_type, passphrase.clone()).is_err(),
            "{scenario}: LightWallet::read_encrypted opens the envelope with {passphrase:?}"
        );
        assert!(
            WalletFile::read_encrypted_any(envelope, passphrase.clone()).is_err(),
            "{scenario}: WalletFile::read_encrypted_any opens the envelope with {passphrase:?}"
        );
    }
}

#[tokio::test]
async fn encrypted() {
    let net = launch().await;
    let encryption = EncryptionConfig::with_params(PASSPHRASE.to_string(), KDF);
    let mut client = funded("encrypted", &net, Some(encryption)).await;

    let file = assert_round_trips("encrypted", &client).await;
    let envelope = seal(&client).await;
    assert_sealed("encrypted", &client, &file, &envelope).await;
    let resealed = seal(&client).await;
    assert_sealed("encrypted resave", &client, &file, &resealed).await;
    assert_rejected(
        "encrypted",
        &envelope,
        flips(envelope.len()).chain(truncations(envelope.len())),
        read_encrypted_any,
    );

    let mut restored = restore(&net, &envelope, Some(PASSPHRASE)).await;
    assert!(
        restored.wallet().read().await.is_encrypted(),
        "encrypted: the reloaded client keeps encrypting"
    );
    assert_reload_invisible("encrypted", &net, &mut client, &mut restored).await;
}

fn custom_settings() -> WalletSettings {
    WalletSettings {
        sync_config: SyncConfig {
            transparent_address_discovery: TransparentAddressDiscovery {
                gap_limit: 13,
                scopes: TransparentAddressDiscoveryScopes {
                    external: true,
                    internal: true,
                    refund: false,
                },
            },
            performance_level: PerformanceLevel::Medium,
            event_channel_capacity: 97,
        },
        min_confirmations: NonZeroU32::new(7).expect("7 is non-zero"),
    }
}

/// `PriceList` has no setters for its prices, so the list is built through its own reader.
fn prices() -> PriceList {
    let mut bytes = vec![PriceList::VERSION];
    Optional::write(&mut bytes, Some(1_700_000_000), |w, time| {
        w.write_u32::<LittleEndian>(time)
    })
    .expect("in-memory writes succeed");
    Optional::write(
        &mut bytes,
        Some((1_700_086_400, 31.25)),
        |w, (time, usd)| {
            w.write_u32::<LittleEndian>(time)?;
            w.write_f32::<LittleEndian>(usd)
        },
    )
    .expect("in-memory writes succeed");
    Vector::write(
        &mut bytes,
        &[(1_699_920_000, 29.5), (1_700_006_400, 30.75)],
        |w, &(time, usd)| {
            w.write_u32::<LittleEndian>(time)?;
            w.write_f32::<LittleEndian>(usd)
        },
    )
    .expect("in-memory writes succeed");
    PriceList::read(bytes.as_slice(), ()).expect("a hand-built price list reads")
}

fn encode_prices(prices: &PriceList) -> Vec<u8> {
    let mut bytes = vec![];
    prices
        .write(&mut bytes, ())
        .expect("a price list encodes in memory");
    bytes
}

#[tokio::test]
async fn settings() {
    let net = launch().await;
    let mut client = open(&net, from_seed(1, custom_settings()), None, None).await;
    client.wallet().write().await.price_list = prices();
    fund_pools("settings", &net, &mut client).await;

    let file = assert_round_trips("settings", &client).await;
    let reloaded = LightWallet::read(file.as_slice(), client.chain_type())
        .expect("settings: a fresh save reads back");
    assert_eq!(
        reloaded.wallet_settings,
        custom_settings(),
        "settings: wallet settings change through a reload"
    );
    assert_eq!(
        reloaded.price_list.daily_prices().len(),
        2,
        "settings: daily prices are lost through a reload"
    );
    assert_eq!(
        encode_prices(&reloaded.price_list),
        encode_prices(&prices()),
        "settings: the price list changes through a reload"
    );

    let mut restored = restore(&net, &file, None).await;
    assert_reload_invisible("settings", &net, &mut client, &mut restored).await;
}

fn external_orchard_address() -> String {
    let mut external = SyntheticWalletBuilder::new(zingo_test_vectors::seeds::ABANDON_ART_SEED)
        .activation_heights(activation_heights())
        .build();
    let (_, address) = external
        .generate_unified_address(ReceiverSelection::orchard_only(), AccountId::ZERO)
        .expect("an orchard address generates");
    address.encode(&external.chain_type())
}

/// Needs the real Sapling parameters in `zingolib/zcash-params`: the faucet and the wallet
/// both prove their transactions.
#[tokio::test]
async fn spent_slow() {
    let net = launch().await;
    let mut client = open(
        &net,
        from_seed(1, default_test_wallet_settings()),
        None,
        None,
    )
    .await;
    let address = fund_pools("spent", &net, &mut client).await;
    let chain_type = client.chain_type();
    let (_, own_sapling) = client
        .generate_unified_address(ReceiverSelection::sapling_only(), AccountId::ZERO)
        .await
        .expect("a sapling address generates");
    let funding = faucet_funding_transaction_for(
        activation_heights(),
        vec![(&address.encode(&chain_type), 500_000, None)],
    )
    .await;
    net.chain.write().await.mine_block(vec![funding]);
    sync("spent", &mut client).await;

    from_inputs::quick_send(
        &mut client,
        vec![
            (&external_orchard_address(), 40_000, Some("out")),
            (&own_sapling.encode(&chain_type), 30_000, Some("self")),
        ],
    )
    .await
    .expect("spent: the send succeeds");
    let sent_at = net.chain_height().await + 1;
    net.chain.write().await.mine_mempool();
    net.chain.write().await.mine_empty_blocks(1);
    sync("spent", &mut client).await;
    let summaries = client
        .transaction_summaries(false)
        .await
        .expect("spent: summaries build");
    assert!(
        summaries.0.iter().any(|summary| {
            !summary.outgoing_orchard_notes.is_empty()
                || !summary.outgoing_ironwood_notes.is_empty()
        }),
        "spent: the wallet records outgoing notes"
    );

    let file = assert_round_trips("spent", &client).await;
    let mut restored = restore(&net, &file, None).await;
    assert_reload_invisible("spent", &net, &mut client, &mut restored).await;

    let depth = net.chain_height().await - (sent_at - 1);
    remine_later(&net, depth).await;
    sync_twins("spent", &mut client, &mut restored).await;
    assert_twins("spent", "re-mining the send", &client, &restored).await;
}
