//! Wallet file codec of [`SyncState`].

use std::{
    collections::BTreeSet,
    io::{Read, Write},
};

use crate::{
    serialization::{ReadableWriteable, check_oldest_version, collect_unique, invalid_data},
    wallet::ScanTarget,
};
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use zcash_encoding::Vector;
use zcash_protocol::consensus::BlockHeight;

use super::{ScanPriority, ScanRange, SyncState};

impl ReadableWriteable for SyncState {
    // Version 4 inserts the ironwood shard ranges after the orchard ones.
    const VERSION: u8 = 4;

    fn read<R: Read>(mut reader: R, _input: ()) -> std::io::Result<Self> {
        let version = check_oldest_version(Self::get_version(&mut reader)?, 3, "sync state")?;
        let scan_ranges = Vector::read(&mut reader, |r| {
            let start = BlockHeight::from_u32(r.read_u32::<LittleEndian>()?);
            let end = BlockHeight::from_u32(r.read_u32::<LittleEndian>()?);
            let priority = match r.read_u8()? {
                0 => Ok(ScanPriority::RefetchingNullifiers),
                1 => Ok(ScanPriority::Scanning),
                2 => Ok(ScanPriority::Scanned),
                3 => Ok(ScanPriority::ScannedWithoutMapping),
                4 => Ok(ScanPriority::Historic),
                5 => Ok(ScanPriority::OpenAdjacent),
                6 => Ok(ScanPriority::FoundNote),
                7 => Ok(ScanPriority::ChainTip),
                8 => Ok(ScanPriority::Verify),
                _ => Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "invalid scan priority",
                )),
            }?;
            if start > end {
                return Err(invalid_data(format!(
                    "scan range {start}..{end} ends before it starts"
                )));
            }

            Ok(ScanRange::from_parts(start..end, priority))
        })?;
        if let Some(pair) = scan_ranges
            .windows(2)
            .find(|pair| pair[0].block_range().end != pair[1].block_range().start)
        {
            return Err(invalid_data(format!(
                "scan ranges {} and {} overlap or leave a gap",
                pair[0], pair[1]
            )));
        }
        let sapling_shard_ranges = Vector::read(&mut reader, |r| {
            let start = BlockHeight::from_u32(r.read_u32::<LittleEndian>()?);
            let end = BlockHeight::from_u32(r.read_u32::<LittleEndian>()?);

            Ok(start..end)
        })?;
        let orchard_shard_ranges = Vector::read(&mut reader, |r| {
            let start = BlockHeight::from_u32(r.read_u32::<LittleEndian>()?);
            let end = BlockHeight::from_u32(r.read_u32::<LittleEndian>()?);

            Ok(start..end)
        })?;
        let ironwood_shard_ranges = if version >= 4 {
            Vector::read(&mut reader, |r| {
                let start = BlockHeight::from_u32(r.read_u32::<LittleEndian>()?);
                let end = BlockHeight::from_u32(r.read_u32::<LittleEndian>()?);

                Ok(start..end)
            })?
        } else {
            Vec::new()
        };
        let scan_targets = collect_unique(
            Vector::read(&mut reader, |r| ScanTarget::read(r, ()))?,
            BTreeSet::len,
            "sync state scan target",
        )?;

        Ok(Self::from_parts(
            scan_ranges,
            sapling_shard_ranges,
            orchard_shard_ranges,
            ironwood_shard_ranges,
            scan_targets,
        ))
    }

    fn write<W: Write>(&self, mut writer: W, _input: ()) -> std::io::Result<()> {
        writer.write_u8(Self::VERSION)?;
        Vector::write(&mut writer, self.scan_ranges(), |w, scan_range| {
            w.write_u32::<LittleEndian>(scan_range.block_range().start.into())?;
            w.write_u32::<LittleEndian>(scan_range.block_range().end.into())?;
            w.write_u8(scan_range.priority() as u8)
        })?;
        Vector::write(
            &mut writer,
            self.sapling_shard_ranges(),
            |w, shard_range| {
                w.write_u32::<LittleEndian>(shard_range.start.into())?;
                w.write_u32::<LittleEndian>(shard_range.end.into())
            },
        )?;
        Vector::write(
            &mut writer,
            self.orchard_shard_ranges(),
            |w, shard_range| {
                w.write_u32::<LittleEndian>(shard_range.start.into())?;
                w.write_u32::<LittleEndian>(shard_range.end.into())
            },
        )?;
        Vector::write(
            &mut writer,
            self.ironwood_shard_ranges(),
            |w, shard_range| {
                w.write_u32::<LittleEndian>(shard_range.start.into())?;
                w.write_u32::<LittleEndian>(shard_range.end.into())
            },
        )?;
        Vector::write(
            &mut writer,
            &self.scan_targets().iter().collect::<Vec<_>>(),
            |w, &scan_target| scan_target.write(w, ()),
        )
    }
}

