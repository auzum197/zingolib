//! Sync state updates that need the wallet or the server. Each reads or fetches what it needs and hands the values to
//! the scan scheduler in [`super::scheduler`].

use std::{
    cmp,
    collections::{BTreeSet, HashMap},
    ops::Range,
};

use tokio::sync::mpsc;

use zcash_protocol::consensus::{self, BlockHeight, NetworkUpgrade};

use crate::{
    client::{self, FetchRequest},
    error::{ServerError, SyncError},
    keys::transparent::TransparentAddressId,
    scan::task::ScanTask,
    wallet::{
        TreeBounds,
        traits::{SyncBlocks, SyncNullifiers, SyncWallet},
    },
};

use super::{ScanPriority, scheduler::VerifyEnd};

/// Update scan ranges for scanning.
/// Returns the block height that reorg detection will start from.
pub(super) async fn update_scan_ranges<W>(
    consensus_parameters: &impl consensus::Parameters,
    fetch_request_sender: mpsc::UnboundedSender<FetchRequest>,
    last_known_chain_height: BlockHeight,
    chain_height: BlockHeight,
    wallet: &mut W,
) -> Result<BlockHeight, SyncError<W::Error>>
where
    W: SyncWallet + SyncBlocks,
{
    let reorg_detection_start_height = wallet
        .get_sync_state_mut()
        .map_err(SyncError::WalletError)?
        .update_scan_ranges(consensus_parameters, last_known_chain_height, chain_height);

    if reorg_detection_start_height > chain_height {
        let chain_height_server_block =
            client::get_compact_block(fetch_request_sender, chain_height).await?;
        let chain_height_wallet_block = wallet
            .get_wallet_block(chain_height)
            .map_err(SyncError::WalletError)?;
        if chain_height_wallet_block.block_hash().0.to_vec() != chain_height_server_block.hash {
            wallet
                .get_sync_state_mut()
                .map_err(SyncError::WalletError)?
                .set_verify_scan_range(chain_height, VerifyEnd::VerifyHighest);
        }
    }

    Ok(reorg_detection_start_height)
}

/// Creates a scan task to be sent to a [`crate::scan::task::ScanWorker`] for scanning.
pub(crate) fn create_scan_task<W>(
    consensus_parameters: &impl consensus::Parameters,
    wallet: &mut W,
    nullifier_map_limit_exceeded: bool,
) -> Result<Option<ScanTask>, W::Error>
where
    W: SyncWallet + SyncBlocks + SyncNullifiers,
{
    let Some(selected_range) = wallet
        .get_sync_state_mut()?
        .select_scan_range(consensus_parameters, nullifier_map_limit_exceeded)
    else {
        return Ok(None);
    };

    if selected_range.priority() == ScanPriority::ScannedWithoutMapping {
        // all continuity checks and scanning is already complete, the scan worker will only re-fetch the nullifiers
        // for final spend detection.
        return Ok(Some(ScanTask::from_parts(
            selected_range,
            None,
            None,
            BTreeSet::new(),
            HashMap::new(),
        )));
    }

    let start_seam_block = wallet
        .get_wallet_block(selected_range.block_range().start - 1)
        .ok();
    let end_seam_block = wallet
        .get_wallet_block(selected_range.block_range().end)
        .ok();

    let scan_targets = wallet
        .get_sync_state()?
        .find_scan_targets(selected_range.block_range());
    let transparent_addresses: HashMap<String, TransparentAddressId> = wallet
        .get_transparent_addresses()?
        .iter()
        .map(|(id, address)| (address.clone(), *id))
        .collect();

    Ok(Some(ScanTask::from_parts(
        selected_range,
        start_seam_block,
        end_seam_block,
        scan_targets,
        transparent_addresses,
    )))
}

/// Sets the `initial_sync_state` field at the start of the sync session
pub(super) async fn set_initial_state<W>(
    consensus_parameters: &impl consensus::Parameters,
    fetch_request_sender: mpsc::UnboundedSender<FetchRequest>,
    wallet: &mut W,
    chain_height: BlockHeight,
) -> Result<(), SyncError<W::Error>>
where
    W: SyncWallet + SyncBlocks,
{
    let birthday = wallet
        .get_sync_state()
        .map_err(SyncError::WalletError)?
        .wallet_birthday()
        .expect("scan ranges must be non-empty");
    let previously_scanned_outputs =
        calculate_scanned_outputs(wallet).map_err(SyncError::WalletError)?;
    let (
        birthday_sapling_initial_tree_size,
        birthday_orchard_initial_tree_size,
        birthday_ironwood_initial_tree_size,
    ) = if let Ok(block) = wallet.get_wallet_block(birthday) {
        (
            block.tree_bounds.sapling_initial_tree_size,
            block.tree_bounds.orchard_initial_tree_size,
            block.tree_bounds.ironwood_initial_tree_size,
        )
    } else {
        final_tree_sizes(
            consensus_parameters,
            fetch_request_sender.clone(),
            wallet,
            birthday - 1,
        )
        .await?
    };
    let (
        chain_tip_sapling_final_tree_size,
        chain_tip_orchard_final_tree_size,
        chain_tip_ironwood_final_tree_size,
    ) = final_tree_sizes(
        consensus_parameters,
        fetch_request_sender.clone(),
        wallet,
        chain_height,
    )
    .await?;

    wallet
        .get_sync_state_mut()
        .map_err(SyncError::WalletError)?
        .set_initial_state(
            chain_height,
            TreeBounds {
                sapling_initial_tree_size: birthday_sapling_initial_tree_size,
                sapling_final_tree_size: chain_tip_sapling_final_tree_size,
                orchard_initial_tree_size: birthday_orchard_initial_tree_size,
                orchard_final_tree_size: chain_tip_orchard_final_tree_size,
                ironwood_initial_tree_size: birthday_ironwood_initial_tree_size,
                ironwood_final_tree_size: chain_tip_ironwood_final_tree_size,
            },
            previously_scanned_outputs,
        );

    Ok(())
}

