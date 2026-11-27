//! Building, proving and broadcasting transactions from a proposal.

use std::convert::Infallible;

use nonempty::NonEmpty;

use zcash_client_backend::data_api::wallet::SpendingKeys;
use zcash_client_backend::proposal::Proposal;
use zcash_client_backend::zip321::TransactionRequest;
use zcash_primitives::transaction::{TxId, fees::zip317};
use zcash_protocol::consensus::Parameters as _;

use chain_router::SourceError;
use ekdromos_common::status::ConfirmationStatus;

use super::error::{CalculateTransactionError, SendError, TransmissionError};
use super::proposal::StoredProposal;
use crate::lightclient::LightClient;
use crate::wallet::LightWallet;
use crate::wallet::error::{KeyError, WalletError};
use crate::wallet::output::OutputRef;

const MAX_RETRIES: u8 = 3;

/// The Sapling proving parameters, downloaded by the build script when `testutils` is on.
#[derive(rust_embed::RustEmbed)]
#[folder = "zcash-params/"]
struct SaplingParams;

fn read_sapling_params() -> Result<(Vec<u8>, Vec<u8>), String> {
    let read = |name: &str| {
        SaplingParams::get(name)
            .map(|file| file.data.into_owned())
            .ok_or_else(|| format!("{name} is not embedded"))
    };
    Ok((
        read("sapling-output.params")?,
        read("sapling-spend.params")?,
    ))
}

impl LightWallet {
    /// Creates and stores transaction from the given `proposal`, returning the txids for each calculated transaction.
    pub(crate) async fn calculate_transactions<NoteRef>(
        &mut self,
        proposal: Proposal<zip317::FeeRule, NoteRef>,
        sending_account: zip32::AccountId,
    ) -> Result<NonEmpty<TxId>, CalculateTransactionError<NoteRef>> {
        let calculated_txids = match proposal.steps().len() {
            1 => {
                self.create_proposed_transactions(proposal, sending_account)
                    .await?
            }
            2 if proposal.steps()[1]
                .transaction_request()
                .payments()
                .values()
                .any(|payment| {
                    matches!(
                        payment
                            .recipient_address()
                            .clone()
                            .convert_if_network::<zcash_keys::address::Address>(
                                self.chain_type().network_type()
                            ),
                        Ok(zcash_keys::address::Address::Tex(_))
                    )
                }) =>
            {
                self.create_proposed_transactions(proposal, sending_account)
                    .await?
            }

            _ => return Err(CalculateTransactionError::NonTexMultiStep),
        };
        self.mark_dirty();

        Ok(calculated_txids)
    }

    async fn create_proposed_transactions<NoteRef>(
        &mut self,
        proposal: Proposal<zcash_primitives::transaction::fees::zip317::FeeRule, NoteRef>,
        sending_account: zip32::AccountId,
    ) -> Result<NonEmpty<TxId>, CalculateTransactionError<NoteRef>> {
        let chain_type = self.chain_type();
        let usk: zcash_keys::keys::UnifiedSpendingKey = self
            .unified_key_store
            .get(&sending_account)
            .ok_or(KeyError::NoAccountKeys)?
            .try_into()?;

        // TODO:  Remove fallible sapling operations from Orchard only sends.
        let (sapling_output, sapling_spend): (Vec<u8>, Vec<u8>) =
            read_sapling_params().map_err(CalculateTransactionError::SaplingParams)?;
        let sapling_prover =
            zcash_proofs::prover::LocalTxProver::from_bytes(&sapling_spend, &sapling_output);
        zcash_client_backend::data_api::wallet::create_proposed_transactions(
            self,
            &chain_type,
            &sapling_prover,
            &sapling_prover,
            &SpendingKeys::new(usk),
            zcash_client_backend::wallet::OvkPolicy::Sender,
            &proposal,
            None,
        )
        .map_err(CalculateTransactionError::Calculation)
    }
}

impl LightClient {
    async fn send(
        &mut self,
        proposal: Proposal<zip317::FeeRule, OutputRef>,
        sending_account: zip32::AccountId,
    ) -> Result<NonEmpty<TxId>, SendError> {
        let calculated_txids = self
            .wallet()
            .write()
            .await
            .calculate_transactions(proposal, sending_account)
            .await
            .map_err(SendError::CalculateSendError)?;

        self.transmit_transactions(calculated_txids).await
    }

    async fn shield(
        &mut self,
        proposal: Proposal<zip317::FeeRule, Infallible>,
        shielding_account: zip32::AccountId,
    ) -> Result<NonEmpty<TxId>, SendError> {
        let calculated_txids = self
            .wallet()
            .write()
            .await
            .calculate_transactions(proposal, shielding_account)
            .await
            .map_err(SendError::CalculateShieldError)?;

        self.transmit_transactions(calculated_txids).await
    }

