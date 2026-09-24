//! The scan scheduler: [`SyncState`] and every operation on its scan ranges.
//!
//! Nothing here touches the wallet or the network. Callers in [`super::state`] and [`super`] fetch what the
//! scheduler needs and pass it in as values.

use std::{collections::BTreeSet, ops::Range};

use zcash_primitives::transaction::TxId;
use zcash_protocol::{
    ShieldedPool,
    consensus::{self, BlockHeight},
};

use crate::wallet::{InitialSyncState, ScanTarget, TreeBounds, WalletTransaction};

use super::{ScanPriority, ScanRange, VERIFY_BLOCK_RANGE_SIZE};

const NARROW_SCAN_AREA: u32 = 10_000;

/// Used to determine which end of the scan range is verified.
pub(crate) enum VerifyEnd {
    VerifyHighest,
    VerifyLowest,
}

/// Encapsulates the current state of sync
///
/// The scan ranges cover every block from the wallet birthday to the last known chain height, in block height order
/// with no overlaps, gaps or empty ranges. The birthday and chain height are defined by the first and last range.
/// Every method that changes the scan ranges checks this before returning.
#[derive(Debug, Clone)]
pub struct SyncState {
    scan_ranges: Vec<ScanRange>,
    /// The block ranges that contain all sapling outputs of complete sapling shards.
    ///
    /// There is an edge case where a range may include two (or more) shards. However, this only occurs when the lower
    /// shards are already scanned so will cause no issues when punching in the higher scan priorites.
    sapling_shard_ranges: Vec<Range<BlockHeight>>,
    /// The block ranges that contain all orchard outputs of complete orchard shards.
    ///
    /// There is an edge case where a range may include two (or more) shards. However, this only occurs when the lower
    /// shards are already scanned so will cause no issues when punching in the higher scan priorites.
    orchard_shard_ranges: Vec<Range<BlockHeight>>,
    /// The block ranges that contain all ironwood outputs of complete ironwood shards.
    ///
    /// There is an edge case where a range may include two (or more) shards. However, this only occurs when the lower
    /// shards are already scanned so will cause no issues when punching in the higher scan priorites.
    ironwood_shard_ranges: Vec<Range<BlockHeight>>,
    /// Scan targets for relevant transactions to the wallet.
    scan_targets: BTreeSet<ScanTarget>,
    initial_sync_state: InitialSyncState,
}

impl SyncState {
    /// Create new `SyncState`
    #[must_use]
    pub fn new() -> Self {
        SyncState {
            scan_ranges: Vec::new(),
            sapling_shard_ranges: Vec::new(),
            orchard_shard_ranges: Vec::new(),
            ironwood_shard_ranges: Vec::new(),
            scan_targets: BTreeSet::new(),
            initial_sync_state: InitialSyncState::new(),
        }
    }

    /// Rebuilds a sync state read from a wallet file. The initial sync state is not persisted.
    #[cfg(feature = "wallet_essentials")]
    pub(crate) fn from_parts(
        scan_ranges: Vec<ScanRange>,
        sapling_shard_ranges: Vec<Range<BlockHeight>>,
        orchard_shard_ranges: Vec<Range<BlockHeight>>,
        ironwood_shard_ranges: Vec<Range<BlockHeight>>,
        scan_targets: BTreeSet<ScanTarget>,
    ) -> Self {
        SyncState {
            scan_ranges,
            sapling_shard_ranges,
            orchard_shard_ranges,
            ironwood_shard_ranges,
            scan_targets,
            initial_sync_state: InitialSyncState::new(),
        }
    }

    /// Scan ranges
    #[must_use]
    pub fn scan_ranges(&self) -> &[ScanRange] {
        &self.scan_ranges
    }

    /// Sapling shard ranges
    #[must_use]
    pub fn sapling_shard_ranges(&self) -> &[Range<BlockHeight>] {
        &self.sapling_shard_ranges
    }

    /// Orchard shard ranges
    #[must_use]
    pub fn orchard_shard_ranges(&self) -> &[Range<BlockHeight>] {
        &self.orchard_shard_ranges
    }

    /// Ironwood shard ranges
    #[must_use]
    pub fn ironwood_shard_ranges(&self) -> &[Range<BlockHeight>] {
        &self.ironwood_shard_ranges
    }

    /// Scan targets that have yet to be scanned.
    #[must_use]
    pub fn scan_targets(&self) -> &BTreeSet<ScanTarget> {
        &self.scan_targets
    }

    pub(crate) fn initial_sync_state(&self) -> &InitialSyncState {
        &self.initial_sync_state
    }

    /// Returns true if all scan ranges are scanned.
    pub(crate) fn scan_complete(&self) -> bool {
        self.scan_ranges
            .iter()
            .all(|scan_range| scan_range.priority() == ScanPriority::Scanned)
    }

    /// Returns the block height at which all blocks equal to and below this height are scanned.
    /// Returns `None` if `self.scan_ranges` is empty.
    #[must_use]
    pub fn fully_scanned_height(&self) -> Option<BlockHeight> {
        if let Some(scan_range) = self
            .scan_ranges
            .iter()
            .find(|scan_range| scan_range.priority() != ScanPriority::Scanned)
        {
            Some(scan_range.block_range().start - 1)
        } else {
            self.scan_ranges
                .last()
                .map(|range| range.block_range().end - 1)
        }
    }

    /// Returns the highest block height that has been scanned.
    /// If no scan ranges have been scanned, returns the block below the wallet birthday.
    /// Returns `None` if `self.scan_ranges` is empty.
    #[must_use]
    pub fn highest_scanned_height(&self) -> Option<BlockHeight> {
        if let Some(last_scanned_range) = self.scanned_ranges().next_back() {
            Some(last_scanned_range.block_range().end - 1)
        } else {
            self.wallet_birthday().map(|start| start - 1)
        }
    }

    /// Returns the wallet birthday or `None` if `self.scan_ranges` is empty.
    ///
    #[must_use]
    pub fn wallet_birthday(&self) -> Option<BlockHeight> {
        self.scan_ranges
            .first()
            .map(|range| range.block_range().start)
    }

    /// Returns the last known chain height to the wallet or `None` if `self.scan_ranges` is empty.
    #[must_use]
    pub fn last_known_chain_height(&self) -> Option<BlockHeight> {
        self.scan_ranges
            .last()
            .map(|range| range.block_range().end - 1)
    }

    /// Scan ranges whose blocks have been scanned, including those still waiting on their nullifiers.
    pub(crate) fn scanned_ranges(&self) -> impl DoubleEndedIterator<Item = &ScanRange> {
        self.scan_ranges.iter().filter(|scan_range| {
            scan_range.priority() == ScanPriority::Scanned
                || scan_range.priority() == ScanPriority::ScannedWithoutMapping
                || scan_range.priority() == ScanPriority::RefetchingNullifiers
        })
    }

    pub(crate) fn calculate_scanned_blocks(&self) -> u32 {
        self.scanned_ranges()
            .map(ScanRange::block_range)
            .fold(0, |acc, block_range| {
                acc + (block_range.end - block_range.start)
            })
    }

    /// Returns the scan targets within `block_range`.
    pub(crate) fn find_scan_targets(
        &self,
        block_range: &Range<BlockHeight>,
    ) -> BTreeSet<ScanTarget> {
        self.scan_targets
            .range(
                ScanTarget {
                    block_height: block_range.start,
                    txid: TxId::from_bytes([0; 32]),
                    narrow_scan_area: false,
                }..ScanTarget {
                    block_height: block_range.end,
                    txid: TxId::from_bytes([0; 32]),
                    narrow_scan_area: false,
                },
            )
            .copied()
            .collect()
    }

    pub(crate) fn add_scan_targets(&mut self, scan_targets: impl IntoIterator<Item = ScanTarget>) {
        self.scan_targets.extend(scan_targets);
    }

    /// Drops scan targets at or below `fully_scanned_height`, which have all been scanned.
    pub(crate) fn remove_scanned_scan_targets(&mut self, fully_scanned_height: BlockHeight) {
        self.scan_targets
            .retain(|scan_target| scan_target.block_height > fully_scanned_height);
    }

