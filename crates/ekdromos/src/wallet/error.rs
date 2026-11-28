//! Errors for [`crate::wallet`] and sub-modules

use std::convert::Infallible;

use pepper_sync::{error::ScanError, wallet::OutputId};
use shardtree::error::ShardTreeError;
use zcash_primitives::transaction::TxId;
use zcash_protocol::{PoolType, ShieldedPool, consensus::BlockHeight};

pub use ekdromos_common::keys::KeyError;

/// Top level wallet errors
// TODO: remove external types from public API
#[derive(Debug, thiserror::Error)]
pub enum WalletError {
    #[error("Key error. {0}")]
    KeyError(#[from] KeyError),
    #[error("Mnemonic not found.")]
    MnemonicNotFound,
    #[error("Mnemonic error. {0}")]
    MnemonicError(#[from] bip0039::Error),
    #[error("Value outside valid range of zatoshis. {0:?}")]
    InvalidValue(#[from] zcash_protocol::value::BalanceError),
    #[error("Failed to read transaction. {0:?}")]
    TransactionRead(std::io::Error),
    #[error("Failed to write transaction. {0:?}")]
    TransactionWrite(std::io::Error),
    #[error(
        "Removal error. Transaction has not failed. Only failed transactions may be removed from the wallet."
    )]
    RemovalError,
    #[error("Transaction not found in the wallet: {0}")]
    TransactionNotFound(TxId),
    #[error("Wallet block at height {0} not found in the wallet.")]
    BlockNotFound(BlockHeight),
    #[error("Minimum confirmations must be non-zero.")]
    MinimumConfirmationError,
    #[error("Failed to scan calculated transaction. {0}")]
    CalculatedTxScanError(#[from] ScanError),
    #[error("Address parse error. {0}")]
    ParseError(#[from] zcash_address::ParseError),
    #[error("No sync data. Wallet has never been synced with the block chain.")]
    NoSyncData,
    #[error("Maximum number of accounts already in use.")]
    AccountCreationFailed,
    #[error("{shielded_protocol:?} shard store checkpoint not found at anchor height {height}.")]
    CheckpointNotFound {
        shielded_protocol: ShieldedPool,
        height: BlockHeight,
    },
    #[error("Shard tree error. {0}")]
    ShardTreeError(#[from] ShardTreeError<Infallible>),
    // TODO: move to lightclient?
    #[error("Conversion failed. {0}")]
    ConversionFailed(#[from] crate::utils::error::ConversionError),
    #[error(
        "birthday {0} below sapling activation height {1}. pre-sapling wallets are not supported!"
    )]
    BirthdayBelowSapling(u32, u32),
    #[error("The birthday is the chain tip, which only a lightclient can get from its indexer.")]
    ChainTipUnknown,
    #[error("Wallet file error. {0}")]
    File(#[from] ekdromos_file_format::WalletFileError),
    /// At-rest encryption setup failed (e.g. key derivation).
    #[error("Wallet encryption error. {0}")]
    Encryption(#[from] crate::wallet::encryption::WalletEncryptionError),
}

#[derive(Debug, thiserror::Error)]
pub enum PriceError {
    #[error("price error. {0}")]
    PriceError(#[from] crate::price::PriceError),
    #[error("price list not initialised. please wait for sync to obtain time of wallet birthday")]
    NotInitialised,
}

#[derive(Debug, thiserror::Error)]
pub enum SummaryError {
    #[error("key error. {0}")]
    KeyError(#[from] KeyError),
    #[error("address parse error. {0}")]
    ParseError(#[from] zcash_address::ParseError),
    #[error("spend error. {0}")]
    SpendError(#[from] SpendError),
}

/// Errors associated with calculating transaction fee
#[derive(Debug)]
pub enum FeeError {
    /// Transparent spend not found in wallet
    SpendNotFound {
        txid: TxId,
        spend: String,
    },
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
    #[error("key error. {0}")]
    KeyError(#[from] KeyError),
    #[error("conversion failed. {0}")]
    ConversionFailed(#[from] crate::utils::error::ConversionError),
    #[error("overflow occured during summation.")]
    Overflow,
}
