//! Types and impls for conveniently displaying information to the user or converting to JSON for interfacing with a larger stack.
use std::collections::HashSet;
/// A "snapshot" of the state of the items in the wallet at the time the summary was constructed.
/// Not to be used for internal logic in the system.
use std::{cmp::Ordering, collections::HashMap};

use zcash_protocol::PoolType;
use zcash_protocol::memo::Memo;

use pepper_sync::keys::transparent;
use pepper_sync::wallet::{
    KeyIdInterface, NoteInterface, OutgoingNoteInterface, OutputInterface, TransparentCoin,
    WalletTransaction,
};
use zcash_primitives::transaction::TxId;

use super::LightWallet;
use super::error::{KeyError, SummaryError};
use data::finsight::{
    TotalMemoBytesToAddress, TotalSendsToAddress, TotalValueToAddress, ValuesSentToAddress,
};
use data::{
    BasicCoinSummary, BasicNoteSummary, CoinSummary, NoteSummaries, NoteSummary,
    OutgoingCoinSummary, OutgoingNoteSummary, Scope, SelfSendWalletEvent, SendType,
    SentWalletEvent, TransactionKind, TransactionSummaries, TransactionSummary, WalletEvent,
    WalletEventKind, WalletEvents,
};

pub mod data;

impl LightWallet {
    /// Returns summaries of all transactions in the wallet, sorted by confirmation status (confirmed first) and then
    /// blockheight (lowest first).
    ///
    /// `reverse_sort` will sort by unconfirmed first and then highest first.
    pub async fn transaction_summaries(
        &self,
        reverse_sort: bool,
    ) -> Result<TransactionSummaries, SummaryError> {
        let mut transaction_summaries = self
            .wallet_transactions
            .values()
            .map(|transaction| self.build_transaction_summary(transaction))
            .collect::<Result<Vec<_>, SummaryError>>()?;

        transaction_summaries.sort_by(|summary_a, summary_b| {
            match summary_a.status.cmp(&summary_b.status) {
                Ordering::Equal => {
                    // TODO: order tex transactions correctly by checking inputs / outputs are the wallet's refund addresses
                    summary_a.txid.cmp(&summary_b.txid)
                }
                otherwise => otherwise,
            }
        });

        if reverse_sort {
            transaction_summaries.reverse();
        }

        Ok(TransactionSummaries::new(transaction_summaries))
    }

    /// Returns a summary of the wallet transaction with the given `txid`, or `None` if the
    /// transaction is not in the wallet.
    pub fn transaction_summary(
        &self,
        txid: TxId,
    ) -> Result<Option<TransactionSummary>, SummaryError> {
        self.wallet_transactions
            .get(&txid)
            .map(|transaction| self.build_transaction_summary(transaction))
            .transpose()
    }

