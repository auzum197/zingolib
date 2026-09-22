//! Whether a note can be spent: its spend status is settled and a witness can be built for it.
//! Used by the spendable balance and note queries.

use std::ops::Range;

use pepper_sync::sync::{ScanPriority, ScanRange};
use pepper_sync::wallet::NoteInterface;
use zcash_protocol::ShieldedPool;
use zcash_protocol::consensus::BlockHeight;

use super::LightWallet;

impl LightWallet {
    /// Returns the block height at which all blocks equal to and above this height are scanned (scan ranges set to
    /// `Scanned`, `ScannedWithoutMapping` or `RefetchingNullifiers` priority).
    /// Returns `None` if `self.scan_ranges` is empty.
    ///
    /// Useful for determining which height all the nullifiers have been mapped from for guaranteeing if a note is
    /// unspent.
    ///
    /// `all_spends_known` may be set if all the spend locations are already known before scanning starts. For example,
    /// the location of all transparent spends are known due to the pre-scan gRPC calls. In this case, the height returned
    /// is the lowest height where there are no higher scan ranges with `FoundNote` or higher scan priority.
    pub(crate) fn spend_horizon(&self, all_spends_known: bool) -> Option<BlockHeight> {
        if let Some(scan_range) = self
            .sync_state
            .scan_ranges()
            .iter()
            .rev()
            .find(|scan_range| {
                if all_spends_known {
                    scan_range.priority() >= ScanPriority::FoundNote
                        || scan_range.priority() == ScanPriority::Scanning
                } else {
                    scan_range.priority() != ScanPriority::Scanned
                        && scan_range.priority() != ScanPriority::ScannedWithoutMapping
                        && scan_range.priority() != ScanPriority::RefetchingNullifiers
                }
            })
        {
            Some(scan_range.block_range().end)
        } else {
            self.sync_state
                .scan_ranges()
                .first()
                .map(|range| range.block_range().start)
        }
    }

    /// Returns `true` if all nullifiers above `note_height` have been checked for this note's spend status.
    ///
    /// Requires that `note_height >= spend_horizon` (all ranges above the note are scanned) and that every
    /// `refetch_nullifier_range` recorded on the note is fully contained within a `Scanned` scan range
    /// (nullifiers that were discarded due to memory constraints have since been re-fetched).
    pub(crate) fn note_spends_confirmed(
        &self,
        note_height: BlockHeight,
        spend_horizon: BlockHeight,
        refetch_nullifier_ranges: &[std::ops::Range<BlockHeight>],
    ) -> bool {
        note_height >= spend_horizon
            && refetch_nullifier_ranges.iter().all(|refetch_range| {
                self.sync_state.scan_ranges().iter().any(|scan_range| {
                    scan_range.priority() == ScanPriority::Scanned
                        && scan_range.block_range().contains(&refetch_range.start)
                        && scan_range.block_range().contains(&(refetch_range.end - 1))
                })
            })
    }

    pub(crate) fn can_build_witness<N>(
        &self,
        note_height: BlockHeight,
        anchor_height: BlockHeight,
    ) -> bool
    where
        N: NoteInterface,
    {
        let Some(birthday) = self.sync_state.wallet_birthday() else {
            return false;
        };
        let scan_ranges = self.sync_state.scan_ranges();

        match N::SHIELDED_PROTOCOL {
            ShieldedPool::Ironwood => check_note_shards_are_scanned(
                note_height,
                anchor_height,
                birthday,
                scan_ranges,
                self.sync_state.ironwood_shard_ranges(),
            ),
            ShieldedPool::Orchard => check_note_shards_are_scanned(
                note_height,
                anchor_height,
                birthday,
                scan_ranges,
                self.sync_state.orchard_shard_ranges(),
            ),
            ShieldedPool::Sapling => check_note_shards_are_scanned(
                note_height,
                anchor_height,
                birthday,
                scan_ranges,
                self.sync_state.sapling_shard_ranges(),
            ),
        }
    }
}

