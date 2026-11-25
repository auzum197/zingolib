//! Scan ranges, their priorities and the scheduler that keeps them.

use std::ops::Range;

use crate::wallet::TreeBounds;
use zcash_protocol::consensus::BlockHeight;

mod scheduler;
mod serialization;

pub use scheduler::SyncState;

/// Number of blocks set to `Verify` priority at a reorg detection point.
const VERIFY_BLOCK_RANGE_SIZE: u32 = 10;

/// Scanning range priority levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ScanPriority {
    /// Block ranges that are currently refetching nullifiers.
    RefetchingNullifiers,
    /// Block ranges that are currently being scanned.
    Scanning,
    /// Block ranges that have already been scanned will not be re-scanned.
    Scanned,
    /// Block ranges that have already been scanned. The nullifiers from this range were not mapped after scanning and
    /// spend detection to reduce memory consumption and/or storage for non-linear scanning. These nullifiers will need
    /// to be re-fetched for final spend detection when this range is the lowest unscanned range in the wallet's list
    /// of scan ranges.
    ScannedWithoutMapping,
    /// Block ranges to be scanned to advance the fully-scanned height.
    Historic,
    /// Block ranges adjacent to heights at which the user opened the wallet.
    OpenAdjacent,
    /// Blocks that must be scanned to complete note commitment tree shards adjacent to found notes.
    FoundNote,
    /// Blocks that must be scanned to complete the latest note commitment tree shard.
    ChainTip,
    /// A previously scanned range that must be verified to check it is still in the
    /// main chain, has highest priority.
    Verify,
}

/// A range of blocks to be scanned, along with its associated priority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanRange {
    block_range: Range<BlockHeight>,
    priority: ScanPriority,
}

impl std::fmt::Display for ScanRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:?}({}..{})",
            self.priority, self.block_range.start, self.block_range.end,
        )
    }
}

impl ScanRange {
    /// Constructs a scan range from its constituent parts.
    #[must_use]
    pub fn from_parts(block_range: Range<BlockHeight>, priority: ScanPriority) -> Self {
        assert!(
            block_range.end >= block_range.start,
            "{block_range:?} is invalid for ScanRange({priority:?})",
        );
        ScanRange {
            block_range,
            priority,
        }
    }

    /// Returns the range of block heights to be scanned.
    #[must_use]
    pub fn block_range(&self) -> &Range<BlockHeight> {
        &self.block_range
    }

    /// Returns the priority with which the scan range should be scanned.
    #[must_use]
    pub fn priority(&self) -> ScanPriority {
        self.priority
    }

    /// Returns whether or not the scan range is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.block_range.is_empty()
    }

    /// Returns the number of blocks in the scan range.
    #[must_use]
    pub fn len(&self) -> usize {
        usize::try_from(u32::from(self.block_range.end) - u32::from(self.block_range.start))
            .expect("due to number of max blocks should always be valid usize")
    }

    /// Shifts the start of the block range to the right if `block_height >
    /// self.block_range().start`. Returns `None` if the resulting range would
    /// be empty (or the range was already empty).
    #[must_use]
    pub fn truncate_start(&self, block_height: BlockHeight) -> Option<Self> {
        if block_height >= self.block_range.end || self.is_empty() {
            None
        } else {
            Some(ScanRange {
                block_range: self.block_range.start.max(block_height)..self.block_range.end,
                priority: self.priority,
            })
        }
    }

    /// Shifts the end of the block range to the left if `block_height <
    /// self.block_range().end`. Returns `None` if the resulting range would
    /// be empty (or the range was already empty).
    #[must_use]
    pub fn truncate_end(&self, block_height: BlockHeight) -> Option<Self> {
        if block_height <= self.block_range.start || self.is_empty() {
            None
        } else {
            Some(ScanRange {
                block_range: self.block_range.start..self.block_range.end.min(block_height),
                priority: self.priority,
            })
        }
    }

    /// Splits this scan range at the specified height, such that the provided height becomes the
    /// end of the first range returned and the start of the second. Returns `None` if
    /// `p <= self.block_range().start || p >= self.block_range().end`.
    #[must_use]
    pub fn split_at(&self, p: BlockHeight) -> Option<(Self, Self)> {
        (p > self.block_range.start && p < self.block_range.end).then_some((
            ScanRange {
                block_range: self.block_range.start..p,
                priority: self.priority,
            },
            ScanRange {
                block_range: p..self.block_range.end,
                priority: self.priority,
            },
        ))
    }
}

/// Initial sync state.
///
/// All fields will be reset when a new sync session starts.
#[derive(Debug, Clone)]
pub struct InitialSyncState {
    /// One block above the fully scanned wallet height at start of sync session.
    ///
    /// If chain height is not larger than fully scanned height when sync is called, this value will be set to chain
    /// height instead.
    pub(crate) sync_start_height: BlockHeight,
    /// The tree sizes below the wallet birthday and at the chain tip at start of sync session.
    pub(crate) wallet_tree_bounds: TreeBounds,
    /// Total number of blocks scanned in previous sync sessions.
    pub(crate) previously_scanned_blocks: u32,
    /// Total number of sapling outputs scanned in previous sync sessions.
    pub(crate) previously_scanned_sapling_outputs: u32,
    /// Total number of orchard outputs scanned in previous sync sessions.
    pub(crate) previously_scanned_orchard_outputs: u32,
    /// Total number of ironwood outputs scanned in previous sync sessions.
    pub(crate) previously_scanned_ironwood_outputs: u32,
}

impl InitialSyncState {
    /// Create new `InitialSyncState`
    #[must_use]
    pub fn new() -> Self {
        InitialSyncState {
            sync_start_height: 0.into(),
            wallet_tree_bounds: TreeBounds {
                sapling_initial_tree_size: 0,
                sapling_final_tree_size: 0,
                orchard_initial_tree_size: 0,
                orchard_final_tree_size: 0,
                ironwood_initial_tree_size: 0,
                ironwood_final_tree_size: 0,
            },
            previously_scanned_blocks: 0,
            previously_scanned_sapling_outputs: 0,
            previously_scanned_orchard_outputs: 0,
            previously_scanned_ironwood_outputs: 0,
        }
    }
}

impl Default for InitialSyncState {
    fn default() -> Self {
        Self::new()
    }
}