    fn build_transaction_summary(
        &self,
        transaction: &WalletTransaction,
    ) -> Result<TransactionSummary, SummaryError> {
        let kind = self.transaction_kind(transaction)?;
        let value = match kind {
            TransactionKind::Received | TransactionKind::Sent(SendType::Shield) => {
                transaction.total_value_received()
            }
            TransactionKind::Sent(SendType::Send) => transaction.total_value_sent(),
            TransactionKind::Sent(SendType::SendToSelf) => {
                intended_value(&SelfIntent::SendToSelf, &self_outputs(transaction))
            }
            TransactionKind::Sent(SendType::PoolMove { .. }) => self
                .pools_moved_into(transaction)?
                .iter()
                .map(|(_, value)| value)
                .sum(),
        };
        let fee: Option<u64> = self
            .calculate_transaction_fee(transaction)
            .ok()
            .map(zcash_protocol::value::Zatoshis::into_u64);
        let ironwood_notes = transaction
            .ironwood_notes()
            .iter()
            .map(|output| {
                let spend_status = self.output_spend_status(output);

                let memo = text_memo(output.memo());

                BasicNoteSummary::from_parts(
                    output.value(),
                    spend_status,
                    output.output_id().output_index(),
                    memo,
                )
            })
            .collect::<Vec<_>>();
        let orchard_notes = transaction
            .orchard_notes()
            .iter()
            .map(|output| {
                let spend_status = self.output_spend_status(output);

                let memo = text_memo(output.memo());

                BasicNoteSummary::from_parts(
                    output.value(),
                    spend_status,
                    output.output_id().output_index(),
                    memo,
                )
            })
            .collect::<Vec<_>>();
        let sapling_notes = transaction
            .sapling_notes()
            .iter()
            .map(|output| {
                let spend_status = self.output_spend_status(output);

                let memo = text_memo(output.memo());

                BasicNoteSummary::from_parts(
                    output.value(),
                    spend_status,
                    output.output_id().output_index(),
                    memo,
                )
            })
            .collect::<Vec<_>>();
        let transparent_coins = transaction
            .transparent_coins()
            .iter()
            .map(|output| {
                let spend_status = self.output_spend_status(output);

                BasicCoinSummary::from_parts(
                    output.value(),
                    spend_status,
                    output.output_id().output_index(),
                )
            })
            .collect::<Vec<_>>();

        let outgoing_ironwood_notes = transaction
            .outgoing_ironwood_notes()
            .iter()
            .map(|note| {
                let memo = text_memo(note.memo());

                Ok(OutgoingNoteSummary {
                    memo,
                    value: note.value(),
                    recipient: note
                        .encoded_recipient(&self.chain_type)
                        .map_err(zcash_address::ParseError::Unified)?,
                    recipient_unified_address: note
                        .encoded_recipient_full_unified_address(&self.chain_type),
                    output_index: note.output_id().output_index(),
                    account_id: note.key_id().account_id,
                    scope: Scope::from(note.key_id().scope),
                })
            })
            .collect::<Result<Vec<_>, SummaryError>>()?;
        let outgoing_orchard_notes = transaction
            .outgoing_orchard_notes()
            .iter()
            .map(|note| {
                let memo = text_memo(note.memo());

                Ok(OutgoingNoteSummary {
                    memo,
                    value: note.value(),
                    recipient: note
                        .encoded_recipient(&self.chain_type)
                        .map_err(zcash_address::ParseError::Unified)?,
                    recipient_unified_address: note
                        .encoded_recipient_full_unified_address(&self.chain_type),
                    output_index: note.output_id().output_index(),
                    account_id: note.key_id().account_id,
                    scope: Scope::from(note.key_id().scope),
                })
            })
            .collect::<Result<Vec<_>, SummaryError>>()?;
        let outgoing_sapling_notes = transaction
            .outgoing_sapling_notes()
            .iter()
            .map(|note| {
                let memo = text_memo(note.memo());

                OutgoingNoteSummary {
                    output_index: note.output_id().output_index(),
                    memo,
                    value: note.value(),
                    recipient: note
                        .encoded_recipient(&self.chain_type)
                        .expect("infallible"),
                    recipient_unified_address: note
                        .encoded_recipient_full_unified_address(&self.chain_type),
                    account_id: note.key_id().account_id,
                    scope: Scope::from(note.key_id().scope),
                }
            })
            .collect::<Vec<_>>();
        let outgoing_transparent_coins = if kind == TransactionKind::Received {
            Vec::new()
        } else {
            transaction
                .transaction()
                .transparent_bundle()
                .map_or(Vec::new(), |bundle| {
                    bundle
                        .vout
                        .iter()
                        .enumerate()
                        .filter_map(|(output_index, transparent_output)| {
                            transparent_output.recipient_address().map(|address| {
                                OutgoingCoinSummary {
                                    value: transparent_output.value().into_u64(),
                                    recipient: transparent::encode_address(
                                        &self.chain_type,
                                        address,
                                    ),
                                    output_index: output_index
                                        .try_into()
                                        .expect("output index should be valid u32"),
                                }
                            })
                        })
                        .collect()
                })
        };

        // add price to transaction summary
        // takes price from the day of transaction's datetime. otherwise, current price.
        // TODO: historical prices currently unimplemented
        // let mut price = None;
        // for daily_price in self.price_list.daily_prices() {
        //     if daily_price.time > transaction.datetime() {
        //         assert!(daily_price.time - transaction.datetime() < 24 * 60 * 60);
        //         price = Some(daily_price.price_usd);
        //         break;
        //     }
        // }
        // if price.is_none() {
        //     price = self.price_list.current_price().and_then(|current_price| {
        //         if transaction.datetime() <= current_price.time
        //             && transaction.datetime() > current_price.time - 2 * 24 * 60 * 60
        //         // exchange APIs may start daily prices 2 days back
        //         {
        //             Some(current_price.price_usd)
        //         } else {
        //             None
        //         }
        //     });
        // }

        Ok(TransactionSummary {
            txid: transaction.txid(),
            datetime: transaction.datetime(),
            status: transaction.status(),
            blockheight: transaction.status().get_height(),
            kind,
            value,
            fee,
            zec_price: None,
            ironwood_notes,
            orchard_notes,
            sapling_notes,
            transparent_coins,
            outgoing_ironwood_notes,
            outgoing_orchard_notes,
            outgoing_sapling_notes,
            outgoing_transparent_coins,
        })
    }

