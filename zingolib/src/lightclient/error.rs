//! Errors assoicated with [`crate::lightclient::LightClient`].

use std::sync::Arc;

use pepper_sync::error::{SyncError, SyncModeError};
use zingo_netutils::GetClientError;

use crate::wallet::error::WalletError;

#[derive(Debug, thiserror::Error)]
pub enum LightClientError {
    #[error("Sync failed to launch.")]
    SyncLaunchError,
    #[error("No sync handle. Sync is not running.")]
    SyncNotRunning,
    #[error("Sync error. {0}")]
    SyncError(#[from] SyncError<WalletError>),
    #[error("Sync mode error. {0}")]
    SyncModeError(#[from] SyncModeError),
}

/// Why the wallet could not be saved to its store.
#[derive(Clone, Debug, thiserror::Error)]
#[error("Wallet save failed. {0}")]
pub struct SaveError(Arc<std::io::Error>);

impl From<std::io::Error> for SaveError {
    fn from(e: std::io::Error) -> Self {
        Self(Arc::new(e))
    }
}

/// Why [`crate::lightclient::LightClient::create`] failed.
#[derive(Debug, thiserror::Error)]
pub enum CreateError {
    #[error("The store already holds a wallet.")]
    AlreadyExists,
    #[error("Wallet store error. {0}")]
    Store(#[from] std::io::Error),
    #[error("Wallet error. {0}")]
    Wallet(#[from] WalletError),
    #[error("gRPC client error. {0}")]
    Indexer(#[from] GetClientError),
    #[error("Failed to get the chain tip for the wallet birthday. {0}")]
    ChainTip(#[from] zingo_netutils::Status),
}

/// Why [`crate::lightclient::LightClient::open`] failed.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("The store holds no wallet.")]
    NotFound,
    #[error("The wallet is encrypted and needs a passphrase.")]
    PassphraseRequired,
    #[error("A passphrase was given but the wallet is not encrypted.")]
    NotEncrypted,
    /// The passphrase is wrong or the file is damaged. The two cannot be told apart.
    #[error("Decryption failed. The passphrase is wrong or the wallet is damaged.")]
    DecryptionFailed,
    #[error("Wallet store error. {0}")]
    Store(#[from] std::io::Error),
    #[error("Wallet error. {0}")]
    Wallet(WalletError),
    #[error("gRPC client error. {0}")]
    Indexer(#[from] GetClientError),
}
