use std::{
    collections::{BTreeSet, HashMap},
    convert::Infallible,
    num::{NonZeroU32, NonZeroU64},
};

use secrecy::SecretVec;
use shardtree::{ShardTree, error::ShardTreeError, store::ShardStore};
use zcash_address::ZcashAddress;
use zcash_client_backend::{
    data_api::{
        Account, AccountBirthday, AccountMeta, AccountPurpose, AccountSource, AddressInfo,
        AddressSource, Balance, BlockMetadata, CoinbaseFilter, DecryptedTransaction, InputSource,
        NoteFilter, NullifierQuery, ORCHARD_SHARD_HEIGHT, OutputLockStore, PoolMeta, ReceivedNotes,
        ReceivedTransactionOutput, SAPLING_SHARD_HEIGHT, TargetValue, TransactionDataRequest,
        TransparentKeyOrigin, WalletCommitmentTrees, WalletRead, WalletSummary, WalletWrite,
        Zip32Derivation,
        chain::{ChainState, CommitmentTreeRoot},
        error::FindAccountForAddressError,
        locking::{LockError, LockFilter, LockOwner},
        scanning::ScanPriority,
        wallet::{ConfirmationsPolicy, TargetHeight},
    },
    wallet::{
        Exposure, Note, NoteId, ReceivedNote, TransparentAddressMetadata, WalletTransparentOutput,
    },
};
use zcash_keys::{
    address::{Address, UnifiedAddress},
    keys::UnifiedFullViewingKey,
};
use zcash_primitives::{
    block::BlockHash,
    transaction::{Transaction, TxId},
};
use zcash_protocol::{
    PoolType, ShieldedPool,
    consensus::{self, BlockHeight, Parameters},
    memo::Memo,
    value::{BalanceError, Zatoshis},
};
use zcash_transparent::address::TransparentAddress;
use zcash_transparent::bundle::{OutPoint, TxOut};
use zcash_transparent::keys::TransparentKeyScope;
use zip32::{AccountId, DiversifierIndex, fingerprint::SeedFingerprint};

use super::{
    LightWallet,
    error::WalletError,
    keys::unified::UnifiedKeyStore,
    locks::{LockTier, OutputLock},
    output::OutputRef,
};
use crate::wallet::output::RemainingNeeded;
use pepper_sync::{
    error::SyncError,
    keys::{
        KeyId,
        transparent::{self, TransparentScope},
    },
    wallet::{
        IronwoodNote, IronwoodShardStore, KeyIdInterface, NoteInterface, OrchardNote,
        OrchardShardStore, OutgoingIronwoodNote, OutgoingNoteInterface, OutgoingOrchardNote,
        OutgoingSaplingNote, OutputId, OutputInterface, SaplingNote, SaplingShardStore,
        TransparentCoin, WalletTransaction, traits::SyncWallet,
    },
};
use zingolib_common::status::ConfirmationStatus;

pub struct ZingoAccount {
    id: zip32::AccountId,
    ufvk: UnifiedFullViewingKey,
    birthday: BlockHeight,
    source: AccountSource,
}

impl Account for ZingoAccount {
    type AccountId = zip32::AccountId;

    fn id(&self) -> Self::AccountId {
        self.id
    }

    fn name(&self) -> Option<&str> {
        None
    }

    fn birthday_height(&self) -> BlockHeight {
        self.birthday
    }

    fn source(&self) -> &AccountSource {
        &self.source
    }

    fn ufvk(&self) -> Option<&UnifiedFullViewingKey> {
        Some(&self.ufvk)
    }

    fn uivk(&self) -> zcash_keys::keys::UnifiedIncomingViewingKey {
        self.ufvk.to_unified_incoming_viewing_key()
    }
}

fn find_output<Op: OutputInterface>(
    transaction: &WalletTransaction,
    output_id: OutputId,
) -> Option<&Op> {
    Op::transaction_outputs(transaction)
        .iter()
        .find(|output| output.output_id() == output_id)
}

fn zcb_output_ref(output: &OutputRef) -> zcash_client_backend::wallet::OutputRef {
    zcash_client_backend::wallet::OutputRef::new(
        output.txid(),
        output.pool_type(),
        output.output_index(),
    )
}

fn wallet_output_ref(output: &zcash_client_backend::wallet::OutputRef) -> OutputRef {
    OutputRef::new(
        OutputId::new(*output.txid(), output.output_index()),
        output.pool(),
    )
}

fn external_sent_values<'a, Outgoing, Incoming>(
    transaction: &'a WalletTransaction,
    account: zip32::AccountId,
) -> impl Iterator<Item = u64> + 'a
where
    Outgoing: OutgoingNoteInterface + 'a,
    Incoming: OutputInterface,
{
    Outgoing::transaction_outgoing_notes(transaction)
        .iter()
        .filter(move |outgoing_note| {
            outgoing_note.key_id().account_id == account
                && !Incoming::transaction_outputs(transaction)
                    .iter()
                    .any(|wallet_note| wallet_note.output_id() == outgoing_note.output_id())
        })
        .map(OutgoingNoteInterface::value)
}

/// The maximum of each of `buckets` equal-height tiles over `sorted`, matching SQL's `NTILE`.
fn ntile_maxima(sorted: &[u64], buckets: usize) -> Vec<u64> {
    let buckets = buckets.min(sorted.len());
    let base = sorted.len().checked_div(buckets).unwrap_or(0);
    let extra = sorted.len().checked_rem(buckets).unwrap_or(0);
    let mut start = 0;
    (0..buckets)
        .map(|bucket| {
            let end = start + base + usize::from(bucket < extra);
            let max = sorted[end - 1];
            start = end;
            max
        })
        .collect()
}

impl LightWallet {
    fn account(
        &self,
        account_id: zip32::AccountId,
        key_store: &UnifiedKeyStore,
    ) -> Result<ZingoAccount, WalletError> {
        let ufvk = UnifiedFullViewingKey::try_from(key_store)?;
        let source = match self.mnemonic() {
            Some(mnemonic) => AccountSource::Derived {
                derivation: Zip32Derivation::new(
                    SeedFingerprint::from_seed(&mnemonic.to_seed(""))
                        .expect("a BIP 39 seed is 64 bytes"),
                    account_id,
                ),
                key_source: None,
            },
            None => AccountSource::Imported {
                purpose: if key_store.is_spending_key() {
                    AccountPurpose::Spending { derivation: None }
                } else {
                    AccountPurpose::ViewOnly
                },
                key_source: None,
            },
        };

        Ok(ZingoAccount {
            id: account_id,
            ufvk,
            birthday: self.birthday,
            source,
        })
    }

    fn decode_transparent_address(&self, encoded: &str) -> Result<TransparentAddress, WalletError> {
        Ok(ZcashAddress::try_from_encoded(encoded)?
            .convert_if_network::<TransparentAddress>(self.chain_type.network_type())
            .expect("incorrect network should be checked on wallet load"))
    }

