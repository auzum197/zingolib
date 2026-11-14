//! Serialization and de-serialization of wallet structs in [`crate::wallet`] including utilities.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Write},
    ops::Range,
    sync::Arc,
};

use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};

use incrementalmerkletree::{Hashable, Level, Position};
use shardtree::{
    LocatedPrunableTree, PrunableTree, RetentionFlags, ShardTree, Tree,
    store::{Checkpoint, ShardStore, TreeState, memory::MemoryShardStore},
};
use zcash_client_backend::serialization::shardtree::write_shard;
use zcash_encoding::{Optional, Vector};
use zcash_primitives::{
    block::BlockHash,
    merkle_tree::HashSer,
    transaction::{Transaction, TxId},
};
use zcash_protocol::{
    consensus::{self, BlockHeight},
    value::Zatoshis,
};
use zcash_transparent::address::Script;

use crate::{
    keys::{KeyId, TransparentAddressId, TransparentScope, decode_unified_address},
    serialization::{ReadableWriteable, check_oldest_version, collect_unique, invalid_data},
    status::ConfirmationStatus,
};
use zcash_transparent::keys::NonHardenedChildIndex;

use super::{
    IronwoodNote, KeyIdInterface, NullifierMap, OrchardNote, OutgoingIronwoodNote, OutgoingNote,
    OutgoingNoteInterface, OutgoingOrchardNote, OutgoingSaplingNote, OutputId, OutputInterface,
    SHARDTREE_CHECKPOINT_ROLLING_WINDOW_SIZE, SaplingNote, ScanTarget, ShardTrees, TransparentCoin,
    TreeBounds, WalletBlock, WalletNote, WalletTransaction, decode_memo_relaxed,
};

fn read_string<R: Read>(mut reader: R) -> std::io::Result<String> {
    let str_len = reader.read_u64::<LittleEndian>()?;
    let mut str_bytes = Vec::new();
    reader.take(str_len).read_to_end(&mut str_bytes)?;
    if str_bytes.len() as u64 != str_len {
        return Err(invalid_data(format!(
            "string length {str_len} runs past the end of the input"
        )));
    }

    String::from_utf8(str_bytes).map_err(invalid_data)
}

fn write_string<W: Write>(mut writer: W, str: &str) -> std::io::Result<()> {
    writer.write_u64::<LittleEndian>(str.len() as u64)?;
    writer.write_all(str.as_bytes())
}

impl OutputId {
    /// Unversioned, like [`TxId`]: the txid followed by the output index.
    pub fn read<R: Read>(mut reader: R) -> std::io::Result<Self> {
        let txid = TxId::read(&mut reader)?;
        let output_index = reader.read_u32::<LittleEndian>()?;
        Ok(Self::new(txid, output_index))
    }

    /// Serialize into `writer`
    pub fn write<W: Write>(&self, mut writer: W) -> std::io::Result<()> {
        self.txid().write(&mut writer)?;
        writer.write_u32::<LittleEndian>(self.output_index())
    }
}

impl KeyId {
    /// Unversioned: the account id followed by the scope tag.
    pub fn read<R: Read>(mut reader: R) -> std::io::Result<Self> {
        let account_id =
            zip32::AccountId::try_from(reader.read_u32::<LittleEndian>()?).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("failed to read account id. {e}"),
                )
            })?;
        let scope = match reader.read_u8()? {
            0 => Ok(zip32::Scope::External),
            1 => Ok(zip32::Scope::Internal),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid scope value",
            )),
        }?;
        Ok(Self { account_id, scope })
    }

    /// Serialize into `writer`
    pub fn write<W: Write>(&self, mut writer: W) -> std::io::Result<()> {
        writer.write_u32::<LittleEndian>(self.account_id.into())?;
        writer.write_u8(self.scope as u8)
    }
}

impl TransparentAddressId {
    /// Unversioned: the account id, the scope tag, then the address index.
    pub fn read<R: Read>(mut reader: R) -> std::io::Result<Self> {
        let account_id =
            zip32::AccountId::try_from(reader.read_u32::<LittleEndian>()?).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("failed to read account id. {e}"),
                )
            })?;
        let scope = TransparentScope::try_from(reader.read_u8()?)?;
        let address_index = NonHardenedChildIndex::from_index(reader.read_u32::<LittleEndian>()?)
            .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "transparent address index is hardened",
            )
        })?;
        Ok(Self::new(account_id, scope, address_index))
    }

    /// Serialize into `writer`
    pub fn write<W: Write>(&self, mut writer: W) -> std::io::Result<()> {
        writer.write_u32::<LittleEndian>(self.account_id().into())?;
        writer.write_u8(self.scope() as u8)?;
        writer.write_u32::<LittleEndian>(self.address_index().index())
    }
}

impl ReadableWriteable for ScanTarget {
    const VERSION: u8 = 0;

    fn read<R: Read>(mut reader: R, _input: ()) -> std::io::Result<Self> {
        Self::get_version(&mut reader)?;
        let block_height = BlockHeight::from_u32(reader.read_u32::<LittleEndian>()?);
        let txid = TxId::read(&mut reader)?;
        let narrow_scan_area = match reader.read_u8()? {
            0 => false,
            1 => true,
            other => {
                return Err(invalid_data(format!(
                    "invalid scan target narrow scan area flag {other}"
                )));
            }
        };

        Ok(Self {
            block_height,
            txid,
            narrow_scan_area,
        })
    }

    fn write<W: Write>(&self, mut writer: W, _input: ()) -> std::io::Result<()> {
        writer.write_u8(Self::VERSION)?;
        writer.write_u32::<LittleEndian>(self.block_height.into())?;
        self.txid.write(&mut writer)?;
        writer.write_u8(u8::from(self.narrow_scan_area))
    }
}

impl ReadableWriteable for TreeBounds {
    // Version 1 appends the ironwood tree sizes.
    const VERSION: u8 = 1;

    fn read<R: Read>(mut reader: R, _input: ()) -> std::io::Result<Self> {
        let version = Self::get_version(&mut reader)?;
        let sapling_initial_tree_size = reader.read_u32::<LittleEndian>()?;
        let sapling_final_tree_size = reader.read_u32::<LittleEndian>()?;
        let orchard_initial_tree_size = reader.read_u32::<LittleEndian>()?;
        let orchard_final_tree_size = reader.read_u32::<LittleEndian>()?;
        let (ironwood_initial_tree_size, ironwood_final_tree_size) = if version >= 1 {
            (
                reader.read_u32::<LittleEndian>()?,
                reader.read_u32::<LittleEndian>()?,
            )
        } else {
            (0, 0)
        };
        for (pool, initial, last) in [
            (
                "sapling",
                sapling_initial_tree_size,
                sapling_final_tree_size,
            ),
            (
                "orchard",
                orchard_initial_tree_size,
                orchard_final_tree_size,
            ),
            (
                "ironwood",
                ironwood_initial_tree_size,
                ironwood_final_tree_size,
            ),
        ] {
            if initial > last {
                return Err(invalid_data(format!(
                    "{pool} tree shrinks from {initial} to {last} within a block"
                )));
            }
        }

        Ok(Self {
            sapling_initial_tree_size,
            sapling_final_tree_size,
            orchard_initial_tree_size,
            orchard_final_tree_size,
            ironwood_initial_tree_size,
            ironwood_final_tree_size,
        })
    }

