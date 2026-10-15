use std::cmp;
use std::collections::{BTreeSet, HashMap};
use std::ops::Range;

use tokio::sync::mpsc;

use zcash_keys::keys::UnifiedFullViewingKey;
use zcash_protocol::consensus::{self, BlockHeight};
use zcash_transparent::keys::NonHardenedChildIndex;
use zip32::AccountId;

use crate::client::{self, FetchRequest};
use crate::config::TransparentAddressDiscovery;
use crate::error::{ServerError, SyncError};
use crate::keys;
use crate::keys::transparent::{TransparentAddressId, TransparentScope};
use crate::wallet::traits::{SyncTransactions, SyncWallet};
use crate::wallet::{KeyIdInterface, ScanTarget};

use super::MAX_REORG_ALLOWANCE;

/// Discovers all addresses in use by the wallet and returns `scan_targets` for any new relevant transactions to scan transparent
/// bundles.
/// `last_known_chain_height` should be the value before updating to latest chain height.
pub(crate) async fn update_addresses_and_scan_targets<W: SyncWallet + SyncTransactions>(
    consensus_parameters: &impl consensus::Parameters,
    wallet: &mut W,
    fetch_request_sender: mpsc::UnboundedSender<FetchRequest>,
    ufvks: &HashMap<AccountId, UnifiedFullViewingKey>,
    last_known_chain_height: BlockHeight,
    chain_height: BlockHeight,
    config: TransparentAddressDiscovery,
) -> Result<(), SyncError<W::Error>> {
    if !config.scopes.external && !config.scopes.internal && !config.scopes.refund {
        return add_pending_spend_scan_targets(wallet, fetch_request_sender, chain_height).await;
    }

    let wallet_addresses = wallet
        .get_transparent_addresses_mut()
        .map_err(SyncError::WalletError)?;
    let mut scan_targets: BTreeSet<ScanTarget> = BTreeSet::new();
    let sapling_activation_height = consensus_parameters
        .activation_height(consensus::NetworkUpgrade::Sapling)
        .expect("sapling activation height should always return Some");
    let block_range_start = last_known_chain_height.saturating_sub(MAX_REORG_ALLOWANCE) + 1;
    let checked_block_range_start = match block_range_start.cmp(&sapling_activation_height) {
        cmp::Ordering::Greater | cmp::Ordering::Equal => block_range_start,
        cmp::Ordering::Less => sapling_activation_height,
    };
    let block_range = Range {
        start: checked_block_range_start,
        end: chain_height + 1,
    };

    // find scan_targets for any new transactions relevant to known addresses
    for address in wallet_addresses.values() {
        let transactions = client::get_transparent_address_transactions(
            fetch_request_sender.clone(),
            consensus_parameters,
            address.clone(),
            block_range.clone(),
        )
        .await?;

        // The transaction is not scanned here, instead the scan target is stored to be later sent to a scan task for these reasons:
        // - We must search for all relevant transactions MAX_REORG_ALLOWANCE blocks below wallet height in case of re-org.
        // These would be scanned again which would be inefficient
        // - In case of re-org, any scanned transactions with heights within the re-org range would be wrongly invalidated
        // - The scan target will cause the surrounding range to be set to high priority which will often also contain shielded notes
        // relevant to the wallet
        // - Scanning a transaction without scanning the surrounding range of compact blocks in the context of a scan task creates
        // complications. Instead of writing all the information into a wallet transaction once, it would result in "incomplete"
        // transactions that only contain transparent outputs and must be updated with shielded notes and other data when scanned.
        // - We would need to add additional processing here to fetch the compact block for transaction metadata such as block time
        // and append this to the wallet.
        // - It allows SyncState to maintain complete knowledge and control of all the tasks that have and will be performed by the
        // sync engine.
        //
        // To summarise, keeping transaction scanning within the scanner is much better co-ordinated and allows us to leverage
        // any new developments to sync state management and scanning. It also separates concerns, with tasks happening in one
        // place and performed once, wherever possible.
        for (height, tx) in &transactions {
            scan_targets.insert(ScanTarget {
                block_height: *height,
                txid: tx.txid(),
                narrow_scan_area: true,
            });
        }
    }

    let mut scopes = Vec::new();
    if config.scopes.external {
        scopes.push(TransparentScope::External);
    }
    if config.scopes.internal {
        scopes.push(TransparentScope::Internal);
    }
    if config.scopes.refund {
        scopes.push(TransparentScope::Refund);
    }

    // discover new addresses and find scan_targets for relevant transactions
    for (account_id, ufvk) in ufvks {
        if let Some(account_pubkey) = ufvk.transparent() {
            for scope in &scopes {
                // start with the first address index previously unused by the wallet
                let mut address_index = if let Some(id) = wallet_addresses
                    .keys()
                    .filter(|id| id.account_id() == *account_id && id.scope() == *scope)
                    .next_back()
                {
                    id.address_index().index() + 1
                } else {
                    0
                };
                let mut unused_address_count: usize = 0;
                let mut addresses: Vec<(TransparentAddressId, String)> = Vec::new();

                while unused_address_count < config.gap_limit as usize {
                    let address_id = TransparentAddressId::new(
                        *account_id,
                        *scope,
                        NonHardenedChildIndex::from_index(address_index)
                            .expect("all non-hardened addresses in use!"),
                    );
                    let address = keys::transparent::derive_address(
                        consensus_parameters,
                        account_pubkey,
                        address_id,
                    )
                    .map_err(SyncError::TransparentAddressDerivationError)?;
                    addresses.push((address_id, address.clone()));

                    let transactions = client::get_transparent_address_transactions(
                        fetch_request_sender.clone(),
                        consensus_parameters,
                        address,
                        block_range.clone(),
                    )
                    .await?;

                    if transactions.is_empty() {
                        unused_address_count += 1;
                    } else {
                        for (height, tx) in &transactions {
                            scan_targets.insert(ScanTarget {
                                block_height: *height,
                                txid: tx.txid(),
                                narrow_scan_area: true,
                            });
                        }
                        unused_address_count = 0;
                    }

                    address_index += 1;
                }

                addresses.truncate(addresses.len() - config.gap_limit as usize);
                for (id, address) in addresses {
                    wallet_addresses.insert(id, address);
                }
            }
        }
    }

    wallet
        .get_sync_state_mut()
        .map_err(SyncError::WalletError)?
        .add_scan_targets(scan_targets);

    Ok(())
}