    /// Provides the wallet events related to this capability.
    /// Transaction-backed events group notes sent to the same receiver.
    pub async fn wallet_events(
        &self,
        sort_highest_to_lowest: bool,
    ) -> Result<WalletEvents, SummaryError> {
        let mut wallet_events: Vec<WalletEvent> = Vec::new();
        let transaction_summaries = self.transaction_summaries(sort_highest_to_lowest).await?.0;

        for transaction in transaction_summaries {
            match transaction.kind {
                TransactionKind::Sent(SendType::Send) => {
                    // Create one sent event for each non-self recipient address.
                    // if recipient_ua is available it overrides recipient_address
                    wallet_events.append(&mut self.create_send_wallet_events(&transaction)?);
                    // and 1 per pool for whatever the transaction also sent to the wallet itself
                    wallet_events.append(&mut self.create_self_wallet_events(&transaction)?);
                }
                TransactionKind::Sent(SendType::Shield) => {
                    // Create one shielding event for each receiving pool.
                    if !transaction.ironwood_notes.is_empty() {
                        let value: u64 = transaction
                            .ironwood_notes
                            .iter()
                            .map(|output| output.value)
                            .sum();
                        let memos: Vec<String> = transaction
                            .ironwood_notes
                            .iter()
                            .filter_map(|note| note.memo.clone())
                            .collect();
                        wallet_events.push(WalletEvent {
                            txid: transaction.txid,
                            datetime: transaction.datetime,
                            status: transaction.status,
                            blockheight: transaction.blockheight,
                            transaction_fee: transaction.fee,
                            zec_price: transaction.zec_price,
                            kind: WalletEventKind::Sent(SentWalletEvent::SendToSelf(
                                SelfSendWalletEvent::Shield,
                            )),
                            value,
                            recipient_address: None,
                            pool_received: Some(PoolType::IRONWOOD.to_string()),
                            memos,
                        });
                    }
                    if !transaction.orchard_notes.is_empty() {
                        let value: u64 = transaction
                            .orchard_notes
                            .iter()
                            .map(|output| output.value)
                            .sum();
                        let memos: Vec<String> = transaction
                            .orchard_notes
                            .iter()
                            .filter_map(|note| note.memo.clone())
                            .collect();
                        wallet_events.push(WalletEvent {
                            txid: transaction.txid,
                            datetime: transaction.datetime,
                            status: transaction.status,
                            blockheight: transaction.blockheight,
                            transaction_fee: transaction.fee,
                            zec_price: transaction.zec_price,
                            kind: WalletEventKind::Sent(SentWalletEvent::SendToSelf(
                                SelfSendWalletEvent::Shield,
                            )),
                            value,
                            recipient_address: None,
                            pool_received: Some(PoolType::ORCHARD.to_string()),
                            memos,
                        });
                    }
                    if !transaction.sapling_notes.is_empty() {
                        let value: u64 = transaction
                            .sapling_notes
                            .iter()
                            .map(|output| output.value)
                            .sum();
                        let memos: Vec<String> = transaction
                            .sapling_notes
                            .iter()
                            .filter_map(|note| note.memo.clone())
                            .collect();
                        wallet_events.push(WalletEvent {
                            txid: transaction.txid,
                            datetime: transaction.datetime,
                            status: transaction.status,
                            blockheight: transaction.blockheight,
                            transaction_fee: transaction.fee,
                            zec_price: transaction.zec_price,
                            kind: WalletEventKind::Sent(SentWalletEvent::SendToSelf(
                                SelfSendWalletEvent::Shield,
                            )),
                            value,
                            recipient_address: None,
                            pool_received: Some(PoolType::SAPLING.to_string()),
                            memos,
                        });
                    }
                }
                TransactionKind::Sent(SendType::SendToSelf | SendType::PoolMove { .. }) => {
                    // One event per pool the wallet sent to itself, carrying the real value.
                    let mut self_events = self.create_self_wallet_events(&transaction)?;
                    if self_events.is_empty() {
                        // Every transaction creates at least one wallet event.
                        self_events.push(WalletEvent {
                            txid: transaction.txid,
                            datetime: transaction.datetime,
                            status: transaction.status,
                            blockheight: transaction.blockheight,
                            transaction_fee: transaction.fee,
                            zec_price: transaction.zec_price,
                            kind: WalletEventKind::Sent(SentWalletEvent::SendToSelf(
                                SelfSendWalletEvent::Basic,
                            )),
                            value: 0,
                            recipient_address: None,
                            pool_received: None,
                            memos: Vec::new(),
                        });
                    }
                    wallet_events.append(&mut self_events);

                    // in the case Zennies For Zingo! is active
                    wallet_events.append(&mut self.create_send_wallet_events(&transaction)?);
                }
                TransactionKind::Received => {
                    // Create one received event for each receiving pool.
                    if !transaction.ironwood_notes.is_empty() {
                        let value: u64 = transaction
                            .ironwood_notes
                            .iter()
                            .map(|output| output.value)
                            .sum();
                        let memos: Vec<String> = transaction
                            .ironwood_notes
                            .iter()
                            .filter_map(|note| note.memo.clone())
                            .collect();
                        wallet_events.push(WalletEvent {
                            txid: transaction.txid,
                            datetime: transaction.datetime,
                            status: transaction.status,
                            blockheight: transaction.blockheight,
                            transaction_fee: transaction.fee,
                            zec_price: transaction.zec_price,
                            kind: WalletEventKind::Received,
                            value,
                            recipient_address: None,
                            pool_received: Some(PoolType::IRONWOOD.to_string()),
                            memos,
                        });
                    }
                    if !transaction.orchard_notes.is_empty() {
                        let value: u64 = transaction
                            .orchard_notes
                            .iter()
                            .map(|output| output.value)
                            .sum();
                        let memos: Vec<String> = transaction
                            .orchard_notes
                            .iter()
                            .filter_map(|note| note.memo.clone())
                            .collect();
                        wallet_events.push(WalletEvent {
                            txid: transaction.txid,
                            datetime: transaction.datetime,
                            status: transaction.status,
                            blockheight: transaction.blockheight,
                            transaction_fee: transaction.fee,
                            zec_price: transaction.zec_price,
                            kind: WalletEventKind::Received,
                            value,
                            recipient_address: None,
                            pool_received: Some(PoolType::ORCHARD.to_string()),
                            memos,
                        });
                    }
                    if !transaction.sapling_notes.is_empty() {
                        let value: u64 = transaction
                            .sapling_notes
                            .iter()
                            .map(|output| output.value)
                            .sum();
                        let memos: Vec<String> = transaction
                            .sapling_notes
                            .iter()
                            .filter_map(|note| note.memo.clone())
                            .collect();
                        wallet_events.push(WalletEvent {
                            txid: transaction.txid,
                            datetime: transaction.datetime,
                            status: transaction.status,
                            blockheight: transaction.blockheight,
                            transaction_fee: transaction.fee,
                            zec_price: transaction.zec_price,
                            kind: WalletEventKind::Received,
                            value,
                            recipient_address: None,
                            pool_received: Some(PoolType::SAPLING.to_string()),
                            memos,
                        });
                    }
                    if !transaction.transparent_coins.is_empty() {
                        let value: u64 = transaction
                            .transparent_coins
                            .iter()
                            .map(|output| output.value)
                            .sum();
                        wallet_events.push(WalletEvent {
                            txid: transaction.txid,
                            datetime: transaction.datetime,
                            status: transaction.status,
                            blockheight: transaction.blockheight,
                            transaction_fee: transaction.fee,
                            zec_price: transaction.zec_price,
                            kind: WalletEventKind::Received,
                            value,
                            recipient_address: None,
                            pool_received: Some(PoolType::TRANSPARENT.to_string()),
                            memos: Vec::new(),
                        });
                    }
                }
            }
        }

        Ok(WalletEvents::new(wallet_events))
    }