fn check_note_shards_are_scanned(
    note_height: BlockHeight,
    anchor_height: BlockHeight,
    wallet_birthday: BlockHeight,
    scan_ranges: &[ScanRange],
    shard_ranges: &[Range<BlockHeight>],
) -> bool {
    let incomplete_shard_range = if let Some(shard_range) = shard_ranges.last() {
        shard_range.end - 1..anchor_height + 1
    } else {
        wallet_birthday..anchor_height + 1
    };
    let mut shard_ranges = shard_ranges.to_vec();
    shard_ranges.push(incomplete_shard_range);

    let mut scanned_ranges = scan_ranges
        .iter()
        .filter(|scan_range| {
            scan_range.priority() == ScanPriority::Scanned
                || scan_range.priority() == ScanPriority::ScannedWithoutMapping
                || scan_range.priority() == ScanPriority::RefetchingNullifiers
        })
        .cloned()
        .collect::<Vec<_>>();
    'main: loop {
        if scanned_ranges.is_empty() {
            break;
        }
        let mut peekable_ranges = scanned_ranges.iter().enumerate().peekable();
        while let Some((index, range)) = peekable_ranges.next() {
            if let Some((next_index, next_range)) = peekable_ranges.peek() {
                if range.block_range().end == next_range.block_range().start {
                    assert!(*next_index == index + 1);
                    scanned_ranges.splice(
                        index..=*next_index,
                        vec![ScanRange::from_parts(
                            Range {
                                start: range.block_range().start,
                                end: next_range.block_range().end,
                            },
                            ScanPriority::Scanned,
                        )],
                    );
                    continue 'main;
                }
            } else {
                break 'main;
            }
        }
    }

    // a single block may contain two shards at the boundary so we check both are scanned in this case
    shard_ranges
        .iter()
        .filter(|&shard_range| shard_range.contains(&note_height))
        .all(|note_shard_range| {
            scanned_ranges
                .iter()
                .map(ScanRange::block_range)
                .any(|block_range| {
                    block_range.contains(&(note_shard_range.end - 1))
                        && (block_range.contains(&note_shard_range.start)
                            || note_shard_range.start < wallet_birthday)
                })
        })
}

#[cfg(test)]
mod check_note_shards_are_scanned {
    use pepper_sync::sync::{ScanPriority, ScanRange};
    use zcash_protocol::consensus::BlockHeight;

    use super::check_note_shards_are_scanned;

    #[test]
    fn birthday_within_note_shard_range() {
        let min_confirmations = 3;
        let wallet_birthday = BlockHeight::from_u32(10);
        let last_known_chain_height = BlockHeight::from_u32(202);
        let note_height = BlockHeight::from_u32(20);
        let anchor_height = last_known_chain_height + 1 - min_confirmations;
        let scan_ranges = vec![ScanRange::from_parts(
            wallet_birthday..last_known_chain_height + 1,
            ScanPriority::Scanned,
        )];
        let shard_ranges = vec![
            1.into()..51.into(),
            50.into()..101.into(),
            100.into()..151.into(),
        ];

        assert!(check_note_shards_are_scanned(
            note_height,
            anchor_height,
            wallet_birthday,
            &scan_ranges,
            &shard_ranges,
        ));
    }

    #[test]
    fn note_within_complete_shard() {
        let min_confirmations = 3;
        let wallet_birthday = BlockHeight::from_u32(10);
        let last_known_chain_height = BlockHeight::from_u32(202);
        let note_height = BlockHeight::from_u32(70);
        let anchor_height = last_known_chain_height + 1 - min_confirmations;
        let scan_ranges = vec![ScanRange::from_parts(
            wallet_birthday..last_known_chain_height + 1,
            ScanPriority::Scanned,
        )];
        let shard_ranges = vec![
            1.into()..51.into(),
            50.into()..101.into(),
            100.into()..151.into(),
        ];

        assert!(check_note_shards_are_scanned(
            note_height,
            anchor_height,
            wallet_birthday,
            &scan_ranges,
            &shard_ranges,
        ));
    }

    #[test]
    fn note_within_incomplete_shard() {
        let min_confirmations = 3;
        let wallet_birthday = BlockHeight::from_u32(10);
        let last_known_chain_height = BlockHeight::from_u32(202);
        let note_height = BlockHeight::from_u32(170);
        let anchor_height = last_known_chain_height + 1 - min_confirmations;
        let scan_ranges = vec![ScanRange::from_parts(
            wallet_birthday..last_known_chain_height + 1,
            ScanPriority::Scanned,
        )];
        let shard_ranges = vec![
            1.into()..51.into(),
            50.into()..101.into(),
            100.into()..151.into(),
        ];

        assert!(check_note_shards_are_scanned(
            note_height,
            anchor_height,
            wallet_birthday,
            &scan_ranges,
            &shard_ranges,
        ));
    }

    #[test]
    fn note_height_on_shard_boundary() {
        let min_confirmations = 3;
        let wallet_birthday = BlockHeight::from_u32(10);
        let last_known_chain_height = BlockHeight::from_u32(202);
        let note_height = BlockHeight::from_u32(100);
        let anchor_height = last_known_chain_height + 1 - min_confirmations;
        let scan_ranges = vec![ScanRange::from_parts(
            wallet_birthday..last_known_chain_height + 1,
            ScanPriority::Scanned,
        )];
        let shard_ranges = vec![
            1.into()..51.into(),
            50.into()..101.into(),
            100.into()..151.into(),
        ];

        assert!(check_note_shards_are_scanned(
            note_height,
            anchor_height,
            wallet_birthday,
            &scan_ranges,
            &shard_ranges,
        ));
    }
}