#[cfg(test)]
mod tests {
    use zcash_primitives::transaction::TxId;

    use super::*;

    // Helper: build a minimal v3 SyncState byte blob (no ironwood_shard_ranges).
    // Format: version(1) | scan_ranges[0] | sapling_shard_ranges[0] |
    //         orchard_shard_ranges[0] | scan_targets[0]
    fn v3_sync_state_bytes() -> Vec<u8> {
        let mut out = Vec::new();
        out.write_u8(3).unwrap();
        Vector::write(&mut out, &[] as &[()], |_, _| Ok(())).unwrap();
        Vector::write(&mut out, &[] as &[()], |_, _| Ok(())).unwrap();
        Vector::write(&mut out, &[] as &[()], |_, _| Ok(())).unwrap();
        Vector::write(&mut out, &[] as &[()], |_, _| Ok(())).unwrap();
        out
    }

    #[test]
    fn sync_state_v3_reads_with_empty_ironwood_ranges() {
        let bytes = v3_sync_state_bytes();
        let sync_state = SyncState::read(bytes.as_slice(), ()).expect("v3 should read cleanly");
        assert!(sync_state.ironwood_shard_ranges().is_empty());
    }

    #[test]
    fn sync_state_v4_roundtrip_preserves_ironwood_ranges() {
        let state = SyncState::from_parts(
            vec![ScanRange::from_parts(
                BlockHeight::from_u32(100)..BlockHeight::from_u32(400),
                ScanPriority::Historic,
            )],
            Vec::new(),
            Vec::new(),
            vec![
                BlockHeight::from_u32(100)..BlockHeight::from_u32(200),
                BlockHeight::from_u32(300)..BlockHeight::from_u32(400),
            ],
            BTreeSet::new(),
        );
        let mut bytes = Vec::new();
        state.write(&mut bytes, ()).expect("write should succeed");
        let recovered = SyncState::read(bytes.as_slice(), ()).expect("read should succeed");
        assert_eq!(
            recovered.ironwood_shard_ranges(),
            state.ironwood_shard_ranges()
        );
        assert_eq!(recovered.scan_ranges(), state.scan_ranges());
    }

    /// Bytes written by `SyncState::write` before the scan scheduler was extracted, for a state with every scan
    /// priority, shard ranges for each pool (sapling's overlapping at a shard boundary) and two scan targets.
    const SYNC_STATE_V4_BYTES: &str = "0409640000006e000000026e0000007800000003780000008200000000820000008c000000018c000000960000000496000000a000000005a0000000aa00000006aa000000b400000007b4000000be0000000802640000009600000095000000aa0000000178000000a000000001aa000000b400000002007d00000007070707070707070707070707070707070707070707070707070707070707070000af000000090909090909090909090909090909090909090909090909090909090909090901";

