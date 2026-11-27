//! Errors from proposing, building and transmitting transactions.

use std::convert::Infallible;

use zcash_protocol::TxId;

use crate::wallet::error::WalletError;
use crate::wallet::output::OutputRef;

/// Anything that stops a proposal from reaching the chain.
#[derive(Debug, thiserror::Error)]
pub enum SendError {
    /// Propose send error.
    #[error("Propose send error. {0}")]
    ProposeSendError(#[from] ProposeSendError),
    /// Propose shield error.
    #[error("Propose shield error. {0}")]
    ProposeShieldError(#[from] ProposeShieldError),
    /// Failed to construct sending transaction.
    #[error("Failed to construct sending transaction. {0}")]
    CalculateSendError(CalculateTransactionError<OutputRef>),
    /// Failed to construct shielding transaction.
    #[error("Failed to construct shielding transaction. {0}")]
    CalculateShieldError(CalculateTransactionError<Infallible>),
    /// No proposal found in the wallet.
    #[error("No proposal found in the wallet.")]
    NoStoredProposal,
    /// Transmission error.
    #[error("Transmission error. {0}")]
    TransmissionError(#[from] TransmissionError),
    /// Wallet error.
    #[error("Wallet error. {0}")]
    WalletError(#[from] WalletError),
}

/// The indexer refused or mangled a transaction.
#[derive(Debug, thiserror::Error)]
pub enum TransmissionError {
    /// Transmission failed.
    #[error("Transmission failed. {0}")]
    TransmissionFailed(String),
    /// Transaction to transmit does not have `Calculated` status: {0}
    #[error("Transaction to transmit does not have `Calculated` status: {0}")]
    IncorrectTransactionStatus(TxId),
    /// Txid reported by server does not match calculated txid.
    #[error(
        "Server error: txid reported by the server does not match calculated txid.\ncalculated txid:\n{0}\ntxid from server: {1}"
    )]
    IncorrectTxidFromServer(TxId, TxId),
}

#[allow(missing_docs)] // error types document themselves
#[derive(Debug, thiserror::Error)]
pub enum CalculateTransactionError<NoteRef> {
    #[error("No unified spending key found for this account. {0}")]
    NoSpendingKey(#[from] crate::wallet::error::KeyError),
    #[error("Failed to load sapling paramaters. {0}")]
    SaplingParams(String),
    #[error("Failed to calculate transaction. {0}")]
    Calculation(
        zcash_client_backend::data_api::error::Error<
            WalletError,
            Infallible,
            Infallible,
            zcash_primitives::transaction::fees::zip317::FeeError,
            zcash_primitives::transaction::fees::zip317::FeeError,
            NoteRef,
        >,
    ),
    #[error("Only tex multistep transactions are supported!")]
    NonTexMultiStep,
}

/// Errors that can result from constructing send proposals.
#[derive(Debug, thiserror::Error)]
pub enum ProposeSendError {
    /// error in using trait to create spend proposal
    #[error("{0}")]
    Proposal(
        zcash_client_backend::data_api::error::Error<
            WalletError,
            WalletError,
            zcash_client_backend::data_api::wallet::input_selection::GreedyInputSelectorError,
            zcash_primitives::transaction::fees::zip317::FeeError,
            zcash_primitives::transaction::fees::zip317::FeeError,
            OutputRef,
        >,
    ),
    /// failed to construct a transaction request
    #[error("{0}")]
    TransactionRequestFailed(#[from] zcash_client_backend::zip321::Zip321Error),
    /// send all is transferring no value
    #[error("send all is transferring no value. only enough funds to pay the fees!")]
    ZeroValueSendAll,
    /// failed to calculate balance.
    #[error("failed to calculated balance. {0}")]
    BalanceError(#[from] crate::wallet::error::BalanceError),
}

/// Errors that can result from constructing shield proposals.
#[derive(Debug, thiserror::Error)]
pub enum ProposeShieldError {
    /// error in using trait to create shielding proposal
    #[error("{0}")]
    Component(
        zcash_client_backend::data_api::error::Error<
            WalletError,
            WalletError,
            zcash_client_backend::data_api::wallet::input_selection::GreedyInputSelectorError,
            zcash_primitives::transaction::fees::zip317::FeeError,
            zcash_primitives::transaction::fees::zip317::FeeError,
            Infallible,
        >,
    ),
    /// Insufficient transparent funds to shield.
    #[error("insufficient transparent funds to shield.")]
    InsufficientFunds,
    /// Address parse error.
    #[error("address parse error. {0}")]
    AddressParseError(#[from] zcash_address::ParseError),
}
