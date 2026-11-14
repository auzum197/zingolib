//! Operations on the wallet data that only a sync engine performs.
//!
//! The traits are sealed, so no other crate implements them, and a consumer of the wallet data
//! reaches these operations only by importing them from here.

use std::{collections::BTreeSet, ops::Range};

use incrementalmerkletree::Position;
use zcash_keys::address::UnifiedAddress;
use zcash_primitives::{block::BlockHash, transaction::TxId};
use zcash_protocol::{
    ShieldedPool,
    consensus::{self, BlockHeight},
    memo::Memo,
    value::Zatoshis,
};
use zcash_transparent::address::Script;
use zip32::Scope;

use crate::{
    keys::{KeyId, TransparentAddressId},
    status::ConfirmationStatus,
    sync::{InitialSyncState, ScanPriority, ScanRange, SyncState},
    wallet::{
        IronwoodNote, OrchardNote, OutgoingIronwoodNote, OutgoingNote, OutgoingOrchardNote,
        OutgoingSaplingNote, OutputId, SaplingNote, ScanTarget, TransparentCoin, TreeBounds,
        WalletBlock, WalletNote, WalletTransaction,
    },
};

mod sealed {
    pub trait Sealed {}
}

impl sealed::Sealed for SyncState {}
impl sealed::Sealed for InitialSyncState {}
impl sealed::Sealed for WalletBlock {}
impl sealed::Sealed for WalletTransaction {}
impl sealed::Sealed for TransparentCoin {}
impl<N, Nf: Copy, P> sealed::Sealed for WalletNote<N, Nf, P> {}
impl<N, P> sealed::Sealed for OutgoingNote<N, P> {}
impl sealed::Sealed for KeyId {}

/// Encodes an orchard receiver as a single-receiver unified address.
pub fn encode_orchard_receiver(
    parameters: &impl consensus::Parameters,
    orchard_address: &orchard::Address,
) -> Result<String, zcash_address::unified::ParseError> {
    Ok(zcash_address::unified::Encoding::encode(
        &<zcash_address::unified::Address as zcash_address::unified::Encoding>::try_from_items(
            vec![zcash_address::unified::Receiver::Orchard(
                orchard_address.to_raw_address_bytes(),
            )],
        )?,
        &parameters.network_type(),
    ))
}

/// Used to determine which end of the scan range is verified.
pub enum VerifyEnd {
    /// The verify range ends at the given height.
    VerifyHighest,
    /// The verify range starts at the given height.
    VerifyLowest,
}

/// Engine constructor of [`KeyId`].
pub trait KeyIdOps: sealed::Sealed {
    /// Constructs a key id from its account and scope.
    #[must_use]
    fn from_parts(account_id: zip32::AccountId, scope: Scope) -> Self;
}

impl KeyIdOps for KeyId {
    fn from_parts(account_id: zip32::AccountId, scope: Scope) -> Self {
        Self { account_id, scope }
    }
}

/// Engine constructor of [`WalletBlock`].
pub trait WalletBlockOps: sealed::Sealed {
    /// Records a scanned block: its header fields, the ids of every transaction it holds and the
    /// note commitment tree sizes at its bounds.
    #[must_use]
    fn from_parts(
        block_height: BlockHeight,
        block_hash: BlockHash,
        prev_hash: BlockHash,
        time: u32,
        txids: Vec<TxId>,
        tree_bounds: TreeBounds,
    ) -> Self;
}

impl WalletBlockOps for WalletBlock {
    fn from_parts(
        block_height: BlockHeight,
        block_hash: BlockHash,
        prev_hash: BlockHash,
        time: u32,
        txids: Vec<TxId>,
        tree_bounds: TreeBounds,
    ) -> Self {
        Self {
            block_height,
            block_hash,
            prev_hash,
            time,
            txids,
            tree_bounds,
        }
    }
}

/// Engine constructor of [`WalletTransaction`].
pub trait WalletTransactionOps: sealed::Sealed {
    /// Records a scanned transaction with the coins and notes decrypted from it.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    fn from_parts(
        txid: TxId,
        status: ConfirmationStatus,
        transaction: zcash_primitives::transaction::Transaction,
        datetime: u32,
        transparent_coins: Vec<TransparentCoin>,
        sapling_notes: Vec<SaplingNote>,
        orchard_notes: Vec<OrchardNote>,
        ironwood_notes: Vec<IronwoodNote>,
        outgoing_sapling_notes: Vec<OutgoingSaplingNote>,
        outgoing_orchard_notes: Vec<OutgoingOrchardNote>,
        outgoing_ironwood_notes: Vec<OutgoingIronwoodNote>,
    ) -> Self;
}