    #[test]
    fn sync_state_bytes_are_unchanged_by_read_and_write() {
        let bytes = hex::decode(SYNC_STATE_V4_BYTES).expect("valid hex");
        let state = SyncState::read(bytes.as_slice(), ()).expect("read should succeed");

        let priorities = [
            ScanPriority::Scanned,
            ScanPriority::ScannedWithoutMapping,
            ScanPriority::RefetchingNullifiers,
            ScanPriority::Scanning,
            ScanPriority::Historic,
            ScanPriority::OpenAdjacent,
            ScanPriority::FoundNote,
            ScanPriority::ChainTip,
            ScanPriority::Verify,
        ];
        let expected_scan_ranges = (100..)
            .step_by(10)
            .zip(priorities)
            .map(|(start, priority)| {
                ScanRange::from_parts(
                    BlockHeight::from_u32(start)..BlockHeight::from_u32(start + 10),
                    priority,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(state.scan_ranges(), expected_scan_ranges);
        assert_eq!(
            state.sapling_shard_ranges(),
            [
                BlockHeight::from_u32(100)..BlockHeight::from_u32(150),
                BlockHeight::from_u32(149)..BlockHeight::from_u32(170),
            ]
        );
        assert_eq!(
            state.orchard_shard_ranges(),
            [BlockHeight::from_u32(120)..BlockHeight::from_u32(160)]
        );
        assert_eq!(
            state.ironwood_shard_ranges(),
            [BlockHeight::from_u32(170)..BlockHeight::from_u32(180)]
        );
        assert_eq!(
            state.scan_targets().iter().copied().collect::<Vec<_>>(),
            [
                ScanTarget {
                    block_height: BlockHeight::from_u32(125),
                    txid: TxId::from_bytes([7; 32]),
                    narrow_scan_area: false,
                },
                ScanTarget {
                    block_height: BlockHeight::from_u32(175),
                    txid: TxId::from_bytes([9; 32]),
                    narrow_scan_area: true,
                },
            ]
        );

        let mut written = Vec::new();
        state.write(&mut written, ()).expect("write should succeed");
        assert_eq!(hex::encode(written), SYNC_STATE_V4_BYTES);
    }

    fn scan_target_bytes(block_height: u32) -> Vec<u8> {
        let mut out = Vec::new();
        ScanTarget {
            block_height: BlockHeight::from_u32(block_height),
            txid: TxId::from_bytes([3; 32]),
            narrow_scan_area: false,
        }
        .write(&mut out, ())
        .unwrap();
        out
    }

    #[test]
    fn sync_state_before_version_3_is_rejected() {
        let mut bytes = v3_sync_state_bytes();
        bytes[0] = 2;
        assert!(SyncState::read(bytes.as_slice(), ()).is_err());
    }

    fn sync_state_bytes(scan_ranges: &[(u32, u32)], scan_targets: &[Vec<u8>]) -> Vec<u8> {
        let mut out = vec![4];
        Vector::write(&mut out, scan_ranges, |w, &(start, end)| {
            w.write_u32::<LittleEndian>(start)?;
            w.write_u32::<LittleEndian>(end)?;
            w.write_u8(ScanPriority::Scanned as u8)
        })
        .unwrap();
        out.extend([0, 0, 0]);
        Vector::write(&mut out, scan_targets, |w, target| w.write_all(target)).unwrap();
        out
    }

    #[test]
    fn sync_state_rejects_a_scan_range_that_ends_before_it_starts() {
        let bytes = sync_state_bytes(&[(10, 5)], &[]);
        assert!(SyncState::read(bytes.as_slice(), ()).is_err());
    }

    #[test]
    fn sync_state_rejects_scan_ranges_with_a_gap_or_overlap() {
        for ranges in [[(1, 5), (6, 10)], [(1, 6), (5, 10)]] {
            let bytes = sync_state_bytes(&ranges, &[]);
            assert!(SyncState::read(bytes.as_slice(), ()).is_err());
        }
        let contiguous = sync_state_bytes(&[(1, 5), (5, 10)], &[]);
        assert!(SyncState::read(contiguous.as_slice(), ()).is_ok());
    }

    #[test]
    fn sync_state_rejects_a_repeated_scan_target() {
        let target = scan_target_bytes(500);
        let bytes = sync_state_bytes(&[], &[target.clone(), target]);
        assert!(SyncState::read(bytes.as_slice(), ()).is_err());
    }
}
