use std::{
    io::{self, Read, Write},
    num::NonZeroU32,
};

use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use pepper_sync::config::SyncConfig;
use zingolib_common::serialization::ReadableWriteable;

/// Wallet settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalletSettings {
    /// Sync configuration.
    pub sync_config: SyncConfig,
    /// Minimum confirmations.
    pub min_confirmations: NonZeroU32,
}

impl Default for WalletSettings {
    fn default() -> Self {
        Self {
            sync_config: SyncConfig::default(),
            min_confirmations: NonZeroU32::try_from(3).expect("hard-coded non-zero integer"),
        }
    }
}

impl WalletSettings {
    /// Unversioned: the sync config followed by the minimum confirmations. Layout changes
    /// are gated by the wallet file version.
    pub fn read<R: Read>(mut reader: R) -> io::Result<Self> {
        let sync_config = SyncConfig::read(&mut reader, ())?;
        let min_confirmations =
            NonZeroU32::try_from(reader.read_u32::<LittleEndian>()?).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("minimum confirmations must be non-zero. {e}"),
                )
            })?;
        Ok(Self {
            sync_config,
            min_confirmations,
        })
    }

    /// Serialize into `writer`
    pub fn write<W: Write>(&self, mut writer: W) -> io::Result<()> {
        self.sync_config.write(&mut writer, ())?;
        writer.write_u32::<LittleEndian>(self.min_confirmations.into())
    }
}