    fn write<W: Write>(&self, mut writer: W, _input: ()) -> std::io::Result<()> {
        writer.write_u8(Self::VERSION)?;
        writer.write_u32::<LittleEndian>(self.sapling_initial_tree_size)?;
        writer.write_u32::<LittleEndian>(self.sapling_final_tree_size)?;
        writer.write_u32::<LittleEndian>(self.orchard_initial_tree_size)?;
        writer.write_u32::<LittleEndian>(self.orchard_final_tree_size)?;
        writer.write_u32::<LittleEndian>(self.ironwood_initial_tree_size)?;
        writer.write_u32::<LittleEndian>(self.ironwood_final_tree_size)
    }
}

impl ReadableWriteable for NullifierMap {
    // Version 2 appends the ironwood nullifier map.
    const VERSION: u8 = 2;

    fn read<R: Read>(mut reader: R, _input: ()) -> std::io::Result<Self> {
        let version = check_oldest_version(Self::get_version(&mut reader)?, 1, "nullifier map")?;
        let sapling = collect_unique(
            Vector::read(&mut reader, |r| {
                let mut nullifier_bytes = [0u8; 32];
                r.read_exact(&mut nullifier_bytes)?;
                let nullifier =
                    sapling_crypto::Nullifier::from_slice(&nullifier_bytes).map_err(|e| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("failed to read nullifier. {e}"),
                        )
                    })?;
                let scan_target = ScanTarget::read(r, ())?;

                Ok((nullifier, scan_target))
            })?,
            BTreeMap::len,
            "sapling nullifier",
        )?;

        let orchard = collect_unique(
            Vector::read(&mut reader, |r| {
                Ok((read_orchard_nullifier(&mut *r)?, ScanTarget::read(r, ())?))
            })?,
            BTreeMap::len,
            "orchard nullifier",
        )?;

        let ironwood = if version >= 2 {
            collect_unique(
                Vector::read(&mut reader, |r| {
                    Ok((read_orchard_nullifier(&mut *r)?, ScanTarget::read(r, ())?))
                })?,
                BTreeMap::len,
                "ironwood nullifier",
            )?
        } else {
            BTreeMap::new()
        };

        Ok(NullifierMap {
            sapling,
            orchard,
            ironwood,
        })
    }

    fn write<W: Write>(&self, mut writer: W, _input: ()) -> std::io::Result<()> {
        writer.write_u8(Self::VERSION)?;
        Vector::write(
            &mut writer,
            &self.sapling.iter().collect::<Vec<_>>(),
            |w, &(&nullifier, &scan_target)| {
                w.write_all(nullifier.as_ref())?;
                scan_target.write(w, ())
            },
        )?;
        Vector::write(
            &mut writer,
            &self.orchard.iter().collect::<Vec<_>>(),
            |w, &(&nullifier, &scan_target)| {
                w.write_all(&nullifier.to_bytes())?;
                scan_target.write(w, ())
            },
        )?;
        Vector::write(
            &mut writer,
            &self.ironwood.iter().collect::<Vec<_>>(),
            |w, &(&nullifier, &scan_target)| {
                w.write_all(&nullifier.to_bytes())?;
                scan_target.write(w, ())
            },
        )
    }
}

impl ReadableWriteable for WalletBlock {
    const VERSION: u8 = 0;

    fn read<R: Read>(mut reader: R, _input: ()) -> std::io::Result<Self> {
        Self::get_version(&mut reader)?;
        let block_height = BlockHeight::from_u32(reader.read_u32::<LittleEndian>()?);
        let mut block_hash = BlockHash([0u8; 32]);
        reader.read_exact(&mut block_hash.0)?;
        let mut prev_hash = BlockHash([0u8; 32]);
        reader.read_exact(&mut prev_hash.0)?;
        let time = reader.read_u32::<LittleEndian>()?;
        let txids = Vector::read(&mut reader, |r| TxId::read(r))?;
        let tree_bounds = TreeBounds::read(&mut reader, ())?;

        Ok(Self {
            block_height,
            block_hash,
            prev_hash,
            time,
            txids,
            tree_bounds,
        })
    }

    fn write<W: Write>(&self, mut writer: W, _input: ()) -> std::io::Result<()> {
        writer.write_u8(Self::VERSION)?;
        writer.write_u32::<LittleEndian>(self.block_height.into())?;
        writer.write_all(&self.block_hash.0)?;
        writer.write_all(&self.prev_hash.0)?;
        writer.write_u32::<LittleEndian>(self.time)?;
        Vector::write(&mut writer, self.txids(), |w, txid| txid.write(w))?;
        self.tree_bounds.write(&mut writer, ())
    }
}

impl<P: consensus::Parameters> ReadableWriteable<&P, &P> for WalletTransaction {
    // Version 1 appends the ironwood note collections.
    const VERSION: u8 = 1;

    fn read<R: Read>(mut reader: R, consensus_parameters: &P) -> std::io::Result<Self> {
        let version = <Self as ReadableWriteable<&P, &P>>::get_version(&mut reader)?;
        let txid = TxId::read(&mut reader)?;
        let status = ConfirmationStatus::read(&mut reader, ())?;
        let transaction = Transaction::read(
            &mut reader,
            consensus::BranchId::for_height(consensus_parameters, status.get_height()),
        )?;
        let datetime = reader.read_u32::<LittleEndian>()?;
        let transparent_coins = Vector::read(&mut reader, |r| TransparentCoin::read(r, ()))?;
        let sapling_notes = Vector::read(&mut reader, |r| SaplingNote::read(r, ()))?;
        let orchard_notes = Vector::read(&mut reader, |r| OrchardNote::read(r, ()))?;
        let outgoing_sapling_notes = Vector::read(&mut reader, |r| {
            OutgoingSaplingNote::read(r, consensus_parameters)
        })?;
        let outgoing_orchard_notes = Vector::read(&mut reader, |r| {
            OutgoingOrchardNote::read(r, consensus_parameters)
        })?;
        let (ironwood_notes, outgoing_ironwood_notes) = if version >= 1 {
            (
                Vector::read(&mut reader, |r| IronwoodNote::read(r, ()))?,
                Vector::read(&mut reader, |r| {
                    OutgoingIronwoodNote::read(r, consensus_parameters)
                })?,
            )
        } else {
            (Vec::new(), Vec::new())
        };

        Ok(Self {
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
        })
    }

    fn write<W: Write>(&self, mut writer: W, consensus_parameters: &P) -> std::io::Result<()> {
        writer.write_u8(<Self as ReadableWriteable<&P, &P>>::VERSION)?;
        self.txid.write(&mut writer)?;
        self.status.write(&mut writer, ())?;
        self.transaction.write(&mut writer)?;
        writer.write_u32::<LittleEndian>(self.datetime)?;
        Vector::write(&mut writer, self.transparent_coins(), |w, output| {
            output.write(w, ())
        })?;
        Vector::write(&mut writer, self.sapling_notes(), |w, output| {
            output.write(w, ())
        })?;
        Vector::write(&mut writer, self.orchard_notes(), |w, output| {
            output.write(w, ())
        })?;
        Vector::write(&mut writer, self.outgoing_sapling_notes(), |w, output| {
            output.write(w, consensus_parameters)
        })?;
        Vector::write(&mut writer, self.outgoing_orchard_notes(), |w, output| {
            output.write(w, consensus_parameters)
        })?;
        Vector::write(&mut writer, self.ironwood_notes(), |w, output| {
            output.write(w, ())
        })?;
        Vector::write(&mut writer, self.outgoing_ironwood_notes(), |w, output| {
            output.write(w, consensus_parameters)
        })
    }
}

impl ReadableWriteable for TransparentCoin {
    const VERSION: u8 = 1;