impl WalletTransactionOps for WalletTransaction {
    fn from_parts(
        txid: TxId,
        status: ConfirmationStatus,
        transaction: zcash_primitives::transaction::Transaction,
        datetime: u32,
        transparent_coins: Vec<TransparentCoin>,
        sapling_notes: Vec<SaplingNote>,
        orchard_notes: Vec<OrchardNote>,
        ironwood_notes: Vec<IronwoodNote>,
        outgoing_sapling_notes: Vec<OutgoingSaplingNote>,
        outgoing_orchard_notes: Vec<OutgoingOrchardNote>,
        outgoing_ironwood_notes: Vec<OutgoingIronwoodNote>,
    ) -> Self {
        Self {
            txid,
            status,
            transaction,
            datetime,
            transparent_coins,
            sapling_notes,
            orchard_notes,
            ironwood_notes,
            outgoing_sapling_notes,
            outgoing_orchard_notes,
            outgoing_ironwood_notes,
        }
    }
}

/// Engine constructor of [`TransparentCoin`].
pub trait TransparentCoinOps: sealed::Sealed {
    /// Records an unspent coin received to one of the wallet's transparent addresses.
    #[must_use]
    fn from_parts(
        output_id: OutputId,
        key_id: TransparentAddressId,
        address: String,
        script: Script,
        value: Zatoshis,
    ) -> Self;
}

impl TransparentCoinOps for TransparentCoin {
    fn from_parts(
        output_id: OutputId,
        key_id: TransparentAddressId,
        address: String,
        script: Script,
        value: Zatoshis,
    ) -> Self {
        Self {
            output_id,
            key_id,
            address,
            script,
            value,
            spending_transaction: None,
        }
    }
}

/// Engine operations on a [`WalletNote`].
pub trait WalletNoteOps<N, Nf>: sealed::Sealed {
    /// Records a note decrypted during scanning. The nullifier and position are known only when
    /// the note was decrypted from a compact block with its commitment tree data.
    #[must_use]
    fn from_parts(
        output_id: OutputId,
        key_id: KeyId,
        note: N,
        nullifier: Option<Nf>,
        position: Option<Position>,
        memo: Memo,
    ) -> Self;

    /// Records the nullifier and commitment tree position found when the note's compact output was
    /// scanned.
    fn set_nullifier_and_position(&mut self, nullifier: Nf, position: Position);

    /// Records a nullifier derived from the full viewing key, for a note scanned without its
    /// commitment tree data.
    fn set_nullifier(&mut self, nullifier: Nf);

    /// Replaces the block ranges where nullifiers must be re-fetched before the note is known to be
    /// unspent. Empty once every range has been re-fetched.
    fn set_refetch_nullifier_ranges(&mut self, block_ranges: Vec<Range<BlockHeight>>);
}

impl<N, Nf: Copy, P> WalletNoteOps<N, Nf> for WalletNote<N, Nf, P> {
    fn from_parts(
        output_id: OutputId,
        key_id: KeyId,
        note: N,
        nullifier: Option<Nf>,
        position: Option<Position>,
        memo: Memo,
    ) -> Self {
        Self {
            output_id,
            key_id,
            note,
            nullifier,
            position,
            memo,
            spending_transaction: None,
            refetch_nullifier_ranges: Vec::new(),
            marker: std::marker::PhantomData,
        }
    }

    fn set_nullifier_and_position(&mut self, nullifier: Nf, position: Position) {
        self.nullifier = Some(nullifier);
        self.position = Some(position);
    }

    fn set_nullifier(&mut self, nullifier: Nf) {
        self.nullifier = Some(nullifier);
    }

    fn set_refetch_nullifier_ranges(&mut self, block_ranges: Vec<Range<BlockHeight>>) {
        self.refetch_nullifier_ranges = block_ranges;
    }
}

/// Engine operations on an [`OutgoingNote`].
pub trait OutgoingNoteOps<N>: sealed::Sealed {
    /// Records a note recovered with one of the wallet's outgoing viewing keys. The recipient's
    /// full unified address is attached later, when an encoded memo in the same transaction
    /// names it.
    #[must_use]
    fn from_parts(output_id: OutputId, key_id: KeyId, note: N, memo: Memo) -> Self;

    /// Attaches the recipient's full unified address recorded in an encoded memo.
    fn set_recipient_full_unified_address(&mut self, unified_address: UnifiedAddress);
}