    #[must_use]
    pub fn note_summaries<N>(&self, include_spent_notes: bool) -> NoteSummaries
    where
        N: NoteInterface<KeyId = pepper_sync::keys::KeyId>,
    {
        let note_summaries = self
            .wallet_outputs::<N>()
            .into_iter()
            .filter(|&note| {
                if include_spent_notes {
                    true
                } else {
                    note.spending_transaction().is_none()
                }
            })
            .map(|note| {
                let memo = text_memo(note.memo());
                let transaction = self.output_transaction(note);

                NoteSummary {
                    value: note.value(),
                    status: transaction.status(),
                    block_height: transaction.status().get_height(),
                    spend_status: self.output_spend_status(note),
                    memo,
                    time: transaction.datetime(),
                    txid: note.output_id().txid(),
                    output_index: note.output_id().output_index(),
                    account_id: note.key_id().account_id,
                    scope: note.key_id().scope.into(),
                }
            })
            .collect();

        NoteSummaries::new(note_summaries)
    }

    #[must_use]
    pub fn coin_summaries(&self, include_spent_coins: bool) -> Vec<CoinSummary> {
        self.wallet_outputs::<TransparentCoin>()
            .into_iter()
            .filter(|&coin| {
                if include_spent_coins {
                    true
                } else {
                    coin.spending_transaction().is_none()
                }
            })
            .map(|coin| {
                let transaction = self.output_transaction(coin);

                CoinSummary {
                    value: coin.value(),
                    status: transaction.status(),
                    block_height: transaction.status().get_height(),
                    spend_status: self.output_spend_status(coin),
                    time: transaction.datetime(),
                    txid: coin.output_id().txid(),
                    output_index: coin.output_id().output_index(),
                    account_id: coin.key_id().account_id(),
                    scope: coin.key_id().scope(),
                    address_index: coin.key_id().address_index().index(),
                }
            })
            .collect()
    }