    fn read<R: Read>(mut reader: R, _input: ()) -> std::io::Result<Self> {
        check_oldest_version(
            Self::get_version(&mut reader)?,
            Self::VERSION,
            "transparent coin",
        )?;

        let output_id = OutputId::read(&mut reader)?;

        let key_id = TransparentAddressId::read(&mut reader)?;

        let address = read_string(&mut reader)?;
        let script = Script::read(&mut reader)?;
        let value = Zatoshis::from_u64(reader.read_u64::<LittleEndian>()?)
            .map_err(|e| invalid_data(format!("invalid transparent coin value. {e}")))?;
        let spending_transaction = Optional::read(&mut reader, TxId::read)?;

        Ok(Self {
            output_id,
            key_id,
            address,
            value,
            script,
            spending_transaction,
        })
    }

    fn write<W: Write>(&self, mut writer: W, _input: ()) -> std::io::Result<()> {
        writer.write_u8(Self::VERSION)?;

        self.output_id.write(&mut writer)?;

        self.key_id.write(&mut writer)?;

        write_string(&mut writer, &self.address)?;
        self.script.write(&mut writer)?;
        writer.write_u64::<LittleEndian>(self.value())?;
        Optional::write(&mut writer, self.spending_transaction, |w, txid| {
            txid.write(w)
        })?;

        Ok(())
    }
}

const WALLET_NOTE_VERSION: u8 = 2;

fn read_orchard_nullifier<R: Read>(mut reader: R) -> std::io::Result<orchard::note::Nullifier> {
    let mut nullifier_bytes = [0u8; 32];
    reader.read_exact(&mut nullifier_bytes)?;
    Option::from(orchard::note::Nullifier::from_bytes(&nullifier_bytes))
        .ok_or_else(|| invalid_data("orchard nullifier is not a canonical field element"))
}

fn read_sapling_rseed<R: Read>(mut reader: R) -> std::io::Result<sapling_crypto::Rseed> {
    let rseed_zip212 = reader.read_u8()?;
    let mut rseed_bytes = [0u8; 32];
    reader.read_exact(&mut rseed_bytes)?;
    match rseed_zip212 {
        0 => Option::from(jubjub::Fr::from_bytes(&rseed_bytes))
            .map(sapling_crypto::Rseed::BeforeZip212)
            .ok_or_else(|| invalid_data("sapling rseed is not a canonical scalar")),
        1 => Ok(sapling_crypto::Rseed::AfterZip212(rseed_bytes)),
        _ => Err(invalid_data("invalid rseed zip212 byte")),
    }
}

/// Reads the recipient, value, rho and rseed of an Orchard-protocol note and checks that they form a
/// valid note.
fn read_orchard_protocol_note_parts<R: Read>(
    mut reader: R,
    note_version: orchard::note::NoteVersion,
) -> std::io::Result<orchard::Note> {
    let mut address_bytes = [0u8; 43];
    reader.read_exact(&mut address_bytes)?;
    let recipient = Option::from(orchard::Address::from_raw_address_bytes(&address_bytes))
        .ok_or_else(|| invalid_data("invalid orchard recipient address"))?;
    let value = orchard::value::NoteValue::from_raw(reader.read_u64::<LittleEndian>()?);
    let mut rho_bytes = [0u8; 32];
    reader.read_exact(&mut rho_bytes)?;
    let rho = Option::from(orchard::note::Rho::from_bytes(&rho_bytes))
        .ok_or_else(|| invalid_data("orchard rho is not a canonical field element"))?;
    let mut rseed_bytes = [0u8; 32];
    reader.read_exact(&mut rseed_bytes)?;
    let rseed = Option::from(orchard::note::RandomSeed::from_bytes(rseed_bytes, &rho))
        .ok_or_else(|| invalid_data("invalid orchard random seed"))?;

    Option::from(orchard::note::Note::from_parts(
        recipient,
        value,
        rho,
        rseed,
        note_version,
    ))
    .ok_or_else(|| invalid_data("orchard note has no valid commitment"))
}

fn read_refetch_nullifier_ranges(
    reader: &mut impl Read,
) -> std::io::Result<Vec<Range<BlockHeight>>> {
    Vector::read(reader, |r| {
        let start = r.read_u32::<LittleEndian>()?;
        let end = r.read_u32::<LittleEndian>()?;
        if start > end {
            return Err(invalid_data(format!(
                "refetch nullifier range {start}..{end} ends before it starts"
            )));
        }
        Ok(BlockHeight::from_u32(start)..BlockHeight::from_u32(end))
    })
}

fn write_refetch_nullifier_ranges(
    writer: &mut impl Write,
    ranges: &[Range<BlockHeight>],
) -> std::io::Result<()> {
    Vector::write(writer, ranges, |w, range| {
        w.write_u32::<LittleEndian>(range.start.into())?;
        w.write_u32::<LittleEndian>(range.end.into())
    })
}

impl ReadableWriteable for SaplingNote {
    const VERSION: u8 = WALLET_NOTE_VERSION;

    fn read<R: Read>(mut reader: R, _input: ()) -> std::io::Result<Self> {
        check_oldest_version(
            Self::get_version(&mut reader)?,
            Self::VERSION,
            "sapling note",
        )?;

        let output_id = OutputId::read(&mut reader)?;

        let key_id = KeyId::read(&mut reader)?;

        let mut address_bytes = [0u8; 43];
        reader.read_exact(&mut address_bytes)?;
        let recipient =
            sapling_crypto::PaymentAddress::from_bytes(&address_bytes).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "failed to read payment address",
                )
            })?;
        let value = sapling_crypto::value::NoteValue::from_raw(reader.read_u64::<LittleEndian>()?);
        let rseed = read_sapling_rseed(&mut reader)?;

        let nullifier = Optional::read(&mut reader, |r| {
            let mut nullifier_bytes = [0u8; 32];
            r.read_exact(&mut nullifier_bytes)?;

            sapling_crypto::Nullifier::from_slice(&nullifier_bytes).map_err(|e| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("failed to read nullifier. {e}"),
                )
            })
        })?;
        let position = Optional::read(&mut reader, |r| {
            Ok(Position::from(r.read_u64::<LittleEndian>()?))
        })?;
        let mut memo_bytes = [0u8; 512];
        reader.read_exact(&mut memo_bytes)?;
        let memo = decode_memo_relaxed(&memo_bytes).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("failed to read memo. {e}"),
            )
        })?;

        let spending_transaction = Optional::read(&mut reader, TxId::read)?;
        let refetch_nullifier_ranges = read_refetch_nullifier_ranges(&mut reader)?;

        Ok(Self {
            output_id,
            key_id,
            note: sapling_crypto::Note::from_parts(recipient, value, rseed),
            nullifier,
            position,
            memo,
            spending_transaction,
            refetch_nullifier_ranges,
            marker: std::marker::PhantomData,
        })
    }

    fn write<W: Write>(&self, mut writer: W, _input: ()) -> std::io::Result<()> {
        writer.write_u8(Self::VERSION)?;

        self.output_id.write(&mut writer)?;

        self.key_id.write(&mut writer)?;

        writer.write_all(&self.note.recipient().to_bytes())?;
        writer.write_u64::<LittleEndian>(self.value())?;
        match self.note.rseed() {
            sapling_crypto::Rseed::BeforeZip212(fr) => {
                writer.write_u8(0)?;
                writer.write_all(&fr.to_bytes())?;
            }
            sapling_crypto::Rseed::AfterZip212(bytes) => {
                writer.write_u8(1)?;
                writer.write_all(bytes)?;
            }
        }

        Optional::write(&mut writer, self.nullifier, |w, nullifier| {
            w.write_all(nullifier.as_ref())
        })?;
        Optional::write(&mut writer, self.position, |w, position| {
            w.write_u64::<LittleEndian>(position.into())
        })?;
        writer.write_all(self.memo.encode().as_array())?;

        Optional::write(&mut writer, self.spending_transaction, |w, txid| {
            txid.write(w)
        })?;

        write_refetch_nullifier_ranges(&mut writer, &self.refetch_nullifier_ranges)
    }
}

