//! Errors assoicated with [`crate::lightclient::LightClient`].

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
    #[error("gRPC client error. {0}")]
    ClientError(#[from] GetClientError),
    #[error("File error. {0}")]
    FileError(std::io::Error),
    #[error("Wallet error. {0}")]
    WalletError(#[from] WalletError),
}