    /// The account owning `output` and the transaction spending it, if the wallet holds it.
    fn output_info(&self, output: &OutputRef) -> Option<(zip32::AccountId, Option<TxId>)> {
        fn info<Op: OutputInterface>(
            transaction: &WalletTransaction,
            output_id: OutputId,
        ) -> Option<(zip32::AccountId, Option<TxId>)> {
            find_output::<Op>(transaction, output_id)
                .map(|output| (output.key_id().account_id(), output.spending_transaction()))
        }

        let transaction = self.wallet_transactions.get(&output.txid())?;
        let output_id = output.output_id();
        match output.pool_type() {
            PoolType::Transparent => info::<TransparentCoin>(transaction, output_id),
            PoolType::Shielded(ShieldedPool::Sapling) => {
                info::<SaplingNote>(transaction, output_id)
            }
            PoolType::Shielded(ShieldedPool::Orchard) => {
                info::<OrchardNote>(transaction, output_id)
            }
            PoolType::Shielded(ShieldedPool::Ironwood) => {
                info::<IronwoodNote>(transaction, output_id)
            }
        }
    }

    fn output_account(&self, output: &OutputRef) -> Option<zip32::AccountId> {
        self.output_info(output).map(|(account, _)| account)
    }

    fn release_spent_output_locks(&mut self) {
        let spent = self
            .output_locks
            .iter()
            .map(|(output, _)| *output)
            .filter(|output| {
                self.output_info(output)
                    .is_some_and(|(_, spending_transaction)| spending_transaction.is_some())
            })
            .collect::<BTreeSet<_>>();
        self.output_locks
            .release_where(|output| spent.contains(output));
    }

    /// Notes of `account` that are confirmed before `target_height` and not spent by a live
    /// transaction.
    fn confirmed_unspent_notes<N: NoteInterface>(
        &self,
        account: zip32::AccountId,
        target_height: BlockHeight,
    ) -> Vec<&N> {
        self.wallet_transactions
            .values()
            .filter(|transaction| transaction.status().is_confirmed_before(&target_height))
            .flat_map(|transaction| {
                N::transaction_outputs(transaction).iter().filter(|note| {
                    note.key_id().account_id() == account
                        && self.output_spend_status(*note).is_unspent()
                })
            })
            .collect()
    }

    fn spendable_note<N: NoteInterface>(
        &self,
        output_id: OutputId,
        target_height: BlockHeight,
        lock_filter: LockFilter<'_>,
    ) -> Option<&N> {
        let transaction = self.wallet_transactions.get(&output_id.txid())?;
        let note = find_output::<N>(transaction, output_id)?;
        let output = OutputRef::new(output_id, PoolType::Shielded(N::SHIELDED_PROTOCOL));
        (transaction.status().is_confirmed_before(&target_height)
            && self.output_spend_status(note).is_unspent()
            && note.nullifier().is_some()
            && note.position().is_some()
            && self
                .output_locks
                .eligible(&output, target_height, lock_filter))
        .then_some(note)
    }

    fn received_note<N, Z>(
        &self,
        note: &N,
        wrap: impl FnOnce(&N::ZcashNote) -> Z,
    ) -> ReceivedNote<OutputRef, Z>
    where
        N: NoteInterface<KeyId = KeyId>,
    {
        ReceivedNote::from_parts(
            OutputRef::new(note.output_id(), PoolType::Shielded(N::SHIELDED_PROTOCOL)),
            note.output_id().txid(),
            note.output_id()
                .output_index()
                .try_into()
                .expect("shielded notes are always valid u16"),
            wrap(note.note()),
            note.key_id().scope,
            note.position()
                .expect("note selection should filter on notes with positions"),
            self.output_transaction(note)
                .status()
                .get_confirmed_height(),
            None,
        )
    }

    /// Output IDs of `account`'s notes that `lock_filter` makes ineligible, and those it defers
    /// behind the preferred tier.
    fn lock_exclusions<N: NoteInterface>(
        &self,
        account: zip32::AccountId,
        target_height: BlockHeight,
        lock_filter: LockFilter<'_>,
    ) -> (Vec<OutputId>, Vec<OutputId>) {
        let mut ineligible = Vec::new();
        let mut deferred = Vec::new();
        for note in self.wallet_outputs::<N>() {
            if note.key_id().account_id() != account {
                continue;
            }
            let output = OutputRef::new(note.output_id(), PoolType::Shielded(N::SHIELDED_PROTOCOL));
            match self.output_locks.tier(&output, target_height, lock_filter) {
                LockTier::Preferred => (),
                LockTier::Deferred => deferred.push(note.output_id()),
                LockTier::Ineligible => ineligible.push(note.output_id()),
            }
        }
        (ineligible, deferred)
    }

    fn unspent_received_notes<N, Z>(
        &self,
        account: zip32::AccountId,
        target_height: BlockHeight,
        exclude: &[OutputRef],
        lock_filter: LockFilter<'_>,
        wrap: impl Fn(&N::ZcashNote) -> Z,
    ) -> Vec<ReceivedNote<OutputRef, Z>>
    where
        N: NoteInterface<KeyId = KeyId>,
    {
        self.confirmed_unspent_notes::<N>(account, target_height)
            .into_iter()
            .filter(|note| note.nullifier().is_some() && note.position().is_some())
            .filter(|note| {
                let output =
                    OutputRef::new(note.output_id(), PoolType::Shielded(N::SHIELDED_PROTOCOL));
                !exclude.contains(&output)
                    && self
                        .output_locks
                        .eligible(&output, target_height, lock_filter)
            })
            .map(|note| self.received_note(note, &wrap))
            .collect()
    }

    fn pool_meta<N: NoteInterface>(
        &self,
        account: zip32::AccountId,
        min_value: Zatoshis,
        target_height: BlockHeight,
        exclude: &[OutputRef],
        lock_filter: LockFilter<'_>,
    ) -> Result<PoolMeta, WalletError> {
        let (note_count, total_value) = self
            .confirmed_unspent_notes::<N>(account, target_height)
            .into_iter()
            .filter(|note| note.value() > min_value.into_u64())
            .filter(|note| {
                let output =
                    OutputRef::new(note.output_id(), PoolType::Shielded(N::SHIELDED_PROTOCOL));
                !exclude.contains(&output)
                    && self
                        .output_locks
                        .eligible(&output, target_height, lock_filter)
            })
            .fold((0, 0), |(count, total), note| {
                (count + 1, total + note.value())
            });

        Ok(PoolMeta::new(note_count, Zatoshis::from_u64(total_value)?))
    }