    /// Provides wallet events associated with the sender, or containing the string.
    pub async fn messages_containing(
        &self,
        filter: Option<&str>,
    ) -> Result<WalletEvents, SummaryError> {
        let mut wallet_events = self.wallet_events(true).await?;
        wallet_events.reverse();

        // Filter out events where all memos are empty.
        wallet_events.retain(|event| event.memos.iter().any(|memo| !memo.is_empty()));

        match filter {
            Some(s) => {
                wallet_events.retain(|event| {
                    if event.memos.is_empty() {
                        return false;
                    }

                    if event.recipient_address == Some(s.to_string()) {
                        true
                    } else {
                        for memo in &event.memos {
                            if memo.contains(s) {
                                return true;
                            }
                        }
                        false
                    }
                });
            }
            None => wallet_events.retain(|event| !event.memos.is_empty()),
        }

        Ok(wallet_events)
    }

    /// TODO: Add Doc Comment Here!
    pub async fn do_total_memobytes_to_address(
        &self,
    ) -> Result<TotalMemoBytesToAddress, SummaryError> {
        let wallet_events = self.wallet_events(true).await?;
        let mut memobytes_by_address = HashMap::new();
        for wallet_event in &wallet_events {
            if let WalletEventKind::Sent(SentWalletEvent::Send) = wallet_event.kind {
                let address = wallet_event
                    .recipient_address
                    .clone()
                    .expect("sent wallet event should always have a recipient_address");
                let bytes = wallet_event.memos.iter().fold(0, |sum, m| sum + m.len());
                memobytes_by_address
                    .entry(address)
                    .and_modify(|e| *e += bytes)
                    .or_insert(bytes);
            }
        }
        Ok(TotalMemoBytesToAddress(memobytes_by_address))
    }

    /// TODO: Add Doc Comment Here!
    pub async fn do_total_spends_to_address(&self) -> Result<TotalSendsToAddress, SummaryError> {
        let values_sent_to_addresses = self.wallet_events_by_to_address().await?;
        let mut by_address_number_sends = HashMap::new();
        for key in values_sent_to_addresses.0.keys() {
            let number_sends = values_sent_to_addresses.0[key].len() as u64;
            by_address_number_sends.insert(key.clone(), number_sends);
        }

        Ok(TotalSendsToAddress(by_address_number_sends))
    }

    /// TODO: Add Doc Comment Here!
    pub async fn do_total_value_to_address(&self) -> Result<TotalValueToAddress, SummaryError> {
        let values_sent_to_addresses = self.wallet_events_by_to_address().await?;
        let mut by_address_total = HashMap::new();
        for key in values_sent_to_addresses.0.keys() {
            let sum = values_sent_to_addresses.0[key].iter().sum();
            by_address_total.insert(key.clone(), sum);
        }

        Ok(TotalValueToAddress(by_address_total))
    }

    async fn wallet_events_by_to_address(&self) -> Result<ValuesSentToAddress, SummaryError> {
        let wallet_events = self.wallet_events(false).await?;
        let mut amount_by_address = HashMap::new();
        for wallet_event in &wallet_events {
            if let WalletEventKind::Sent(SentWalletEvent::Send) = wallet_event.kind {
                let address = wallet_event
                    .recipient_address
                    .clone()
                    .expect("sent wallet event should always have a recipient_address");
                amount_by_address
                    .entry(address)
                    .and_modify(|e: &mut Vec<u64>| e.push(wallet_event.value))
                    .or_insert(vec![wallet_event.value]);
            }
        }

        Ok(ValuesSentToAddress(amount_by_address))
    }

