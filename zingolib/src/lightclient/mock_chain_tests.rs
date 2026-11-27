//! Ports of chain-bound libtonode tests onto the stateful mock indexer
//! ([`crate::testutils::mock_indexer`]): the real wallet pipeline
//! (`GrpcIndexer`, pepper-sync scanning, record building, spend
//! bookkeeping) driven against a fabricated in-process chain, with no
//! zebrad or zainod.
//!
//! Every port here is an OFFLINE TWIN: the live original stays in
//! `zingolib/tests/concrete.rs` as the control group.

use pepper_sync::wallet::IronwoodNote;
use zcash_address::ZcashAddress;
use zcash_protocol::PoolType;
use zcash_protocol::ShieldedPool;
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::value::Zatoshis;

use crate::check_client_balances;
use crate::testutils::chain_generics::conduct_chain::ConductChain as _;
use crate::testutils::lightclient::{from_inputs, get_base_address};
use crate::testutils::mock_indexer::{MockNet, faucet_funding_transaction};
use crate::testutils::send::error::ProposeSendError;
use crate::testutils::synthetic_wallet::SyntheticWalletBuilder;
use crate::wallet::keys::unified::ReceiverSelection;

/// An address belonging to no wallet on the mock net, so sends to it
/// are external.
fn external_address(pool: PoolType) -> String {
    let mut external_wallet =
        SyntheticWalletBuilder::new(zingo_test_vectors::seeds::ABANDON_ART_SEED).build();
    let selection = match pool {
        PoolType::ORCHARD | PoolType::IRONWOOD => ReceiverSelection::orchard_only(),
        PoolType::SAPLING => ReceiverSelection::sapling_only(),
        _ => unimplemented!("only shielded external destinations are needed here"),
    };
    let (_, unified_address) = external_wallet
        .generate_unified_address(selection, zip32::AccountId::ZERO)
        .unwrap();
    unified_address.encode(&external_wallet.chain_type())
}

/// Returns a TEX-encoded taddr from an external wallet.
fn external_tex_address() -> String {
    use pepper_sync::keys::decode_address;
    use zcash_client_backend::address::Address;
    use zcash_transparent::address::TransparentAddress;

    let external_wallet =
        SyntheticWalletBuilder::new(zingo_test_vectors::seeds::ABANDON_ART_SEED).build();
    let taddr = external_wallet
        .transparent_addresses()
        .values()
        .next()
        .unwrap()
        .clone();
    let Address::Transparent(TransparentAddress::PublicKeyHash(taddr_bytes)) =
        decode_address(&external_wallet.chain_type(), &taddr).unwrap()
    else {
        panic!("a wallet-generated first taddr is p2pkh")
    };
    crate::testutils::interpret_taddr_as_tex_addr(taddr_bytes, &external_wallet.chain_type())
}

/// Funds `receivers` with one faucet-built transaction mined into the next
/// mock block, followed by `extra_blocks` empty blocks.
async fn fund(net: &MockNet, receivers: Vec<(&str, u64, Option<&str>)>, extra_blocks: u32) {
    let funding = faucet_funding_transaction(receivers).await;
    let mut chain = net.chain.write().await;
    chain.mine_block(vec![funding]);
    chain.mine_empty_blocks(extra_blocks);
}

/// The mock-net proof: a funding block scans to a confirmed balance,
/// a real quick_send round-trips through the mock's mempool into the
/// next block, and the post-confirmation balance carries the exact
/// ZIP-317 arithmetic.
#[tokio::test]
async fn funded_send_confirms_on_the_mock_chain() {
    let mut net = MockNet::launch().await;
    let mut recipient = net
        .client(zingo_test_vectors::seeds::HOSPITAL_MUSEUM_SEED)
        .await;
    let recipient_ua =
        get_base_address(&recipient, PoolType::Shielded(ShieldedPool::Orchard)).await;

    net.chain.write().await.mine_empty_blocks(1);
    fund(&net, vec![(&recipient_ua, 100_000, None)], 1).await;

    recipient.sync_and_await().await.unwrap();
    check_client_balances!(recipient, i: 100_000 o: 0 s: 0 t: 0);

    from_inputs::quick_send(
        &mut recipient,
        vec![(&external_address(PoolType::ORCHARD), 20_000, None)],
    )
    .await
    .unwrap();
    net.chain.write().await.mine_mempool();
    recipient.sync_and_await().await.unwrap();

    // 100_000 funding minus the 20_000 payment and its 10_000 one-orchard-
    // spend, two-logical-action ZIP-317 fee.
    check_client_balances!(recipient, i: 70_000 o: 0 s: 0 t: 0);
}

/// Tests that max_send_value() returns a non-zero value for a wallet
/// with funds.
#[tokio::test]
async fn max_send_value_to_tex_empties_the_wallet() {
    let funding = 100_000;
    let mut net = MockNet::launch().await;
    let mut sender = net
        .client(zingo_test_vectors::seeds::HOSPITAL_MUSEUM_SEED)
        .await;
    let sender_ua = get_base_address(&sender, PoolType::Shielded(ShieldedPool::Orchard)).await;

    net.chain.write().await.mine_empty_blocks(1);
    fund(&net, vec![(&sender_ua, funding, None)], 1).await;
    sender.sync_and_await().await.unwrap();
    check_client_balances!(sender, i: funding o: 0 s: 0 t: 0);

    let tex_address = external_tex_address();
    let max_send_value = sender
        .max_send_value(
            ZcashAddress::try_from_encoded(&tex_address).unwrap(),
            zip32::AccountId::ZERO,
        )
        .await
        .unwrap();
    assert!(
        max_send_value > Zatoshis::ZERO,
        "a funded wallet can send to a TEX address"
    );

    from_inputs::quick_send(
        &mut sender,
        vec![(&tex_address, max_send_value.into_u64(), None)],
    )
    .await
    .unwrap();
    net.chain.write().await.mine_mempool();
    sender.sync_and_await().await.unwrap();

    check_client_balances!(sender, i: 0 o: 0 s: 0 t: 0);
}

/// Tests that max_send_value() to a shielded address
/// spends the whole balance across the ironwood and sapling pools.
#[tokio::test]
async fn max_send_value_to_shielded_empties_the_wallet() {
    let ironwood_funding = 100_000;
    let sapling_funding = 50_000;
    let mut net = MockNet::launch().await;
    let mut sender = net
        .client(zingo_test_vectors::seeds::HOSPITAL_MUSEUM_SEED)
        .await;
    let sender_ua = get_base_address(&sender, PoolType::Shielded(ShieldedPool::Orchard)).await;
    let sender_sapling = get_base_address(&sender, PoolType::Shielded(ShieldedPool::Sapling)).await;

    net.chain.write().await.mine_empty_blocks(1);
    fund(
        &net,
        vec![
            (&sender_ua, ironwood_funding, None),
            (&sender_sapling, sapling_funding, None),
        ],
        1,
    )
    .await;
    sender.sync_and_await().await.unwrap();
    check_client_balances!(sender, i: ironwood_funding o: 0 s: sapling_funding t: 0);

    let recipient = external_address(PoolType::ORCHARD);
    let max_send_value = sender
        .max_send_value(
            ZcashAddress::try_from_encoded(&recipient).unwrap(),
            zip32::AccountId::ZERO,
        )
        .await
        .unwrap();
    assert!(
        max_send_value > Zatoshis::ZERO,
        "a funded wallet can send to a shielded address"
    );

    let one_zat = Zatoshis::const_from_u64(1);
    let past_max = (max_send_value + one_zat).unwrap();
    assert!(matches!(
        from_inputs::propose(&mut sender, vec![(&recipient, past_max.into_u64(), None)]).await,
        Err(ProposeSendError::Proposal(
            zcash_client_backend::data_api::error::Error::InsufficientFunds { .. }
        ))
    ));

    from_inputs::quick_send(
        &mut sender,
        vec![(&recipient, max_send_value.into_u64(), None)],
    )
    .await
    .unwrap();
    net.chain.write().await.mine_mempool();
    sender.sync_and_await().await.unwrap();

    check_client_balances!(sender, i: 0 o: 0 s: 0 t: 0);
}