    /// The smallest note value `filter` admits, or `None` when `filter` cannot be evaluated
    /// against the wallet's history.
    fn note_filter_min_value(
        &self,
        account: zip32::AccountId,
        filter: &NoteFilter,
        target_height: BlockHeight,
    ) -> Result<Option<Zatoshis>, WalletError> {
        Ok(match filter {
            NoteFilter::ExceedsMinValue(value) => Some(*value),
            NoteFilter::ExceedsPriorSendPercentile(percentile) => {
                let mut sent = self
                    .wallet_transactions
                    .values()
                    .filter(|transaction| transaction.status().is_confirmed())
                    .flat_map(|transaction| {
                        external_sent_values::<OutgoingSaplingNote, SaplingNote>(
                            transaction,
                            account,
                        )
                        .chain(external_sent_values::<OutgoingOrchardNote, OrchardNote>(
                            transaction,
                            account,
                        ))
                        .chain(external_sent_values::<
                            OutgoingIronwoodNote,
                            IronwoodNote,
                        >(
                            transaction, account
                        ))
                    })
                    .collect::<Vec<_>>();
                sent.sort_unstable();
                let bucket_maxima = ntile_maxima(&sent, 10);
                let bucket =
                    (bucket_maxima.len() * usize::from(percentile.value()) / 100).saturating_sub(1);
                bucket_maxima
                    .get(bucket)
                    .map(|value| Zatoshis::from_u64(*value))
                    .transpose()?
            }
            NoteFilter::ExceedsBalancePercentage(percentage) => {
                let notes_value = self
                    .confirmed_unspent_notes::<SaplingNote>(account, target_height)
                    .iter()
                    .map(|note| note.value())
                    .chain(
                        self.confirmed_unspent_notes::<OrchardNote>(account, target_height)
                            .iter()
                            .map(|note| note.value()),
                    )
                    .chain(
                        self.confirmed_unspent_notes::<IronwoodNote>(account, target_height)
                            .iter()
                            .map(|note| note.value()),
                    )
                    .map(Zatoshis::from_u64)
                    .try_fold(None, |balance: Option<Zatoshis>, value| {
                        let value = value?;
                        Ok::<_, BalanceError>(Some(
                            (balance.unwrap_or(Zatoshis::ZERO) + value)
                                .ok_or(BalanceError::Overflow)?,
                        ))
                    })?;
                notes_value
                    .map(|balance| {
                        let scaled = (balance * u64::from(percentage.value()))
                            .ok_or(BalanceError::Overflow)?;
                        Ok::<_, BalanceError>(
                            scaled / NonZeroU64::new(100).expect("constant is nonzero"),
                        )
                    })
                    .transpose()?
            }
            NoteFilter::Combine(a, b) => {
                let a = self.note_filter_min_value(account, a, target_height)?;
                let b = self.note_filter_min_value(account, b, target_height)?;
                a.zip(b).map(|(a, b)| std::cmp::max(a, b)).or(a).or(b)
            }
            NoteFilter::Attempt {
                condition,
                fallback,
            } => match self.note_filter_min_value(account, condition, target_height)? {
                Some(value) => Some(value),
                None => self.note_filter_min_value(account, fallback, target_height)?,
            },
        })
    }
}

impl WalletRead for LightWallet {
    type Error = WalletError;
    type AccountId = zip32::AccountId;
    type Account = ZingoAccount;

    fn get_account_ids(&self) -> Result<Vec<Self::AccountId>, Self::Error> {
        Ok(self.unified_key_store.keys().copied().collect())
    }

    fn get_account(
        &self,
        account_id: Self::AccountId,
    ) -> Result<Option<Self::Account>, Self::Error> {
        self.unified_key_store
            .get(&account_id)
            .map(|key_store| self.account(account_id, key_store))
            .transpose()
    }

    fn get_derived_account(
        &self,
        _account_id: &Zip32Derivation,
    ) -> Result<Option<Self::Account>, Self::Error> {
        unimplemented!()
    }

    fn validate_seed(
        &self,
        _account_id: Self::AccountId,
        _seed: &secrecy::SecretVec<u8>,
    ) -> Result<bool, Self::Error> {
        unimplemented!()
    }

    fn seed_relevance_to_derived_accounts(
        &self,
        _seed: &secrecy::SecretVec<u8>,
    ) -> Result<zcash_client_backend::data_api::SeedRelevance<Self::AccountId>, Self::Error> {
        unimplemented!()
    }

    fn get_account_for_ufvk(
        &self,
        ufvk: &UnifiedFullViewingKey,
    ) -> Result<Option<Self::Account>, Self::Error> {
        self.unified_key_store
            .iter()
            .find(|(_, unified_key)| {
                UnifiedFullViewingKey::try_from(*unified_key).is_ok_and(|account_ufvk| {
                    account_ufvk.encode(&self.chain_type) == *ufvk.encode(&self.chain_type)
                })
            })
            .map(|(account_id, key_store)| self.account(*account_id, key_store))
            .transpose()
    }

    fn list_addresses(&self, account: Self::AccountId) -> Result<Vec<AddressInfo>, Self::Error> {
        let unified = self
            .unified_addresses
            .iter()
            .filter(|(address_id, _)| address_id.account_id == account)
            .filter_map(|(address_id, address)| {
                AddressInfo::from_parts(
                    Address::Unified(address.clone()),
                    AddressSource::Derived {
                        diversifier_index: DiversifierIndex::from(address_id.address_index),
                        transparent_key_scope: None,
                    },
                )
            });
        let transparent = self
            .transparent_addresses
            .iter()
            .filter(|(address_id, _)| address_id.account_id() == account)
            .map(|(address_id, encoded_address)| {
                Ok(AddressInfo::from_parts(
                    Address::Transparent(self.decode_transparent_address(encoded_address)?),
                    AddressSource::Derived {
                        diversifier_index: DiversifierIndex::from(
                            address_id.address_index().index(),
                        ),
                        transparent_key_scope: Some(address_id.scope().into()),
                    },
                ))
            })
            .collect::<Result<Vec<_>, WalletError>>()?;

        Ok(unified.chain(transparent.into_iter().flatten()).collect())
    }

    fn get_last_generated_address_matching(
        &self,
        _account: Self::AccountId,
        _address_filter: zcash_keys::keys::UnifiedAddressRequest,
    ) -> Result<Option<UnifiedAddress>, Self::Error> {
        unimplemented!()
    }

    fn get_account_birthday(&self, _account: Self::AccountId) -> Result<BlockHeight, Self::Error> {
        unimplemented!()
    }

    fn get_wallet_birthday(&self) -> Result<Option<BlockHeight>, Self::Error> {
        unimplemented!()
    }

    fn get_wallet_recover_until(&self) -> Result<Option<BlockHeight>, Self::Error> {
        Ok(None)
    }

    fn get_wallet_summary(
        &self,
        _min_confirmations: ConfirmationsPolicy,
    ) -> Result<Option<WalletSummary<Self::AccountId>>, Self::Error> {
        unimplemented!()
    }

    fn chain_height(&self) -> Result<Option<BlockHeight>, Self::Error> {
        Ok(self.sync_state.last_known_chain_height())
    }

    fn get_block_hash(&self, _block_height: BlockHeight) -> Result<Option<BlockHash>, Self::Error> {
        unimplemented!()
    }

    fn block_metadata(&self, _height: BlockHeight) -> Result<Option<BlockMetadata>, Self::Error> {
        unimplemented!()
    }

    fn block_fully_scanned(&self) -> Result<Option<BlockMetadata>, Self::Error> {
        unimplemented!()
    }

    fn get_max_height_hash(&self) -> Result<Option<(BlockHeight, BlockHash)>, Self::Error> {
        unimplemented!()
    }

    fn block_max_scanned(&self) -> Result<Option<BlockMetadata>, Self::Error> {
        unimplemented!()
    }

    fn suggest_scan_ranges(
        &self,
    ) -> Result<Vec<zcash_client_backend::data_api::scanning::ScanRange>, Self::Error> {
        unimplemented!()
    }