    /// Creates wallet events for all notes in a transaction sent to another recipient.
    /// Each event groups notes sent to the same receiver and follows output index order.
    /// Events for what `transaction` sent back to the wallet: one per pool, each carrying
    /// the full value of the outputs it stands for.
    ///
    /// Which outputs count depends on what the transaction did, see [`SelfOutput`].
    fn create_self_wallet_events(
        &self,
        transaction: &TransactionSummary,
    ) -> Result<Vec<WalletEvent>, SummaryError> {
        let Some(wallet_transaction) = self.wallet_transactions.get(&transaction.txid) else {
            return Ok(Vec::new());
        };
        let outputs = self_outputs(wallet_transaction);
        let intent = match transaction.kind {
            TransactionKind::Sent(SendType::PoolMove { .. }) => SelfIntent::Move(
                self.pools_moved_into(wallet_transaction)?
                    .into_iter()
                    .map(|(pool, _)| pool)
                    .collect(),
            ),
            TransactionKind::Sent(SendType::SendToSelf) => SelfIntent::SendToSelf,
            _ => SelfIntent::Payment,
        };
        Ok(group_self_outputs(&intent, &outputs)
            .into_iter()
            .map(|group| WalletEvent {
                txid: transaction.txid,
                datetime: transaction.datetime,
                status: transaction.status,
                blockheight: transaction.blockheight,
                transaction_fee: transaction.fee,
                zec_price: transaction.zec_price,
                kind: WalletEventKind::Sent(SentWalletEvent::SendToSelf(group.kind)),
                value: group.value,
                recipient_address: None,
                pool_received: Some(group.pool.to_string()),
                memos: group.memos,
            })
            .collect())
    }

    fn create_send_wallet_events(
        &self,
        transaction: &TransactionSummary,
    ) -> Result<Vec<WalletEvent>, KeyError> {
        let mut wallet_events: Vec<WalletEvent> = Vec::new();
        let outgoing_notes = transaction
            .outgoing_ironwood_notes
            .iter()
            .chain(transaction.outgoing_orchard_notes.iter())
            .chain(transaction.outgoing_sapling_notes.iter())
            .collect::<Vec<_>>();
        let outgoing_coins = &transaction.outgoing_transparent_coins;
        let mut addresses = HashSet::new();

        outgoing_notes.iter().try_for_each(|&note| {
            if note.scope == Scope::External && self.is_wallet_address(&note.recipient)?.is_none() {
                let encoded_address = note
                    .recipient_unified_address
                    .clone()
                    .unwrap_or(note.recipient.clone());
                addresses.insert(encoded_address);
            }

            Ok::<(), KeyError>(())
        })?;
        outgoing_coins.iter().try_for_each(|coin| {
            if self.is_wallet_address(&coin.recipient)?.is_none() {
                addresses.insert(coin.recipient.clone());
            }

            Ok::<(), KeyError>(())
        })?;
        let mut addresses = addresses.into_iter().collect::<Vec<_>>();
        addresses.sort();
        for address in addresses {
            let outgoing_notes_to_address: Vec<&OutgoingNoteSummary> = outgoing_notes
                .iter()
                .filter(|&&note| {
                    let query_address = if let Some(ua) = note.recipient_unified_address.clone() {
                        ua
                    } else {
                        note.recipient.clone()
                    };
                    query_address == address
                })
                .copied()
                .collect();
            let outgoing_coins_to_address: Vec<&OutgoingCoinSummary> = outgoing_coins
                .iter()
                .filter(|&coin| coin.recipient.clone() == address)
                .collect();
            let value: u64 = outgoing_notes_to_address
                .iter()
                .map(|&note| note.value)
                .chain(outgoing_coins_to_address.iter().map(|&coin| coin.value))
                .sum();
            let memos: Vec<String> = outgoing_notes_to_address
                .iter()
                .filter_map(|&note| note.memo.clone())
                .collect();
            wallet_events.push(WalletEvent {
                txid: transaction.txid,
                datetime: transaction.datetime,
                status: transaction.status,
                blockheight: transaction.blockheight,
                transaction_fee: transaction.fee,
                zec_price: transaction.zec_price,
                kind: WalletEventKind::Sent(SentWalletEvent::Send),
                value,
                recipient_address: Some(address),
                pool_received: None,
                memos,
            });
        }

        Ok(wallet_events)
    }
}