/// Mock-chain twin of libtonode `slow::zero_value_receipts` (live
/// original kept as the control): a zero-value receipt must surface as
/// exactly one Received{0, Ironwood} value transfer and must not perturb
/// spendable arithmetic across a subsequent send.
#[tokio::test]
async fn zero_value_receipts() {
    use crate::wallet::summary::data::{SentWalletEvent, WalletEventKind};

    let mut net = MockNet::launch().await;
    let mut recipient = net
        .client(zingo_test_vectors::seeds::HOSPITAL_MUSEUM_SEED)
        .await;
    let recipient_ua =
        get_base_address(&recipient, PoolType::Shielded(ShieldedPool::Orchard)).await;

    net.chain.write().await.mine_empty_blocks(1);
    fund(&net, vec![(&recipient_ua, 100_000, None)], 1).await;
    // The zero-value receipt, in its own block as on the live chain.
    fund(&net, vec![(&recipient_ua, 0, None)], 1).await;

    recipient.sync_and_await().await.unwrap();
    from_inputs::quick_send(
        &mut recipient,
        vec![(&external_address(PoolType::ORCHARD), 1_000, None)],
    )
    .await
    .unwrap();
    net.chain.write().await.mine_mempool();
    net.chain.write().await.mine_empty_blocks(1);
    recipient.sync_and_await().await.unwrap();

    // The recipient holds the 100_000 funding
    // note less the 1_000 payment and its 10_000 ZIP-317 fee.
    check_client_balances!(recipient, i: 89_000 o: 0 s: 0 t: 0);

    let wallet_events = recipient.wallet_events(true).await.unwrap();
    assert!(
        wallet_events
            .iter()
            .any(|vt| vt.kind == WalletEventKind::Received && vt.value == 100_000)
    );
    assert_eq!(
        wallet_events
            .iter()
            .filter(|vt| vt.kind == WalletEventKind::Received
                && vt.value == 0
                && vt.pool_received.as_deref() == Some("Ironwood"))
            .count(),
        1
    );
    assert!(wallet_events.iter().any(|vt| {
        vt.kind == WalletEventKind::Sent(SentWalletEvent::Send)
            && vt.value == 1_000
            && vt.transaction_fee == Some(10_000)
    }));
}

/// Mock-chain twin of libtonode `slow::wallet_events_check_fees`
/// (live original kept as the control): a two-output cross-pool send to
/// the wallet's own transparent and sapling addresses costs the exact
/// composite ZIP-317 fee of 25_000. Every pool
/// balance lands where the arithmetic says. The
/// self-receipts arrive through real scanning of the mock blocks.
#[tokio::test]
async fn wallet_events_check_fees() {
    let mut net = MockNet::launch().await;
    let mut recipient = net
        .client(zingo_test_vectors::seeds::HOSPITAL_MUSEUM_SEED)
        .await;
    let recipient_ua =
        get_base_address(&recipient, PoolType::Shielded(ShieldedPool::Orchard)).await;
    let recipient_taddr = get_base_address(&recipient, PoolType::Transparent).await;
    let recipient_sapling =
        get_base_address(&recipient, PoolType::Shielded(ShieldedPool::Sapling)).await;

    net.chain.write().await.mine_empty_blocks(1);
    fund(&net, vec![(&recipient_ua, 100_000, None)], 1).await;
    recipient.sync_and_await().await.unwrap();
    check_client_balances!(recipient, i: 100_000 o: 0 s: 0 t: 0);

    from_inputs::quick_send(
        &mut recipient,
        vec![
            (recipient_taddr.as_str(), 30_000, None),
            (recipient_sapling.as_str(), 30_000, None),
        ],
    )
    .await
    .unwrap();
    net.chain.write().await.mine_mempool();
    recipient.sync_and_await().await.unwrap();

    // 100_000 − 30_000 − 30_000 − 25_000 fee = 15_000 ironwood change.
    check_client_balances!(recipient, i: 15_000 o: 0 s: 30_000 t: 30_000);
}