impl<N, P> OutgoingNoteOps<N> for OutgoingNote<N, P> {
    fn from_parts(output_id: OutputId, key_id: KeyId, note: N, memo: Memo) -> Self {
        Self {
            output_id,
            key_id,
            note,
            memo,
            recipient_full_unified_address: None,
            marker: std::marker::PhantomData,
        }
    }

    fn set_recipient_full_unified_address(&mut self, unified_address: UnifiedAddress) {
        self.recipient_full_unified_address = Some(unified_address);
    }
}

/// Engine accessors of [`InitialSyncState`].
pub trait InitialSyncStateOps: sealed::Sealed {
    /// One block above the fully scanned wallet height at start of sync session.
    #[must_use]
    fn sync_start_height(&self) -> BlockHeight;

    /// The tree sizes of the fully scanned height and chain tip at start of sync session.
    #[must_use]
    fn wallet_tree_bounds(&self) -> &TreeBounds;

    /// Total number of blocks scanned in previous sync sessions.
    #[must_use]
    fn previously_scanned_blocks(&self) -> u32;

    /// Total number of sapling outputs scanned in previous sync sessions.
    #[must_use]
    fn previously_scanned_sapling_outputs(&self) -> u32;

    /// Total number of orchard outputs scanned in previous sync sessions.
    #[must_use]
    fn previously_scanned_orchard_outputs(&self) -> u32;

    /// Total number of ironwood outputs scanned in previous sync sessions.
    #[must_use]
    fn previously_scanned_ironwood_outputs(&self) -> u32;
}

impl InitialSyncStateOps for InitialSyncState {
    fn sync_start_height(&self) -> BlockHeight {
        self.sync_start_height
    }

    fn wallet_tree_bounds(&self) -> &TreeBounds {
        &self.wallet_tree_bounds
    }

    fn previously_scanned_blocks(&self) -> u32 {
        self.previously_scanned_blocks
    }

    fn previously_scanned_sapling_outputs(&self) -> u32 {
        self.previously_scanned_sapling_outputs
    }

    fn previously_scanned_orchard_outputs(&self) -> u32 {
        self.previously_scanned_orchard_outputs
    }

    fn previously_scanned_ironwood_outputs(&self) -> u32 {
        self.previously_scanned_ironwood_outputs
    }
}

/// The scan scheduler operations of [`SyncState`]. The implementation is next to the state it
/// changes.
pub trait SyncStateOps: sealed::Sealed {
    /// State of the wallet at the start of the current sync session.
    #[must_use]
    fn initial_sync_state(&self) -> &InitialSyncState;

    /// Returns true if all scan ranges are scanned.
    fn scan_complete(&self) -> bool;

    /// Scan ranges whose blocks have been scanned, including those still waiting on their nullifiers.
    fn scanned_ranges(&self) -> impl DoubleEndedIterator<Item = &ScanRange>;

    /// Total number of blocks in the scanned ranges.
    #[must_use]
    fn calculate_scanned_blocks(&self) -> u32;

    /// Returns the scan targets within `block_range`.
    fn find_scan_targets(&self, block_range: &Range<BlockHeight>) -> BTreeSet<ScanTarget>;

    /// Adds scan targets for relevant transactions that have yet to be scanned.
    fn add_scan_targets(&mut self, scan_targets: impl IntoIterator<Item = ScanTarget>);

    /// Drops scan targets at or below `fully_scanned_height`, which have all been scanned.
    fn remove_scanned_scan_targets(&mut self, fully_scanned_height: BlockHeight);

    /// Plans the scan ranges at the start of a sync session.
    ///
    /// Recovers from an interrupted session, extends the ranges to `chain_height`, prioritises scan targets and the
    /// chain tip, and sets up verification of the lowest unscanned blocks. Returns the block height that reorg
    /// detection starts from.
    ///
    /// If the returned height is above `chain_height` there is nothing left to verify above the scanned blocks. The
    /// caller should then compare the chain tip block hash with the wallet's and call
    /// [`Self::set_verify_scan_range`] with [`VerifyEnd::VerifyHighest`] if they differ.
    fn update_scan_ranges(
        &mut self,
        consensus_parameters: &impl consensus::Parameters,
        last_known_chain_height: BlockHeight,
        chain_height: BlockHeight,
    ) -> BlockHeight;

    /// Sets the initial sync state at the start of the sync session.
    ///
    /// `wallet_tree_bounds` are the tree sizes below the wallet birthday and at `chain_height`.
    /// `previously_scanned_outputs` are the sapling, orchard and ironwood outputs in the already scanned ranges.
    fn set_initial_state(
        &mut self,
        chain_height: BlockHeight,
        wallet_tree_bounds: TreeBounds,
        previously_scanned_outputs: (u32, u32, u32),
    );

