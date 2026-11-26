//! Saves the wallet to its store whenever it changes.

use std::{
    io,
    sync::{Arc, Weak},
    time::Duration,
};

use tokio::sync::{RwLock, watch};

use super::{LightClient, error::SaveError, store::WalletStore};
use crate::wallet::LightWallet;

/// The wallet change count that the last save covered, or why the last save failed.
pub type SaveState = Result<u64, SaveError>;

/// Starts the task that saves `wallet` to `store`, at most once a second. A failed save is retried.
/// The task ends when the wallet is dropped. `wallet` must already be saved.
pub(super) fn spawn(
    wallet: Weak<RwLock<LightWallet>>,
    mut changes: watch::Receiver<u64>,
    store: Arc<dyn WalletStore>,
) -> watch::Receiver<SaveState> {
    let (state, save_state) = watch::channel(Ok(*changes.borrow_and_update()));
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        while changes.changed().await.is_ok() {
            interval.tick().await;
            let Some(wallet) = wallet.upgrade() else {
                return;
            };
            let (count, bytes) = {
                let mut wallet = wallet.write().await;
                (*changes.borrow_and_update(), wallet.save())
            };
            let written = bytes.and_then(|bytes| bytes.map_or(Ok(()), |bytes| store.write(&bytes)));
            if written.is_err() {
                wallet.write().await.mark_dirty();
            }
            state.send_modify(|state| *state = written.map(|()| count).map_err(SaveError::from));
        }
    });
    save_state
}

impl LightClient {
    /// The result of the last save. It changes after every save.
    pub fn save_state(&self) -> watch::Receiver<SaveState> {
        self.save_state.clone()
    }

    /// Returns when every change made to the wallet so far is in the store, or when a save fails.
    pub async fn flush(&self) -> Result<(), SaveError> {
        let target = *self.changes.borrow();
        self.save_state()
            .wait_for(|state| !matches!(state, Ok(saved) if *saved < target))
            .await
            .map_err(io::Error::other)?
            .clone()
            .map(drop)
    }

    /// Stops sync and saves the wallet. The result of the stopped sync is discarded.
    pub async fn close(mut self) -> Result<(), SaveError> {
        if self.stop_sync().is_ok() {
            let _sync_result = self.await_sync().await;
        }
        self.flush().await
    }
}