/// Mock-chain twin of libtonode
/// `slow::send_to_transparent_and_sapling_maintain_balance` (live
/// original kept as the control): full transaction-summary pinning
/// across funding waves, cross-pool sends, and the
/// Transmitted-to-Confirmed transition of an unmined send.
#[tokio::test]
async fn send_to_transparent_and_sapling_maintain_balance() {
    use zcash_protocol::consensus::BlockHeight;
    use zingo_test_vectors::TEST_TXID;
    use zingolib_common::status::ConfirmationStatus;

    use crate::testutils::{
        assert_transaction_summary_equality, assert_transaction_summary_exists,
    };
    use crate::utils::conversion::txid_from_hex_encoded_str;
    use crate::wallet::output::SpendStatus;
    use crate::wallet::summary::data::Scope as SummaryScope;
    use crate::wallet::summary::data::{
        BasicNoteSummary, OutgoingNoteSummary, SendType, TransactionKind, TransactionSummary,
    };

    let recipient_initial_funds = 100_000_000;
    let mut net = MockNet::launch().await;
    let mut recipient = net
        .client(zingo_test_vectors::seeds::HOSPITAL_MUSEUM_SEED)
        .await;
    let recipient_ua = get_base_address(&recipient, PoolType::IRONWOOD).await;
    // The external destinations: the abandon-art wallet's sapling UA and
    // first taddr, the same derivations the live faucet answers with.
    let external_sapling = external_address(PoolType::SAPLING);
    let external_taddr = {
        let external_wallet =
            SyntheticWalletBuilder::new(zingo_test_vectors::seeds::ABANDON_ART_SEED).build();
        external_wallet
            .transparent_addresses()
            .values()
            .next()
            .unwrap()
            .clone()
    };

    // Funding at height 2, as in the live layout (one empty launch block).
    net.chain.write().await.mine_empty_blocks(1);
    fund(
        &net,
        vec![(&recipient_ua, recipient_initial_funds, None)],
        0,
    )
    .await;
    recipient.sync_and_await().await.unwrap();

    let placeholder_txid = txid_from_hex_encoded_str(TEST_TXID).unwrap();
    let summary_orchard_receipt = TransactionSummary {
        txid: placeholder_txid,
        datetime: 0,
        status: ConfirmationStatus::Confirmed(BlockHeight::from_u32(2)),
        blockheight: BlockHeight::from_u32(2),
        kind: TransactionKind::Received,
        value: recipient_initial_funds,
        fee: Some(10_000),
        zec_price: None,
        ironwood_notes: vec![BasicNoteSummary::from_parts(
            recipient_initial_funds,
            SpendStatus::Spent(placeholder_txid),
            0,
            None,
        )],
        orchard_notes: vec![],
        sapling_notes: vec![],
        transparent_coins: vec![],
        outgoing_ironwood_notes: vec![],
        outgoing_orchard_notes: vec![],
        outgoing_sapling_notes: vec![],
        outgoing_transparent_coins: vec![],
    };

    // Send to external sapling, mined at height 3.
    let first_send_to_sapling = 20_000;
    from_inputs::quick_send(
        &mut recipient,
        vec![(&external_sapling, first_send_to_sapling, None)],
    )
    .await
    .unwrap();
    net.chain.write().await.mine_mempool();
    recipient.sync_and_await().await.unwrap();
    let summary_external_sapling = TransactionSummary {
        txid: placeholder_txid,
        datetime: 0,
        status: ConfirmationStatus::Confirmed(BlockHeight::from_u32(3)),
        blockheight: BlockHeight::from_u32(3),
        kind: TransactionKind::Sent(SendType::Send),
        value: first_send_to_sapling,
        fee: Some(20_000),
        zec_price: None,
        ironwood_notes: vec![BasicNoteSummary::from_parts(
            99_960_000,
            SpendStatus::TransmittedSpent(placeholder_txid),
            0,
            None,
        )],
        orchard_notes: vec![],
        sapling_notes: vec![],
        transparent_coins: vec![],
        outgoing_ironwood_notes: vec![],
        outgoing_orchard_notes: vec![],
        outgoing_sapling_notes: vec![OutgoingNoteSummary {
            output_index: 0,
            value: first_send_to_sapling,
            memo: None,
            recipient: "zregtestsapling1sa4rckrf4zs6ny3l3ljnezupacvxfnjjn90lpeaa4ddtjeyww2ypzqr3jxfsta3t8dn3jk8cm4f".to_string(),
            recipient_unified_address: Some("uregtest183rtm3qhxxermx3nxwa706va0xnypt3td648tayetchlp28hue08vrcnwq02ryyk5rh3y0xhftay8a5ynjdg8kr3juq5x0d9ygd5ffht".to_string()),
            account_id: zip32::AccountId::ZERO,
            scope: SummaryScope::from(zip32::Scope::External),
        }],
        outgoing_transparent_coins: vec![],
    };

    // Send to external transparent, left in the mempool: Transmitted,
    // targeting height 4.
    let first_send_to_transparent = 20_000;
    let summary_external_transparent = TransactionSummary {
        txid: placeholder_txid,
        datetime: 0,
        status: ConfirmationStatus::Transmitted(BlockHeight::from_u32(4)),
        blockheight: BlockHeight::from_u32(4),
        kind: TransactionKind::Sent(SendType::Send),
        value: first_send_to_transparent,
        fee: Some(15_000),
        zec_price: None,
        ironwood_notes: vec![BasicNoteSummary::from_parts(
            99_925_000,
            SpendStatus::Unspent,
            0,
            None,
        )],
        orchard_notes: vec![],
        sapling_notes: vec![],
        transparent_coins: vec![],
        outgoing_ironwood_notes: vec![],
        outgoing_orchard_notes: vec![],
        outgoing_sapling_notes: vec![],
        outgoing_transparent_coins: vec![],
    };
    from_inputs::quick_send(
        &mut recipient,
        vec![(&external_taddr, first_send_to_transparent, None)],
    )
    .await
    .unwrap();

    assert_transaction_summary_equality(
        &recipient.transaction_summaries(false).await.unwrap().0[0],
        &summary_orchard_receipt,
    );
    assert_transaction_summary_equality(
        &recipient.transaction_summaries(false).await.unwrap().0[1],
        &summary_external_sapling,
    );
    assert_transaction_summary_equality(
        &recipient.transaction_summaries(false).await.unwrap().0[2],
        &summary_external_transparent,
    );

    // Mid-flight balances: everything sits in the unmined send's change.
    let expected_funds = recipient_initial_funds
        - first_send_to_sapling
        - 20_000
        - first_send_to_transparent
        - 15_000;
    {
        let recipient_wallet = recipient.wallet();
        let recipient_wallet = recipient_wallet.read().await;
        assert_eq!(
            recipient_wallet
                .unconfirmed_balance::<IronwoodNote>(zip32::AccountId::ZERO)
                .unwrap(),
            expected_funds.try_into().unwrap()
        );
        assert_eq!(
            recipient_wallet
                .confirmed_balance::<IronwoodNote>(zip32::AccountId::ZERO)
                .unwrap(),
            0.try_into().unwrap()
        );
    }

    // The pending transparent send confirms at height 4.
    net.chain.write().await.mine_mempool();
    recipient.sync_and_await().await.unwrap();

    // Second funding wave, with a memo, at height 5.
    let recipient_second_funding = 1_000_000;
    fund(
        &net,
        vec![(
            &recipient_ua,
            recipient_second_funding,
            Some("Second wave incoming"),
        )],
        0,
    )
    .await;
    recipient.sync_and_await().await.unwrap();
    let summary_orchard_receipt_2 = TransactionSummary {
        txid: placeholder_txid,
        datetime: 0,
        status: ConfirmationStatus::Confirmed(BlockHeight::from_u32(5)),
        blockheight: BlockHeight::from_u32(5),
        kind: TransactionKind::Received,
        value: recipient_second_funding,
        fee: Some(10_000),
        zec_price: None,
        ironwood_notes: vec![BasicNoteSummary::from_parts(
            recipient_second_funding,
            SpendStatus::Spent(placeholder_txid),
            0,
            Some("Second wave incoming".to_string()),
        )],
        orchard_notes: vec![],
        sapling_notes: vec![],
        transparent_coins: vec![],
        outgoing_ironwood_notes: vec![],
        outgoing_orchard_notes: vec![],
        outgoing_sapling_notes: vec![],
        outgoing_transparent_coins: vec![],
    };

    // Second wave of external sends: transparent and sapling mined into
    // the same block, height 6.
    let second_send_to_transparent = 20_000;
    let second_send_to_sapling = 20_000;
    from_inputs::quick_send(
        &mut recipient,
        vec![(&external_taddr, second_send_to_transparent, None)],
    )
    .await
    .unwrap();
    from_inputs::quick_send(
        &mut recipient,
        vec![(&external_sapling, second_send_to_sapling, None)],
    )
    .await
    .unwrap();
    net.chain.write().await.mine_mempool();
    recipient.sync_and_await().await.unwrap();
    let summary_external_transparent_2 = TransactionSummary {
        txid: placeholder_txid,
        datetime: 0,
        status: ConfirmationStatus::Confirmed(BlockHeight::from_u32(6)),
        blockheight: BlockHeight::from_u32(6),
        kind: TransactionKind::Sent(SendType::Send),
        value: second_send_to_transparent,
        fee: Some(15_000),
        zec_price: None,
        ironwood_notes: vec![BasicNoteSummary::from_parts(
            965_000,
            SpendStatus::Spent(placeholder_txid),
            0,
            None,
        )],
        orchard_notes: vec![],
        sapling_notes: vec![],
        transparent_coins: vec![],
        outgoing_ironwood_notes: vec![],
        outgoing_orchard_notes: vec![],
        outgoing_sapling_notes: vec![],
        outgoing_transparent_coins: vec![],
    };
    let summary_external_sapling_2 = TransactionSummary {
        txid: placeholder_txid,
        datetime: 0,
        status: ConfirmationStatus::Confirmed(BlockHeight::from_u32(6)),
        blockheight: BlockHeight::from_u32(6),
        kind: TransactionKind::Sent(SendType::Send),
        value: second_send_to_sapling,
        fee: Some(20_000),
        zec_price: None,
        ironwood_notes: vec![BasicNoteSummary::from_parts(
            99_885_000,
            SpendStatus::Unspent,
            0,
            None,
        )],
        orchard_notes: vec![],
        sapling_notes: vec![],
        transparent_coins: vec![],
        outgoing_ironwood_notes: vec![],
        outgoing_orchard_notes: vec![],
        outgoing_sapling_notes: vec![OutgoingNoteSummary {
            output_index: 0,
            value: second_send_to_sapling,
            memo: None,
            recipient: "zregtestsapling1sa4rckrf4zs6ny3l3ljnezupacvxfnjjn90lpeaa4ddtjeyww2ypzqr3jxfsta3t8dn3jk8cm4f".to_string(),
            recipient_unified_address: Some("uregtest183rtm3qhxxermx3nxwa706va0xnypt3td648tayetchlp28hue08vrcnwq02ryyk5rh3y0xhftay8a5ynjdg8kr3juq5x0d9ygd5ffht".to_string()),
            account_id: zip32::AccountId::ZERO,
            scope: SummaryScope::from(zip32::Scope::External),
        }],
        outgoing_transparent_coins: vec![],
    };

    // Third external transparent, mined at height 7.
    let external_transparent_3 = 20_000;
    from_inputs::quick_send(
        &mut recipient,
        vec![(&external_taddr, external_transparent_3, None)],
    )
    .await
    .unwrap();
    net.chain.write().await.mine_mempool();
    recipient.sync_and_await().await.unwrap();
    let summary_external_transparent_3 = TransactionSummary {
        txid: placeholder_txid,
        datetime: 0,
        status: ConfirmationStatus::Confirmed(BlockHeight::from_u32(7)),
        blockheight: BlockHeight::from_u32(7),
        kind: TransactionKind::Sent(SendType::Send),
        value: external_transparent_3,
        fee: Some(15_000),
        zec_price: None,
        ironwood_notes: vec![BasicNoteSummary::from_parts(
            930_000,
            SpendStatus::Unspent,
            0,
            None,
        )],
        orchard_notes: vec![],
        sapling_notes: vec![],
        transparent_coins: vec![],
        outgoing_ironwood_notes: vec![],
        outgoing_orchard_notes: vec![],
        outgoing_sapling_notes: vec![],
        outgoing_transparent_coins: vec![],
    };

    assert_transaction_summary_equality(
        &recipient.transaction_summaries(false).await.unwrap().0[3],
        &summary_orchard_receipt_2,
    );
    // Two sends share height 6, so summary order within the block is
    // not pinned. Assert existence as the live test does.
    assert_transaction_summary_exists(&recipient, &summary_external_transparent_2).await;
    assert_transaction_summary_exists(&recipient, &summary_external_sapling_2).await;
    assert_transaction_summary_equality(
        &recipient.transaction_summaries(false).await.unwrap().0[6],
        &summary_external_transparent_3,
    );

    let second_wave_expected_funds = expected_funds + recipient_second_funding
        - second_send_to_sapling
        - second_send_to_transparent
        - external_transparent_3
        - 50_000;
    assert_eq!(
        recipient
            .wallet()
            .read()
            .await
            .confirmed_balance::<IronwoodNote>(zip32::AccountId::ZERO)
            .unwrap(),
        second_wave_expected_funds.try_into().unwrap(),
    );
}