    /// Plans the scan ranges at the start of a sync session.
    ///
    /// Recovers from an interrupted session, extends the ranges to `chain_height`, prioritises scan targets and the
    /// chain tip, and sets up verification of the lowest unscanned blocks. Returns the block height that reorg
    /// detection starts from.
    ///
    /// If the returned height is above `chain_height` there is nothing left to verify above the scanned blocks. The
    /// caller should then compare the chain tip block hash with the wallet's and call
    /// [`Self::set_verify_scan_range`] with [`VerifyEnd::VerifyHighest`] if they differ.
    pub(crate) fn update_scan_ranges(
        &mut self,
        consensus_parameters: &impl consensus::Parameters,
        last_known_chain_height: BlockHeight,
        chain_height: BlockHeight,
    ) -> BlockHeight {
        self.reset_scan_ranges();
        self.create_scan_range(last_known_chain_height, chain_height);
        let scan_targets = self.scan_targets.clone();
        self.set_found_note_scan_ranges(
            consensus_parameters,
            ShieldedPool::Ironwood,
            scan_targets.into_iter(),
        );
        self.set_chain_tip_scan_range(consensus_parameters, chain_height);
        self.merge_scan_ranges(ScanPriority::ChainTip);

        let reorg_detection_start_height = self
            .highest_scanned_height()
            .expect("scan ranges must be non-empty")
            + 1;
        if reorg_detection_start_height <= chain_height {
            self.set_verify_scan_range(reorg_detection_start_height, VerifyEnd::VerifyLowest);
        }

        reorg_detection_start_height
    }

    /// Sets the initial sync state at the start of the sync session.
    ///
    /// `wallet_tree_bounds` are the tree sizes below the wallet birthday and at `chain_height`.
    /// `previously_scanned_outputs` are the sapling, orchard and ironwood outputs in the already scanned ranges.
    pub(crate) fn set_initial_state(
        &mut self,
        chain_height: BlockHeight,
        wallet_tree_bounds: TreeBounds,
        previously_scanned_outputs: (u32, u32, u32),
    ) {
        let fully_scanned_height = self
            .fully_scanned_height()
            .expect("scan ranges must be non-empty");
        let (
            previously_scanned_sapling_outputs,
            previously_scanned_orchard_outputs,
            previously_scanned_ironwood_outputs,
        ) = previously_scanned_outputs;

        self.initial_sync_state = InitialSyncState {
            sync_start_height: if chain_height > fully_scanned_height {
                fully_scanned_height + 1
            } else {
                chain_height
            },
            wallet_tree_bounds,
            previously_scanned_blocks: self.calculate_scanned_blocks(),
            previously_scanned_sapling_outputs,
            previously_scanned_orchard_outputs,
            previously_scanned_ironwood_outputs,
        };
    }

