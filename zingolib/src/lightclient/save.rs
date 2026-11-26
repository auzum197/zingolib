use futures::FutureExt as _;
use std::{borrow::BorrowMut as _, sync::atomic};

use super::LightClient;
use crate::data::PollReport;

impl LightClient {
    /// Launches a task for saving the wallet data to persistance when the wallet's `save_required` flag is set.
    pub async fn save_task(&mut self) {
        if self.save_active.load(atomic::Ordering::Acquire) {
            return;
        }

        self.save_active.store(true, atomic::Ordering::Release);
        let save_active = self.save_active.clone();
        let wallet = self.wallet().clone();
        let store = self.wallet.store.clone();
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let save_handle = tokio::spawn(async move {
            loop {
                interval.tick().await;
                if let Some(wallet_bytes) = wallet.write().await.save()? {
                    store.write(&wallet_bytes)?;
                }
                if !save_active.load(atomic::Ordering::Acquire) {
                    return Ok(());
                }
            }
        });
        self.save_handle = Some(save_handle);
    }

    /// Wait until the wallet's `save_required` flag is not set.
    pub async fn wait_for_save(&self) {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            if !self.wallet().read().await.save_required {
                return;
            }
        }
    }

    /// Polls the save task, returning [`self::PollReport`].
    fn poll_save_task(&mut self) -> PollReport<(), std::io::Error> {
        if let Some(mut save_handle) = self.save_handle.take() {
            if let Some(save_result) = save_handle.borrow_mut().now_or_never() {
                PollReport::Ready(save_result.expect("task panicked"))
            } else {
                self.save_handle = Some(save_handle);
                PollReport::NotReady
            }
        } else {
            PollReport::NoHandle
        }
    }

    /// Checks the save task handle in case of failure.
    /// On save task failure, restarts the save task and returns the error.
    pub async fn check_save_error(&mut self) -> std::io::Result<()> {
        match self.poll_save_task() {
            PollReport::Ready(save_result) => {
                if save_result.is_err() {
                    self.save_task().await;
                }
                save_result
            }
            _ => Ok(()),
        }
    }

    pub async fn shutdown_save_task(&mut self) -> std::io::Result<()> {
        self.save_active.store(false, atomic::Ordering::Release);
        if let Some(save_handle) = self.save_handle.take() {
            save_handle.await.expect("task panicked")
        } else {
            Ok(())
        }
    }
}