    /// Creates and transmits transactions from a stored proposal.
    ///
    /// If sync was running prior to creating a send proposal, sync will have been paused. If `resume_sync` is `true`, sync will be resumed after sending the stored proposal.
    pub async fn send_stored_proposal(
        &mut self,
        resume_sync: bool,
    ) -> Result<NonEmpty<TxId>, SendError> {
        let opt_proposal = self.wallet().write().await.take_proposal();
        if let Some(proposal) = opt_proposal {
            let txids = match proposal {
                StoredProposal::Send {
                    proposal,
                    sending_account,
                } => self.send(proposal, sending_account).await,
                StoredProposal::Shield {
                    proposal,
                    shielding_account,
                } => self.shield(proposal, shielding_account).await,
            }?;

            if resume_sync {
                let _ignore_error = self.resume_sync();
            }

            Ok(txids)
        } else {
            Err(SendError::NoStoredProposal)
        }
    }

    /// Proposes and transmits transactions from a transaction request skipping proposal confirmation.
    ///
    /// If sync is running, sync will be paused before creating the send proposal. If `resume_sync` is `true`, sync will be resumed after send.
    pub async fn quick_send(
        &mut self,
        request: TransactionRequest,
        account_id: zip32::AccountId,
        resume_sync: bool,
    ) -> Result<NonEmpty<TxId>, SendError> {
        let _ignore_error = self.pause_sync();
        let proposal = self
            .wallet()
            .write()
            .await
            .create_send_proposal(request, account_id)
            .map_err(SendError::ProposeSendError)?;
        let txids = self.send(proposal, account_id).await?;
        if resume_sync {
            let _ignore_error = self.resume_sync();
        }

        Ok(txids)
    }

    /// Shields all transparent funds skipping proposal confirmation.
    pub async fn quick_shield(
        &mut self,
        account_id: zip32::AccountId,
    ) -> Result<NonEmpty<TxId>, SendError> {
        let proposal = self
            .wallet()
            .write()
            .await
            .create_shield_proposal(account_id)
            .map_err(SendError::ProposeShieldError)?;

        self.shield(proposal, account_id).await
    }

    /// Tranmits calculated transactions stored in the wallet matching txids of `calculated_txids` in the given order.
    /// Returns list of txids for successfully transmitted transactions.
    ///
    /// An error reply from a source does not set a transaction to `Failed`. The transaction gets
    /// `Transmitted` status and the error is [`TransmissionError::OutcomeUnknown`].
    async fn transmit_transactions(
        &mut self,
        calculated_txids: NonEmpty<TxId>,
    ) -> Result<NonEmpty<TxId>, SendError> {
        let mut wallet = self.wallet().write().await;
        for txid in calculated_txids.iter() {
            let calculated_transaction = wallet
                .wallet_transactions
                .get(txid)
                .ok_or(WalletError::TransactionNotFound(*txid))?;
            let height = calculated_transaction.status().get_height();

            if !matches!(
                calculated_transaction.status(),
                ConfirmationStatus::Calculated(_)
            ) {
                return Err(SendError::TransmissionError(
                    TransmissionError::IncorrectTransactionStatus(*txid),
                ));
            }

            let mut transaction_bytes = vec![];
            calculated_transaction
                .transaction()
                .write(&mut transaction_bytes)
                .map_err(|e| {
                    pepper_sync::set_transactions_failed(
                        &mut wallet.wallet_transactions,
                        vec![*txid],
                    );
                    wallet.mark_dirty();
                    WalletError::TransactionWrite(e)
                })?;

            let mut retry_count = 0;
            let reply = loop {
                match self
                    .source_set()
                    .submit_transaction(transaction_bytes.clone())
                    .await
                {
                    Err(SourceError::NoSource | SourceError::IdentifierNotAllowed(_)) => {
                        pepper_sync::set_transactions_failed(
                            &mut wallet.wallet_transactions,
                            vec![*txid],
                        );
                        wallet.mark_dirty();
                        return Err(TransmissionError::NotSubmitted(*txid).into());
                    }
                    Err(_) if retry_count < MAX_RETRIES => {
                        retry_count += 1;
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                    reply => break reply,
                }
            };

            // A source took the bytes, so the transaction can be in the mempool even when the
            // reply is an error. Sync sets the status from the mempool and the chain, and sets
            // `Failed` if the transaction is not in a block at its expiry height.
            wallet
                .wallet_transactions
                .get_mut(txid)
                .ok_or(WalletError::TransactionNotFound(*txid))?
                .update_status(ConfirmationStatus::Transmitted(height), crate::utils::now());
            wallet.mark_dirty();

            let txid_from_server = reply
                .map_err(|e| TransmissionError::OutcomeUnknown(*txid, e.to_string()))?
                .value;
            if txid_from_server != *txid {
                return Err(SendError::TransmissionError(
                    TransmissionError::IncorrectTxidFromServer(*txid, txid_from_server),
                ));
            }
        }

        Ok(calculated_txids)
    }
}