/// Shared reader for the Orchard-protocol note layout. Orchard and Ironwood
/// notes serialize identically, differing only in the note version fixed at
/// construction.
fn read_orchard_protocol_note<R: Read, P>(
    mut reader: R,
    note_version: orchard::note::NoteVersion,
) -> std::io::Result<WalletNote<orchard::Note, orchard::note::Nullifier, P>> {
    let output_id = OutputId::read(&mut reader)?;

    let key_id = KeyId::read(&mut reader)?;

    let note = read_orchard_protocol_note_parts(&mut reader, note_version)?;

    let nullifier = Optional::read(&mut reader, read_orchard_nullifier)?;
    let position = Optional::read(&mut reader, |r| {
        Ok(Position::from(r.read_u64::<LittleEndian>()?))
    })?;
    let mut memo_bytes = [0u8; 512];
    reader.read_exact(&mut memo_bytes)?;
    let memo = decode_memo_relaxed(&memo_bytes).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("failed to read memo. {e}"),
        )
    })?;

    let spending_transaction = Optional::read(&mut reader, TxId::read)?;
    let refetch_nullifier_ranges = read_refetch_nullifier_ranges(&mut reader)?;

    Ok(WalletNote {
        output_id,
        key_id,
        note,
        nullifier,
        position,
        memo,
        spending_transaction,
        refetch_nullifier_ranges,
        marker: std::marker::PhantomData,
    })
}

/// Shared writer for the Orchard-protocol note layout.
fn write_orchard_protocol_note<W: Write, P>(
    note: &WalletNote<orchard::Note, orchard::note::Nullifier, P>,
    mut writer: W,
) -> std::io::Result<()> {
    note.output_id.write(&mut writer)?;

    note.key_id.write(&mut writer)?;

    writer.write_all(&note.note.recipient().to_raw_address_bytes())?;
    writer.write_u64::<LittleEndian>(note.note.value().inner())?;
    writer.write_all(&note.note.rho().to_bytes())?;
    writer.write_all(note.note.rseed().as_bytes())?;

    Optional::write(&mut writer, note.nullifier, |w, nullifier| {
        w.write_all(&nullifier.to_bytes())
    })?;
    Optional::write(&mut writer, note.position, |w, position| {
        w.write_u64::<LittleEndian>(position.into())
    })?;
    writer.write_all(note.memo.encode().as_array())?;
    Optional::write(&mut writer, note.spending_transaction, |w, txid| {
        txid.write(w)
    })?;

    write_refetch_nullifier_ranges(&mut writer, &note.refetch_nullifier_ranges)
}

impl ReadableWriteable for OrchardNote {
    const VERSION: u8 = WALLET_NOTE_VERSION;

    fn read<R: Read>(mut reader: R, _input: ()) -> std::io::Result<Self> {
        check_oldest_version(
            Self::get_version(&mut reader)?,
            Self::VERSION,
            "orchard note",
        )?;
        read_orchard_protocol_note(reader, orchard::note::NoteVersion::V2)
    }

    fn write<W: Write>(&self, mut writer: W, _input: ()) -> std::io::Result<()> {
        writer.write_u8(Self::VERSION)?;
        write_orchard_protocol_note(self, writer)
    }
}

impl ReadableWriteable for IronwoodNote {
    const VERSION: u8 = WALLET_NOTE_VERSION;

    fn read<R: Read>(mut reader: R, _input: ()) -> std::io::Result<Self> {
        check_oldest_version(
            Self::get_version(&mut reader)?,
            Self::VERSION,
            "ironwood note",
        )?;
        read_orchard_protocol_note(reader, orchard::note::NoteVersion::V3)
    }

    fn write<W: Write>(&self, mut writer: W, _input: ()) -> std::io::Result<()> {
        writer.write_u8(Self::VERSION)?;
        write_orchard_protocol_note(self, writer)
    }
}

const OUTGOING_NOTE_VERSION: u8 = 1;

impl<P: consensus::Parameters> ReadableWriteable<&P, &P> for OutgoingSaplingNote {
    const VERSION: u8 = OUTGOING_NOTE_VERSION;

    fn read<R: Read>(mut reader: R, consensus_parameters: &P) -> std::io::Result<Self> {
        check_oldest_version(
            <Self as ReadableWriteable<&P, &P>>::get_version(&mut reader)?,
            OUTGOING_NOTE_VERSION,
            "outgoing sapling note",
        )?;

        let output_id = OutputId::read(&mut reader)?;

        let key_id = KeyId::read(&mut reader)?;

        let mut address_bytes = [0u8; 43];
        reader.read_exact(&mut address_bytes)?;
        let recipient =
            sapling_crypto::PaymentAddress::from_bytes(&address_bytes).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "failed to read payment address",
                )
            })?;
        let value = sapling_crypto::value::NoteValue::from_raw(reader.read_u64::<LittleEndian>()?);
        let rseed = read_sapling_rseed(&mut reader)?;

        let mut memo_bytes = [0u8; 512];
        reader.read_exact(&mut memo_bytes)?;
        let memo = decode_memo_relaxed(&memo_bytes).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("failed to read memo. {e}"),
            )
        })?;

        let recipient_unified_address = Optional::read(&mut reader, |r| {
            let encoded_address = read_string(r)?;

            decode_unified_address(consensus_parameters, &encoded_address)
        })?;

        Ok(Self {
            output_id,
            key_id,
            note: sapling_crypto::Note::from_parts(recipient, value, rseed),
            memo,
            recipient_full_unified_address: recipient_unified_address,
            marker: std::marker::PhantomData,
        })
    }

    fn write<W: Write>(&self, mut writer: W, consensus_parameters: &P) -> std::io::Result<()> {
        writer.write_u8(<Self as ReadableWriteable<&P, &P>>::VERSION)?;

        self.output_id.write(&mut writer)?;

        self.key_id.write(&mut writer)?;

        writer.write_all(&self.note.recipient().to_bytes())?;
        writer.write_u64::<LittleEndian>(self.value())?;
        match self.note.rseed() {
            sapling_crypto::Rseed::BeforeZip212(fr) => {
                writer.write_u8(0)?;
                writer.write_all(&fr.to_bytes())?;
            }
            sapling_crypto::Rseed::AfterZip212(bytes) => {
                writer.write_u8(1)?;
                writer.write_all(bytes)?;
            }
        }

        writer.write_all(self.memo.encode().as_array())?;
        Optional::write(
            &mut writer,
            self.recipient_full_unified_address.as_ref(),
            |w, unified_address| write_string(w, &unified_address.encode(consensus_parameters)),
        )?;

        Ok(())
    }
}