    fn get_target_and_anchor_heights(
        &self,
        min_confirmations: NonZeroU32,
    ) -> Result<Option<(TargetHeight, BlockHeight)>, Self::Error> {
        let target_height = if let Some(height) = self.sync_state.last_known_chain_height() {
            height + 1
        } else {
            return Ok(None);
        };

        let max_checkpoint_height = self
            .shard_trees
            .sapling
            .store()
            .max_checkpoint_id()
            .expect("infallible")
            .expect("should be at least 1 checkpoint");

        let anchor_height = std::cmp::min(
            max_checkpoint_height,
            target_height - min_confirmations.get(),
        );

        Ok(Some((
            target_height.into(),
            std::cmp::max(1.into(), anchor_height),
        )))
    }

    fn get_tx_height(&self, txid: TxId) -> Result<Option<BlockHeight>, Self::Error> {
        Ok(self
            .wallet_transactions
            .get(&txid)
            .and_then(|transaction| transaction.status().get_confirmed_height()))
    }

    fn get_unified_full_viewing_keys(
        &self,
    ) -> Result<HashMap<Self::AccountId, UnifiedFullViewingKey>, Self::Error> {
        unimplemented!()
    }

    fn get_memo(&self, _note_id: NoteId) -> Result<Option<Memo>, Self::Error> {
        unimplemented!()
    }

    fn get_transaction(&self, _txid: TxId) -> Result<Option<Transaction>, Self::Error> {
        unimplemented!()
    }

    fn get_sapling_nullifiers(
        &self,
        _query: NullifierQuery,
    ) -> Result<Vec<(Self::AccountId, sapling_crypto::Nullifier)>, Self::Error> {
        unimplemented!()
    }

    fn get_orchard_nullifiers(
        &self,
        _query: NullifierQuery,
    ) -> Result<Vec<(Self::AccountId, orchard::note::Nullifier)>, Self::Error> {
        unimplemented!()
    }

    fn get_ironwood_nullifiers(
        &self,
        _query: NullifierQuery,
    ) -> Result<Vec<(Self::AccountId, orchard::note::Nullifier)>, Self::Error> {
        unimplemented!()
    }

    fn get_transparent_receivers(
        &self,
        account: Self::AccountId,
        // TODO: only get internal receivers if true
        _include_change: bool,
        _include_standalone_receivers: bool,
    ) -> Result<HashMap<TransparentAddress, TransparentAddressMetadata>, Self::Error> {
        self.transparent_addresses
            .iter()
            .filter(|(address_id, _)| {
                address_id.account_id() == account && address_id.scope() != TransparentScope::Refund
            })
            .map(|(address_id, encoded_address)| {
                let address = self.decode_transparent_address(encoded_address)?;
                let address_metadata = TransparentAddressMetadata::derived(
                    address_id.scope().into(),
                    address_id.address_index(),
                    Exposure::CannotKnow, // TODO: add exposure to wallet transparent address metadata
                    None,
                );

                Ok((address, address_metadata))
            })
            .collect()
    }

    fn get_ephemeral_transparent_receivers(
        &self,
        account: Self::AccountId,
        _exposure_depth: u32,
        _exclude_used: bool,
    ) -> Result<HashMap<TransparentAddress, TransparentAddressMetadata>, Self::Error> {
        self.transparent_addresses
            .iter()
            .filter(|(address_id, _)| {
                address_id.account_id() == account && address_id.scope() == TransparentScope::Refund
            })
            .map(|(address_id, encoded_address)| {
                let address = self.decode_transparent_address(encoded_address)?;
                let address_metadata = TransparentAddressMetadata::derived(
                    address_id.scope().into(),
                    address_id.address_index(),
                    Exposure::CannotKnow, // TODO: add exposure to wallet transparent address metadata
                    None,
                );

                Ok((address, address_metadata))
            })
            .collect()
    }

    fn get_transparent_balances(
        &self,
        _account: Self::AccountId,
        _max_height: TargetHeight,
        _confirmations_policy: ConfirmationsPolicy,
    ) -> Result<HashMap<TransparentAddress, (TransparentKeyOrigin, Balance)>, Self::Error> {
        unimplemented!()
    }

    fn get_transparent_address_metadata(
        &self,
        account: Self::AccountId,
        address: &TransparentAddress,
    ) -> Result<Option<TransparentAddressMetadata>, Self::Error> {
        Ok(
            if let Some(result) = self
                .get_transparent_receivers(account, true, true)?
                .get(address)
            {
                Some(result.clone())
            } else {
                self.get_ephemeral_transparent_receivers(account, u32::MAX, false)?
                    .get(address)
                    .cloned()
            },
        )
    }

    fn utxo_query_height(
        &self,
        _account: Self::AccountId,
    ) -> Result<zcash_protocol::consensus::BlockHeight, Self::Error> {
        unimplemented!()
    }

    fn transaction_data_requests(&self) -> Result<Vec<TransactionDataRequest>, Self::Error> {
        unimplemented!()
    }

    fn find_account_for_address<P: consensus::Parameters>(
        &self,
        _params: &P,
        _address: &zcash_keys::address::Address,
    ) -> Result<Option<Self::AccountId>, FindAccountForAddressError<Self::Error>> {
        unimplemented!()
    }

    fn get_received_outputs(
        &self,
        _txid: TxId,
        _target_height: TargetHeight,
        _confirmations_policy: ConfirmationsPolicy,
    ) -> Result<Vec<ReceivedTransactionOutput>, Self::Error> {
        unimplemented!()
    }
}

impl OutputLockStore for LightWallet {
    type Error = WalletError;
    type AccountId = zip32::AccountId;

    fn lock_outputs(
        &mut self,
        outputs: &[zcash_client_backend::wallet::OutputRef],
        owner: LockOwner,
        lock_expiry_height: BlockHeight,
    ) -> Result<usize, LockError<WalletError>> {
        let chain_tip = self.sync_state.last_known_chain_height();
        let lockable = outputs
            .iter()
            .map(|output| {
                let output_ref = wallet_output_ref(output);
                if self.output_account(&output_ref).is_some()
                    && self.output_locks.lockable_by(&output_ref, owner, chain_tip)
                {
                    Ok(output_ref)
                } else {
                    Err(LockError::LockFailure(*output))
                }
            })
            .collect::<Result<Vec<_>, _>>()?;

        for output_ref in &lockable {
            self.output_locks.insert(
                *output_ref,
                OutputLock {
                    owner,
                    expiry_height: lock_expiry_height,
                },
            );
        }

        Ok(lockable.len())
    }

    fn unlock_output(
        &mut self,
        output: &zcash_client_backend::wallet::OutputRef,
        owner: LockOwner,
    ) -> Result<bool, WalletError> {
        Ok(self.output_locks.release(&wallet_output_ref(output), owner))
    }

    fn clear_locked_outputs(&mut self, account: zip32::AccountId) -> Result<usize, WalletError> {
        let owned = self
            .output_locks
            .iter()
            .map(|(output, _)| *output)
            .filter(|output| self.output_account(output) == Some(account))
            .collect::<BTreeSet<_>>();

        Ok(self
            .output_locks
            .release_where(|output| owned.contains(output)))
    }