/// A text memo, or `None` when there is no memo or it is empty. A zero-filled memo field decodes
/// as empty text and carries nothing, so it must not count as a message.
fn text_memo(memo: &Memo) -> Option<String> {
    match memo {
        Memo::Text(text) if !text.is_empty() => Some(text.to_string()),
        _ => None,
    }
}

/// Every output `transaction` created for the wallet itself.
fn self_outputs(transaction: &WalletTransaction) -> Vec<SelfOutput> {
    let external = zip32::Scope::External;
    let mut outputs = Vec::new();
    for note in transaction.ironwood_notes() {
        outputs.push(SelfOutput::new(
            PoolType::IRONWOOD,
            note.value(),
            note.key_id().scope == external,
            note.memo(),
        ));
    }
    for note in transaction.orchard_notes() {
        outputs.push(SelfOutput::new(
            PoolType::ORCHARD,
            note.value(),
            note.key_id().scope == external,
            note.memo(),
        ));
    }
    for note in transaction.sapling_notes() {
        outputs.push(SelfOutput::new(
            PoolType::SAPLING,
            note.value(),
            note.key_id().scope == external,
            note.memo(),
        ));
    }
    for coin in transaction.transparent_coins() {
        outputs.push(SelfOutput {
            pool: PoolType::TRANSPARENT,
            value: coin.value(),
            external: coin.key_id().scope() == transparent::TransparentScope::External,
            memo: None,
        });
    }
    outputs
}

/// One output a transaction created for the wallet itself.
#[derive(Clone, Debug, PartialEq)]
struct SelfOutput {
    pool: PoolType,
    value: u64,
    /// Sent to one of the wallet's own external addresses, rather than its internal change address.
    external: bool,
    memo: Option<String>,
}

impl SelfOutput {
    fn new(pool: PoolType, value: u64, external: bool, memo: &Memo) -> Self {
        Self {
            pool,
            value,
            external,
            memo: text_memo(memo),
        }
    }
}

/// What a transaction meant to do with the value it sent to the wallet itself.
#[derive(Debug)]
enum SelfIntent {
    /// Paid someone else. Outputs to the wallet's external addresses were sent on purpose; the
    /// rest is change.
    Payment,
    /// Sent to itself within the pools it spent from. Outputs to its external addresses are the
    /// point; with none, the transaction consolidated into change and every output is.
    SendToSelf,
    /// Moved value into these pools, which it did not spend from. Outputs there are the point;
    /// whatever returned to a spent pool is change.
    Move(Vec<PoolType>),
}

/// Value sent to the wallet itself in one pool.
#[derive(Debug, PartialEq)]
struct SelfGroup {
    pool: PoolType,
    kind: SelfSendWalletEvent,
    value: u64,
    memos: Vec<String>,
}

/// Groups the outputs a transaction sent to the wallet by pool, keeping those that were the
/// point of the transaction plus any that carry a memo, so a memo never appears without the
/// value it arrived with. Plain change is left out: it is the remainder, not a transfer. Every
/// group carries the full value of its outputs, including zero.
/// Whether `output` was the point of the transaction rather than change, given all of
/// `outputs`.
fn intended(intent: &SelfIntent, outputs: &[SelfOutput], output: &SelfOutput) -> bool {
    match intent {
        SelfIntent::Payment => output.external,
        SelfIntent::SendToSelf => output.external || !outputs.iter().any(|o| o.external),
        SelfIntent::Move(pools) => pools.contains(&output.pool),
    }
}

/// The value a transaction sent to the wallet on purpose, leaving change out.
fn intended_value(intent: &SelfIntent, outputs: &[SelfOutput]) -> u64 {
    outputs
        .iter()
        .filter(|o| intended(intent, outputs, o))
        .map(|o| o.value)
        .sum()
}

fn group_self_outputs(intent: &SelfIntent, outputs: &[SelfOutput]) -> Vec<SelfGroup> {
    let intended = |o: &SelfOutput| intended(intent, outputs, o);
    [
        PoolType::IRONWOOD,
        PoolType::ORCHARD,
        PoolType::SAPLING,
        PoolType::TRANSPARENT,
    ]
    .into_iter()
    .filter_map(|pool| {
        let kept: Vec<&SelfOutput> = outputs
            .iter()
            .filter(|o| o.pool == pool && (intended(o) || o.memo.is_some()))
            .collect();
        if kept.is_empty() {
            return None;
        }
        let memos: Vec<String> = kept.iter().filter_map(|o| o.memo.clone()).collect();
        let kind = match intent {
            SelfIntent::Move(pools) if pools.contains(&pool) => SelfSendWalletEvent::PoolMove,
            _ if !memos.is_empty() => SelfSendWalletEvent::MemoToSelf,
            _ => SelfSendWalletEvent::Basic,
        };
        Some(SelfGroup {
            pool,
            kind,
            value: kept.iter().map(|o| o.value).sum(),
            memos,
        })
    })
    .collect()
}