    /// Merges all adjacent ranges of a given `scan_priority`.
    pub(crate) fn merge_scan_ranges(&mut self, scan_priority: ScanPriority) {
        'main: loop {
            let filtered_ranges = self
                .scan_ranges
                .iter()
                .cloned()
                .enumerate()
                .filter(|(_, scan_range)| scan_range.priority() == scan_priority)
                .collect::<Vec<_>>();
            if filtered_ranges.is_empty() {
                break;
            }
            let mut peekable_ranges = filtered_ranges.iter().peekable();
            while let Some((index, range)) = peekable_ranges.next() {
                if let Some((next_index, next_range)) = peekable_ranges.peek() {
                    if range.block_range().end == next_range.block_range().start {
                        assert!(*next_index == *index + 1);
                        self.scan_ranges.splice(
                            *index..=*next_index,
                            vec![ScanRange::from_parts(
                                Range {
                                    start: range.block_range().start,
                                    end: next_range.block_range().end,
                                },
                                scan_priority,
                            )],
                        );
                        continue 'main;
                    }
                } else {
                    break 'main;
                }
            }
        }
        debug_assert_eq!(self.check_invariants(), Ok(()));
    }

    /// Create scan range between the wallet height and the chain height from the server.
    fn create_scan_range(
        &mut self,
        last_known_chain_height: BlockHeight,
        chain_height: BlockHeight,
    ) {
        if last_known_chain_height == chain_height {
            return;
        }

        let new_scan_range = ScanRange::from_parts(
            Range {
                start: last_known_chain_height + 1,
                end: chain_height + 1,
            },
            ScanPriority::Historic,
        );
        self.scan_ranges.push(new_scan_range);
        debug_assert_eq!(self.check_invariants(), Ok(()));
    }

    /// Splits the range containing [`truncate_height` + 1] and removes all ranges containing block heights above
    /// `truncate_height`.
    /// If `truncate_height` is zero, the sync state will be cleared completely.
    pub(crate) fn truncate_scan_ranges(&mut self, truncate_height: BlockHeight) {
        if truncate_height == consensus::H0 {
            *self = SyncState::new();
        }
        let Some((index, range_to_split)) = self
            .scan_ranges
            .iter()
            .enumerate()
            .find(|(_index, range)| range.block_range().contains(&(truncate_height + 1)))
        else {
            return;
        };

        if let Some((first_segment, second_segment)) = range_to_split.split_at(truncate_height + 1)
        {
            self.replace_scan_range(index, vec![first_segment, second_segment]);
        }

        let truncated_len = self
            .scan_ranges
            .partition_point(|range| range.block_range().start <= truncate_height);
        self.scan_ranges.truncate(truncated_len);
        assert_eq!(
            self.check_invariants(),
            Ok(()),
            "scan ranges are invalid after truncating to {truncate_height}"
        );
    }

    /// Resets scan ranges to recover from previous sync interruptions.
    ///
    /// A range that was previously scanning when sync was last interrupted is set to `FoundNote` to be prioritised for
    /// scanning.
    /// A range that was previously refetching nullifiers when sync was last interrupted is set to `ScannedWithoutMapping`
    /// so the nullifiers can be fetched again.
    fn reset_scan_ranges(&mut self) {
        for scan_range in &mut self.scan_ranges {
            match scan_range.priority() {
                ScanPriority::Scanning => {
                    *scan_range = ScanRange::from_parts(
                        scan_range.block_range().clone(),
                        ScanPriority::FoundNote,
                    );
                }
                ScanPriority::RefetchingNullifiers => {
                    *scan_range = ScanRange::from_parts(
                        scan_range.block_range().clone(),
                        ScanPriority::ScannedWithoutMapping,
                    );
                }
                _ => {}
            }
        }
        debug_assert_eq!(self.check_invariants(), Ok(()));
    }

    /// Splits out the highest or lowest `VERIFY_BLOCK_RANGE_SIZE` blocks from the scan range containing the given `block height`
    /// and sets it's priority to `Verify`.
    /// Returns a clone of the scan range to be verified.
    pub(crate) fn set_verify_scan_range(
        &mut self,
        block_height: BlockHeight,
        verify_end: VerifyEnd,
    ) -> ScanRange {
        let (index, scan_range) = self
            .scan_ranges
            .iter()
            .enumerate()
            .find(|(_, range)| range.block_range().contains(&block_height))
            .expect("scan range containing given block height should always exist!");

        let block_range_to_verify = match verify_end {
            VerifyEnd::VerifyHighest => Range {
                start: scan_range.block_range().end - VERIFY_BLOCK_RANGE_SIZE,
                end: scan_range.block_range().end,
            },
            VerifyEnd::VerifyLowest => Range {
                start: scan_range.block_range().start,
                end: scan_range.block_range().start + VERIFY_BLOCK_RANGE_SIZE,
            },
        };

        let split_ranges =
            split_out_scan_range(scan_range, block_range_to_verify, ScanPriority::Verify);

        let scan_range_to_verify = match verify_end {
            VerifyEnd::VerifyHighest => split_ranges
                .last()
                .expect("vec should always be non-empty")
                .clone(),
            VerifyEnd::VerifyLowest => split_ranges
                .first()
                .expect("vec should always be non-empty")
                .clone(),
        };

        self.replace_scan_range(index, split_ranges);
        debug_assert_eq!(self.check_invariants(), Ok(()));

        scan_range_to_verify
    }

    /// Punches in the chain tip block range with `ScanPriority::ChainTip`.
    ///
    /// Determines the chain tip block range by finding the lowest start height of the latest incomplete shard for each
    /// shielded protocol.
    fn set_chain_tip_scan_range(
        &mut self,
        consensus_parameters: &impl consensus::Parameters,
        chain_height: BlockHeight,
    ) {
        let chain_tip = [
            ShieldedPool::Sapling,
            ShieldedPool::Orchard,
            ShieldedPool::Ironwood,
        ]
        .into_iter()
        .map(|pool| self.determine_block_range(consensus_parameters, chain_height, Some(pool)))
        .min_by_key(|r| r.start)
        .expect("non-empty");

        self.punch_scan_priority(chain_tip, ScanPriority::ChainTip);
    }

    /// Punches in the `shielded_protocol` shard block ranges surrounding each scan target with `ScanPriority::FoundNote`.
    ///
    /// If all `scan_targets` have the `narrow_scan_area` set to `true`, `shielded_protocol` is irrelevant.
    pub(crate) fn set_found_note_scan_ranges<T: Iterator<Item = ScanTarget>>(
        &mut self,
        consensus_parameters: &impl consensus::Parameters,
        shielded_protocol: ShieldedPool,
        scan_targets: T,
    ) {
        for scan_target in scan_targets {
            self.set_found_note_scan_range(
                consensus_parameters,
                if scan_target.narrow_scan_area {
                    None
                } else {
                    Some(shielded_protocol)
                },
                scan_target.block_height,
            );
        }
    }

    /// Punches in the `shielded_protocol` shard block range surrounding the `block_height` with `ScanPriority::FoundNote`.
    ///
    /// If `shielded_protocol` is `None`, punch in the surrounding [`self::NARROW_SCAN_AREA`] blocks starting from the
    /// closest lower multiple of [`self::NARROW_SCAN_AREA`] to `block_height`.
    pub(crate) fn set_found_note_scan_range(
        &mut self,
        consensus_parameters: &impl consensus::Parameters,
        shielded_protocol: Option<ShieldedPool>,
        block_height: BlockHeight,
    ) {
        let block_range =
            self.determine_block_range(consensus_parameters, block_height, shielded_protocol);
        self.punch_scan_priority(block_range, ScanPriority::FoundNote);
    }

    /// Updates the `shielded_protocol` shard range to `FoundNote` scan priority if the `wallet_transaction` contains
    /// a note from the corresponding `shielded_protocol`.
    pub(crate) fn update_found_note_shard_priority(
        &mut self,
        consensus_parameters: &impl consensus::Parameters,
        shielded_protocol: ShieldedPool,
        wallet_transaction: &WalletTransaction,
    ) {
        let found_note = match shielded_protocol {
            ShieldedPool::Sapling => !wallet_transaction.sapling_notes().is_empty(),
            ShieldedPool::Orchard => !wallet_transaction.orchard_notes().is_empty(),
            ShieldedPool::Ironwood => !wallet_transaction.ironwood_notes().is_empty(),
        };
        if found_note {
            self.set_found_note_scan_range(
                consensus_parameters,
                Some(shielded_protocol),
                wallet_transaction.status().get_height(),
            );
        }
    }

    /// Sets `scanned_range` to `Scanned`, or `ScannedWithoutMapping` if its nullifiers were not mapped.
    ///
    /// Panics if no single scan range contains `scanned_range`.
    pub(crate) fn set_scanned_scan_range(
        &mut self,
        scanned_range: Range<BlockHeight>,
        nullifiers_mapped: bool,
    ) {
        let index = self.index_containing(&scanned_range).unwrap_or_else(|| {
            panic!("scan range containing scanned range {scanned_range:?} should exist!")
        });

        let split_ranges = split_out_scan_range(
            &self.scan_ranges[index],
            scanned_range,
            if nullifiers_mapped {
                ScanPriority::Scanned
            } else {
                ScanPriority::ScannedWithoutMapping
            },
        );
        self.replace_scan_range(index, split_ranges);
        debug_assert_eq!(self.check_invariants(), Ok(()));
    }

    /// Sets `invalid_refetch_range` back to `ScannedWithoutMapping` so its nullifiers are fetched again.
    ///
    /// Panics if no single scan range contains `invalid_refetch_range`.
    pub(crate) fn reset_refetching_nullifiers_scan_range(
        &mut self,
        invalid_refetch_range: Range<BlockHeight>,
    ) {
        let index = self
            .index_containing(&invalid_refetch_range)
            .unwrap_or_else(|| {
                panic!(
                    "scan range containing invalid refetch range {invalid_refetch_range:?} should exist!"
                )
            });

        let split_ranges = split_out_scan_range(
            &self.scan_ranges[index],
            invalid_refetch_range,
            ScanPriority::ScannedWithoutMapping,
        );
        self.replace_scan_range(index, split_ranges);
        debug_assert_eq!(self.check_invariants(), Ok(()));
    }

    /// Sets the scan range with `block_range` to the given `scan_priority`.
    ///
    /// Panics if no scan range is found with a block range of exactly `block_range`.
    pub(crate) fn set_scan_priority(
        &mut self,
        block_range: &Range<BlockHeight>,
        scan_priority: ScanPriority,
    ) {
        let Some(scan_range) = self
            .scan_ranges
            .iter_mut()
            .find(|range| range.block_range() == block_range)
        else {
            panic!("scan range with block range {block_range:?} not found!")
        };
        *scan_range = ScanRange::from_parts(block_range.clone(), scan_priority);
        debug_assert_eq!(self.check_invariants(), Ok(()));
    }

    /// Punches in a `scan_priority` for a given `block_range`.
    ///
    /// This function will set all scan ranges with block range bounds contained by `block_range` to the given
    /// `scan_priority`.
    /// If any scan ranges are found to overlap with the given `block_range`, they will be split at the boundary and
    /// the new scan ranges contained by `block_range` will be set to `scan_priority`.
    /// Any scan ranges that fully contain the `block_range` will be split out with the given `scan_priority`.
    /// Any scan ranges with `Scanning`, `RefetchingNullifiers` or `Scanned` priority or with higher (or equal) priority than
    /// `scan_priority` will be ignored.
    pub(crate) fn punch_scan_priority(
        &mut self,
        block_range: Range<BlockHeight>,
        scan_priority: ScanPriority,
    ) {
        let mut scan_ranges_contained_by_block_range = Vec::new();
        let mut scan_ranges_for_splitting = Vec::new();

        for (index, scan_range) in self.scan_ranges.iter().enumerate() {
            if scan_range.priority() == ScanPriority::Scanned
                || scan_range.priority() == ScanPriority::ScannedWithoutMapping
                || scan_range.priority() == ScanPriority::Scanning
                || scan_range.priority() == ScanPriority::RefetchingNullifiers
                || scan_range.priority() >= scan_priority
            {
                continue;
            }

            match (
                block_range.contains(&scan_range.block_range().start),
                block_range.contains(&(scan_range.block_range().end - 1)),
                scan_range.block_range().contains(&block_range.start),
            ) {
                (true, true, _) => scan_ranges_contained_by_block_range.push(index),
                (true, false, _) | (false, true, _) | (false, false, true) => {
                    scan_ranges_for_splitting.push(index);
                }
                (false, false, false) => {}
            }
        }

        for index in scan_ranges_contained_by_block_range {
            let scan_range = &mut self.scan_ranges[index];
            *scan_range = ScanRange::from_parts(scan_range.block_range().clone(), scan_priority);
        }

        // split out the scan ranges in reverse order to maintain the correct index for lower scan ranges
        for index in scan_ranges_for_splitting.into_iter().rev() {
            let split_ranges =
                split_out_scan_range(&self.scan_ranges[index], block_range.clone(), scan_priority);
            self.replace_scan_range(index, split_ranges);
        }
        debug_assert_eq!(self.check_invariants(), Ok(()));
    }

    /// Determines the block range which contains all the note commitments for the shard of a given `shielded_protocol` surrounding
    /// the specified `block_height`.
    ///
    /// If no shard range exists for the given `block_height`, return the range of the incomplete shard at the chain tip.
    /// If `block_height` contains note commitments from multiple shards, return the block range of all of those shards combined.
    ///
    /// If `shielded_protocol` is `None`, return the surrounding [`self::NARROW_SCAN_AREA`] blocks starting from the
    /// closest lower multiple of [`self::NARROW_SCAN_AREA`] to `block_height`.
    fn determine_block_range(
        &self,
        consensus_parameters: &impl consensus::Parameters,
        block_height: BlockHeight,
        shielded_protocol: Option<ShieldedPool>,
    ) -> Range<BlockHeight> {
        let Some(mut shielded_protocol) = shielded_protocol else {
            let block_height = u32::from(block_height);
            let lower_bound =
                BlockHeight::from_u32(block_height - (block_height % NARROW_SCAN_AREA));
            let higher_bound = lower_bound + NARROW_SCAN_AREA;

            return lower_bound..higher_bound;
        };

        loop {
            match shielded_protocol {
                ShieldedPool::Sapling => {
                    if block_height
                        < consensus_parameters
                            .activation_height(consensus::NetworkUpgrade::Sapling)
                            .expect("network activation height should be set")
                    {
                        panic!("pre-sapling not supported");
                    } else {
                        break;
                    }
                }
                ShieldedPool::Orchard => {
                    if block_height
                        < consensus_parameters
                            .activation_height(consensus::NetworkUpgrade::Nu5)
                            .expect("network activation height should be set")
                    {
                        shielded_protocol = ShieldedPool::Sapling;
                    } else {
                        break;
                    }
                }
                ShieldedPool::Ironwood => {
                    // Treat a missing NU6.3 activation height as not active.
                    if consensus_parameters
                        .activation_height(consensus::NetworkUpgrade::Nu6_3)
                        .is_none_or(|activation| block_height < activation)
                        || self.ironwood_shard_ranges.is_empty()
                    {
                        shielded_protocol = ShieldedPool::Orchard;
                    } else {
                        break;
                    }
                }
            }
        }

        let shard_ranges = self.shard_ranges(shielded_protocol);

        let target_ranges = shard_ranges
            .iter()
            .filter(|range| range.contains(&block_height))
            .collect::<Vec<_>>();

        if let (Some(first), Some(last)) = (target_ranges.first(), target_ranges.last()) {
            return first.start..last.end;
        }

        let start = if let Some(range) = shard_ranges.last() {
            range.end - 1
        } else {
            // With no shard ranges at all (a server that does not serve
            // this pool, or a pool freshly activated), fall back to the
            // pool's own history: the wallet birthday clamped to the
            // pool's activation height. An unclamped birthday would let
            // the chain-tip punch flood the entire wallet range.
            let pool_upgrade = match shielded_protocol {
                ShieldedPool::Sapling => consensus::NetworkUpgrade::Sapling,
                ShieldedPool::Orchard => consensus::NetworkUpgrade::Nu5,
                ShieldedPool::Ironwood => consensus::NetworkUpgrade::Nu6_3,
            };
            let activation = consensus_parameters
                .activation_height(pool_upgrade)
                .expect("the pool was selected because its upgrade is active");
            self.wallet_birthday()
                .expect("scan range should not be empty")
                .max(activation)
        };
        let end = self
            .last_known_chain_height()
            .expect("scan range should not be empty")
            + 1;

        let range = Range { start, end };

        assert!(
            range.contains(&block_height),
            "block height should always be within the incomplete shard at chain tip when no complete shard range is found!"
        );

        range
    }

    /// Selects and prepares the next scan range for scanning.
    ///
    /// Sets the range for scanning to `Scanning` priority but returns the scan range with its initial priority for use
    /// in scanning and post-scan processing.
    /// If the selected range is of `ScannedWithoutMapping` priority, the range is set to `RefetchingNullifiers` but
    /// returns the scan range with priority `ScannedWithoutMapping` for use in scanning and post-scan processing.
    /// Returns `None` if there are no more ranges to scan.
    ///
    /// Set `nullifier_map_limit_exceeded` to `true` if the nullifiers are not going to be mapped to the wallet's main
    /// nullifier map due to performance constraints.
    pub(crate) fn select_scan_range(
        &mut self,
        consensus_parameters: &impl consensus::Parameters,
        nullifier_map_limit_exceeded: bool,
    ) -> Option<ScanRange> {
        assert_eq!(
            self.check_invariants(),
            Ok(()),
            "scan ranges are invalid before selecting a range to scan"
        );

        let (first_unscanned_index, first_unscanned_range) = self
            .scan_ranges
            .iter()
            .enumerate()
            .find(|(_, scan_range)| {
                scan_range.priority() != ScanPriority::Scanned
                    && scan_range.priority() != ScanPriority::RefetchingNullifiers
            })?;
        let (selected_index, selected_scan_range) = if first_unscanned_range.priority()
            == ScanPriority::ScannedWithoutMapping
        {
            // prioritise re-fetching the nullifiers when a range with priority `ScannedWithoutMapping` is the first
            // unscanned range.
            // the `first_unscanned_range` may have `Scanning` priority here as we must select a `ScannedWithoutMapping` range only when all ranges below are `Scanned`. if a `ScannedwithoutMapping` range is selected and completes *before* a lower range that is currently `Scanning`, the nullifiers will need to be discarded and re-fetched afterwards. so this avoids a race condition that results in a sync inefficiency.
            (first_unscanned_index, first_unscanned_range.clone())
        } else {
            // scan ranges are sorted from lowest to highest priority.
            // scan ranges with the same priority are sorted in block height order.
            // the highest priority scan range is then selected from the end of the list, the highest priority with highest
            // starting block height.
            // if the highest priority is `Historic` the range with the lowest starting block height is selected instead.
            // if nullifiers are not being mapped to the wallet's main nullifier map due to performance constraints
            // (`nullifier_map_limit_exceeded` is set `true`) then the range with the highest priority and lowest starting block
            // height is selected to allow notes to be spendable quickly on rescan, otherwise spends would not be detected as nullifiers will be temporarily discarded.
            // TODO: add this documentation of performance levels and order of scanning to pepper-sync doc comments
            let mut scan_ranges_priority_sorted: Vec<(usize, ScanRange)> =
                self.scan_ranges.iter().cloned().enumerate().collect();
            if nullifier_map_limit_exceeded {
                scan_ranges_priority_sorted
                    .sort_by(|(_, a), (_, b)| b.block_range().start.cmp(&a.block_range().start));
            }
            scan_ranges_priority_sorted.sort_by_key(|(_, scan_range)| scan_range.priority());

            scan_ranges_priority_sorted
                .last()
                .map(|(index, highest_priority_range)| {
                    if highest_priority_range.priority() == ScanPriority::Historic {
                        if nullifier_map_limit_exceeded {
                            // in this case, scan ranges are already sorted from highest to lowest and we are selecting
                            // the last range (lowest range with historic priority)
                            (*index, highest_priority_range.clone())
                        } else {
                            // in this case, scan ranges are sorted from lowest to highest and we are selecting
                            // the lowest range with historic priority
                            scan_ranges_priority_sorted
                                .iter()
                                .find(|(_, range)| range.priority() == ScanPriority::Historic)
                                .expect("range with Historic priority exists in this scope")
                                .clone()
                        }
                    } else {
                        // select the last range in the list
                        (*index, highest_priority_range.clone())
                    }
                })?
        };

        let selected_priority = selected_scan_range.priority();

        if selected_priority == ScanPriority::Scanned
            || selected_priority == ScanPriority::Scanning
            || selected_priority == ScanPriority::RefetchingNullifiers
        {
            return None;
        }

        // historic scan ranges can be larger than a shard block range so must be split out.
        // otherwise, just set the scan priority of selected range to `Scanning`.
        // if the selected range is of `ScannedWithoutMapping` priority, set the range to `RefetchingNullifiers` priority
        // instead of `Scanning`.
        let selected_block_range = match selected_priority {
            ScanPriority::Historic => {
                let shard_block_range = self.determine_block_range(
                    consensus_parameters,
                    selected_scan_range.block_range().start,
                    Some(ShieldedPool::Ironwood),
                );
                let split_ranges = split_out_scan_range(
                    &selected_scan_range,
                    shard_block_range,
                    ScanPriority::Scanning,
                );
                let selected_block_range = split_ranges
                    .first()
                    .expect("split ranges should always be non-empty")
                    .block_range()
                    .clone();
                self.replace_scan_range(selected_index, split_ranges);

                selected_block_range
            }
            ScanPriority::ScannedWithoutMapping => {
                let selected_scan_range = &mut self.scan_ranges[selected_index];
                *selected_scan_range = ScanRange::from_parts(
                    selected_scan_range.block_range().clone(),
                    ScanPriority::RefetchingNullifiers,
                );

                selected_scan_range.block_range().clone()
            }
            _ => {
                let selected_scan_range = &mut self.scan_ranges[selected_index];
                *selected_scan_range = ScanRange::from_parts(
                    selected_scan_range.block_range().clone(),
                    ScanPriority::Scanning,
                );

                selected_scan_range.block_range().clone()
            }
        };
        debug_assert_eq!(self.check_invariants(), Ok(()));

        Some(ScanRange::from_parts(
            selected_block_range,
            selected_priority,
        ))
    }

    /// Creates block ranges that contain all outputs for the shards completed at `subtree_completing_heights` and adds
    /// them to the `shielded_protocol` shard ranges.
    ///
    /// The network upgrade activation height for the `shielded_protocol` is the first shard start height for the case
    /// where the shard ranges are empty.
    #[cfg_attr(feature = "darkside_test", allow(dead_code))]
    pub(crate) fn add_shard_ranges(
        &mut self,
        consensus_parameters: &impl consensus::Parameters,
        shielded_protocol: ShieldedPool,
        subtree_completing_heights: impl IntoIterator<Item = BlockHeight>,
    ) {
        let network_upgrade = match shielded_protocol {
            ShieldedPool::Sapling => consensus::NetworkUpgrade::Sapling,
            ShieldedPool::Orchard => consensus::NetworkUpgrade::Nu5,
            ShieldedPool::Ironwood => consensus::NetworkUpgrade::Nu6_3,
        };
        let network_upgrade_activation_height = consensus_parameters
            .activation_height(network_upgrade)
            .expect("activation height should exist for this network upgrade!");

        let shard_ranges = match shielded_protocol {
            ShieldedPool::Sapling => &mut self.sapling_shard_ranges,
            ShieldedPool::Orchard => &mut self.orchard_shard_ranges,
            ShieldedPool::Ironwood => &mut self.ironwood_shard_ranges,
        };

        let highest_subtree_completing_height = shard_ranges
            .last()
            .map_or(network_upgrade_activation_height, |shard_range| {
                shard_range.end - 1
            });

        subtree_completing_heights.into_iter().fold(
            highest_subtree_completing_height,
            |previous_subtree_completing_height, subtree_completing_height| {
                shard_ranges.push(Range {
                    start: previous_subtree_completing_height,
                    end: subtree_completing_height + 1,
                });

                tracing::debug!(
                    "{:?} subtree root height: {}",
                    shielded_protocol,
                    subtree_completing_height
                );

                subtree_completing_height
            },
        );
    }

    fn shard_ranges(&self, shielded_protocol: ShieldedPool) -> &[Range<BlockHeight>] {
        match shielded_protocol {
            ShieldedPool::Sapling => &self.sapling_shard_ranges,
            ShieldedPool::Orchard => &self.orchard_shard_ranges,
            ShieldedPool::Ironwood => &self.ironwood_shard_ranges,
        }
    }

    /// Index of the scan range that contains every block of `block_range`.
    fn index_containing(&self, block_range: &Range<BlockHeight>) -> Option<usize> {
        self.scan_ranges.iter().position(|scan_range| {
            scan_range.block_range().contains(&block_range.start)
                && scan_range.block_range().contains(&(block_range.end - 1))
        })
    }

    /// Replaces the scan range at `index` with `split_ranges`, which must cover exactly the same blocks.
    fn replace_scan_range(&mut self, index: usize, split_ranges: Vec<ScanRange>) {
        debug_assert_eq!(
            split_ranges
                .first()
                .zip(split_ranges.last())
                .map(|(first, last)| first.block_range().start..last.block_range().end),
            Some(self.scan_ranges[index].block_range().clone()),
            "split ranges {split_ranges:?} must cover the scan range they replace"
        );
        self.scan_ranges.splice(index..=index, split_ranges);
    }

    /// Checks that the scan ranges run from the wallet birthday to the last known chain height in block height order
    /// with no overlaps, gaps or empty ranges.
    fn check_invariants(&self) -> Result<(), String> {
        if let Some(empty) = self.scan_ranges.iter().find(|range| range.is_empty()) {
            return Err(format!("empty scan range {empty}"));
        }
        if let Some(pair) = self
            .scan_ranges
            .windows(2)
            .find(|pair| pair[0].block_range().end != pair[1].block_range().start)
        {
            return Err(format!(
                "scan ranges {} and {} overlap or leave a gap",
                pair[0], pair[1]
            ));
        }
        Ok(())
    }
}