/// Mock-chain twin of libtonode `slow::from_t_z_o_tz_to_zo_tzo_to_orchard`
/// (live original kept as the control): the full pool-promotion ledger,
/// every funding source and self-send combination, two shields, exact
/// per-step balances, and the cumulative confirmed-fee total, driven
/// through real scanning of the mock chain.
///
/// Ignored because its ledger predates V6 and every step needs
/// re-deriving. Once re-derived it still meets the `darkside_test` hazard
/// of zingolabs/zingolib#2447: that subtractive pepper-sync feature
/// compiles out the transparent-address discovery this test's funding
/// depends on whenever feature unification enables it in multi-package
/// builds.
#[ignore = "The ledger's fees and amounts predate V6. Re-derive every step \
            before un-ignoring (step 10 under-drains, stranding 10_000 in sapling, because \
            V6's two-bundle fees lead the planner to leave the sapling note unspent)"]
#[tokio::test]
async fn from_t_z_o_tz_to_zo_tzo_to_orchard() {
    use crate::testutils::lightclient::get_fees_paid_by_client;
    use crate::testutils::send::error::ProposeSendError;
    use crate::testutils::send::error::SendError;
    use zcash_protocol::value::Zatoshis;

    let mut net = MockNet::launch().await;
    let mut client = net
        .client(zingo_test_vectors::seeds::HOSPITAL_MUSEUM_SEED)
        .await;
    let pmc_unified = get_base_address(&client, PoolType::Shielded(ShieldedPool::Orchard)).await;
    let pmc_taddr = get_base_address(&client, PoolType::Transparent).await;
    let pmc_sapling = get_base_address(&client, PoolType::Shielded(ShieldedPool::Sapling)).await;

    net.chain.write().await.mine_empty_blocks(1);

    macro_rules! bump_and_check {
        (o: $o:tt i: $i:tt s: $s:tt t: $t:tt) => {
            net.chain.write().await.mine_mempool();
            client.sync_and_await().await.unwrap();
            check_client_balances!(client, i: $i o:$o s:$s t:$t);
        };
    }

    let mut total_expected_fee = 0;
    // 1 receive 50_000 transparent. Expanded rather than using
    // bump_and_check so a failure surfaces the mock's taddr-request
    // ledger and the wallet's record count (nextest shows this stderr
    // only when the test fails).
    fund(&net, vec![(&pmc_taddr, 50_000, None)], 0).await;
    net.chain.write().await.mine_mempool();
    client.sync_and_await().await.unwrap();
    {
        let chain = net.chain.read().await;
        eprintln!("step-1 diagnostics: mock tip {}", chain.tip());
        eprintln!("taddr requests served: {:#?}", chain.taddr_request_log());
        let wallet = client.wallet();
        let wallet = wallet.read().await;
        eprintln!("wallet transactions: {}", wallet.wallet_transactions.len());
    }
    check_client_balances!(client, i: 0 o: 0 s: 0 t: 50_000);
    assert_eq!(get_fees_paid_by_client(&client).await, total_expected_fee);

    // 2 shield 50_000 transparent to orchard: 15_000 (1 t-in, 2 orchard)
    client.quick_shield(zip32::AccountId::ZERO).await.unwrap();
    bump_and_check!(o: 0 i: 35_000 s: 0 t: 0);
    total_expected_fee += 15_000;
    assert_eq!(get_fees_paid_by_client(&client).await, total_expected_fee);

    // 3 receive 50_000 sapling
    fund(&net, vec![(&pmc_sapling, 50_000, None)], 0).await;
    bump_and_check!(o: 0 i: 35_000 s: 50_000 t: 0);
    assert_eq!(get_fees_paid_by_client(&client).await, total_expected_fee);

    // 4 migrate sapling to orchard: 20_000 (2 sapling, 2 orchard)
    from_inputs::quick_send(&mut client, vec![(&pmc_unified, 30_000, None)])
        .await
        .unwrap();
    bump_and_check!(o: 0 i: 65_000 s: 0 t: 0);
    total_expected_fee += 20_000;
    assert_eq!(get_fees_paid_by_client(&client).await, total_expected_fee);

    // 5 orchard self-send: 10_000
    from_inputs::quick_send(&mut client, vec![(&pmc_unified, 55_000, None)])
        .await
        .unwrap();
    bump_and_check!(o: 0 i: 55_000 s: 0 t: 0);
    total_expected_fee += 10_000;
    assert_eq!(get_fees_paid_by_client(&client).await, total_expected_fee);

    // 6 orchard to own transparent and sapling: 25_000
    from_inputs::quick_send(
        &mut client,
        vec![(&pmc_taddr, 10_000, None), (&pmc_sapling, 10_000, None)],
    )
    .await
    .unwrap();
    bump_and_check!(o: 0 i: 10_000 s: 10_000 t: 10_000);
    total_expected_fee += 25_000;
    assert_eq!(get_fees_paid_by_client(&client).await, total_expected_fee);

    // 7 receive 500_000 transparent
    fund(&net, vec![(&pmc_taddr, 500_000, None)], 0).await;
    bump_and_check!(o: 0 i: 10_000 s: 10_000 t: 510_000);
    assert_eq!(get_fees_paid_by_client(&client).await, total_expected_fee);

    // 8 shield both coins: 20_000 (2 t-in, 2 orchard)
    client.quick_shield(zip32::AccountId::ZERO).await.unwrap();
    bump_and_check!(o: 0 i: 500_000 s: 10_000 t: 0);
    total_expected_fee += 20_000;
    assert_eq!(get_fees_paid_by_client(&client).await, total_expected_fee);

    // 9 orchard self-send: 10_000
    from_inputs::quick_send(&mut client, vec![(&pmc_unified, 30_000, None)])
        .await
        .unwrap();
    bump_and_check!(o: 0 i: 490_000 s: 10_000 t: 0);
    total_expected_fee += 10_000;
    assert_eq!(get_fees_paid_by_client(&client).await, total_expected_fee);

    // 10 orchard + sapling demoted to own transparent: 30_000
    from_inputs::quick_send(&mut client, vec![(&pmc_taddr, 470_000, None)])
        .await
        .unwrap();
    bump_and_check!(o: 0 i: 0 s: 0 t: 470_000);
    total_expected_fee += 30_000;
    assert_eq!(get_fees_paid_by_client(&client).await, total_expected_fee);

    // 10b transparent-to-transparent is refused: transparent funds are
    // not send-spendable.
    match from_inputs::quick_send(&mut client, vec![(&pmc_taddr, 10_000, None)]).await {
        Err(SendError::ProposeSendError(ProposeSendError::Proposal(
            zcash_client_backend::data_api::error::Error::InsufficientFunds {
                available,
                required,
            },
        ))) => {
            assert_eq!(available, Zatoshis::from_u64(0).unwrap());
            assert_eq!(required, Zatoshis::from_u64(20_000).unwrap());
        }
        other => panic!("expected InsufficientFunds, got {other:?}"),
    }
    bump_and_check!(o: 0 i: 0 s: 0 t: 470_000);
    assert_eq!(get_fees_paid_by_client(&client).await, total_expected_fee);

    // 11 transparent-to-sapling likewise refused.
    match from_inputs::quick_send(&mut client, vec![(&pmc_sapling, 50_000, None)]).await {
        Err(SendError::ProposeSendError(ProposeSendError::Proposal(
            zcash_client_backend::data_api::error::Error::InsufficientFunds {
                available,
                required,
            },
        ))) => {
            assert_eq!(available, Zatoshis::from_u64(0).unwrap());
            assert_eq!(required, Zatoshis::from_u64(60_000).unwrap());
        }
        other => panic!("expected InsufficientFunds, got {other:?}"),
    }
    bump_and_check!(o: 0 i: 0 s: 0 t: 470_000);
    assert_eq!(get_fees_paid_by_client(&client).await, total_expected_fee);

    // 12 shield: 15_000 (1 t-in, 2 orchard)
    client.quick_shield(zip32::AccountId::ZERO).await.unwrap();
    bump_and_check!(o: 0 i: 455_000 s: 0 t: 0);
    total_expected_fee += 15_000;
    assert_eq!(get_fees_paid_by_client(&client).await, total_expected_fee);

    // 13 orchard to own sapling: 20_000
    from_inputs::quick_send(&mut client, vec![(&pmc_sapling, 10_000, None)])
        .await
        .unwrap();
    bump_and_check!(o: 0 i: 425_000 s: 10_000 t: 0);
    total_expected_fee += 20_000;
    assert_eq!(get_fees_paid_by_client(&client).await, total_expected_fee);

    // 14 orchard self-send: 10_000
    from_inputs::quick_send(&mut client, vec![(&pmc_unified, 20_000, None)])
        .await
        .unwrap();
    bump_and_check!(o: 0 i: 415_000 s: 10_000 t: 0);
    total_expected_fee += 10_000;
    assert_eq!(get_fees_paid_by_client(&client).await, total_expected_fee);

    // 15 orchard + sapling to own sapling: 20_000
    from_inputs::quick_send(&mut client, vec![(&pmc_sapling, 405_000, None)])
        .await
        .unwrap();
    bump_and_check!(o: 0 i: 0 s: 405_000 t: 0);
    total_expected_fee += 20_000;
    assert_eq!(get_fees_paid_by_client(&client).await, total_expected_fee);

    // 16 sapling self-send: 10_000
    from_inputs::quick_send(&mut client, vec![(&pmc_sapling, 380_000, None)])
        .await
        .unwrap();
    bump_and_check!(o: 0 i: 0 s: 395_000 t: 0);
    total_expected_fee += 10_000;
    assert_eq!(get_fees_paid_by_client(&client).await, total_expected_fee);
}

/// Deterministic reproduction of issue #2450 (no live original: the
/// live shape is a load-dependent flake, three libtonode tests
/// failing whenever a slow validator response crosses the wallet's
/// send timeout). The first submission is accepted into the mock
/// mempool but its response is lost. The wallet's retry then receives
/// the validator's duplicate rejection, verbatim as zainod surfaces it
/// (zingolabs/zaino#1392). That rejection is proof of successful
/// transmission: the send must return Ok, the transaction must not be
/// marked Failed, and it must confirm with ordinary balance
/// arithmetic.
#[ignore = "needs upstream zingolib 852537e09, which treats a duplicate-in-mempool rejection as transmission success"]
#[tokio::test]
async fn send_survives_lost_response_and_duplicate_rejection() {
    use crate::testutils::mock_indexer::LostSendDestination;
    use zingolib_common::status::ConfirmationStatus;

    let mut net = MockNet::launch().await;
    let mut recipient = net
        .client(zingo_test_vectors::seeds::HOSPITAL_MUSEUM_SEED)
        .await;
    let recipient_ua =
        get_base_address(&recipient, PoolType::Shielded(ShieldedPool::Orchard)).await;

    net.chain.write().await.mine_empty_blocks(1);
    fund(&net, vec![(&recipient_ua, 100_000, None)], 1).await;
    recipient.sync_and_await().await.unwrap();
    check_client_balances!(recipient, i: 100_000 o: 0 s: 0 t: 0);

    net.chain.write().await.lose_next_send_response = Some(LostSendDestination::Mempool);

    let txids = from_inputs::quick_send(
        &mut recipient,
        vec![(&external_address(PoolType::ORCHARD), 20_000, None)],
    )
    .await
    .expect("a duplicate-in-mempool rejection proves transmission succeeded");

    // The wallet must not record the live transaction as Failed.
    {
        let wallet = recipient.wallet().read().await;
        for txid in txids.iter() {
            let status = wallet
                .wallet_transactions
                .get(txid)
                .expect("the transmitted transaction stays in the wallet")
                .status();
            assert!(
                !matches!(status, ConfirmationStatus::Failed(_)),
                "transaction {txid} marked Failed while live in the mempool"
            );
        }
    }

    net.chain.write().await.mine_mempool();
    recipient.sync_and_await().await.unwrap();
    // Identical arithmetic to `funded_send_confirms_on_the_mock_chain`:
    // the lost response and duplicate rejection must not perturb it.
    check_client_balances!(recipient, i: 70_000 o: 0 s: 0 t: 0);
}

/// Twin of [`send_survives_lost_response_and_duplicate_rejection`] for
/// the validator's earlier phase: the lost-response submission is
/// still in the download/verification queue when the retry arrives, so
/// the rejection reads "transaction dropped because it is already
/// queued for download" (zebra's pre-acceptance duplicate check),
/// observed live in the 2026-07-11 container runs, where verification
/// lagged the send by seconds under load. That rejection proves
/// delivery but not minability, so the wallet must hold success until
/// its probes see the storage-backed mempool rejection, and only then
/// return Ok, keeping send-Ok ⇒ minable-now. The mock answers two
/// queued rejections before promoting, so the probe loop is exercised
/// deterministically. Mining immediately after the send must therefore
/// confirm the transaction.
#[ignore = "needs upstream zingolib 96c7e2833, which holds send success on a queued-for-download rejection as transmission success"]
#[tokio::test]
async fn send_survives_lost_response_and_queued_duplicate_rejection() {
    use crate::testutils::mock_indexer::LostSendDestination;
    use zingolib_common::status::ConfirmationStatus;

    let mut net = MockNet::launch().await;
    let mut recipient = net
        .client(zingo_test_vectors::seeds::HOSPITAL_MUSEUM_SEED)
        .await;
    let recipient_ua =
        get_base_address(&recipient, PoolType::Shielded(ShieldedPool::Orchard)).await;

    net.chain.write().await.mine_empty_blocks(1);
    fund(&net, vec![(&recipient_ua, 100_000, None)], 1).await;
    recipient.sync_and_await().await.unwrap();
    check_client_balances!(recipient, i: 100_000 o: 0 s: 0 t: 0);

    {
        let mut chain = net.chain.write().await;
        chain.lose_next_send_response = Some(LostSendDestination::DownloadQueue);
        chain.queued_rejections_before_promotion = 2;
    }

    let txids = from_inputs::quick_send(
        &mut recipient,
        vec![(&external_address(PoolType::ORCHARD), 20_000, None)],
    )
    .await
    .expect("probing resubmissions reach the storage-backed verdict");

    // The wallet must not record the delivered transaction as Failed.
    {
        let wallet = recipient.wallet().read().await;
        for txid in txids.iter() {
            let status = wallet
                .wallet_transactions
                .get(txid)
                .expect("the transmitted transaction stays in the wallet")
                .status();
            assert!(
                !matches!(status, ConfirmationStatus::Failed(_)),
                "transaction {txid} marked Failed while queued for download"
            );
        }
    }

    // send-Ok means minable NOW: mining immediately (the very race
    // that broke test_scanning_in_watch_only_mode live) must include
    // the transaction, with no verification-delay allowance.
    net.chain.write().await.mine_mempool();
    recipient.sync_and_await().await.unwrap();
    check_client_balances!(recipient, i: 70_000 o: 0 s: 0 t: 0);
}

mod strict_chain {
    use tonic::Code;
    use zcash_protocol::consensus::COINBASE_MATURITY_BLOCKS;
    use zcash_protocol::value::Zatoshis;
    use zingolib_common::status::ConfirmationStatus;

    use crate::lightclient::LightClient;
    use crate::testutils::chain_generics::fixtures;
    use crate::testutils::mock_indexer::{Fault, Rpc};

    use super::*;

    const FUNDING: u64 = 100_000;
    const PAYMENT: u64 = 10_000;
    const REWARD_ZATS: u64 = 1_000_000;
    const REWARD: Zatoshis = Zatoshis::const_from_u64(REWARD_ZATS);

    async fn funded_sender(net: &mut MockNet) -> LightClient {
        let mut sender = net
            .client(zingo_test_vectors::seeds::HOSPITAL_MUSEUM_SEED)
            .await;
        let sender_ua = get_base_address(&sender, PoolType::Shielded(ShieldedPool::Orchard)).await;
        fund(net, vec![(&sender_ua, FUNDING, None)], 1).await;
        sender.sync_and_await().await.unwrap();
        check_client_balances!(sender, i: FUNDING o: 0 s: 0 t: 0);
        sender
    }

    #[tokio::test]
    async fn transmitted_transaction_has_mempool_status_before_mining() {
        let mut net = MockNet::launch().await;
        let mut sender = funded_sender(&mut net).await;
        let txids = from_inputs::quick_send(
            &mut sender,
            vec![(&external_address(PoolType::ORCHARD), PAYMENT, None)],
        )
        .await
        .unwrap();
        assert_eq!(net.chain.read().await.mempool_len(), 1);

        sender.sync_and_await().await.unwrap();
        let summaries = sender.transaction_summaries(false).await.unwrap();
        let sent = summaries
            .iter()
            .find(|summary| summary.txid == txids[0])
            .unwrap();
        assert!(
            matches!(sent.status, ConfirmationStatus::Mempool(_)),
            "{:?}",
            sent.status
        );

        net.chain.write().await.mine_mempool();
        sender.sync_and_await().await.unwrap();
        let summaries = sender.transaction_summaries(false).await.unwrap();
        let sent = summaries
            .iter()
            .find(|summary| summary.txid == txids[0])
            .unwrap();
        assert!(matches!(sent.status, ConfirmationStatus::Confirmed(_)));
    }

    #[tokio::test]
    async fn sent_transaction_is_not_requested_by_txid_after_it_is_mined() {
        let mut net = MockNet::launch().await;
        let mut sender = funded_sender(&mut net).await;
        let txids = from_inputs::quick_send(
            &mut sender,
            vec![(&external_address(PoolType::ORCHARD), PAYMENT, None)],
        )
        .await
        .unwrap();
        {
            let mut chain = net.chain.write().await;
            chain.mine_mempool();
            let outage = Fault::Fail(Code::Unavailable, "mock outage".to_string());
            chain.faults.inject(Rpc::Transaction, outage);
        }

        sender.sync_and_await().await.unwrap();
        assert_eq!(net.chain.read().await.faults.pending(Rpc::Transaction), 1);
        let summaries = sender.transaction_summaries(false).await.unwrap();
        let sent = summaries
            .iter()
            .find(|summary| summary.txid == txids[0])
            .unwrap();
        assert!(matches!(sent.status, ConfirmationStatus::Confirmed(_)));
    }

    #[tokio::test]
    async fn transaction_list_returns_each_transaction_in_the_requested_sequence() {
        use futures::TryStreamExt as _;

        let mut net = MockNet::launch().await;
        let mut sender = funded_sender(&mut net).await;
        let mut txids = Vec::new();
        let address = external_address(PoolType::ORCHARD);
        for _ in 0..2 {
            let payment = vec![(address.as_str(), PAYMENT, None)];
            txids.extend(from_inputs::quick_send(&mut sender, payment).await.unwrap());
            net.chain.write().await.mine_mempool();
            sender.sync_and_await().await.unwrap();
        }
        txids.reverse();

        let fetched = sender
            .source_set()
            .transactions(txids.clone())
            .await
            .unwrap();
        let transactions: Vec<_> = fetched.value.try_collect().await.unwrap();

        let branch_id = zcash_protocol::consensus::BranchId::Nu6_1;
        let returned: Vec<_> = transactions
            .iter()
            .map(|raw| {
                let transaction =
                    zcash_primitives::transaction::Transaction::read(&raw.data[..], branch_id);
                transaction.unwrap().txid()
            })
            .collect();
        assert_eq!(returned, txids);
    }

    #[tokio::test]
    async fn sync_recovers_from_a_truncated_block_stream() {
        let mut net = MockNet::launch().await;
        let mut recipient = net
            .client(zingo_test_vectors::seeds::HOSPITAL_MUSEUM_SEED)
            .await;
        let recipient_ua =
            get_base_address(&recipient, PoolType::Shielded(ShieldedPool::Orchard)).await;
        fund(&net, vec![(&recipient_ua, FUNDING, None)], 3).await;
        net.chain
            .write()
            .await
            .faults
            .inject(Rpc::BlockRange, Fault::TruncateStream { after: 1 });

        recipient.sync_and_await().await.unwrap();
        assert_eq!(net.chain.read().await.faults.pending(Rpc::BlockRange), 0);
        check_client_balances!(recipient, i: FUNDING o: 0 s: 0 t: 0);
    }

    #[tokio::test]
    async fn sync_fails_while_the_indexer_is_unavailable() {
        let mut net = MockNet::launch().await;
        let mut client = net
            .client(zingo_test_vectors::seeds::HOSPITAL_MUSEUM_SEED)
            .await;
        net.chain.write().await.mine_empty_blocks(2);
        {
            let mut chain = net.chain.write().await;
            for _ in 0..5 {
                chain.faults.inject(
                    Rpc::LatestBlock,
                    Fault::Fail(Code::Unavailable, "mock outage".to_string()),
                );
            }
        }
        assert!(client.sync_and_await().await.is_err());
    }

    #[tokio::test]
    async fn coinbase_reward_becomes_spendable_after_maturity() {
        let mut net = MockNet::launch().await;
        let mut miner = net
            .client(zingo_test_vectors::seeds::HOSPITAL_MUSEUM_SEED)
            .await;
        let miner_taddr = get_base_address(&miner, PoolType::Transparent).await;
        let coinbase = {
            let mut chain = net.chain.write().await;
            let coinbase = chain.mine_block_rewarding(&miner_taddr, REWARD, vec![]);
            chain.mine_empty_blocks(1);
            coinbase
        };
        miner.sync_and_await().await.unwrap();
        check_client_balances!(miner, i: 0 o: 0 s: 0 t: 0);
        let summaries = miner.transaction_summaries(false).await.unwrap();
        assert!(summaries.iter().any(|summary| summary.value == REWARD_ZATS
            && summary.status == ConfirmationStatus::Confirmed(BlockHeight::from_u32(1))));
        assert!(!coinbase.is_empty());

        net.chain
            .write()
            .await
            .mine_empty_blocks(COINBASE_MATURITY_BLOCKS);
        miner.sync_and_await().await.unwrap();
        check_client_balances!(miner, i: 0 o: 0 s: 0 t: REWARD_ZATS);

        miner.quick_shield(zip32::AccountId::ZERO).await.unwrap();
        assert_eq!(net.chain.read().await.mempool_len(), 1);
        net.chain.write().await.mine_mempool();
        miner.sync_and_await().await.unwrap();
        let balance = miner.account_balance(zip32::AccountId::ZERO).await.unwrap();
        assert_eq!(balance.confirmed_transparent_balance.unwrap().into_u64(), 0);
        assert!(balance.total_ironwood_balance.unwrap().into_u64() > 0);
    }

    #[tokio::test]
    async fn generate_a_range_of_wallet_events_on_the_mock_chain() {
        fixtures::create_various_wallet_events::<MockNet>().await;
    }

    #[tokio::test]
    async fn send_shield_cycle_on_the_mock_chain() {
        fixtures::send_shield_cycle::<MockNet>(1).await;
    }
}

#[tokio::test]
async fn chain_tip_birthday_is_100_blocks_below_the_tip() {
    let mut net = MockNet::launch().await;
    net.generate_blocks(150).await;

    let client = net.create_client().await;

    let chain_tip = client.server_info().await.unwrap().block_height;
    assert_eq!(client.birthday(), u32::from(chain_tip) - 100);
}

mod sources {
    use std::num::NonZeroUsize;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use chain_router::{
        AllowedIdentifiers, BoxFuture, Connection, Connector, Integrity, Quorum, RequestEvent,
        RequestKind, SourceError, SourceExposure, SourceId, TransportInfo, TxidRule,
        WalletIdentifier,
    };
    use lightwallet_grpc_client::GrpcIndexer;
    use pepper_sync::AssumedOnly;
    use pepper_sync::config::TransparentAddressDiscovery;
    use pepper_sync::error::{ServerError, SyncError};
    use pepper_sync::sync::SyncResult;

    use super::*;
    use crate::config::{Birthday, WalletConfig};
    use crate::lightclient::error::LightClientError;
    use crate::lightclient::store::MemoryStore;
    use crate::lightclient::{Encryption, LightClient, LightClientBuilder};
    use crate::testutils::default_test_wallet_settings;

    async fn indexer(net: &MockNet) -> GrpcIndexer {
        GrpcIndexer::new(net.indexer_uri())
            .await
            .unwrap()
            .allow_identifiers(AllowedIdentifiers::all())
    }

    async fn create(builder: LightClientBuilder) -> LightClient {
        let wallet_config = WalletConfig::NewSeed {
            no_of_accounts: 1.try_into().unwrap(),
            birthday: Birthday::Height(1),
            wallet_settings: default_test_wallet_settings(),
        };
        builder
            .create(MemoryStore::default(), wallet_config, Encryption::None)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn assumed_only_strategy_fails_on_an_unverified_source() {
        let net = MockNet::launch().await;
        net.generate_blocks(10).await;
        let builder = LightClient::builder(net.chain_type())
            .source(indexer(&net).await)
            .sync(AssumedOnly);
        let mut client = create(builder).await;

        let error = client.sync_and_await().await.unwrap_err();
        assert!(matches!(
            error,
            LightClientError::SyncError(SyncError::ServerError(ServerError::IntegrityRequired))
        ));
    }

    struct CountingTcp(Arc<AtomicUsize>);

    impl Connector for CountingTcp {
        fn connect<'a>(
            &'a self,
            host: &'a str,
            port: u16,
        ) -> BoxFuture<'a, std::io::Result<Box<dyn Connection>>> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move {
                let stream = tokio::net::TcpStream::connect((host, port)).await?;
                Ok(Box::new(stream) as Box<dyn Connection>)
            })
        }

        fn info(&self) -> TransportInfo {
            let mut info = TransportInfo::direct();
            info.exposed_to_source = SourceExposure::Nothing;
            info
        }
    }

    #[tokio::test]
    async fn sync_works_through_a_custom_connector() {
        let net = MockNet::launch().await;
        net.generate_blocks(10).await;
        let dials = Arc::new(AtomicUsize::new(0));
        let indexer = GrpcIndexer::with_connector(net.indexer_uri(), CountingTcp(dials.clone()))
            .unwrap()
            .allow_identifiers(AllowedIdentifiers::all());
        let mut client = create(LightClient::builder(net.chain_type()).source(indexer)).await;

        let synced = client.sync_and_await().await.unwrap();
        assert_eq!(u32::from(synced.sync_end_height), net.chain_height().await);
        assert!(dials.load(Ordering::Relaxed) > 0);
        let transport = client.source_set().sources().first().unwrap().transport;
        assert_eq!(transport.exposed_to_source, SourceExposure::Nothing);
    }

    #[tokio::test]
    async fn indexer_short_form_gives_an_event_for_each_request() {
        let net = MockNet::launch().await;
        net.generate_blocks(10).await;
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let copy = events.clone();
        let builder = LightClient::builder(net.chain_type())
            .indexer(
                net.indexer_uri(),
                Integrity::Assumed,
                AllowedIdentifiers::all(),
            )
            .unwrap()
            .request_events(move |event: &RequestEvent| copy.lock().unwrap().push(event.clone()));
        let mut client = create(builder).await;

        client.sync_and_await().await.unwrap();

        let events = events.lock().unwrap();
        assert!(events.iter().any(|event| event.kind == RequestKind::Blocks));
        assert!(events.iter().all(|event| event.result.is_ok()));
        assert!(events.iter().all(|event| event.source == SourceId(0)));
    }

    #[tokio::test]
    async fn assumed_only_strategy_syncs_from_an_assumed_source() {
        let net = MockNet::launch().await;
        net.generate_blocks(10).await;
        let builder = LightClient::builder(net.chain_type())
            .source(indexer(&net).await.with_integrity(Integrity::Assumed))
            .sync(AssumedOnly);
        let mut client = create(builder).await;

        let synced = client.sync_and_await().await.unwrap();
        assert_eq!(u32::from(synced.sync_end_height), net.chain_height().await);
    }

    /// The result of one sync through an indexer that allows `identifiers`, and the requests
    /// that the sync sent.
    async fn sync_through(
        identifiers: AllowedIdentifiers,
        discovery: TransparentAddressDiscovery,
    ) -> (Result<SyncResult, LightClientError>, Vec<RequestKind>) {
        let net = MockNet::launch().await;
        net.generate_blocks(10).await;
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let copy = events.clone();
        let builder = LightClient::builder(net.chain_type())
            .indexer(net.indexer_uri(), Integrity::Assumed, identifiers)
            .unwrap()
            .request_events(move |event: &RequestEvent| copy.lock().unwrap().push(event.kind));
        let mut wallet_settings = default_test_wallet_settings();
        wallet_settings.sync_config.transparent_address_discovery = discovery;
        let wallet_config = WalletConfig::NewSeed {
            no_of_accounts: 1.try_into().unwrap(),
            birthday: Birthday::Height(1),
            wallet_settings,
        };
        let mut client = builder
            .create(MemoryStore::default(), wallet_config, Encryption::None)
            .await
            .unwrap();
        events.lock().unwrap().clear();

        let synced = client.sync_and_await().await;
        let sent = events.lock().unwrap().clone();
        (synced, sent)
    }

    const ONLY_TXID: AllowedIdentifiers = AllowedIdentifiers {
        txid: TxidRule::Alone,
        ..AllowedIdentifiers::none()
    };

    fn not_allowed(error: LightClientError) -> Option<WalletIdentifier> {
        match error {
            LightClientError::SyncError(SyncError::ServerError(ServerError::RequestFailed(
                SourceError::IdentifierNotAllowed(identifier),
            ))) => Some(identifier),
            _ => None,
        }
    }

    #[tokio::test]
    async fn sync_fails_before_any_request_if_no_source_allows_a_needed_identifier() {
        let only_address = AllowedIdentifiers {
            transparent_address: true,
            ..AllowedIdentifiers::none()
        };
        let cases = [
            (only_address, WalletIdentifier::Txid),
            (ONLY_TXID, WalletIdentifier::TransparentAddress),
        ];
        for (allowed, missing) in cases {
            let (synced, sent) =
                sync_through(allowed, TransparentAddressDiscovery::minimal()).await;
            assert_eq!(not_allowed(synced.unwrap_err()), Some(missing));
            assert!(sent.is_empty(), "{sent:?}");
        }
    }

    #[tokio::test]
    async fn sync_needs_no_transparent_address_if_discovery_is_off() {
        let (synced, sent) = sync_through(ONLY_TXID, TransparentAddressDiscovery::disabled()).await;

        synced.unwrap();
        assert!(sent.contains(&RequestKind::Blocks));
        assert!(!sent.contains(&RequestKind::AddressTransactions));
    }

    type Kinds = Arc<std::sync::Mutex<Vec<RequestKind>>>;

    /// A client whose indexer can receive a txid only among `minimum` txids, and the kinds of
    /// the requests that it sends.
    async fn decoy_client(net: &MockNet, minimum: usize) -> (LightClient, Kinds) {
        let kinds = Kinds::default();
        let copy = kinds.clone();
        let allowed = AllowedIdentifiers {
            txid: TxidRule::Among(minimum),
            transparent_address: true,
            sent_transaction: false,
        };
        let builder = LightClient::builder(net.chain_type())
            .indexer(net.indexer_uri(), Integrity::Assumed, allowed)
            .unwrap()
            .request_events(move |event: &RequestEvent| copy.lock().unwrap().push(event.kind));
        (create(builder).await, kinds)
    }

    fn decoy_requests(kinds: &Kinds) -> usize {
        let mut kinds = kinds.lock().unwrap();
        assert!(!kinds.contains(&RequestKind::Transaction));
        let count = kinds
            .iter()
            .filter(|kind| **kind == RequestKind::Transactions)
            .count();
        kinds.clear();
        count
    }

    #[tokio::test]
    async fn sync_gets_wallet_transactions_among_decoys() {
        let net = MockNet::launch().await;
        let (mut client, kinds) = decoy_client(&net, 2).await;
        let address = get_base_address(&client, PoolType::Shielded(ShieldedPool::Orchard)).await;
        fund(&net, vec![(&address, 100_000, None)], 1).await;
        fund(&net, vec![(&address, 50_000, None)], 1).await;

        client.sync_and_await().await.unwrap();

        check_client_balances!(client, i: 150_000 o: 0 s: 0 t: 0);
        assert!(decoy_requests(&kinds) > 0);
    }

    #[tokio::test]
    async fn wallet_transaction_at_the_tip_needs_no_request_after_the_tip_request() {
        let net = MockNet::launch().await;
        let (mut client, kinds) = decoy_client(&net, 1).await;
        let address = get_base_address(&client, PoolType::Shielded(ShieldedPool::Orchard)).await;
        fund(&net, vec![(&address, 100_000, None)], 0).await;

        client.sync_and_await().await.unwrap();

        check_client_balances!(client, i: 100_000 o: 0 s: 0 t: 0);
        assert_eq!(decoy_requests(&kinds), 1);
    }

    #[tokio::test]
    async fn sync_fails_if_the_chain_has_too_few_transactions_for_the_decoy_minimum() {
        let net = MockNet::launch().await;
        let (mut client, _kinds) = decoy_client(&net, 1000).await;
        let address = get_base_address(&client, PoolType::Shielded(ShieldedPool::Orchard)).await;
        fund(&net, vec![(&address, 100_000, None)], 1).await;

        let not_allowed = not_allowed(client.sync_and_await().await.unwrap_err());
        assert_eq!(not_allowed, Some(WalletIdentifier::Txid));
    }

    #[tokio::test]
    async fn sync_requests_the_tip_transactions_once_for_each_new_tip() {
        let net = MockNet::launch().await;
        let (mut client, kinds) = decoy_client(&net, 1).await;
        let other = external_address(PoolType::ORCHARD);
        fund(&net, vec![(&other, 100_000, None)], 1).await;

        client.sync_and_await().await.unwrap();
        assert_eq!(decoy_requests(&kinds), 1);

        client.sync_and_await().await.unwrap();
        assert_eq!(decoy_requests(&kinds), 0);

        fund(&net, vec![(&other, 100_000, None)], 0).await;
        client.sync_and_await().await.unwrap();
        assert_eq!(decoy_requests(&kinds), 1);
    }

    /// The time for two transaction requests at the same time, each with a delay at the indexer.
    async fn two_delayed_requests(indexer: GrpcIndexer, net: &MockNet) -> Duration {
        use crate::testutils::mock_indexer;

        const DELAY: Duration = Duration::from_millis(300);
        for _ in 0..2 {
            let mut chain = net.chain.write().await;
            chain.faults.inject(
                mock_indexer::Rpc::Transaction,
                mock_indexer::Fault::Delay(DELAY),
            );
        }
        let indexer = indexer.allow_identifiers(AllowedIdentifiers::all());
        let sources = chain_router::SourceSet::single(indexer);
        let txid = zcash_protocol::TxId::from_bytes([0; 32]);

        let start = Instant::now();
        let _ignore_errors = tokio::join!(sources.transaction(txid), sources.transaction(txid));
        start.elapsed()
    }

    #[tokio::test]
    async fn concurrency_limit_applies_to_all_requests_on_the_connection() {
        let net = MockNet::launch().await;
        let one = NonZeroUsize::new(1).unwrap();

        let limited = indexer(&net).await.with_concurrency_limit(one);
        assert!(two_delayed_requests(limited, &net).await >= Duration::from_millis(600));
        let unlimited = indexer(&net).await;
        assert!(two_delayed_requests(unlimited, &net).await < Duration::from_millis(550));
    }

    #[tokio::test]
    async fn unverified_sources_that_disagree_stop_sync() {
        let mut funded = MockNet::launch().await;
        funded.create_faucet().await;
        let empty = MockNet::launch().await;
        empty.generate_blocks(funded.chain_height().await).await;
        let builder = LightClient::builder(funded.chain_type())
            .source(indexer(&funded).await)
            .source(indexer(&empty).await)
            .router(Quorum::new(2));
        let mut client = create(builder).await;

        let error = client.sync_and_await().await.unwrap_err();
        assert!(error.to_string().contains("sources disagree"), "{error}");
    }
}

