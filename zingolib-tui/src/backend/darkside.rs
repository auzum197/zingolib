//! End-to-end check of the backend bridge against darksidewalletd, a fake lightwalletd that
//! serves a fabricated regtest chain. No network. Needs a darkside-capable `lightwalletd`
//! binary in `test_binaries/bins`, so it is ignored by default:
//!
//! ```text
//! cargo test -p zingolib-tui -- --ignored darkside
//! ```

use std::time::{Duration, Instant};

use darkside_tests::utils::scenarios::{DarksideEnvironment, DarksideSender};
use tokio::sync::mpsc;
use zcash_keys::keys::UnifiedFullViewingKey;
use zcash_protocol::{PoolType, ShieldedPool};
use zingo_common_components::protocol::ActivationHeights;
use zingolib::config::ChainType;

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
#[ignore = "needs a darkside-capable lightwalletd binary in test_binaries/bins"]
async fn watch_only_wallet_sees_a_payment_through_the_backend() {
    // a funded faucet and a seeded recipient whose viewing key the watch-only wallet imports
    let mut env =
        DarksideEnvironment::default_faucet_recipient(PoolType::Shielded(ShieldedPool::Orchard))
            .await;
    env.stage_and_apply_blocks(3, 0).await;
    env.get_faucet().sync_and_await().await.unwrap();

    let regtest = ChainType::Regtest(ActivationHeights::default());
    let (ufvk, address) = {
        let recipient = env.get_lightclient(0);
        let ufvk = {
            let wallet = recipient.wallet().read().await;
            let keys = wallet
                .unified_key_store
                .get(&zip32::AccountId::ZERO)
                .expect("recipient has an account");
            UnifiedFullViewingKey::try_from(keys)
                .expect("seeded wallet has a full viewing key")
                .encode(&regtest)
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
    env.send_transaction(DarksideSender::Faucet, &address, PAYMENT)
        .await;
    let paid_at = u64::from(*env.get_staged_blockheight());
    env.apply_blocks(paid_at).await;
    env.stage_and_apply_blocks(paid_at + 3, 0).await;

    // create the watch-only wallet through the backend, exactly as the wizard would
    let server = env.get_connector().0.to_string();
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
        state.balance.orchard.confirmed, PAYMENT,
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