    fn get_locked_outputs(
        &self,
        account: zip32::AccountId,
    ) -> Result<Vec<zcash_client_backend::wallet::OutputRef>, WalletError> {
        let chain_tip = self.sync_state.last_known_chain_height();

        Ok(self
            .output_locks
            .iter()
            .filter(|(output, lock)| {
                chain_tip.is_none_or(|tip| lock.expiry_height > tip)
                    && self.output_account(output) == Some(account)
            })
            .map(|(output, _)| zcb_output_ref(output))
            .collect())
    }
}

impl WalletWrite for LightWallet {
    type UtxoRef = u32;

    fn create_account(
        &mut self,
        _account_name: &str,
        _seed: &SecretVec<u8>,
        _birthday: &AccountBirthday,
        _key_source: Option<&str>,
    ) -> Result<(zip32::AccountId, zcash_keys::keys::UnifiedSpendingKey), WalletError> {
        unimplemented!()
    }

    fn import_account_hd(
        &mut self,
        _account_name: &str,
        _seed: &SecretVec<u8>,
        _account_index: zip32::AccountId,
        _birthday: &AccountBirthday,
        _key_source: Option<&str>,
    ) -> Result<(ZingoAccount, zcash_keys::keys::UnifiedSpendingKey), WalletError> {
        unimplemented!()
    }

    fn import_account_ufvk(
        &mut self,
        _account_name: &str,
        _unified_key: &UnifiedFullViewingKey,
        _birthday: &AccountBirthday,
        _purpose: AccountPurpose,
        _key_source: Option<&str>,
    ) -> Result<ZingoAccount, WalletError> {
        unimplemented!()
    }

    fn delete_account(&mut self, _account: zip32::AccountId) -> Result<(), WalletError> {
        unimplemented!()
    }

    fn get_next_available_address(
        &mut self,
        _account: zip32::AccountId,
        _request: zcash_keys::keys::UnifiedAddressRequest,
    ) -> Result<Option<(UnifiedAddress, zip32::DiversifierIndex)>, WalletError> {
        unimplemented!()
    }

    fn get_address_for_index(
        &mut self,
        _account: zip32::AccountId,
        _diversifier_index: zip32::DiversifierIndex,
        _request: zcash_keys::keys::UnifiedAddressRequest,
    ) -> Result<Option<UnifiedAddress>, WalletError> {
        unimplemented!()
    }

    fn update_chain_tip(&mut self, _tip_height: BlockHeight) -> Result<(), WalletError> {
        unimplemented!()
    }

    fn put_blocks(
        &mut self,
        _from_state: &zcash_client_backend::data_api::chain::ChainState,
        _blocks: Vec<zcash_client_backend::data_api::ScannedBlock<zip32::AccountId>>,
    ) -> Result<(), WalletError> {
        unimplemented!()
    }

    fn prune_scan_queue_below(
        &mut self,
        _height: BlockHeight,
        _retain_with_priority: Option<ScanPriority>,
    ) -> Result<u64, WalletError> {
        Ok(0)
    }

    fn put_received_transparent_utxo(
        &mut self,
        _output: &WalletTransparentOutput<AccountId>,
    ) -> Result<Self::UtxoRef, WalletError> {
        unimplemented!()
    }

    fn store_decrypted_tx(
        &mut self,
        received_tx: DecryptedTransaction<Transaction, zip32::AccountId>,
    ) -> Result<(), WalletError> {
        let txid = received_tx.tx().txid();
        if self.wallet_transactions.contains_key(&txid) {
            return Ok(());
        }
        if received_tx.mined_height().is_some() {
            return Err(WalletError::TransactionNotFound(txid));
        }

        let chain_type = self.chain_type;
        let target_height = self
            .sync_state
            .last_known_chain_height()
            .ok_or(WalletError::NoSyncData)?
            + 1;
        let mut transaction_bytes = vec![];
        received_tx
            .tx()
            .write(&mut transaction_bytes)
            .map_err(WalletError::TransactionWrite)?;
        let transaction = Transaction::read(
            transaction_bytes.as_slice(),
            consensus::BranchId::for_height(&chain_type, target_height),
        )
        .map_err(WalletError::TransactionRead)?;
        let seen_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the unix epoch")
            .as_secs() as u32;

        match pepper_sync::scan_pending_transaction(
            &chain_type,
            &SyncWallet::get_unified_full_viewing_keys(self)?,
            self,
            transaction,
            ConfirmationStatus::Mempool(target_height),
            seen_at,
        ) {
            Ok(()) => Ok(()),
            Err(SyncError::ScanError(e)) => Err(e.into()),
            Err(SyncError::WalletError(e)) => Err(e),
            Err(_) => {
                panic!("`scan_pending_transaction` should only return scan or wallet errors")
            }
        }
    }

    fn set_tx_trust(&mut self, _txid: TxId, _trusted: bool) -> Result<(), WalletError> {
        unimplemented!()
    }

    fn store_transactions_to_be_sent(
        &mut self,
        transactions: &[zcash_client_backend::data_api::SentTransaction<zip32::AccountId>],
    ) -> Result<(), WalletError> {
        let chain_type = self.chain_type;

        for sent_transaction in transactions {
            // this is a workaround as Transaction does not implement Clone
            let mut transaction_bytes = vec![];
            sent_transaction
                .tx()
                .write(&mut transaction_bytes)
                .map_err(WalletError::TransactionWrite)?;
            let transaction = Transaction::read(
                transaction_bytes.as_slice(),
                consensus::BranchId::for_height(
                    &self.chain_type,
                    sent_transaction.target_height().into(),
                ),
            )
            .map_err(WalletError::TransactionRead)?;

            match pepper_sync::scan_pending_transaction(
                &chain_type,
                &SyncWallet::get_unified_full_viewing_keys(self)?,
                self,
                transaction,
                ConfirmationStatus::Calculated(sent_transaction.target_height().into()),
                sent_transaction.created().unix_timestamp() as u32,
            ) {
                Ok(()) => (),
                Err(SyncError::ScanError(e)) => return Err(e.into()),
                Err(SyncError::WalletError(e)) => return Err(e),
                Err(_) => {
                    panic!("`scan_pending_transactions` should only return scan or wallet errors")
                }
            }
        }
        self.release_spent_output_locks();

        Ok(())
    }

    fn truncate_to_height(&mut self, _max_height: BlockHeight) -> Result<BlockHeight, WalletError> {
        unimplemented!()
    }

    fn truncate_to_chain_state(&mut self, _chain_state: ChainState) -> Result<(), WalletError> {
        unimplemented!()
    }

    fn reserve_next_n_ephemeral_addresses(
        &mut self,
        account_id: zip32::AccountId,
        n: usize,
    ) -> Result<Vec<(TransparentAddress, TransparentAddressMetadata)>, WalletError> {
        Ok(self
            .generate_refund_addresses(n, account_id)?
            .into_iter()
            .map(|(address_id, address)| {
                (
                    address,
                    TransparentAddressMetadata::derived(
                        TransparentKeyScope::EPHEMERAL,
                        address_id.address_index(),
                        Exposure::CannotKnow, // TODO: add exposure to wallet transparent address metadata
                        None,
                    ),
                )
            })
            .collect())
    }