/// Adds scan targets for the mined transactions that spend the wallet's transparent coins and are still pending in
/// the wallet.
///
/// With transparent address discovery disabled, the server is not asked for the transactions of the wallet's
/// addresses. A transaction that spends a coin and pays nothing to the wallet is then never found by scanning. It
/// would stay pending until its expiry height and be marked failed, which resets the coins it spent to unspent.
async fn add_pending_spend_scan_targets<W: SyncWallet + SyncTransactions>(
    wallet: &mut W,
    fetch_request_sender: mpsc::UnboundedSender<FetchRequest>,
    chain_height: BlockHeight,
) -> Result<(), SyncError<W::Error>> {
    let wallet_transactions = wallet
        .get_wallet_transactions()
        .map_err(SyncError::WalletError)?;
    let pending_spends: BTreeSet<_> = wallet_transactions
        .values()
        .flat_map(|transaction| transaction.transparent_coins())
        .filter_map(|coin| coin.spending_transaction)
        .filter(|txid| {
            wallet_transactions
                .get(txid)
                .is_some_and(|transaction| transaction.status().is_pending())
        })
        .collect();

    let mut scan_targets = BTreeSet::new();
    for txid in pending_spends {
        match client::get_mined_height(fetch_request_sender.clone(), txid).await {
            // a block above `chain_height` is scanned in the next sync session
            Ok(Some(block_height)) if block_height <= chain_height => {
                scan_targets.insert(ScanTarget {
                    block_height,
                    txid,
                    narrow_scan_area: true,
                });
            }
            // the server answers with an error for a transaction it does not know, as when a pending transaction
            // was never mined
            Ok(_) | Err(ServerError::RequestFailed(_)) => {}
            Err(e) => return Err(e.into()),
        }
    }

    wallet
        .get_sync_state_mut()
        .map_err(SyncError::WalletError)?
        .add_scan_targets(scan_targets);

    Ok(())
}