    /// Merges all adjacent ranges of a given `scan_priority`.
    fn merge_scan_ranges(&mut self, scan_priority: ScanPriority);

    /// Splits the range containing [`truncate_height` + 1] and removes all ranges containing block heights above
    /// `truncate_height`.
    /// If `truncate_height` is zero, the sync state will be cleared completely.
    fn truncate_scan_ranges(&mut self, truncate_height: BlockHeight);

    /// Splits out the highest or lowest `VERIFY_BLOCK_RANGE_SIZE` blocks from the scan range containing the given `block height`
    /// and sets it's priority to `Verify`.
    /// Returns a clone of the scan range to be verified.
    fn set_verify_scan_range(
        &mut self,
        block_height: BlockHeight,
        verify_end: VerifyEnd,
    ) -> ScanRange;

    /// Punches in the `shielded_protocol` shard block ranges surrounding each scan target with `ScanPriority::FoundNote`.
    ///
    /// If all `scan_targets` have the `narrow_scan_area` set to `true`, `shielded_protocol` is irrelevant.
    fn set_found_note_scan_ranges<T: Iterator<Item = ScanTarget>>(
        &mut self,
        consensus_parameters: &impl consensus::Parameters,
        shielded_protocol: ShieldedPool,
        scan_targets: T,
    );

    /// Punches in the `shielded_protocol` shard block range surrounding the `block_height` with `ScanPriority::FoundNote`.
    ///
    /// If `shielded_protocol` is `None`, punch in the surrounding `NARROW_SCAN_AREA` blocks starting from the
    /// closest lower multiple of `NARROW_SCAN_AREA` to `block_height`.
    fn set_found_note_scan_range(
        &mut self,
        consensus_parameters: &impl consensus::Parameters,
        shielded_protocol: Option<ShieldedPool>,
        block_height: BlockHeight,
    );

    /// Updates the `shielded_protocol` shard range to `FoundNote` scan priority if the `wallet_transaction` contains
    /// a note from the corresponding `shielded_protocol`.
    fn update_found_note_shard_priority(
        &mut self,
        consensus_parameters: &impl consensus::Parameters,
        shielded_protocol: ShieldedPool,
        wallet_transaction: &WalletTransaction,
    );

    /// Sets `scanned_range` to `Scanned`, or `ScannedWithoutMapping` if its nullifiers were not mapped.
    ///
    /// Panics if no single scan range contains `scanned_range`.
    fn set_scanned_scan_range(
        &mut self,
        scanned_range: Range<BlockHeight>,
        nullifiers_mapped: bool,
    );

    /// Sets `invalid_refetch_range` back to `ScannedWithoutMapping` so its nullifiers are fetched again.
    ///
    /// Panics if no single scan range contains `invalid_refetch_range`.
    fn reset_refetching_nullifiers_scan_range(&mut self, invalid_refetch_range: Range<BlockHeight>);

    /// Sets the scan range with `block_range` to the given `scan_priority`.
    ///
    /// Panics if no scan range is found with a block range of exactly `block_range`.
    fn set_scan_priority(&mut self, block_range: &Range<BlockHeight>, scan_priority: ScanPriority);

    /// Punches in a `scan_priority` for a given `block_range`.
    ///
    /// This function will set all scan ranges with block range bounds contained by `block_range` to the given
    /// `scan_priority`.
    /// If any scan ranges are found to overlap with the given `block_range`, they will be split at the boundary and
    /// the new scan ranges contained by `block_range` will be set to `scan_priority`.
    /// Any scan ranges that fully contain the `block_range` will be split out with the given `scan_priority`.
    /// Any scan ranges with `Scanning`, `RefetchingNullifiers` or `Scanned` priority or with higher (or equal) priority than
    /// `scan_priority` will be ignored.
    fn punch_scan_priority(&mut self, block_range: Range<BlockHeight>, scan_priority: ScanPriority);

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
    fn select_scan_range(
        &mut self,
        consensus_parameters: &impl consensus::Parameters,
        nullifier_map_limit_exceeded: bool,
    ) -> Option<ScanRange>;

    /// Creates block ranges that contain all outputs for the shards completed at `subtree_completing_heights` and adds
    /// them to the `shielded_protocol` shard ranges.
    ///
    /// The network upgrade activation height for the `shielded_protocol` is the first shard start height for the case
    /// where the shard ranges are empty.
    fn add_shard_ranges(
        &mut self,
        consensus_parameters: &impl consensus::Parameters,
        shielded_protocol: ShieldedPool,
        subtree_completing_heights: impl IntoIterator<Item = BlockHeight>,
    );
}