mod verification {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use chain_router::{
        AllowedIdentifiers, BoxFuture, ChainSource, Integrity, Request, Response, Route, Router,
        SourceError, SourceId, SourceInfo,
    };
    use lightwallet_grpc_client::GrpcIndexer;
    use pepper_sync::Verified;
    use pepper_sync::error::{ServerError, SyncError};

    use super::*;
    use crate::config::{Birthday, WalletConfig};
    use crate::lightclient::error::LightClientError;
    use crate::lightclient::store::MemoryStore;
    use crate::lightclient::{Encryption, LightClient, LightClientBuilder};
    use crate::testutils::default_test_wallet_settings;

    /// Sends block ranges to the second source and all other requests to the first source.
    #[derive(Default)]
    struct BlocksFromSecond(Mutex<Vec<SourceId>>);

    impl Router for &'static BlocksFromSecond {
        fn select_route(&self, request: &Request, _sources: &[SourceInfo]) -> Route {
            match request {
                Request::Blocks(_) => Route::Failover(vec![SourceId(1)]),
                _ => Route::Failover(vec![SourceId(0)]),
            }
        }

        fn on_failure(&self, source: SourceId, error: &SourceError) {
            if matches!(error, SourceError::FailedVerification) {
                self.0.lock().unwrap().push(source);
            }
        }
    }

    async fn indexer(net: &MockNet) -> GrpcIndexer {
        GrpcIndexer::new(net.indexer_uri())
            .await
            .unwrap()
            .allow_identifiers(AllowedIdentifiers::all())
    }

    struct CountingTreeStates(GrpcIndexer, Arc<AtomicUsize>);

    impl ChainSource for CountingTreeStates {
        fn info(&self) -> &SourceInfo {
            self.0.info()
        }

        fn execute(&self, request: Request) -> BoxFuture<'_, Result<Response, SourceError>> {
            if matches!(request, Request::TreeState(_)) {
                self.1.fetch_add(1, Ordering::Relaxed);
            }
            self.0.execute(request)
        }
    }

    /// The number of tree state requests in one sync from one source with `integrity`.
    async fn tree_state_requests(net: &MockNet, integrity: Integrity) -> usize {
        let count = Arc::new(AtomicUsize::new(0));
        let source =
            CountingTreeStates(indexer(net).await.with_integrity(integrity), count.clone());
        let mut client = create(LightClient::builder(net.chain_type()).source(source)).await;
        client.sync_and_await().await.unwrap();
        count.load(Ordering::Relaxed)
    }

    #[tokio::test]
    async fn auto_strategy_does_not_check_blocks_against_tree_states_from_one_unverified_source() {
        let mut net = MockNet::launch().await;
        net.create_faucet().await;
        net.generate_blocks(5).await;

        let unverified = tree_state_requests(&net, Integrity::Unverified).await;
        assert_eq!(
            unverified,
            tree_state_requests(&net, Integrity::Assumed).await
        );
    }

    #[tokio::test]
    async fn verified_strategy_fails_if_the_tree_states_are_from_one_unverified_source() {
        let mut net = MockNet::launch().await;
        net.create_faucet().await;
        let builder = LightClient::builder(net.chain_type())
            .source(indexer(&net).await)
            .sync(Verified);
        let mut client = create(builder).await;

        let error = client.sync_and_await().await.unwrap_err();
        assert!(matches!(
            error,
            LightClientError::SyncError(SyncError::ServerError(ServerError::UnverifiedTreeState))
        ));
    }

    async fn create(builder: LightClientBuilder) -> LightClient {
        create_from(builder, 1).await
    }

    async fn create_from(builder: LightClientBuilder, birthday: u32) -> LightClient {
        let wallet_config = WalletConfig::NewSeed {
            no_of_accounts: 1.try_into().unwrap(),
            birthday: Birthday::Height(birthday),
            wallet_settings: default_test_wallet_settings(),
        };
        builder
            .create(MemoryStore::default(), wallet_config, Encryption::None)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn verified_strategy_starts_from_the_tree_state_below_the_first_block() {
        let mut net = MockNet::launch().await;
        net.create_faucet().await;
        net.generate_blocks(5).await;
        let builder = LightClient::builder(net.chain_type())
            .source(indexer(&net).await.with_integrity(Integrity::Assumed))
            .sync(Verified);
        let mut client = create_from(builder, net.chain_height().await - 2).await;

        let synced = client.sync_and_await().await.unwrap();
        assert_eq!(u32::from(synced.sync_end_height), net.chain_height().await);
    }

    #[tokio::test]
    async fn verified_strategy_syncs_blocks_that_match_the_tree_state() {
        let mut net = MockNet::launch().await;
        net.create_faucet().await;
        net.generate_blocks(5).await;
        let builder = LightClient::builder(net.chain_type())
            .source(indexer(&net).await.with_integrity(Integrity::Assumed))
            .sync(Verified);
        let mut client = create(builder).await;

        let synced = client.sync_and_await().await.unwrap();
        assert_eq!(u32::from(synced.sync_end_height), net.chain_height().await);
        assert!(synced.orchard_outputs_scanned + synced.ironwood_outputs_scanned > 0);
    }

    #[tokio::test]
    async fn blocks_that_hide_outputs_stop_sync_and_their_source_is_reported() {
        let mut honest = MockNet::launch().await;
        honest.create_faucet().await;
        let liar = MockNet::launch().await;
        liar.generate_blocks(honest.chain_height().await).await;
        let router: &'static BlocksFromSecond = Box::leak(Box::default());
        let assumed = indexer(&honest).await.with_integrity(Integrity::Assumed);
        let builder = LightClient::builder(honest.chain_type())
            .source(assumed)
            .source(indexer(&liar).await)
            .router(router);
        let mut client = create(builder).await;

        let error = client.sync_and_await().await.unwrap_err();
        assert!(
            error.to_string().contains("do not lead to the tree state"),
            "{error}"
        );
        assert_eq!(*router.0.lock().unwrap(), [SourceId(1)]);
    }
}
