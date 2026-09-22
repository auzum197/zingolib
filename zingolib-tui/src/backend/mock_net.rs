//! End-to-end check of the backend bridge against zingolib's in-process mock indexer, a
//! fabricated regtest chain served over the lightwalletd protocol. No network, no binaries.

use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use zcash_keys::keys::UnifiedFullViewingKey;
use zingolib::testutils::mock_indexer::{MockNet, faucet_funding_transaction};
use zingolib::wallet::balance::AccountBalance;

use super::{BackendConfig, spawn};
use crate::app::{Action, Command, Key, Network, OpenSpec, Phase, State, TxKind, TxStatus, reduce};

const PAYMENT: u64 = 100_000;
const DEADLINE: Duration = Duration::from_secs(180);

/// Feeds backend actions through the reducer until `done` holds, forwarding the commands the
/// reducer asks for. Panics on a fatal phase or when the deadline passes.
async fn drive(
    state: &mut State,
    actions: &mut mpsc::UnboundedReceiver<Action>,
    commands: &mpsc::UnboundedSender<Command>,
    done: impl Fn(&State) -> bool,
) {
    let deadline = Instant::now() + DEADLINE;
    while !done(state) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let action = tokio::time::timeout(remaining, actions.recv())
            .await
            .expect("backend went quiet before the condition held")
            .expect("backend closed its action channel");
        for command in reduce(state, action) {
            if !matches!(command, Command::Copy(_) | Command::ForceQuit) {
                commands.send(command).unwrap();
            }
        }
        if let Phase::Fatal(message) = &state.phase {
            panic!("backend reported a fatal error: {message}");
        }
    }
}

#[tokio::test]
#[ignore = "AddressRow::received comes from do_total_value_to_address, which counts sends only"]
async fn watch_only_wallet_sees_a_payment_through_the_backend() {
    // a seeded recipient whose viewing key the watch-only wallet imports
    let mut net = MockNet::launch().await;
    let mut recipient = net
        .client(zingo_test_vectors::seeds::HOSPITAL_MUSEUM_SEED)
        .await;
    let (ufvk, address) = {
        let ufvk = {
            let wallet = recipient.wallet().read().await;
            let keys = wallet
                .unified_key_store
                .get(&zip32::AccountId::ZERO)
                .expect("recipient has an account");
            UnifiedFullViewingKey::try_from(keys)
                .expect("seeded wallet has a full viewing key")
                .encode(&net.chain_type())
        };
        let address = recipient
            .unified_addresses()
            .await
            .into_values()
            .find(|address| address.has_orchard())
            .map(|address| address.encode(&recipient.chain_type()))
            .expect("recipient has an orchard-capable address");
        (ufvk, address)
    };

    // pay the recipient, mine the block, then bury it under enough confirmations
    let funding = faucet_funding_transaction(vec![(&address, PAYMENT, None)]).await;
    net.chain.write().await.mine_block(vec![funding]);
    let paid_at = u64::from(net.chain_height().await);
    net.generate_blocks(3).await;

    // with NU6.3 active the orchard receiver can take the payment into either pool, so the
    // recipient's own full wallet decides which balance the watch-only view must match
    recipient.sync_and_await().await.unwrap();
    let AccountBalance {
        confirmed_orchard_balance,
        confirmed_ironwood_balance,
        ..
    } = recipient
        .account_balance(zip32::AccountId::ZERO)
        .await
        .unwrap();
    let confirmed = |zats: Option<zcash_protocol::value::Zatoshis>| zats.map_or(0, u64::from);

    // create the watch-only wallet through the backend, exactly as the wizard would
    let server = net.indexer_uri().to_string();
    let dir = tempfile::tempdir().unwrap();
    let config = BackendConfig {
        network: Network::Regtest,
        data_dir: Some(dir.path().to_path_buf()),
        wallet_name: "watch.dat".into(),
        server: Some(server.clone()),
        kdf_memory_mib: 8,
    };
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (act_tx, mut act_rx) = mpsc::unbounded_channel();
    let backend = spawn(config.clone(), cmd_rx, act_tx);
    cmd_tx
        .send(Command::Open(OpenSpec::Create {
            network: Network::Regtest,
            server,
            ufvk,
            birthday: 1,
            passphrase: Some("hunter2".into()),
        }))
        .unwrap();

    let mut state = State::new(Phase::Loading("Creating wallet".into()), Instant::now());
    drive(&mut state, &mut act_rx, &cmd_tx, |s| {
        s.phase == Phase::Main && s.balance.total() == PAYMENT && s.txs.len() == 1
    })
    .await;

    let wallet = state.wallet.as_ref().unwrap();
    assert!(wallet.has_orchard);
    assert_eq!(wallet.network, Network::Regtest);
    let tx = &state.txs[0];
    assert_eq!(tx.kind, TxKind::Received);
    assert_eq!(tx.value, PAYMENT);
    assert!(matches!(tx.status, TxStatus::Confirmed(h) if u64::from(h) == paid_at));
    assert_eq!(
        state.balance.orchard.confirmed,
        confirmed(confirmed_orchard_balance)
    );
    assert_eq!(
        state.balance.ironwood.confirmed,
        confirmed(confirmed_ironwood_balance)
    );
    assert_eq!(
        state.balance.orchard.confirmed + state.balance.ironwood.confirmed,
        PAYMENT,
        "three confirmations settle it"
    );
    assert!(
        state
            .addresses
            .iter()
            .any(|a| a.address == address && a.received == PAYMENT),
        "the paid address shows what it received"
    );
    assert!(
        state
            .sync
            .tip
            .is_some_and(|tip| u64::from(tip) >= paid_at + 3)
    );

    // quit the way the user does: q, then wait for the backend to confirm it saved
    for command in reduce(&mut state, Action::Key(Key::Char('q'))) {
        cmd_tx.send(command).unwrap();
    }
    assert_eq!(state.phase, Phase::ShuttingDown);
    drive(&mut state, &mut act_rx, &cmd_tx, |s| s.exit).await;
    backend.await.unwrap();

    // the file on disk is encrypted, and reopening it with the passphrase restores the view
    let path = config.wallet_path(Network::Regtest);
    let head = std::fs::read(&path).unwrap();
    assert!(zingolib::wallet::encryption::is_encrypted(&head));

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (act_tx, mut act_rx) = mpsc::unbounded_channel();
    let backend = spawn(config, cmd_rx, act_tx);
    cmd_tx
        .send(Command::Open(OpenSpec::Read {
            passphrase: Some("hunter2".into()),
        }))
        .unwrap();
    let mut state = State::new(Phase::Loading("Unlocking wallet".into()), Instant::now());
    drive(&mut state, &mut act_rx, &cmd_tx, |s| {
        s.phase == Phase::Main && s.balance.total() == PAYMENT
    })
    .await;
    for command in reduce(&mut state, Action::Key(Key::Char('q'))) {
        cmd_tx.send(command).unwrap();
    }
    drive(&mut state, &mut act_rx, &cmd_tx, |s| s.exit).await;
    backend.await.unwrap();
}