impl Default for SyncState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(any(test, feature = "test-features"))]
impl SyncState {
    /// Creates sync state with the given scan ranges, for tests exercising
    /// spendability/witness gating without a chain.
    pub fn new_for_test(scan_ranges: Vec<ScanRange>) -> Self {
        let sync_state = SyncState {
            scan_ranges,
            ..Self::new()
        };
        debug_assert_eq!(sync_state.check_invariants(), Ok(()));
        sync_state
    }
}

/// Takes a `scan_range` and splits it at `block_range.start` and `block_range.end`, returning a vec of scan ranges where
/// the scan range contained within the specified `block_range` has the given `scan_priority`.
///
/// If `block_range` goes beyond the bounds of `scan_range.block_range()` no splitting will occur at the upper and/or
/// lower bound but the priority will still be updated.
///
/// Panics if no blocks in `block_range` are contained within `scan_range.block_range()`.
fn split_out_scan_range(
    scan_range: &ScanRange,
    block_range: Range<BlockHeight>,
    scan_priority: ScanPriority,
) -> Vec<ScanRange> {
    assert!(
        !block_range.is_empty()
            && block_range.start < scan_range.block_range().end
            && scan_range.block_range().start < block_range.end,
        "cannot split {}..{} out of scan range {scan_range}: they share no blocks",
        block_range.start,
        block_range.end,
    );

    let mut split_ranges = Vec::new();
    if let Some((lower_range, higher_range)) = scan_range.split_at(block_range.start) {
        split_ranges.push(lower_range);
        if let Some((middle_range, higher_range)) = higher_range.split_at(block_range.end) {
            // `scan_range` is split at the upper and lower bound of `block_range`
            split_ranges.push(ScanRange::from_parts(
                middle_range.block_range().clone(),
                scan_priority,
            ));
            split_ranges.push(higher_range);
        } else {
            // `scan_range` is split only at the lower bound of `block_range`
            split_ranges.push(ScanRange::from_parts(
                higher_range.block_range().clone(),
                scan_priority,
            ));
        }
    } else if let Some((lower_range, higher_range)) = scan_range.split_at(block_range.end) {
        // `scan_range` is split only at the upper bound of `block_range`
        split_ranges.push(ScanRange::from_parts(
            lower_range.block_range().clone(),
            scan_priority,
        ));
        split_ranges.push(higher_range);
    } else {
        // `scan_range` is not split as it is fully contained within `block_range`
        // only scan priority is updated
        split_ranges.push(ScanRange::from_parts(
            scan_range.block_range().clone(),
            scan_priority,
        ));
    }

    split_ranges
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use zcash_protocol::local_consensus::LocalNetwork;

    use super::*;

    const BASE_NETWORK: LocalNetwork = LocalNetwork {
        overwinter: Some(BlockHeight::from_u32(1)),
        sapling: Some(BlockHeight::from_u32(1)),
        blossom: Some(BlockHeight::from_u32(1)),
        heartwood: Some(BlockHeight::from_u32(1)),
        canopy: Some(BlockHeight::from_u32(1)),
        nu5: Some(BlockHeight::from_u32(1)),
        nu6: Some(BlockHeight::from_u32(1)),
        nu6_1: Some(BlockHeight::from_u32(1)),
        nu6_2: Some(BlockHeight::from_u32(1)),
        nu6_3: Some(BlockHeight::from_u32(100)),
    };

    const NO_NU6_3_NETWORK: LocalNetwork = LocalNetwork {
        nu6_3: None,
        ..BASE_NETWORK
    };

    /// Mainnet-shaped network: NU6.3 activates recently, near the chain tip.
    const TIP_ACTIVATION_NETWORK: LocalNetwork = LocalNetwork {
        nu6_3: Some(BlockHeight::from_u32(19_000)),
        ..BASE_NETWORK
    };

    const BIRTHDAY: u32 = 100;
    const TIP: u32 = 300;

    fn h(height: u32) -> BlockHeight {
        BlockHeight::from_u32(height)
    }

    fn range(start: u32, end: u32, priority: ScanPriority) -> ScanRange {
        ScanRange::from_parts(h(start)..h(end), priority)
    }

    fn sync_state_with_ranges(birthday: u32, tip: u32) -> SyncState {
        SyncState::new_for_test(vec![range(birthday, tip + 1, ScanPriority::Historic)])
    }

    /// Sapling and orchard shards complete at 149, 199 and 249, so neighbouring shard ranges share their boundary
    /// block: `1..150`, `149..200`, `199..250`.
    fn with_shards(mut sync_state: SyncState) -> SyncState {
        for pool in [ShieldedPool::Sapling, ShieldedPool::Orchard] {
            sync_state.add_shard_ranges(&NO_NU6_3_NETWORK, pool, [h(149), h(199), h(249)]);
        }
        sync_state
    }

    /// A first sync session from `BIRTHDAY` to `TIP`.
    fn first_session() -> SyncState {
        let mut sync_state = with_shards(SyncState::new());
        sync_state.update_scan_ranges(&NO_NU6_3_NETWORK, h(BIRTHDAY - 1), h(TIP));
        sync_state
    }

    fn assert_plan(sync_state: &SyncState, birthday: u32, tip: u32, expected: &[ScanRange]) {
        assert_eq!(sync_state.check_invariants(), Ok(()));
        assert_eq!(sync_state.wallet_birthday(), Some(h(birthday)));
        assert_eq!(sync_state.last_known_chain_height(), Some(h(tip)));
        assert_eq!(sync_state.scan_ranges(), expected);
    }

    fn select(sync_state: &mut SyncState) -> Option<ScanRange> {
        sync_state.select_scan_range(&NO_NU6_3_NETWORK, false)
    }

    fn finish(sync_state: &mut SyncState, scan_range: &ScanRange) {
        sync_state.set_scanned_scan_range(scan_range.block_range().clone(), true);
        sync_state.merge_scan_ranges(ScanPriority::Scanned);
    }

    #[test]
    fn first_session_plans_birthday_to_tip() {
        let mut sync_state = with_shards(SyncState::new());

        let reorg_detection_start_height =
            sync_state.update_scan_ranges(&NO_NU6_3_NETWORK, h(BIRTHDAY - 1), h(TIP));

        assert_eq!(reorg_detection_start_height, h(BIRTHDAY));
        assert_plan(
            &sync_state,
            BIRTHDAY,
            TIP,
            &[
                range(100, 110, ScanPriority::Verify),
                range(110, 249, ScanPriority::Historic),
                range(249, 301, ScanPriority::ChainTip),
            ],
        );
    }

    #[test]
    fn select_follows_priority_then_scans_historic_shard_by_shard() {
        let mut sync_state = first_session();

        let verify = select(&mut sync_state).expect("verify range");
        assert_eq!(verify, range(100, 110, ScanPriority::Verify));
        let chain_tip = select(&mut sync_state).expect("chain tip range");
        assert_eq!(chain_tip, range(249, 301, ScanPriority::ChainTip));
        // historic ranges are split at the shard containing their start
        let first_shard = select(&mut sync_state).expect("historic range");
        assert_eq!(first_shard, range(110, 150, ScanPriority::Historic));
        let second_shard = select(&mut sync_state).expect("historic range");
        assert_eq!(second_shard, range(150, 200, ScanPriority::Historic));
        let third_shard = select(&mut sync_state).expect("historic range");
        assert_eq!(third_shard, range(200, 249, ScanPriority::Historic));
        assert_plan(
            &sync_state,
            BIRTHDAY,
            TIP,
            &[
                range(100, 110, ScanPriority::Scanning),
                range(110, 150, ScanPriority::Scanning),
                range(150, 200, ScanPriority::Scanning),
                range(200, 249, ScanPriority::Scanning),
                range(249, 301, ScanPriority::Scanning),
            ],
        );
        assert_eq!(select(&mut sync_state), None);

        finish(&mut sync_state, &chain_tip);
        finish(&mut sync_state, &second_shard);
        assert_eq!(sync_state.fully_scanned_height(), Some(h(99)));
        assert_eq!(sync_state.highest_scanned_height(), Some(h(300)));
        finish(&mut sync_state, &verify);
        finish(&mut sync_state, &first_shard);
        assert_eq!(sync_state.fully_scanned_height(), Some(h(199)));
        assert!(!sync_state.scan_complete());
        finish(&mut sync_state, &third_shard);

        assert_plan(
            &sync_state,
            BIRTHDAY,
            TIP,
            &[range(100, 301, ScanPriority::Scanned)],
        );
        assert!(sync_state.scan_complete());
        assert_eq!(sync_state.calculate_scanned_blocks(), 201);
    }

    #[test]
    fn found_note_on_shard_boundary_punches_both_shards() {
        let mut sync_state = with_shards(sync_state_with_ranges(BIRTHDAY, TIP));

        // block 149 holds the last outputs of the first shard and the first outputs of the second
        assert_eq!(
            sync_state.determine_block_range(
                &NO_NU6_3_NETWORK,
                h(149),
                Some(ShieldedPool::Orchard)
            ),
            h(1)..h(200)
        );
        sync_state.set_found_note_scan_range(
            &NO_NU6_3_NETWORK,
            Some(ShieldedPool::Orchard),
            h(149),
        );

        assert_plan(
            &sync_state,
            BIRTHDAY,
            TIP,
            &[
                range(100, 200, ScanPriority::FoundNote),
                range(200, 301, ScanPriority::Historic),
            ],
        );
    }

    #[test]
    fn found_note_on_shard_boundary_leaves_scanned_lower_shard_alone() {
        let mut sync_state = with_shards(SyncState::new_for_test(vec![
            range(100, 150, ScanPriority::Scanned),
            range(150, 301, ScanPriority::Historic),
        ]));

        sync_state.set_found_note_scan_range(
            &NO_NU6_3_NETWORK,
            Some(ShieldedPool::Sapling),
            h(149),
        );

        assert_plan(
            &sync_state,
            BIRTHDAY,
            TIP,
            &[
                range(100, 150, ScanPriority::Scanned),
                range(150, 200, ScanPriority::FoundNote),
                range(200, 301, ScanPriority::Historic),
            ],
        );
    }

    #[test]
    fn narrow_scan_target_punches_surrounding_area() {
        let mut sync_state = sync_state_with_ranges(9_000, 25_000);

        sync_state.set_found_note_scan_ranges(
            &NO_NU6_3_NETWORK,
            ShieldedPool::Orchard,
            [ScanTarget {
                block_height: h(12_345),
                txid: TxId::from_bytes([1; 32]),
                narrow_scan_area: true,
            }]
            .into_iter(),
        );

        assert_plan(
            &sync_state,
            9_000,
            25_000,
            &[
                range(9_000, 10_000, ScanPriority::Historic),
                range(10_000, 20_000, ScanPriority::FoundNote),
                range(20_000, 25_001, ScanPriority::Historic),
            ],
        );
    }

    #[test]
    fn punch_skips_scanning_scanned_and_higher_priorities() {
        let mut sync_state = SyncState::new_for_test(vec![
            range(100, 150, ScanPriority::Scanned),
            range(150, 170, ScanPriority::ScannedWithoutMapping),
            range(170, 190, ScanPriority::RefetchingNullifiers),
            range(190, 200, ScanPriority::Scanning),
            range(200, 250, ScanPriority::ChainTip),
            range(250, 301, ScanPriority::Historic),
        ]);
        let before = sync_state.scan_ranges().to_vec();

        sync_state.punch_scan_priority(h(100)..h(280), ScanPriority::FoundNote);

        let mut expected = before[..5].to_vec();
        expected.extend([
            range(250, 280, ScanPriority::FoundNote),
            range(280, 301, ScanPriority::Historic),
        ]);
        assert_plan(&sync_state, BIRTHDAY, TIP, &expected);
    }

    #[test]
    fn select_with_nullifier_map_limit_prefers_lowest_range_of_highest_priority() {
        let sync_state = SyncState::new_for_test(vec![
            range(100, 150, ScanPriority::Historic),
            range(150, 200, ScanPriority::FoundNote),
            range(200, 250, ScanPriority::Historic),
            range(250, 301, ScanPriority::FoundNote),
        ]);

        let mut unlimited = sync_state.clone();
        assert_eq!(
            unlimited.select_scan_range(&NO_NU6_3_NETWORK, false),
            Some(range(250, 301, ScanPriority::FoundNote))
        );
        let mut limited = sync_state;
        assert_eq!(
            limited.select_scan_range(&NO_NU6_3_NETWORK, true),
            Some(range(150, 200, ScanPriority::FoundNote))
        );
        assert_eq!(limited.check_invariants(), Ok(()));
        assert_eq!(limited.scan_ranges()[1].priority(), ScanPriority::Scanning);
    }

    #[test]
    fn nullifier_refetch_runs_first_when_lowest_unscanned() {
        let mut sync_state = SyncState::new_for_test(vec![
            range(100, 150, ScanPriority::Scanned),
            range(150, 200, ScanPriority::ScannedWithoutMapping),
            range(200, 301, ScanPriority::ChainTip),
        ]);

        let refetch = select(&mut sync_state).expect("refetch range");
        assert_eq!(
            refetch,
            range(150, 200, ScanPriority::ScannedWithoutMapping)
        );
        assert_eq!(
            sync_state.scan_ranges()[1],
            range(150, 200, ScanPriority::RefetchingNullifiers)
        );

        // a refetch that lost the race with a lower range is discarded and selected again
        sync_state.reset_refetching_nullifiers_scan_range(refetch.block_range().clone());
        assert_eq!(select(&mut sync_state), Some(refetch.clone()));

        finish(&mut sync_state, &refetch);
        assert_plan(
            &sync_state,
            BIRTHDAY,
            TIP,
            &[
                range(100, 200, ScanPriority::Scanned),
                range(200, 301, ScanPriority::ChainTip),
            ],
        );
    }

    #[test]
    fn unmapped_scan_is_marked_for_refetch_and_merged() {
        let mut sync_state = SyncState::new_for_test(vec![
            range(100, 150, ScanPriority::Scanning),
            range(150, 200, ScanPriority::ScannedWithoutMapping),
            range(200, 250, ScanPriority::Scanning),
            range(250, 301, ScanPriority::Historic),
        ]);

        sync_state.set_scanned_scan_range(h(200)..h(250), false);
        sync_state.merge_scan_ranges(ScanPriority::ScannedWithoutMapping);

        assert_plan(
            &sync_state,
            BIRTHDAY,
            TIP,
            &[
                range(100, 150, ScanPriority::Scanning),
                range(150, 250, ScanPriority::ScannedWithoutMapping),
                range(250, 301, ScanPriority::Historic),
            ],
        );
    }

    #[test]
    fn tip_move_appends_chain_tip_and_verifies_new_blocks() {
        let mut sync_state = with_shards(SyncState::new_for_test(vec![range(
            100,
            301,
            ScanPriority::Scanned,
        )]));

        let reorg_detection_start_height =
            sync_state.update_scan_ranges(&NO_NU6_3_NETWORK, h(TIP), h(320));

        assert_eq!(reorg_detection_start_height, h(301));
        assert_plan(
            &sync_state,
            BIRTHDAY,
            320,
            &[
                range(100, 301, ScanPriority::Scanned),
                range(301, 311, ScanPriority::Verify),
                range(311, 321, ScanPriority::ChainTip),
            ],
        );
    }

    #[test]
    fn unchanged_tip_on_scanned_wallet_leaves_verification_to_caller() {
        let mut sync_state = with_shards(SyncState::new_for_test(vec![range(
            100,
            301,
            ScanPriority::Scanned,
        )]));

        let reorg_detection_start_height =
            sync_state.update_scan_ranges(&NO_NU6_3_NETWORK, h(TIP), h(TIP));

        assert_eq!(reorg_detection_start_height, h(TIP + 1));
        assert_plan(
            &sync_state,
            BIRTHDAY,
            TIP,
            &[range(100, 301, ScanPriority::Scanned)],
        );

        // the caller found a different block hash at the tip
        let verify = sync_state.set_verify_scan_range(h(TIP), VerifyEnd::VerifyHighest);
        assert_eq!(verify, range(291, 301, ScanPriority::Verify));
        assert_plan(
            &sync_state,
            BIRTHDAY,
            TIP,
            &[
                range(100, 291, ScanPriority::Scanned),
                range(291, 301, ScanPriority::Verify),
            ],
        );
    }

    #[test]
    fn interrupted_session_is_reset_on_next_session() {
        let mut sync_state = with_shards(SyncState::new_for_test(vec![
            range(100, 150, ScanPriority::Scanned),
            range(150, 200, ScanPriority::RefetchingNullifiers),
            range(200, 250, ScanPriority::Scanning),
            range(250, 301, ScanPriority::Historic),
        ]));

        let reorg_detection_start_height =
            sync_state.update_scan_ranges(&NO_NU6_3_NETWORK, h(TIP), h(TIP));

        assert_eq!(reorg_detection_start_height, h(200));
        assert_plan(
            &sync_state,
            BIRTHDAY,
            TIP,
            &[
                range(100, 150, ScanPriority::Scanned),
                range(150, 200, ScanPriority::ScannedWithoutMapping),
                range(200, 210, ScanPriority::Verify),
                range(210, 249, ScanPriority::FoundNote),
                range(249, 301, ScanPriority::ChainTip),
            ],
        );
    }

    #[test]
    fn scan_targets_are_prioritised_on_next_session() {
        let mut sync_state = with_shards(SyncState::new_for_test(vec![range(
            100,
            301,
            ScanPriority::Historic,
        )]));
        let target = ScanTarget {
            block_height: h(170),
            txid: TxId::from_bytes([2; 32]),
            narrow_scan_area: false,
        };
        sync_state.add_scan_targets([target]);

        sync_state.update_scan_ranges(&NO_NU6_3_NETWORK, h(TIP), h(TIP));

        assert_plan(
            &sync_state,
            BIRTHDAY,
            TIP,
            &[
                range(100, 110, ScanPriority::Verify),
                range(110, 149, ScanPriority::Historic),
                range(149, 200, ScanPriority::FoundNote),
                range(200, 249, ScanPriority::Historic),
                range(249, 301, ScanPriority::ChainTip),
            ],
        );
        assert_eq!(
            sync_state.find_scan_targets(&(h(149)..h(200))),
            BTreeSet::from([target])
        );
        assert!(sync_state.find_scan_targets(&(h(171)..h(200))).is_empty());

        sync_state.remove_scanned_scan_targets(h(170));
        assert!(sync_state.scan_targets().is_empty());
    }

    #[test]
    fn reorg_extends_verification_below_failed_range() {
        let mut sync_state = SyncState::new_for_test(vec![
            range(100, 250, ScanPriority::Scanned),
            range(250, 260, ScanPriority::Scanning),
            range(260, 301, ScanPriority::Historic),
        ]);

        // the verify range starting at 250 failed its continuity check
        sync_state.set_scan_priority(&(h(250)..h(260)), ScanPriority::Verify);
        let verify = sync_state.set_verify_scan_range(h(249), VerifyEnd::VerifyHighest);
        sync_state.merge_scan_ranges(ScanPriority::Verify);

        assert_eq!(verify, range(240, 250, ScanPriority::Verify));
        assert_plan(
            &sync_state,
            BIRTHDAY,
            TIP,
            &[
                range(100, 240, ScanPriority::Scanned),
                range(240, 260, ScanPriority::Verify),
                range(260, 301, ScanPriority::Historic),
            ],
        );
    }

    #[test]
    fn truncate_scan_ranges() {
        let mut sync_state = SyncState::new_for_test(vec![
            range(1, 100, ScanPriority::Historic),
            range(100, 200, ScanPriority::Historic),
            range(200, 300, ScanPriority::Historic),
            range(300, 400, ScanPriority::Historic),
        ]);

        sync_state.truncate_scan_ranges(h(250));

        assert_plan(
            &sync_state,
            1,
            250,
            &[
                range(1, 100, ScanPriority::Historic),
                range(100, 200, ScanPriority::Historic),
                range(200, 251, ScanPriority::Historic),
            ],
        );
    }

    #[test]
    fn truncate_on_range_boundary_keeps_lower_range_whole() {
        let mut sync_state = SyncState::new_for_test(vec![
            range(100, 250, ScanPriority::Scanned),
            range(250, 301, ScanPriority::ChainTip),
        ]);

        sync_state.truncate_scan_ranges(h(249));

        assert_plan(
            &sync_state,
            BIRTHDAY,
            249,
            &[range(100, 250, ScanPriority::Scanned)],
        );
    }

    #[test]
    fn truncate_to_genesis_clears_everything() {
        let mut sync_state = first_session();
        sync_state.add_scan_targets([ScanTarget {
            block_height: h(170),
            txid: TxId::from_bytes([2; 32]),
            narrow_scan_area: false,
        }]);

        sync_state.truncate_scan_ranges(consensus::H0);

        assert!(sync_state.scan_ranges().is_empty());
        assert!(sync_state.sapling_shard_ranges().is_empty());
        assert!(sync_state.orchard_shard_ranges().is_empty());
        assert!(sync_state.scan_targets().is_empty());
    }

    #[test]
    fn check_invariants_names_offending_ranges() {
        let mut sync_state = SyncState::new_for_test(vec![range(100, 301, ScanPriority::Historic)]);
        assert_eq!(sync_state.check_invariants(), Ok(()));

        sync_state.scan_ranges = vec![
            range(100, 150, ScanPriority::Historic),
            range(160, 301, ScanPriority::Historic),
        ];
        assert_eq!(
            sync_state.check_invariants(),
            Err(
                "scan ranges Historic(100..150) and Historic(160..301) overlap or leave a gap"
                    .to_string()
            )
        );

        sync_state.scan_ranges = vec![
            range(100, 150, ScanPriority::Historic),
            range(140, 301, ScanPriority::Historic),
        ];
        assert!(sync_state.check_invariants().is_err());

        sync_state.scan_ranges = vec![
            range(100, 150, ScanPriority::Historic),
            range(150, 150, ScanPriority::Historic),
            range(150, 301, ScanPriority::Historic),
        ];
        assert_eq!(
            sync_state.check_invariants(),
            Err("empty scan range Historic(150..150)".to_string())
        );
    }

    #[test]
    #[should_panic(expected = "scan ranges are invalid before selecting a range to scan")]
    fn select_rejects_invalid_scan_ranges() {
        let mut sync_state = SyncState::new();
        sync_state.scan_ranges = vec![
            range(100, 150, ScanPriority::Historic),
            range(160, 301, ScanPriority::Historic),
        ];

        select(&mut sync_state);
    }

    #[test]
    #[should_panic(expected = "cannot split 200..210 out of scan range Historic(100..150)")]
    fn split_out_scan_range_rejects_disjoint_block_range() {
        split_out_scan_range(
            &range(100, 150, ScanPriority::Historic),
            h(200)..h(210),
            ScanPriority::FoundNote,
        );
    }

    #[test]
    fn ironwood_before_nu6_3_downgrades_to_orchard() {
        let sync_state = sync_state_with_ranges(1, 200);
        let range =
            sync_state.determine_block_range(&BASE_NETWORK, h(50), Some(ShieldedPool::Ironwood));
        assert!(
            range.contains(&h(50)),
            "range should cover the requested height even after downgrade"
        );
    }

    #[test]
    fn ironwood_without_nu6_3_always_downgrades_to_orchard() {
        let sync_state = sync_state_with_ranges(1, 200);
        let range = sync_state.determine_block_range(
            &NO_NU6_3_NETWORK,
            h(150),
            Some(ShieldedPool::Ironwood),
        );
        assert!(range.contains(&h(150)));
    }

    #[test]
    fn ironwood_at_nu6_3_activation_uses_ironwood_shard_ranges() {
        let mut sync_state = sync_state_with_ranges(1, 200);
        sync_state.add_shard_ranges(&BASE_NETWORK, ShieldedPool::Ironwood, [h(149)]);
        let range =
            sync_state.determine_block_range(&BASE_NETWORK, h(120), Some(ShieldedPool::Ironwood));
        assert_eq!(range, h(100)..h(150));
    }

    #[test]
    fn chain_tip_priority_confined_to_tip_when_ironwood_shards_are_empty() {
        // A wallet with a long history: birthday (1_000) far below the chain tip (20_000).
        let mut sync_state = sync_state_with_ranges(1_000, 20_000);

        // Sapling and orchard have complete shards reaching near the tip, so their
        // incomplete tip shards start close to the chain tip (17_999 and 18_499).
        sync_state.sapling_shard_ranges = vec![h(1_000)..h(18_000)];
        sync_state.orchard_shard_ranges = vec![h(1_000)..h(18_500)];
        // Ironwood shard ranges are empty: a tolerated server condition, and the
        // universal state on every network immediately after NU6.3 activation.
        assert!(sync_state.ironwood_shard_ranges.is_empty());

        sync_state.set_chain_tip_scan_range(&TIP_ACTIVATION_NETWORK, h(20_000));

        let chain_tip_start = sync_state
            .scan_ranges()
            .iter()
            .filter(|range| range.priority() == ScanPriority::ChainTip)
            .map(|range| range.block_range().start)
            .min()
            .expect("chain tip punch should mark at least one range");

        // The chain-tip region must stay confined near the tip. The lowest
        // legitimate anchor is the sapling incomplete tip shard start (17_999);
        // an ironwood fallback may reach no lower than the NU6.3 activation
        // height (19_000). It must never flood down to the wallet birthday.
        assert!(
            chain_tip_start >= h(17_999),
            "ChainTip priority flooded down to {chain_tip_start} (wallet birthday is 1_000); \
             expected the chain-tip region confined to the tip shards (start >= 17_999)"
        );
    }

    #[derive(Debug, Clone)]
    enum Op {
        Punch {
            offset: u32,
            len: u32,
            priority: ScanPriority,
        },
        FoundNote {
            offset: u32,
            narrow: bool,
        },
        Select {
            nullifier_map_limit_exceeded: bool,
        },
        FinishScan {
            index: usize,
            nullifiers_mapped: bool,
        },
        /// A new session: resets interrupted ranges and moves the tip up by `blocks_mined`.
        NewSession {
            blocks_mined: u32,
        },
        Reorg {
            depth: u32,
        },
    }

    fn op() -> impl Strategy<Value = Op> {
        let punch_priority = prop::sample::select(vec![
            ScanPriority::Historic,
            ScanPriority::OpenAdjacent,
            ScanPriority::FoundNote,
            ScanPriority::ChainTip,
            ScanPriority::Verify,
        ]);
        prop_oneof![
            1 => (0..400u32, 1..80u32, punch_priority).prop_map(|(offset, len, priority)| {
                Op::Punch {
                    offset,
                    len,
                    priority,
                }
            }),
            1 => (0..400u32, any::<bool>()).prop_map(|(offset, narrow)| Op::FoundNote { offset, narrow }),
            3 => any::<bool>().prop_map(|nullifier_map_limit_exceeded| Op::Select {
                nullifier_map_limit_exceeded
            }),
            3 => (any::<usize>(), any::<bool>()).prop_map(|(index, nullifiers_mapped)| {
                Op::FinishScan {
                    index,
                    nullifiers_mapped,
                }
            }),
            1 => (0..30u32).prop_map(|blocks_mined| Op::NewSession { blocks_mined }),
            1 => (1..=100u32).prop_map(|depth| Op::Reorg { depth }),
        ]
    }

    fn assert_protected_or_at_least(
        sync_state: &SyncState,
        block_range: &Range<BlockHeight>,
        priority: ScanPriority,
    ) {
        for scan_range in sync_state.scan_ranges() {
            let overlaps = scan_range.block_range().start < block_range.end
                && block_range.start < scan_range.block_range().end;
            if overlaps {
                assert!(
                    scan_range.priority() >= priority
                        || matches!(
                            scan_range.priority(),
                            ScanPriority::Scanned
                                | ScanPriority::ScannedWithoutMapping
                                | ScanPriority::Scanning
                                | ScanPriority::RefetchingNullifiers
                        ),
                    "{scan_range} overlaps punched range {block_range:?} but is below {priority:?}"
                );
            }
        }
    }

    proptest! {
        #[test]
        fn scan_ranges_stay_valid_under_any_sequence_of_operations(ops in prop::collection::vec(op(), 1..80)) {
            let mut sync_state = first_session();
            let mut tip = TIP;
            let mut in_flight: Vec<ScanRange> = Vec::new();

            for op in ops {
                match op {
                    Op::Punch { offset, len, priority } => {
                        let start = h(BIRTHDAY + offset % (tip - BIRTHDAY + 1));
                        let block_range = start..start + len;
                        sync_state.punch_scan_priority(block_range.clone(), priority);
                        assert_protected_or_at_least(&sync_state, &block_range, priority);
                    }
                    Op::FoundNote { offset, narrow } => {
                        let height = h(BIRTHDAY + offset % (tip - BIRTHDAY + 1));
                        let pool = (!narrow).then_some(ShieldedPool::Orchard);
                        let block_range = sync_state.determine_block_range(&NO_NU6_3_NETWORK, height, pool);
                        sync_state.set_found_note_scan_range(&NO_NU6_3_NETWORK, pool, height);
                        assert_protected_or_at_least(&sync_state, &block_range, ScanPriority::FoundNote);
                    }
                    Op::Select { nullifier_map_limit_exceeded } => {
                        if let Some(selected) = sync_state.select_scan_range(&NO_NU6_3_NETWORK, nullifier_map_limit_exceeded) {
                            let index = sync_state
                                .index_containing(selected.block_range())
                                .expect("selected range is in the plan");
                            let expected = if selected.priority() == ScanPriority::ScannedWithoutMapping {
                                ScanPriority::RefetchingNullifiers
                            } else {
                                ScanPriority::Scanning
                            };
                            prop_assert_eq!(sync_state.scan_ranges()[index].priority(), expected);
                            in_flight.push(selected);
                        }
                    }
                    Op::FinishScan { index, nullifiers_mapped } => {
                        if !in_flight.is_empty() {
                            let finished = in_flight.swap_remove(index % in_flight.len());
                            if finished.priority() == ScanPriority::ScannedWithoutMapping {
                                sync_state.set_scanned_scan_range(finished.block_range().clone(), true);
                            } else {
                                sync_state.set_scanned_scan_range(finished.block_range().clone(), nullifiers_mapped);
                                sync_state.merge_scan_ranges(ScanPriority::ScannedWithoutMapping);
                            }
                            sync_state.merge_scan_ranges(ScanPriority::Scanned);
                            let index = sync_state
                                .index_containing(finished.block_range())
                                .expect("finished range is in the plan");
                            prop_assert!(matches!(
                                sync_state.scan_ranges()[index].priority(),
                                ScanPriority::Scanned | ScanPriority::ScannedWithoutMapping
                            ));
                        }
                    }
                    Op::NewSession { blocks_mined } => {
                        in_flight.clear();
                        sync_state.update_scan_ranges(&NO_NU6_3_NETWORK, h(tip), h(tip + blocks_mined));
                        tip += blocks_mined;
                        prop_assert!(sync_state.scan_ranges().iter().all(|scan_range| !matches!(
                            scan_range.priority(),
                            ScanPriority::Scanning | ScanPriority::RefetchingNullifiers
                        )));
                    }
                    Op::Reorg { depth } => {
                        // truncating below the birthday is handled by clearing the wallet instead
                        if tip - depth >= BIRTHDAY {
                            in_flight.clear();
                            tip -= depth;
                            sync_state.truncate_scan_ranges(h(tip));
                        }
                    }
                }

                prop_assert_eq!(sync_state.check_invariants(), Ok(()));
                prop_assert_eq!(sync_state.wallet_birthday(), Some(h(BIRTHDAY)));
                prop_assert_eq!(sync_state.last_known_chain_height(), Some(h(tip)));
            }
        }
    }
}