pub(super) fn calculate_scanned_outputs<W>(wallet: &W) -> Result<(u32, u32, u32), W::Error>
where
    W: SyncWallet + SyncBlocks,
{
    Ok(wallet
        .get_sync_state()?
        .scanned_ranges()
        .map(|scanned_range| scanned_range_tree_bounds(wallet, scanned_range.block_range().clone()))
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .fold((0, 0, 0), |acc, tree_bounds| {
            (
                acc.0
                    + (tree_bounds.sapling_final_tree_size - tree_bounds.sapling_initial_tree_size),
                acc.1
                    + (tree_bounds.orchard_final_tree_size - tree_bounds.orchard_initial_tree_size),
                acc.2
                    + (tree_bounds.ironwood_final_tree_size
                        - tree_bounds.ironwood_initial_tree_size),
            )
        }))
}

/// Gets `block_height` final tree sizes from wallet block if it exists, otherwise from frontiers fetched from server.
async fn final_tree_sizes<W>(
    consensus_parameters: &impl consensus::Parameters,
    fetch_request_sender: mpsc::UnboundedSender<FetchRequest>,
    wallet: &mut W,
    block_height: BlockHeight,
) -> Result<(u32, u32, u32), ServerError>
where
    W: SyncBlocks,
{
    if let Ok(block) = wallet.get_wallet_block(block_height) {
        Ok((
            block.tree_bounds().sapling_final_tree_size,
            block.tree_bounds().orchard_final_tree_size,
            block.tree_bounds().ironwood_final_tree_size,
        ))
    } else {
        // TODO: move this whole block into `client::get_frontiers`
        let sapling_activation_height = consensus_parameters
            .activation_height(NetworkUpgrade::Sapling)
            .expect("should have some sapling activation height");

        match block_height.cmp(&(sapling_activation_height - 1)) {
            cmp::Ordering::Greater => {
                let frontiers =
                    client::get_frontiers(fetch_request_sender.clone(), block_height).await?;
                Ok((
                    frontiers
                        .final_sapling_tree()
                        .tree_size()
                        .try_into()
                        .expect("should not be more than 2^32 note commitments in the tree!"),
                    frontiers
                        .final_orchard_tree()
                        .tree_size()
                        .try_into()
                        .expect("should not be more than 2^32 note commitments in the tree!"),
                    frontiers
                        .final_ironwood_tree()
                        .tree_size()
                        .try_into()
                        .expect("should not be more than 2^32 note commitments in the tree!"),
                ))
            }
            cmp::Ordering::Equal => Ok((0, 0, 0)),
            cmp::Ordering::Less => panic!("pre-sapling not supported!"),
        }
    }
}

/// Gets the initial and final tree sizes of a `scanned_range`.
///
/// Panics if `scanned_range` wallet block bounds are not found in the wallet.
fn scanned_range_tree_bounds<W>(
    wallet: &W,
    scanned_range: Range<BlockHeight>,
) -> Result<TreeBounds, W::Error>
where
    W: SyncBlocks,
{
    let start_block = wallet.get_wallet_block(scanned_range.start)?;
    let end_block = wallet.get_wallet_block(scanned_range.end - 1)?;

    Ok(TreeBounds {
        sapling_initial_tree_size: start_block.tree_bounds().sapling_initial_tree_size,
        sapling_final_tree_size: end_block.tree_bounds().sapling_final_tree_size,
        orchard_initial_tree_size: start_block.tree_bounds().orchard_initial_tree_size,
        orchard_final_tree_size: end_block.tree_bounds().orchard_final_tree_size,
        ironwood_initial_tree_size: start_block.tree_bounds().ironwood_initial_tree_size,
        ironwood_final_tree_size: end_block.tree_bounds().ironwood_final_tree_size,
    })
}
