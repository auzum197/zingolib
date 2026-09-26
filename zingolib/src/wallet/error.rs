//! Errors for [`crate::wallet`] and sub-modules

use std::convert::Infallible;

use pepper_sync::{error::ScanError, wallet::OutputId};
use shardtree::error::ShardTreeError;
use zcash_primitives::transaction::TxId;
use zcash_protocol::{PoolType, ShieldedPool, consensus::BlockHeight};

pub use zingolib_common::keys::KeyError;

/// Top level wallet errors
// TODO: remove external types from public API
#[derive(Debug, thiserror::Error)]
pub enum WalletError {
    /// Key error
    #[error("Key error. {0}")]
    KeyError(#[from] KeyError),
    /// Mnemonic not found.
    #[error("Mnemonic not found.")]
    MnemonicNotFound,
    /// Mnemonic error
    #[error("Mnemonic error. {0}")]
    MnemonicError(#[from] bip0039::Error),
    /// Value outside the valid range of zatoshis
    #[error("Value outside valid range of zatoshis. {0:?}")]
    InvalidValue(#[from] zcash_protocol::value::BalanceError),
    /// Failed to read transaction.
    #[error("Failed to read transaction. {0:?}")]
    TransactionRead(std::io::Error),
    /// Failed to write transaction.
    #[error("Failed to write transaction. {0:?}")]
    TransactionWrite(std::io::Error),
    /// Removal error. Transaction has not failed. Only failed transactions may be removed from the wallet.
    #[error(
        "Removal error. Transaction has not failed. Only failed transactions may be removed from the wallet."
    )]
    RemovalError,
    /// Transaction not found in the wallet.
    #[error("Transaction not found in the wallet: {0}")]
    TransactionNotFound(TxId),
    /// Wallet block not found in the wallet.
    #[error("Wallet block at height {0} not found in the wallet.")]
    BlockNotFound(BlockHeight),
    /// Minimum confirmations must be non-zero.
    #[error("Minimum confirmations must be non-zero.")]
    MinimumConfirmationError,
    /// Failed to scan calculated transaction.
    #[error("Failed to scan calculated transaction. {0}")]
    CalculatedTxScanError(#[from] ScanError),
    /// Address parse error
    #[error("Address parse error. {0}")]
    ParseError(#[from] zcash_address::ParseError),
    /// No sync data. Wallet has never been synced with the block chain.
    #[error("No sync data. Wallet has never been synced with the block chain.")]
    NoSyncData,
    /// Maximum number of accounts already in use.
    #[error("Maximum number of accounts already in use.")]
    AccountCreationFailed,
    /// Shard store checkpoint not found.
    #[error("{shielded_protocol:?} shard store checkpoint not found at anchor height {height}.")]
    CheckpointNotFound {
        shielded_protocol: ShieldedPool,
        height: BlockHeight,
    },
    /// Shard tree error.
    #[error("Shard tree error. {0}")]
    ShardTreeError(#[from] ShardTreeError<Infallible>),
    /// Conversion failed
    // TODO: move to lightclient?
    #[error("Conversion failed. {0}")]
    ConversionFailed(#[from] crate::utils::error::ConversionError),
    /// Birthday below sapling error.
    #[error(
        "birthday {0} below sapling activation height {1}. pre-sapling wallets are not supported!"
    )]
    BirthdayBelowSapling(u32, u32),
    /// At-rest encryption setup failed (e.g. key derivation).
    #[error("Wallet encryption error. {0}")]
    Encryption(#[from] crate::wallet::encryption::WalletEncryptionError),
    /// Cannot create a new wallet with a wallet base of `Read` variant as the wallet is already created and stored as bytes.
    #[error(
        "Cannot create a new wallet with a wallet base of `Read` variant as the wallet is already created and stored as bytes."
    )]
    WalletAlreadyCreated,
}

/// Price error
#[derive(Debug, thiserror::Error)]
pub enum PriceError {
    /// Price error
    #[error("price error. {0}")]
    PriceError(#[from] zingolib_price::PriceError),
    /// Price list not initialised
    #[error("price list not initialised. please wait for sync to obtain time of wallet birthday")]
    NotInitialised,
}

/// Summary error
#[derive(Debug, thiserror::Error)]
pub enum SummaryError {
    /// Key error.
    #[error("key error. {0}")]
    KeyError(#[from] KeyError),
    /// Address parse error
    #[error("address parse error. {0}")]
    ParseError(#[from] zcash_address::ParseError),
    /// Spend error
    #[error("spend error. {0}")]
    SpendError(#[from] SpendError),
}

/// Errors associated with calculating transaction fee
#[derive(Debug)]
pub enum FeeError {
    /// Transparent spend not found in wallet
    SpendNotFound { txid: TxId, spend: String },
    /// Balance error
    BalanceError(zcash_protocol::value::BalanceError),
}

impl std::error::Error for FeeError {}

impl std::fmt::Display for FeeError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match &self {
            Self::SpendNotFound { txid, spend } => {
                write!(
                    f,
                    "Transparent spend not found for transaction id {txid}. Is the wallet fully synced? \nMissing spend: {spend}"
                )
            }
            Self::BalanceError(e) => write!(f, "{e}"),
        }
    }
}

impl From<zcash_protocol::value::BalanceError> for FeeError {
    fn from(value: zcash_protocol::value::BalanceError) -> Self {
        Self::BalanceError(value)
    }
}

/// Errors associated with spends
#[derive(Debug, thiserror::Error)]
pub enum SpendError {
    /// Transaction spends not found in wallet
    #[error(
        "spend not found for transaction id {txid}. is the wallet fully synced?\nmissing spend: {spend}"
    )]
    SpendNotFound {
        pool: PoolType,
        txid: TxId,
        spend: String,
    },
    /// Output has incorrect spending transaction id
    #[error("output has incorrect spending transaction id: {txid}.\noutput id: {output_id}")]
    IncorrectSpendingTransaction { output_id: OutputId, txid: TxId },
}

/// Errors associated with balance calculation
#[derive(Debug, thiserror::Error)]
pub enum BalanceError {
    /// Key error
    #[error("key error. {0}")]
    KeyError(#[from] KeyError),
    /// Conversion failed
    #[error("conversion failed. {0}")]
    ConversionFailed(#[from] crate::utils::error::ConversionError),
    /// Summation overflow
    #[error("overflow occured during summation.")]
    Overflow,
}