    fn set_transaction_status(
        &mut self,
        _txid: TxId,
        _status: zcash_client_backend::data_api::TransactionStatus,
    ) -> Result<(), WalletError> {
        unimplemented!()
    }

    fn notify_transaction_enhancement_not_found(&mut self, _txid: TxId) -> Result<(), WalletError> {
        Ok(())
    }

    fn notify_address_checked(
        &mut self,
        _request: zcash_client_backend::data_api::TransactionsInvolvingAddress,
        _as_of_height: BlockHeight,
    ) -> Result<(), WalletError> {
        unimplemented!()
    }

    fn rewind_to_chain_state(
        &mut self,
        _chain_state: ChainState,
        _reset_account_birthdays: std::collections::HashSet<zip32::AccountId>,
    ) -> Result<(), zcash_client_backend::data_api::error::RewindError<zip32::AccountId, WalletError>>
    {
        unimplemented!()
    }
}

impl WalletCommitmentTrees for LightWallet {
    type Error = Infallible;
    type SaplingShardStore<'a> = SaplingShardStore;
    type OrchardShardStore<'a> = OrchardShardStore;

    fn with_sapling_tree_mut<F, A, E>(&mut self, mut callback: F) -> Result<A, E>
    where
        for<'a> F: FnMut(
            &'a mut ShardTree<
                Self::SaplingShardStore<'a>,
                { sapling_crypto::NOTE_COMMITMENT_TREE_DEPTH },
                { SAPLING_SHARD_HEIGHT },
            >,
        ) -> Result<A, E>,
        E: From<ShardTreeError<Self::Error>>,
    {
        callback(&mut self.shard_trees.sapling)
    }

    fn put_sapling_subtree_roots(
        &mut self,
        start_index: u64,
        roots: &[CommitmentTreeRoot<sapling_crypto::Node>],
    ) -> Result<(), ShardTreeError<Self::Error>> {
        self.with_sapling_tree_mut(|t| {
            for (root, i) in roots.iter().zip(0u64..) {
                let root_addr = incrementalmerkletree::Address::from_parts(
                    SAPLING_SHARD_HEIGHT.into(),
                    start_index + i,
                );
                t.insert(root_addr, *root.root_hash())?;
            }
            Ok::<_, ShardTreeError<Self::Error>>(())
        })?;

        Ok(())
    }

    fn get_sapling_subtree_root(
        &mut self,
        index: u64,
    ) -> Result<Option<sapling_crypto::Node>, ShardTreeError<Self::Error>> {
        let root_addr =
            incrementalmerkletree::Address::from_parts(SAPLING_SHARD_HEIGHT.into(), index);
        Ok(self
            .shard_trees
            .sapling
            .store()
            .get_shard(root_addr)
            .map_err(ShardTreeError::Storage)?
            .and_then(|shard| shard.root_hash(root_addr.position_range_end()).ok()))
    }

    fn with_orchard_tree_mut<F, A, E>(&mut self, mut callback: F) -> Result<A, E>
    where
        for<'a> F: FnMut(
            &'a mut ShardTree<
                Self::OrchardShardStore<'a>,
                { orchard::NOTE_COMMITMENT_TREE_DEPTH as u8 },
                { ORCHARD_SHARD_HEIGHT },
            >,
        ) -> Result<A, E>,
        E: From<ShardTreeError<Self::Error>>,
    {
        callback(&mut self.shard_trees.orchard)
    }

    fn put_orchard_subtree_roots(
        &mut self,
        start_index: u64,
        roots: &[CommitmentTreeRoot<orchard::tree::MerkleHashOrchard>],
    ) -> Result<(), ShardTreeError<Self::Error>> {
        self.with_orchard_tree_mut(|t| {
            for (root, i) in roots.iter().zip(0u64..) {
                let root_addr = incrementalmerkletree::Address::from_parts(
                    ORCHARD_SHARD_HEIGHT.into(),
                    start_index + i,
                );
                t.insert(root_addr, *root.root_hash())?;
            }
            Ok::<_, ShardTreeError<Self::Error>>(())
        })?;

        Ok(())
    }

    fn get_orchard_subtree_root(
        &mut self,
        index: u64,
    ) -> Result<Option<orchard::tree::MerkleHashOrchard>, ShardTreeError<Self::Error>> {
        let root_addr =
            incrementalmerkletree::Address::from_parts(ORCHARD_SHARD_HEIGHT.into(), index);
        Ok(self
            .shard_trees
            .orchard
            .store()
            .get_shard(root_addr)
            .map_err(ShardTreeError::Storage)?
            .and_then(|shard| shard.root_hash(root_addr.position_range_end()).ok()))
    }

    // The trait default answers `None`, which makes every step touching the
    // Ironwood pool fail with `ProposalNotSupported`. The trait hands out the
    // Orchard store type, so the Ironwood store is unwrapped for the call and
    // wrapped again after it.
    fn with_ironwood_tree_mut<F, A, E>(&mut self, mut callback: F) -> Result<Option<A>, E>
    where
        for<'a> F: FnMut(
            &'a mut ShardTree<
                Self::OrchardShardStore<'a>,
                { ORCHARD_SHARD_HEIGHT * 2 },
                ORCHARD_SHARD_HEIGHT,
            >,
        ) -> Result<A, E>,
        E: From<ShardTreeError<Self::Error>>,
    {
        let max_checkpoints = self.shard_trees.ironwood.max_checkpoints();
        let ironwood = std::mem::replace(
            &mut self.shard_trees.ironwood,
            ShardTree::new(IronwoodShardStore::empty(), max_checkpoints),
        );
        let mut tree = ShardTree::new(ironwood.into_store().into_inner(), max_checkpoints);
        let answer = callback(&mut tree);
        self.shard_trees.ironwood = ShardTree::new(tree.into_store().into(), max_checkpoints);
        answer.map(Some)
    }

    fn get_ironwood_subtree_root(
        &mut self,
        index: u64,
    ) -> Result<Option<orchard::tree::MerkleHashOrchard>, ShardTreeError<Self::Error>> {
        let root_addr =
            incrementalmerkletree::Address::from_parts(ORCHARD_SHARD_HEIGHT.into(), index);
        Ok(self
            .shard_trees
            .ironwood
            .store()
            .get_shard(root_addr)
            .map_err(ShardTreeError::Storage)?
            .and_then(|shard| shard.root_hash(root_addr.position_range_end()).ok()))
    }
}

impl InputSource for LightWallet {
    type Error = WalletError;
    type AccountId = zip32::AccountId;
    type NoteRef = OutputRef;

    fn get_spendable_note(
        &self,
        txid: &TxId,
        protocol: ShieldedPool,
        index: u32,
        target_height: TargetHeight,
        lock_filter: LockFilter<'_>,
    ) -> Result<Option<ReceivedNote<Self::NoteRef, Note>>, Self::Error> {
        let output_id = OutputId::new(*txid, index);
        let target_height = BlockHeight::from(target_height);

        Ok(match protocol {
            ShieldedPool::Sapling => self
                .spendable_note::<SaplingNote>(output_id, target_height, lock_filter)
                .map(|note| self.received_note(note, |note| Note::Sapling(note.clone()))),
            ShieldedPool::Orchard => self
                .spendable_note::<OrchardNote>(output_id, target_height, lock_filter)
                .map(|note| {
                    self.received_note(note, |note| Note::Orchard {
                        note: *note,
                        pool: orchard::ValuePool::Orchard,
                    })
                }),
            ShieldedPool::Ironwood => self
                .spendable_note::<IronwoodNote>(output_id, target_height, lock_filter)
                .map(|note| {
                    self.received_note(note, |note| Note::Orchard {
                        note: *note,
                        pool: orchard::ValuePool::Ironwood,
                    })
                }),
        })
    }

