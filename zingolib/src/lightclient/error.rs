//! Errors assoicated with [`crate::lightclient::LightClient`].

use pepper_sync::error::{SyncError, SyncModeError};
use zingo_netutils::GetClientError;

use crate::wallet::error::WalletError;

#[derive(Debug, thiserror::Error)]
pub enum LightClientError {
    /// Sync failed to launch..
    #[error("Sync failed to launch.")]
    SyncLaunchError,
    /// Sync not running.
    #[error("No sync handle. Sync is not running.")]
    SyncNotRunning,
    /// Sync error.
    #[error("Sync error. {0}")]
    SyncError(#[from] SyncError<WalletError>),
    /// Sync mode error.
    #[error("Sync mode error. {0}")]
    SyncModeError(#[from] SyncModeError),
    /// gPRC client error.
    #[error("gRPC client error. {0}")]
    ClientError(#[from] GetClientError),
    /// File error.
    #[error("File error. {0}")]
    FileError(std::io::Error),
    /// Wallet error.
    #[error("Wallet error. {0}")]
    WalletError(#[from] WalletError),
}