/// Shared reader for the Orchard-protocol outgoing note layout. Orchard and
/// Ironwood outgoing notes serialize identically, differing only in the note
/// version fixed at construction.
fn read_orchard_protocol_outgoing_note<R: Read, P>(
    mut reader: R,
    consensus_parameters: &impl consensus::Parameters,
    note_version: orchard::note::NoteVersion,
) -> std::io::Result<OutgoingNote<orchard::Note, P>> {
    let output_id = OutputId::read(&mut reader)?;

    let key_id = KeyId::read(&mut reader)?;

    let note = read_orchard_protocol_note_parts(&mut reader, note_version)?;

    let mut memo_bytes = [0u8; 512];
    reader.read_exact(&mut memo_bytes)?;
    let memo = decode_memo_relaxed(&memo_bytes).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("failed to read memo. {e}"),
        )
    })?;

    let recipient_unified_address = Optional::read(&mut reader, |r| {
        let encoded_address = read_string(r)?;

        decode_unified_address(consensus_parameters, &encoded_address)
    })?;

    Ok(OutgoingNote {
        output_id,
        key_id,
        note,
        memo,
        recipient_full_unified_address: recipient_unified_address,
        marker: std::marker::PhantomData,
    })
}

/// Shared writer for the Orchard-protocol outgoing note layout.
fn write_orchard_protocol_outgoing_note<W: Write, P>(
    note: &OutgoingNote<orchard::Note, P>,
    mut writer: W,
    consensus_parameters: &impl consensus::Parameters,
) -> std::io::Result<()> {
    note.output_id.write(&mut writer)?;

    note.key_id.write(&mut writer)?;

    writer.write_all(&note.note.recipient().to_raw_address_bytes())?;
    writer.write_u64::<LittleEndian>(note.note.value().inner())?;
    writer.write_all(&note.note.rho().to_bytes())?;
    writer.write_all(note.note.rseed().as_bytes())?;

    writer.write_all(note.memo.encode().as_array())?;
    Optional::write(
        &mut writer,
        note.recipient_full_unified_address.as_ref(),
        |w, unified_address| write_string(w, &unified_address.encode(consensus_parameters)),
    )?;

    Ok(())
}

impl<P: consensus::Parameters> ReadableWriteable<&P, &P> for OutgoingOrchardNote {
    const VERSION: u8 = OUTGOING_NOTE_VERSION;

    fn read<R: Read>(mut reader: R, consensus_parameters: &P) -> std::io::Result<Self> {
        check_oldest_version(
            <Self as ReadableWriteable<&P, &P>>::get_version(&mut reader)?,
            OUTGOING_NOTE_VERSION,
            "outgoing orchard note",
        )?;
        read_orchard_protocol_outgoing_note(
            reader,
            consensus_parameters,
            orchard::note::NoteVersion::V2,
        )
    }

    fn write<W: Write>(&self, mut writer: W, consensus_parameters: &P) -> std::io::Result<()> {
        writer.write_u8(<Self as ReadableWriteable<&P, &P>>::VERSION)?;
        write_orchard_protocol_outgoing_note(self, writer, consensus_parameters)
    }
}

impl<P: consensus::Parameters> ReadableWriteable<&P, &P> for OutgoingIronwoodNote {
    const VERSION: u8 = OUTGOING_NOTE_VERSION;

    fn read<R: Read>(mut reader: R, consensus_parameters: &P) -> std::io::Result<Self> {
        check_oldest_version(
            <Self as ReadableWriteable<&P, &P>>::get_version(&mut reader)?,
            OUTGOING_NOTE_VERSION,
            "outgoing ironwood note",
        )?;
        read_orchard_protocol_outgoing_note(
            reader,
            consensus_parameters,
            orchard::note::NoteVersion::V3,
        )
    }

    fn write<W: Write>(&self, mut writer: W, consensus_parameters: &P) -> std::io::Result<()> {
        writer.write_u8(<Self as ReadableWriteable<&P, &P>>::VERSION)?;
        write_orchard_protocol_outgoing_note(self, writer, consensus_parameters)
    }
}

const SHARD_SER_V1: u8 = 1;
const SHARD_NIL_TAG: u8 = 0;
const SHARD_LEAF_TAG: u8 = 1;
const SHARD_PARENT_TAG: u8 = 2;

/// Same layout as [`zcash_client_backend::serialization::shardtree::read_shard`], but parent nodes
/// may nest at most `levels` deep. The tree rooted `levels` above the leaves cannot hold anything
/// deeper, and without the bound a run of parent tags in a corrupt file recurses until the stack
/// overflows.
fn read_bounded_shard<H: HashSer, R: Read>(
    mut reader: R,
    levels: u8,
) -> std::io::Result<PrunableTree<H>> {
    fn read_node<H: HashSer, R: Read>(
        reader: &mut R,
        levels: u8,
    ) -> std::io::Result<PrunableTree<H>> {
        match reader.read_u8()? {
            SHARD_PARENT_TAG => {
                let levels = levels
                    .checked_sub(1)
                    .ok_or_else(|| invalid_data("shard parent node sits below the leaf level"))?;
                let ann = Optional::read(&mut *reader, <H as HashSer>::read)?.map(Arc::new);
                let left = read_node(reader, levels)?;
                let right = read_node(reader, levels)?;
                Ok(Tree::parent(ann, left, right))
            }
            SHARD_LEAF_TAG => {
                let value = <H as HashSer>::read(&mut *reader)?;
                let bits = reader.read_u8()?;
                let flags = RetentionFlags::from_bits(bits).ok_or_else(|| {
                    invalid_data(format!("invalid shard leaf retention flags {bits}"))
                })?;
                Ok(Tree::leaf((value, flags)))
            }
            SHARD_NIL_TAG => Ok(Tree::empty()),
            other => Err(invalid_data(format!("unknown shard node tag {other}"))),
        }
    }

    match reader.read_u8()? {
        SHARD_SER_V1 => read_node(&mut reader, levels),
        other => Err(invalid_data(format!(
            "unknown shard serialization version {other}"
        ))),
    }
}

impl ReadableWriteable for ShardTrees {
    // Version 1 appends the Ironwood shard tree after the Orchard one.
    const VERSION: u8 = 1;

    fn read<R: Read>(mut reader: R, _input: ()) -> std::io::Result<Self> {
        let version = Self::get_version(&mut reader)?;
        let sapling = Self::read_shardtree(&mut reader)?;
        let orchard = Self::read_shardtree(&mut reader)?;
        let ironwood = if version >= 1 {
            Self::read_shardtree(&mut reader)?
        } else {
            // Pre-Ironwood wallet files: start with an empty Ironwood tree.
            Self::new().ironwood
        };

        Ok(Self {
            sapling,
            orchard,
            ironwood,
        })
    }

    fn write<W: Write>(&self, mut writer: W, _input: ()) -> std::io::Result<()> {
        writer.write_u8(Self::VERSION)?;
        Self::write_shardtree(&mut writer, &self.sapling)?;
        Self::write_shardtree(&mut writer, &self.orchard)?;
        Self::write_shardtree(&mut writer, &self.ironwood)?;

        Ok(())
    }
}