#[cfg(test)]
mod self_transfer_tests {
    use super::*;

    fn output(pool: PoolType, value: u64, external: bool, memo: Option<&str>) -> SelfOutput {
        SelfOutput {
            pool,
            value,
            external,
            memo: memo.map(String::from),
        }
    }

    #[test]
    fn empty_text_memos_are_no_memo() {
        assert_eq!(text_memo(&Memo::Empty), None);
        assert_eq!(text_memo(&Memo::from_bytes(&[0u8; 512]).unwrap()), None);
        assert_eq!(
            text_memo(&Memo::from_bytes(b"hi").unwrap()),
            Some("hi".to_string())
        );
    }

    #[test]
    fn a_move_reports_the_moved_value_and_leaves_the_padding_out() {
        // 0.0102 ZEC of Orchard moved: 0.01 into Ironwood, a zero-value Orchard padding output
        let outputs = [
            output(PoolType::IRONWOOD, 1_000_000, false, None),
            output(PoolType::ORCHARD, 0, false, None),
        ];
        let groups = group_self_outputs(&SelfIntent::Move(vec![PoolType::IRONWOOD]), &outputs);
        assert_eq!(
            groups,
            vec![SelfGroup {
                pool: PoolType::IRONWOOD,
                kind: SelfSendWalletEvent::PoolMove,
                value: 1_000_000,
                memos: vec![],
            }]
        );
    }

    #[test]
    fn a_send_to_self_reports_its_value_not_zero() {
        let outputs = [
            output(PoolType::IRONWOOD, 17_000, true, None),
            output(PoolType::IRONWOOD, 210_000, false, None),
        ];
        let groups = group_self_outputs(&SelfIntent::SendToSelf, &outputs);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].value, 17_000, "the change is not part of it");
        assert_eq!(groups[0].kind, SelfSendWalletEvent::Basic);
    }

    #[test]
    fn a_send_to_self_is_worth_what_it_sent_to_itself() {
        let outputs = [
            output(PoolType::SAPLING, 30_000, true, None),
            output(PoolType::SAPLING, 210_000, false, None),
        ];
        assert_eq!(intended_value(&SelfIntent::SendToSelf, &outputs), 30_000);
        // The wallet events agree with the amount.
        let groups = group_self_outputs(&SelfIntent::SendToSelf, &outputs);
        assert_eq!(groups.iter().map(|g| g.value).sum::<u64>(), 30_000);
    }

    #[test]
    fn consolidating_into_change_counts_the_change() {
        let outputs = [output(PoolType::SAPLING, 90_000, false, None)];
        let groups = group_self_outputs(&SelfIntent::SendToSelf, &outputs);
        assert_eq!(groups[0].value, 90_000);
    }

    #[test]
    fn a_payment_lists_every_amount_sent_to_the_wallet_itself() {
        // pay someone, plus 17k to the wallet's own Sapling address with a memo and 17k to its
        // own transparent address; the change carries no memo
        let outputs = [
            output(PoolType::SAPLING, 17_000, true, Some("to self")),
            output(PoolType::TRANSPARENT, 17_000, true, None),
            output(PoolType::IRONWOOD, 150_000, false, None),
        ];
        let groups = group_self_outputs(&SelfIntent::Payment, &outputs);
        let summary: Vec<(PoolType, SelfSendWalletEvent, u64)> =
            groups.iter().map(|g| (g.pool, g.kind, g.value)).collect();
        assert_eq!(
            summary,
            vec![
                (PoolType::SAPLING, SelfSendWalletEvent::MemoToSelf, 17_000),
                (PoolType::TRANSPARENT, SelfSendWalletEvent::Basic, 17_000),
            ]
        );
    }

    #[test]
    fn a_memo_on_change_brings_its_value() {
        let outputs = [output(PoolType::IRONWOOD, 150_000, false, Some("note"))];
        let groups = group_self_outputs(&SelfIntent::Payment, &outputs);
        assert_eq!(groups[0].value, 150_000);
        assert_eq!(groups[0].memos, vec!["note".to_string()]);
    }
}