// TODO: process memo encoded address indexes.
// 1. return any memo address ids from scan in ScanResults
// 2. derive the addresses up to that index, add to wallet addresses and send them to GetTaddressTxids
// 3. for each transaction returned:
// a) if the tx is in a range that is not scanned, add scan_targets to sync_state
// b) if the range is scanned and the tx is already in the wallet, rescan the zcash transaction transparent bundles in
// the wallet transaction
// c) if the range is scanned and the tx does not exist in the wallet, fetch the compact block if its not in the wallet
// and scan the transparent bundles

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use tokio::sync::mpsc;
    use zcash_primitives::transaction::{TransactionData, TxId, TxVersion};
    use zcash_protocol::consensus::{BlockHeight, BranchId};
    use zcash_protocol::local_consensus::LocalNetwork;
    use zcash_protocol::value::Zatoshis;
    use zcash_transparent::address::Script;
    use zcash_transparent::keys::NonHardenedChildIndex;
    use zingo_netutils::lightwallet_protocol::RawTransaction;
    use zingolib_common::status::ConfirmationStatus;

    use super::update_addresses_and_scan_targets;
    use crate::client::FetchRequest;
    use crate::config::TransparentAddressDiscovery;
    use crate::error::{ServerError, SyncError};
    use crate::keys::transparent::{TransparentAddressId, TransparentScope};
    use crate::mocks::{MockWallet, MockWalletBuilder};
    use crate::sync::{ScanPriority, ScanRange};
    use crate::wallet::traits::SyncWallet;
    use crate::wallet::{OutputId, ScanTarget, SyncState, TransparentCoin, WalletTransaction};

    const NETWORK: LocalNetwork = LocalNetwork {
        overwinter: Some(BlockHeight::from_u32(1)),
        sapling: Some(BlockHeight::from_u32(1)),
        blossom: Some(BlockHeight::from_u32(1)),
        heartwood: Some(BlockHeight::from_u32(1)),
        canopy: Some(BlockHeight::from_u32(1)),
        nu5: Some(BlockHeight::from_u32(1)),
        nu6: Some(BlockHeight::from_u32(1)),
        nu6_1: Some(BlockHeight::from_u32(1)),
        nu6_2: Some(BlockHeight::from_u32(1)),
        nu6_3: Some(BlockHeight::from_u32(1)),
    };
    const CHAIN_HEIGHT: BlockHeight = BlockHeight::from_u32(100);
    const SPEND_TXID: TxId = TxId::from_bytes([2; 32]);

    fn wallet_transaction(
        txid: TxId,
        status: ConfirmationStatus,
        transparent_coins: Vec<TransparentCoin>,
    ) -> WalletTransaction {
        let transaction = TransactionData::from_parts(
            TxVersion::V5,
            BranchId::Nu5,
            0,
            BlockHeight::from_u32(0),
            None,
            None,
            None,
            None,
        )
        .freeze()
        .unwrap();

        WalletTransaction {
            txid,
            status,
            transaction,
            datetime: 0,
            transparent_coins,
            sapling_notes: Vec::new(),
            orchard_notes: Vec::new(),
            ironwood_notes: Vec::new(),
            outgoing_sapling_notes: Vec::new(),
            outgoing_orchard_notes: Vec::new(),
            outgoing_ironwood_notes: Vec::new(),
        }
    }

    /// A wallet with a confirmed coin spent by `SPEND_TXID`, which has the given status in the wallet.
    fn wallet(spend_status: ConfirmationStatus) -> MockWallet {
        let funding_txid = TxId::from_bytes([1; 32]);
        let coin = TransparentCoin {
            output_id: OutputId::new(funding_txid, 0),
            key_id: TransparentAddressId::new(
                zip32::AccountId::ZERO,
                TransparentScope::External,
                NonHardenedChildIndex::ZERO,
            ),
            address: String::new(),
            script: Script::default(),
            value: Zatoshis::const_from_u64(100_000),
            spending_transaction: Some(SPEND_TXID),
        };
        // pending, but it spends none of the wallet's coins
        let unrelated_txid = TxId::from_bytes([3; 32]);

        MockWalletBuilder::new()
            .sync_state(SyncState::new_for_test(vec![ScanRange::from_parts(
                BlockHeight::from_u32(1)..BlockHeight::from_u32(91),
                ScanPriority::Scanned,
            )]))
            .wallet_transactions(HashMap::from([
                (
                    funding_txid,
                    wallet_transaction(
                        funding_txid,
                        ConfirmationStatus::Confirmed(BlockHeight::from_u32(10)),
                        vec![coin],
                    ),
                ),
                (
                    SPEND_TXID,
                    wallet_transaction(SPEND_TXID, spend_status, Vec::new()),
                ),
                (
                    unrelated_txid,
                    wallet_transaction(
                        unrelated_txid,
                        ConfirmationStatus::Mempool(BlockHeight::from_u32(95)),
                        Vec::new(),
                    ),
                ),
            ]))
            .create_mock_wallet()
    }

    /// Runs the pre-scan transparent step with address discovery disabled against a server that answers every
    /// transaction request with `reply`.
    ///
    /// Returns the wallet's scan targets and the txids requested from the server.
    async fn scan_targets_with_discovery_disabled(
        spend_status: ConfirmationStatus,
        reply: Result<u64, tonic::Code>,
    ) -> (Vec<ScanTarget>, Vec<TxId>) {
        let mut wallet = wallet(spend_status);
        let (fetch_request_sender, mut fetch_request_receiver) = mpsc::unbounded_channel();
        let fetcher = tokio::spawn(async move {
            let mut requested = Vec::new();
            while let Some(request) = fetch_request_receiver.recv().await {
                match request {
                    FetchRequest::Transaction(reply_sender, txid) => {
                        requested.push(txid);
                        let reply = reply
                            .map(|height| RawTransaction {
                                data: Vec::new(),
                                height,
                            })
                            .map_err(|code| tonic::Status::new(code, "no such transaction"));
                        reply_sender.send(reply).unwrap();
                    }
                    other => panic!("unexpected fetch request: {other:?}"),
                }
            }
            requested
        });

        update_addresses_and_scan_targets(
            &NETWORK,
            &mut wallet,
            fetch_request_sender,
            &HashMap::new(),
            BlockHeight::from_u32(90),
            CHAIN_HEIGHT,
            TransparentAddressDiscovery::disabled(),
        )
        .await
        .unwrap();

        let scan_targets = wallet
            .get_sync_state()
            .unwrap()
            .scan_targets()
            .iter()
            .copied()
            .collect();
        (scan_targets, fetcher.await.unwrap())
    }

    #[tokio::test]
    async fn mined_pending_spend_is_targeted_when_discovery_is_disabled() {
        for spend_status in [
            ConfirmationStatus::Calculated(BlockHeight::from_u32(95)),
            ConfirmationStatus::Transmitted(BlockHeight::from_u32(95)),
            ConfirmationStatus::Mempool(BlockHeight::from_u32(95)),
        ] {
            for mined_height in [95, 100] {
                let (scan_targets, requested) =
                    scan_targets_with_discovery_disabled(spend_status, Ok(mined_height)).await;

                assert_eq!(requested, vec![SPEND_TXID], "{spend_status:?}");
                assert_eq!(
                    scan_targets,
                    vec![ScanTarget {
                        block_height: BlockHeight::from_u32(mined_height as u32),
                        txid: SPEND_TXID,
                        narrow_scan_area: true,
                    }],
                    "{spend_status:?} mined at {mined_height}"
                );
            }
        }
    }

    /// The server reports height zero for a mempool transaction and `u64::MAX` for one in a re-orged block, and
    /// answers with an error for a transaction it does not know. A block above the chain height of this session is
    /// not in its scan ranges.
    #[tokio::test]
    async fn pending_spend_not_mined_in_session_range_is_not_targeted() {
        for reply in [
            Ok(0),
            Ok(u64::MAX),
            Ok(101),
            Err(tonic::Code::NotFound),
            Err(tonic::Code::Unknown),
        ] {
            let (scan_targets, requested) = scan_targets_with_discovery_disabled(
                ConfirmationStatus::Mempool(BlockHeight::from_u32(95)),
                reply,
            )
            .await;

            assert_eq!(requested, vec![SPEND_TXID], "{reply:?}");
            assert!(scan_targets.is_empty(), "{reply:?}");
        }
    }

    #[tokio::test]
    async fn settled_spend_is_not_requested() {
        for spend_status in [
            ConfirmationStatus::Confirmed(BlockHeight::from_u32(95)),
            ConfirmationStatus::Failed(BlockHeight::from_u32(95)),
        ] {
            let (scan_targets, requested) =
                scan_targets_with_discovery_disabled(spend_status, Ok(95)).await;

            assert!(requested.is_empty(), "{spend_status:?}");
            assert!(scan_targets.is_empty(), "{spend_status:?}");
        }
    }

    /// Only the server's answer for an unknown transaction is tolerated. Losing the fetcher fails the sync.
    #[tokio::test]
    async fn dropped_fetcher_is_an_error() {
        let mut wallet = wallet(ConfirmationStatus::Mempool(BlockHeight::from_u32(95)));
        let (fetch_request_sender, _) = mpsc::unbounded_channel();

        let result = update_addresses_and_scan_targets(
            &NETWORK,
            &mut wallet,
            fetch_request_sender,
            &HashMap::new(),
            BlockHeight::from_u32(90),
            CHAIN_HEIGHT,
            TransparentAddressDiscovery::disabled(),
        )
        .await;

        assert!(matches!(
            result,
            Err(SyncError::ServerError(ServerError::FetcherDropped))
        ));
    }
}