impl ShardTrees {
    fn read_shardtree<
        H: Hashable + Clone + HashSer + Eq,
        C: Ord + std::fmt::Debug + Copy + From<u32>,
        S: ShardStore<H = H, CheckpointId = C> + From<MemoryShardStore<H, C>>,
        R: Read,
        const DEPTH: u8,
        const SHARD_HEIGHT: u8,
    >(
        mut reader: R,
    ) -> std::io::Result<ShardTree<S, DEPTH, SHARD_HEIGHT>> {
        let shards = Vector::read(&mut reader, |r| {
            let level = Level::from(r.read_u8()?);
            let index = r.read_u64::<LittleEndian>()?;
            let root_addr = incrementalmerkletree::Address::from_parts(level, index);
            let shard = read_bounded_shard(r, SHARD_HEIGHT)?;

            LocatedPrunableTree::from_parts(root_addr, shard).map_err(|addr| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("parent node in root has level 0 relative to root address: {addr:?}"),
                )
            })
        })?;
        let mut store = MemoryShardStore::empty();
        for (position, shard) in shards.into_iter().enumerate() {
            let root_addr = shard.root_addr();
            if root_addr.level() != Level::from(SHARD_HEIGHT)
                || root_addr.index() != position as u64
                || root_addr.index() >= 1 << (DEPTH - SHARD_HEIGHT)
            {
                return Err(invalid_data(format!(
                    "shard {position} has root address {root_addr:?}"
                )));
            }
            let Ok(()) = store.put_shard(shard);
        }
        let checkpoints = Vector::read(&mut reader, |r| {
            let checkpoint_id = C::from(r.read_u32::<LittleEndian>()?);
            let tree_state = match r.read_u8()? {
                0 => TreeState::Empty,
                1 => TreeState::AtPosition(Position::from(r.read_u64::<LittleEndian>()?)),
                otherwise => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "failed to read TreeState. expected boolean value, found {otherwise}"
                        ),
                    ));
                }
            };
            let marks_removed = collect_unique(
                Vector::read(r, |r| r.read_u64::<LittleEndian>().map(Position::from))?,
                BTreeSet::len,
                "checkpoint removed mark",
            )?;
            Ok((
                checkpoint_id,
                Checkpoint::from_parts(tree_state, marks_removed),
            ))
        })?;
        if checkpoints.len() > SHARDTREE_CHECKPOINT_ROLLING_WINDOW_SIZE as usize {
            return Err(invalid_data(format!(
                "{} checkpoints exceed the {SHARDTREE_CHECKPOINT_ROLLING_WINDOW_SIZE} a wallet file keeps",
                checkpoints.len()
            )));
        }
        if let Some(pair) = checkpoints.windows(2).find(|pair| pair[0].0 >= pair[1].0) {
            return Err(invalid_data(format!(
                "checkpoint {:?} follows checkpoint {:?}",
                pair[1].0, pair[0].0
            )));
        }
        for (checkpoint_id, checkpoint) in checkpoints {
            let Ok(()) = store.add_checkpoint(checkpoint_id, checkpoint);
        }
        let Ok(()) = store.put_cap(read_bounded_shard(reader, DEPTH)?);

        Ok(shardtree::ShardTree::new(
            S::from(store),
            SHARDTREE_CHECKPOINT_ROLLING_WINDOW_SIZE as usize,
        ))
    }

    /// Write memory-backed shardstore, represented tree.
    fn write_shardtree<
        H: Hashable + Clone + Eq + HashSer,
        C: Ord + std::fmt::Debug + Copy,
        S: ShardStore<H = H, CheckpointId = C>,
        W: Write,
        const DEPTH: u8,
        const SHARD_HEIGHT: u8,
    >(
        mut writer: W,
        shardtree: &ShardTree<S, DEPTH, SHARD_HEIGHT>,
    ) -> std::io::Result<()>
    where
        u32: From<C>,
    {
        fn write_shards<W, H, S>(mut writer: W, store: &S) -> std::io::Result<()>
        where
            H: Hashable + Clone + Eq + HashSer,
            S: ShardStore<H = H>,
            W: Write,
        {
            let roots = store.get_shard_roots().expect("Infallible");
            Vector::write(&mut writer, &roots, |w, root| {
                w.write_u8(root.level().into())?;
                w.write_u64::<LittleEndian>(root.index())?;
                let shard = store
                    .get_shard(*root)
                    .expect("Infallible")
                    .expect("cannot find root that shard store claims to have");
                write_shard(w, shard.root())
            })
        }

        fn write_checkpoints<W, Cid>(
            mut writer: W,
            checkpoints: &[(Cid, Checkpoint)],
        ) -> std::io::Result<()>
        where
            W: Write,
            Cid: Ord + std::fmt::Debug + Copy,
            u32: From<Cid>,
        {
            Vector::write(
                &mut writer,
                checkpoints,
                |mut w, (checkpoint_id, checkpoint)| {
                    w.write_u32::<LittleEndian>(u32::from(*checkpoint_id))?;
                    match checkpoint.tree_state() {
                        shardtree::store::TreeState::Empty => w.write_u8(0),
                        shardtree::store::TreeState::AtPosition(pos) => {
                            w.write_u8(1)?;
                            w.write_u64::<LittleEndian>(<u64 as From<Position>>::from(pos))
                        }
                    }?;
                    Vector::write(
                        &mut w,
                        &checkpoint.marks_removed().iter().collect::<Vec<_>>(),
                        |w, mark| {
                            w.write_u64::<LittleEndian>(<u64 as From<Position>>::from(**mark))
                        },
                    )
                },
            )
        }

        let store = shardtree.store();
        write_shards(&mut writer, store)?;

        let mut checkpoints = Vec::new();
        let checkpoint_count = store.checkpoint_count().expect("Infallible");
        store
            .for_each_checkpoint(checkpoint_count, |checkpoint_id, checkpoint| {
                checkpoints.push((*checkpoint_id, checkpoint.clone()));
                Ok(())
            })
            .expect("Infallible");
        if checkpoints.len() > SHARDTREE_CHECKPOINT_ROLLING_WINDOW_SIZE as usize {
            let keep_from = checkpoints.len() - SHARDTREE_CHECKPOINT_ROLLING_WINDOW_SIZE as usize;
            checkpoints.drain(..keep_from);
        }
        write_checkpoints(&mut writer, &checkpoints)?;

        write_shard(&mut writer, &store.get_cap().expect("Infallible"))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: build a minimal v1 NullifierMap byte blob (no ironwood BTreeMap).
    fn v1_nullifier_map_bytes() -> Vec<u8> {
        let mut out = Vec::new();
        out.write_u8(1).unwrap();
        Vector::write(&mut out, &[] as &[()], |_, _| Ok(())).unwrap();
        Vector::write(&mut out, &[] as &[()], |_, _| Ok(())).unwrap();
        out
    }

    #[test]
    fn nullifier_map_v1_reads_with_empty_ironwood_map() {
        let bytes = v1_nullifier_map_bytes();
        let map = NullifierMap::read(bytes.as_slice(), ()).expect("v1 should read cleanly");
        assert!(map.ironwood.is_empty());
    }

    #[test]
    fn nullifier_map_v2_roundtrip_preserves_ironwood() {
        let map = NullifierMap::new();
        let mut bytes = Vec::new();
        map.write(&mut bytes, ()).expect("write should succeed");
        let recovered = NullifierMap::read(bytes.as_slice(), ()).expect("read should succeed");
        assert!(recovered.ironwood.is_empty());
    }

    // Helper: build a v0 TreeBounds byte blob (no ironwood tree sizes).
    fn v0_tree_bounds_bytes(
        sapling_initial: u32,
        sapling_final: u32,
        orchard_initial: u32,
        orchard_final: u32,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        out.write_u8(0).unwrap();
        out.write_u32::<LittleEndian>(sapling_initial).unwrap();
        out.write_u32::<LittleEndian>(sapling_final).unwrap();
        out.write_u32::<LittleEndian>(orchard_initial).unwrap();
        out.write_u32::<LittleEndian>(orchard_final).unwrap();
        out
    }

    #[test]
    fn tree_bounds_v0_reads_with_zero_ironwood_sizes() {
        let bytes = v0_tree_bounds_bytes(10, 20, 30, 40);
        let bounds = TreeBounds::read(bytes.as_slice(), ()).expect("v0 should read cleanly");
        assert_eq!(bounds.sapling_initial_tree_size, 10);
        assert_eq!(bounds.orchard_final_tree_size, 40);
        assert_eq!(bounds.ironwood_initial_tree_size, 0);
        assert_eq!(bounds.ironwood_final_tree_size, 0);
    }

    #[test]
    fn tree_bounds_v1_roundtrip() {
        let bounds = TreeBounds {
            sapling_initial_tree_size: 1,
            sapling_final_tree_size: 2,
            orchard_initial_tree_size: 3,
            orchard_final_tree_size: 4,
            ironwood_initial_tree_size: 5,
            ironwood_final_tree_size: 6,
        };
        let mut bytes = Vec::new();
        bounds.write(&mut bytes, ()).expect("write should succeed");
        let recovered = TreeBounds::read(bytes.as_slice(), ()).expect("read should succeed");
        assert_eq!(recovered.ironwood_initial_tree_size, 5);
        assert_eq!(recovered.ironwood_final_tree_size, 6);
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
    fn scan_target_rejects_a_flag_other_than_zero_or_one() {
        let mut bytes = scan_target_bytes(500);
        *bytes.last_mut().unwrap() = 2;
        assert!(ScanTarget::read(bytes.as_slice(), ()).is_err());
    }

    #[test]
    fn string_length_past_the_input_is_an_error_not_an_allocation() {
        let bytes = [u64::MAX.to_le_bytes().as_slice(), b"zs"].concat();
        assert!(read_string(bytes.as_slice()).is_err());
    }

    #[test]
    fn tree_bounds_reject_a_tree_that_shrinks() {
        let bytes = v0_tree_bounds_bytes(5, 4, 0, 0);
        assert!(TreeBounds::read(bytes.as_slice(), ()).is_err());
    }

    #[test]
    fn nullifier_map_before_version_1_is_rejected() {
        let mut bytes = v1_nullifier_map_bytes();
        bytes[0] = 0;
        assert!(NullifierMap::read(bytes.as_slice(), ()).is_err());
    }

    #[test]
    fn nullifier_map_rejects_a_non_canonical_orchard_nullifier() {
        for position in [2, 3] {
            let mut bytes = vec![2, 0, 0, 0];
            bytes[position] = 1;
            let entry = [[0xff; 32].as_slice(), &scan_target_bytes(500)].concat();
            bytes.splice(position + 1..position + 1, entry);
            assert!(NullifierMap::read(bytes.as_slice(), ()).is_err());
        }
    }

    #[test]
    fn nullifier_map_rejects_a_repeated_nullifier() {
        let entry = [[1; 32].as_slice(), &scan_target_bytes(500)].concat();
        let bytes = [&[1, 2][..], &entry, &entry, &[0]].concat();
        assert!(NullifierMap::read(bytes.as_slice(), ()).is_err());
    }

    fn transparent_coin_bytes(version: u8, address_len: u64, value: u64) -> Vec<u8> {
        let mut out = vec![version];
        OutputId::new(TxId::from_bytes([1; 32]), 0)
            .write(&mut out)
            .unwrap();
        TransparentAddressId::new(
            zip32::AccountId::ZERO,
            TransparentScope::External,
            NonHardenedChildIndex::ZERO,
        )
        .write(&mut out)
        .unwrap();
        out.write_u64::<LittleEndian>(address_len).unwrap();
        out.push(0);
        out.write_u64::<LittleEndian>(value).unwrap();
        out.push(0);
        out
    }

    #[test]
    fn transparent_coin_reads_when_well_formed() {
        let bytes = transparent_coin_bytes(1, 0, 5);
        assert!(TransparentCoin::read(bytes.as_slice(), ()).is_ok());
    }

    #[test]
    fn transparent_coin_rejects_a_value_above_max_money() {
        let bytes = transparent_coin_bytes(1, 0, u64::MAX);
        assert!(TransparentCoin::read(bytes.as_slice(), ()).is_err());
    }

    #[test]
    fn transparent_coin_rejects_an_address_length_past_the_input() {
        let bytes = transparent_coin_bytes(1, u64::MAX, 5);
        assert!(TransparentCoin::read(bytes.as_slice(), ()).is_err());
    }

    #[test]
    fn transparent_coin_before_version_1_is_rejected() {
        let bytes = transparent_coin_bytes(0, 0, 5);
        assert!(TransparentCoin::read(bytes.as_slice(), ()).is_err());
    }

    #[test]
    fn sapling_rseed_rejects_a_non_canonical_scalar() {
        let bytes = [[0].as_slice(), &[0xff; 32]].concat();
        assert!(read_sapling_rseed(bytes.as_slice()).is_err());
    }

    fn orchard_note_prefix(version: u8) -> Vec<u8> {
        let mut out = vec![version];
        OutputId::new(TxId::from_bytes([1; 32]), 0)
            .write(&mut out)
            .unwrap();
        KeyId {
            account_id: zip32::AccountId::ZERO,
            scope: zip32::Scope::External,
        }
        .write(&mut out)
        .unwrap();
        out
    }

    fn orchard_address_bytes() -> [u8; 43] {
        let spending_key =
            orchard::keys::SpendingKey::from_zip32_seed(&[0; 32], 1, zip32::AccountId::ZERO)
                .unwrap();
        orchard::keys::FullViewingKey::from(&spending_key)
            .address_at(0u32, orchard::keys::Scope::External)
            .to_raw_address_bytes()
    }

    #[test]
    fn orchard_note_rejects_an_invalid_recipient() {
        let bytes = [orchard_note_prefix(2), vec![0xff; 43 + 8 + 64]].concat();
        assert!(OrchardNote::read(bytes.as_slice(), ()).is_err());
        assert!(IronwoodNote::read(bytes.as_slice(), ()).is_err());
    }

    #[test]
    fn orchard_note_rejects_a_non_canonical_rho() {
        let bytes = [
            orchard_note_prefix(2),
            orchard_address_bytes().to_vec(),
            vec![0; 8],
            vec![0xff; 32],
            vec![0; 32],
        ]
        .concat();
        assert!(OrchardNote::read(bytes.as_slice(), ()).is_err());
    }

    #[test]
    fn orchard_note_before_version_2_is_rejected() {
        let bytes = [orchard_note_prefix(1), vec![0; 43 + 8 + 64]].concat();
        assert!(OrchardNote::read(bytes.as_slice(), ()).is_err());
    }

    #[test]
    fn refetch_nullifier_range_that_ends_before_it_starts_is_rejected() {
        let bytes = [&[1][..], &10u32.to_le_bytes(), &5u32.to_le_bytes()].concat();
        assert!(read_refetch_nullifier_ranges(&mut bytes.as_slice()).is_err());
    }

    #[test]
    fn orchard_nullifier_rejects_a_non_canonical_field_element() {
        assert!(read_orchard_nullifier([0xff; 32].as_slice()).is_err());
    }

    #[test]
    fn outgoing_notes_before_version_1_are_rejected() {
        let params = zcash_protocol::consensus::MAIN_NETWORK;
        let bytes = orchard_note_prefix(0);
        assert!(OutgoingSaplingNote::read(bytes.as_slice(), &params).is_err());
        assert!(OutgoingOrchardNote::read(bytes.as_slice(), &params).is_err());
    }

    const EMPTY_TREE: [u8; 4] = [0, 0, SHARD_SER_V1, SHARD_NIL_TAG];

    fn shard_trees_with_sapling(sapling: &[u8]) -> Vec<u8> {
        [&[1][..], sapling, &EMPTY_TREE, &EMPTY_TREE].concat()
    }

    fn shard_root(level: u8, index: u64) -> Vec<u8> {
        [
            &[level][..],
            &index.to_le_bytes(),
            &[SHARD_SER_V1, SHARD_NIL_TAG],
        ]
        .concat()
    }

    fn checkpoint(id: u32, marks_removed: &[u64]) -> Vec<u8> {
        let mut out = id.to_le_bytes().to_vec();
        out.push(0);
        Vector::write(&mut out, marks_removed, |w, mark| {
            w.write_u64::<LittleEndian>(*mark)
        })
        .unwrap();
        out
    }

    fn sapling_tree(shards: &[Vec<u8>], checkpoints: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        Vector::write(&mut out, shards, |w, shard| w.write_all(shard)).unwrap();
        Vector::write(&mut out, checkpoints, |w, checkpoint| {
            w.write_all(checkpoint)
        })
        .unwrap();
        out.extend([SHARD_SER_V1, SHARD_NIL_TAG]);
        out
    }

    #[test]
    fn shard_trees_read_hand_built_trees() {
        let tree = sapling_tree(
            &[shard_root(16, 0), shard_root(16, 1)],
            &[checkpoint(5, &[1, 2]), checkpoint(6, &[])],
        );
        assert!(ShardTrees::read(shard_trees_with_sapling(&tree).as_slice(), ()).is_ok());
    }

    #[test]
    fn a_run_of_parent_tags_is_an_error_not_a_stack_overflow() {
        let parents = [SHARD_PARENT_TAG, 0].repeat(100_000);
        let in_shard = [&[1, 16][..], &0u64.to_le_bytes(), &[SHARD_SER_V1], &parents].concat();
        let in_cap = [&[0, 0, SHARD_SER_V1][..], &parents].concat();
        for tree in [in_shard, in_cap] {
            assert!(ShardTrees::read(shard_trees_with_sapling(&tree).as_slice(), ()).is_err());
        }
    }

    #[test]
    fn a_far_shard_index_is_an_error_not_an_allocation() {
        let tree = sapling_tree(&[shard_root(16, 1 << 40)], &[]);
        assert!(ShardTrees::read(shard_trees_with_sapling(&tree).as_slice(), ()).is_err());
    }

    #[test]
    fn shards_must_sit_at_the_shard_level_in_index_order() {
        for shards in [
            vec![shard_root(5, 0)],
            vec![shard_root(16, 1)],
            vec![shard_root(16, 0), shard_root(16, 0)],
            vec![shard_root(16, 1), shard_root(16, 0)],
        ] {
            let tree = sapling_tree(&shards, &[]);
            assert!(ShardTrees::read(shard_trees_with_sapling(&tree).as_slice(), ()).is_err());
        }
    }

    #[test]
    fn more_checkpoints_than_the_writer_keeps_are_rejected() {
        let checkpoints = (0..=SHARDTREE_CHECKPOINT_ROLLING_WINDOW_SIZE)
            .map(|id| checkpoint(id, &[]))
            .collect::<Vec<_>>();
        let tree = sapling_tree(&[], &checkpoints);
        assert!(ShardTrees::read(shard_trees_with_sapling(&tree).as_slice(), ()).is_err());
    }

    #[test]
    fn repeated_or_unordered_checkpoints_are_rejected() {
        for checkpoints in [
            [checkpoint(5, &[]), checkpoint(5, &[])],
            [checkpoint(6, &[]), checkpoint(5, &[])],
        ] {
            let tree = sapling_tree(&[], &checkpoints);
            assert!(ShardTrees::read(shard_trees_with_sapling(&tree).as_slice(), ()).is_err());
        }
    }

    #[test]
    fn a_repeated_removed_mark_is_rejected() {
        let tree = sapling_tree(&[], &[checkpoint(5, &[7, 7])]);
        assert!(ShardTrees::read(shard_trees_with_sapling(&tree).as_slice(), ()).is_err());
    }

    #[test]
    fn shardtree_with_leaves_in_two_shards_round_trips_byte_for_byte() {
        let mut shard_trees = ShardTrees::new();
        let leaves = |count| {
            (0..count).map(|_| {
                (
                    sapling_crypto::Node::empty_leaf(),
                    incrementalmerkletree::Retention::Marked,
                )
            })
        };
        shard_trees
            .sapling
            .batch_insert(Position::from(0), leaves(3))
            .unwrap();
        shard_trees
            .sapling
            .batch_insert(Position::from((1 << 16) + 5), leaves(2))
            .unwrap();
        shard_trees
            .sapling
            .checkpoint(BlockHeight::from_u32(10))
            .unwrap();
        shard_trees
            .orchard
            .append(
                orchard::tree::MerkleHashOrchard::empty_leaf(),
                incrementalmerkletree::Retention::Marked,
            )
            .unwrap();

        let mut written = Vec::new();
        shard_trees.write(&mut written, ()).unwrap();
        let read_back = ShardTrees::read(written.as_slice(), ()).unwrap();
        let mut rewritten = Vec::new();
        read_back.write(&mut rewritten, ()).unwrap();
        assert_eq!(rewritten, written);
    }

    #[test]
    fn shardtree_roundtrip_keeps_newest_checkpoints() {
        // checkpoints appended past the rolling window, so the writer drops some
        const APPENDED: u32 = SHARDTREE_CHECKPOINT_ROLLING_WINDOW_SIZE + 49;
        const OLDEST_KEPT: u32 = APPENDED - SHARDTREE_CHECKPOINT_ROLLING_WINDOW_SIZE + 1;

        let mut shard_trees = ShardTrees::new();

        for height in 1..=APPENDED {
            let height = BlockHeight::from_u32(height);
            shard_trees
                .sapling
                .store_mut()
                .add_checkpoint(
                    height,
                    Checkpoint::from_parts(TreeState::Empty, BTreeSet::new()),
                )
                .expect("infallible");
            shard_trees
                .orchard
                .store_mut()
                .add_checkpoint(
                    height,
                    Checkpoint::from_parts(TreeState::Empty, BTreeSet::new()),
                )
                .expect("infallible");
        }

        let mut bytes = Vec::new();
        shard_trees
            .write(&mut bytes, ())
            .expect("write should succeed");
        let roundtripped = ShardTrees::read(bytes.as_slice(), ()).expect("read should succeed");

        let sapling_store = roundtripped.sapling.store();
        let orchard_store = roundtripped.orchard.store();

        assert_eq!(
            sapling_store.checkpoint_count().expect("infallible"),
            SHARDTREE_CHECKPOINT_ROLLING_WINDOW_SIZE as usize
        );
        assert_eq!(
            orchard_store.checkpoint_count().expect("infallible"),
            SHARDTREE_CHECKPOINT_ROLLING_WINDOW_SIZE as usize
        );
        assert_eq!(
            sapling_store.min_checkpoint_id().expect("infallible"),
            Some(BlockHeight::from_u32(OLDEST_KEPT))
        );
        assert_eq!(
            sapling_store.max_checkpoint_id().expect("infallible"),
            Some(BlockHeight::from_u32(APPENDED))
        );
        assert_eq!(
            orchard_store.min_checkpoint_id().expect("infallible"),
            Some(BlockHeight::from_u32(OLDEST_KEPT))
        );
        assert_eq!(
            orchard_store.max_checkpoint_id().expect("infallible"),
            Some(BlockHeight::from_u32(APPENDED))
        );
        assert!(
            sapling_store
                .get_checkpoint(&BlockHeight::from_u32(APPENDED - 1))
                .expect("infallible")
                .is_some()
        );
        assert!(
            sapling_store
                .get_checkpoint(&BlockHeight::from_u32(OLDEST_KEPT - 1))
                .expect("infallible")
                .is_none()
        );
    }
}