    fn select_spendable_notes(
        &self,
        account: Self::AccountId,
        target_value: TargetValue,
        sources: &[ShieldedPool],
        target_height: TargetHeight,
        confirmations_policy: ConfirmationsPolicy,
        exclude: &[Self::NoteRef],
        lock_filter: LockFilter<'_>,
    ) -> Result<ReceivedNotes<Self::NoteRef>, Self::Error> {
        let (_, anchor_height) = self
            .get_target_and_anchor_heights(confirmations_policy.trusted())
            .expect("infallible")
            .ok_or(WalletError::NoSyncData)?;
        let target_height = BlockHeight::from(target_height);

        let excluded_ids = |pool: PoolType| {
            exclude
                .iter()
                .filter(|&note_id| note_id.pool_type() == pool)
                .map(|note_id| OutputId::new(note_id.txid(), note_id.output_index()))
                .collect::<Vec<_>>()
        };
        let (ineligible_sapling, deferred_sapling) =
            self.lock_exclusions::<SaplingNote>(account, target_height, lock_filter);
        let (ineligible_orchard, deferred_orchard) =
            self.lock_exclusions::<OrchardNote>(account, target_height, lock_filter);
        let (ineligible_ironwood, deferred_ironwood) =
            self.lock_exclusions::<IronwoodNote>(account, target_height, lock_filter);

        let mut exclude_sapling = [excluded_ids(PoolType::SAPLING), ineligible_sapling].concat();
        let mut exclude_orchard = [excluded_ids(PoolType::ORCHARD), ineligible_orchard].concat();
        let mut exclude_ironwood = [excluded_ids(PoolType::IRONWOOD), ineligible_ironwood].concat();

        let (selected_sapling_notes, selected_orchard_notes, selected_ironwood_notes) =
            match target_value {
                TargetValue::AtLeast(at_least_value) => {
                    let mut remaining_value_needed = RemainingNeeded::Positive(at_least_value);

                    // prioritises selecting spendable notes that are guaranteed to be unspent first
                    let mut selected_sapling_notes = Vec::new();
                    let mut selected_orchard_notes = Vec::new();
                    let mut selected_ironwood_notes = Vec::new();

                    let has_deferred = !(deferred_sapling.is_empty()
                        && deferred_orchard.is_empty()
                        && deferred_ironwood.is_empty());
                    let tiers: &[bool] = if has_deferred {
                        &[true, false]
                    } else {
                        &[false]
                    };
                    for &defer_locked in tiers {
                        if defer_locked {
                            exclude_sapling.extend(&deferred_sapling);
                            exclude_orchard.extend(&deferred_orchard);
                            exclude_ironwood.extend(&deferred_ironwood);
                        } else {
                            exclude_sapling.retain(|id| !deferred_sapling.contains(id));
                            exclude_orchard.retain(|id| !deferred_orchard.contains(id));
                            exclude_ironwood.retain(|id| !deferred_ironwood.contains(id));
                        }

                        for include_potentially_spent_notes in [false, true] {
                            // prioritise note selection for the given `sources`
                            if sources.contains(&ShieldedPool::Sapling) {
                                let notes = self
                                    .select_spendable_notes_by_pool::<SaplingNote>(
                                        &mut remaining_value_needed,
                                        anchor_height,
                                        &exclude_sapling,
                                        account,
                                        include_potentially_spent_notes,
                                    )?
                                    .into_iter()
                                    .cloned()
                                    .collect::<Vec<_>>();
                                exclude_sapling
                                    .extend(notes.iter().map(OutputInterface::output_id));
                                selected_sapling_notes.extend(notes);
                            }
                            if sources.contains(&ShieldedPool::Orchard) {
                                let notes = self
                                    .select_spendable_notes_by_pool::<OrchardNote>(
                                        &mut remaining_value_needed,
                                        anchor_height,
                                        &exclude_orchard,
                                        account,
                                        include_potentially_spent_notes,
                                    )?
                                    .into_iter()
                                    .cloned()
                                    .collect::<Vec<_>>();
                                exclude_orchard
                                    .extend(notes.iter().map(OutputInterface::output_id));
                                selected_orchard_notes.extend(notes);
                            }
                            if sources.contains(&ShieldedPool::Ironwood) {
                                let notes = self
                                    .select_spendable_notes_by_pool::<IronwoodNote>(
                                        &mut remaining_value_needed,
                                        anchor_height,
                                        &exclude_ironwood,
                                        account,
                                        include_potentially_spent_notes,
                                    )?
                                    .into_iter()
                                    .cloned()
                                    .collect::<Vec<_>>();
                                exclude_ironwood
                                    .extend(notes.iter().map(OutputInterface::output_id));
                                selected_ironwood_notes.extend(notes);
                            }

                            let notes = self
                                .select_spendable_notes_by_pool::<SaplingNote>(
                                    &mut remaining_value_needed,
                                    anchor_height,
                                    &exclude_sapling,
                                    account,
                                    include_potentially_spent_notes,
                                )?
                                .into_iter()
                                .cloned()
                                .collect::<Vec<_>>();
                            exclude_sapling.extend(notes.iter().map(OutputInterface::output_id));
                            selected_sapling_notes.extend(notes);

                            let notes = self
                                .select_spendable_notes_by_pool::<OrchardNote>(
                                    &mut remaining_value_needed,
                                    anchor_height,
                                    &exclude_orchard,
                                    account,
                                    include_potentially_spent_notes,
                                )?
                                .into_iter()
                                .cloned()
                                .collect::<Vec<_>>();
                            exclude_orchard.extend(notes.iter().map(OutputInterface::output_id));
                            selected_orchard_notes.extend(notes);

                            let notes = self
                                .select_spendable_notes_by_pool::<IronwoodNote>(
                                    &mut remaining_value_needed,
                                    anchor_height,
                                    &exclude_ironwood,
                                    account,
                                    include_potentially_spent_notes,
                                )?
                                .into_iter()
                                .cloned()
                                .collect::<Vec<_>>();
                            exclude_ironwood.extend(notes.iter().map(OutputInterface::output_id));
                            selected_ironwood_notes.extend(notes);
                        }
                    }
                    (
                        selected_sapling_notes,
                        selected_orchard_notes,
                        selected_ironwood_notes,
                    )
                }
                TargetValue::AllFunds(max_spend_mode) => {
                    // FIXME: this is not the criteria for `MaxSpendMode::Everything`. this should return an error if sync is not complete in this case.
                    let include_potentially_spent_notes = matches!(
                        max_spend_mode,
                        zcash_client_backend::data_api::MaxSpendMode::Everything
                    );
                    (
                        // FIXME: note filters implemented in `spendable_notes_by_pool` have been missed here such as filtering dust
                        self.spendable_notes::<SaplingNote>(
                            anchor_height,
                            &exclude_sapling,
                            account,
                            include_potentially_spent_notes,
                        )?
                        .into_iter()
                        .cloned()
                        .collect::<Vec<_>>(),
                        self.spendable_notes::<OrchardNote>(
                            anchor_height,
                            &exclude_orchard,
                            account,
                            include_potentially_spent_notes,
                        )?
                        .into_iter()
                        .cloned()
                        .collect::<Vec<_>>(),
                        self.spendable_notes::<IronwoodNote>(
                            anchor_height,
                            &exclude_ironwood,
                            account,
                            include_potentially_spent_notes,
                        )?
                        .into_iter()
                        .cloned()
                        .collect::<Vec<_>>(),
                    )
                }
            };

        /* TODO: Priority
        if selected
            .iter()
            .filter(|n| n.0.protocol() == ShieldedPool::Sapling)
            .count()
            == 1
            || selected
                .iter()
                .filter(|n| n.0.protocol() == ShieldedPool::Orchard)
                .count()
                == 1
        {
            // since we maxed out the target value with only one note in at least one Shielded Pool
            //  we have an option to sweep a dust note into a grace input.
            // we will sweep the biggest dust note we can
            if !dust_notes.is_empty() {
                sweep_dust_into_grace(&mut selected, dust_notes);
            }
            // TODO: re-introduce this optimisation, current bug is that we don't select a note from the same pool as the single selected note
            // (and we don't have information about the pool(s) the outputs are being created for)
            // this is ok for dust as it is excluded if the dust is from a pool where grace inputs are available. however, this doesn't work for
            // non-dust
            //
            // } else {
            //     // we have no extra dust, but we can still save a marginal fee by adding the next smallest note to change
            //     if let Some(smallest_note) = unselected.pop() {
            //         selected.push(smallest_note);
            //     };
            // }
        }
        */

        Ok(ReceivedNotes::new(
            selected_sapling_notes
                .iter()
                .map(|note| self.received_note(note, Clone::clone))
                .collect(),
            selected_orchard_notes
                .iter()
                .map(|note| self.received_note(note, |note| *note))
                .collect(),
            selected_ironwood_notes
                .iter()
                .map(|note| self.received_note(note, |note| *note))
                .collect(),
        ))
    }

    fn get_account_metadata(
        &self,
        account: Self::AccountId,
        selector: &NoteFilter,
        target_height: TargetHeight,
        exclude: &[Self::NoteRef],
        lock_filter: LockFilter<'_>,
    ) -> Result<AccountMeta, Self::Error> {
        let target_height = BlockHeight::from(target_height);
        let Some(min_value) = self.note_filter_min_value(account, selector, target_height)? else {
            return Ok(AccountMeta::new(None, None, None));
        };

        Ok(AccountMeta::new(
            Some(self.pool_meta::<SaplingNote>(
                account,
                min_value,
                target_height,
                exclude,
                lock_filter,
            )?),
            Some(self.pool_meta::<OrchardNote>(
                account,
                min_value,
                target_height,
                exclude,
                lock_filter,
            )?),
            Some(self.pool_meta::<IronwoodNote>(
                account,
                min_value,
                target_height,
                exclude,
                lock_filter,
            )?),
        ))
    }

    fn get_unspent_transparent_output(
        &self,
        outpoint: &OutPoint,
        target_height: TargetHeight,
    ) -> Result<Option<WalletTransparentOutput<Self::AccountId>>, Self::Error> {
        let output_id = OutputId::from(outpoint);
        let target_height = BlockHeight::from(target_height);
        let Some(transaction) = self.wallet_transactions.get(&output_id.txid()) else {
            return Ok(None);
        };
        let Some(coin) = find_output::<TransparentCoin>(transaction, output_id) else {
            return Ok(None);
        };
        let additional_confirmations = transaction
            .transaction()
            .transparent_bundle()
            .map_or(0, |bundle| if bundle.is_coinbase() { 100 } else { 0 });
        if !self.output_spend_status(coin).is_unspent()
            || !transaction
                .status()
                .is_confirmed_before(&(target_height - additional_confirmations))
        {
            return Ok(None);
        }

        Ok(WalletTransparentOutput::from_parts(
            outpoint.clone(),
            TxOut::new(Zatoshis::from_u64(coin.value())?, coin.script().clone()),
            transaction.status().get_confirmed_height(),
            Some(coin.key_id().account_id()),
            Some(coin.key_id().scope().into()),
            None,
        ))
    }

    fn get_spendable_transparent_outputs(
        &self,
        address: &TransparentAddress,
        target_height: TargetHeight,
        confirmations_policy: ConfirmationsPolicy,
        _output_filter: CoinbaseFilter,
        lock_filter: LockFilter<'_>,
    ) -> Result<Vec<WalletTransparentOutput<Self::AccountId>>, Self::Error> {
        let address = transparent::encode_address(&self.chain_type, *address);
        let target_height = BlockHeight::from(target_height);

        Ok(self
            .spendable_transparent_coins(
                target_height,
                confirmations_policy.allow_zero_conf_shielding(),
                false,
            )
            .into_iter()
            .filter(|&output| output.address() == address)
            .filter(|&output| {
                self.output_locks.eligible(
                    &OutputRef::new(output.output_id(), PoolType::Transparent),
                    target_height,
                    lock_filter,
                )
            })
            .filter_map(|output| {
                WalletTransparentOutput::from_parts(
                    output.output_id().into(),
                    TxOut::new(
                        output.value().try_into().expect("value from checked type"),
                        output.script().clone(),
                    ),
                    Some(
                        self.output_transaction(output)
                            .status()
                            .get_confirmed_height()
                            .expect("output must be confirmed in this scope"),
                    ),
                    Some(output.key_id().account_id()),
                    Some(output.key_id().scope().into()),
                    None,
                )
            })
            .collect())
    }

    fn select_unspent_notes(
        &self,
        account: Self::AccountId,
        sources: &[ShieldedPool],
        target_height: TargetHeight,
        exclude: &[Self::NoteRef],
        lock_filter: LockFilter<'_>,
    ) -> Result<ReceivedNotes<Self::NoteRef>, Self::Error> {
        let target_height = BlockHeight::from(target_height);

        Ok(ReceivedNotes::new(
            if sources.contains(&ShieldedPool::Sapling) {
                self.unspent_received_notes::<SaplingNote, _>(
                    account,
                    target_height,
                    exclude,
                    lock_filter,
                    Clone::clone,
                )
            } else {
                Vec::new()
            },
            if sources.contains(&ShieldedPool::Orchard) {
                self.unspent_received_notes::<OrchardNote, _>(
                    account,
                    target_height,
                    exclude,
                    lock_filter,
                    |note| *note,
                )
            } else {
                Vec::new()
            },
            if sources.contains(&ShieldedPool::Ironwood) {
                self.unspent_received_notes::<IronwoodNote, _>(
                    account,
                    target_height,
                    exclude,
                    lock_filter,
                    |note| *note,
                )
            } else {
                Vec::new()
            },
        ))
    }
}
