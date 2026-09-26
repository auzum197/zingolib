//! Sync configuration.

#[cfg(feature = "wallet_essentials")]
use std::io::{Read, Write};

#[cfg(feature = "wallet_essentials")]
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
#[cfg(feature = "wallet_essentials")]
use zingolib_common::serialization::ReadableWriteable;

/// Default capacity of the sync event broadcast channel.
///
/// Sized for cadence tolerance: per-batch fan-out is essentially one `RangeScanned` plus a rare
/// small transaction burst, so this gives a foregrounded subscriber seconds of slack at peak
/// initial-sync cadence. A backgrounded subscriber lags and reconciles by design.
pub const DEFAULT_EVENT_CHANNEL_CAPACITY: usize = 512;

/// Largest event channel capacity read back from a wallet file. The broadcast channel allocates
/// every slot up front and panics on a capacity of zero, so both ends are checked on read.
#[cfg(feature = "wallet_essentials")]
/// Performance level.
///
/// The higher the performance level the higher the memory usage and storage.
// TODO: revisit after implementing nullifier refetching
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub enum PerformanceLevel {
    /// - number of outputs per batch is quartered
    /// - nullifier map only contains chain tip
    Low,
    /// - nullifier map has a small maximum size
    /// - nullifier map only contains chain tip
    Medium,
    /// - nullifier map has a large maximum size
    #[default]
    High,
    /// - number of outputs per batch is quadrupled
    /// - nullifier map has no maximum size
    ///
    /// WARNING: this may cause the wallet to become less responsive on slower systems and may use a lot of memory for
    /// wallets with a lot of transactions.
    Maximum,
}

#[cfg(feature = "wallet_essentials")]
impl ReadableWriteable for PerformanceLevel {
    const VERSION: u8 = 0;

    fn read<R: Read>(mut reader: R, _input: ()) -> std::io::Result<Self> {
        Self::get_version(&mut reader)?;

        Ok(match reader.read_u8()? {
            0 => Self::Low,
            1 => Self::Medium,
            2 => Self::High,
            3 => Self::Maximum,
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "failed to read valid performance level",
                ));
            }
        })
    }

    fn write<W: Write>(&self, mut writer: W, _input: ()) -> std::io::Result<()> {
        writer.write_u8(Self::VERSION)?;

        writer.write_u8(match self {
            Self::Low => 0,
            Self::Medium => 1,
            Self::High => 2,
            Self::Maximum => 3,
        })?;

        Ok(())
    }
}

impl std::fmt::Display for PerformanceLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Low => write!(f, "low"),
            Self::Medium => write!(f, "medium"),
            Self::High => write!(f, "high"),
            Self::Maximum => write!(f, "maximum"),
        }
    }
}

/// Sync configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncConfig {
    /// Transparent address discovery configuration.
    pub transparent_address_discovery: TransparentAddressDiscovery,
    /// Performance level
    pub performance_level: PerformanceLevel,
    /// Capacity of the sync event broadcast channel. See [`crate::events`].
    pub event_channel_capacity: usize,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            transparent_address_discovery: TransparentAddressDiscovery::default(),
            performance_level: PerformanceLevel::default(),
            event_channel_capacity: DEFAULT_EVENT_CHANNEL_CAPACITY,
        }
    }
}

#[cfg(feature = "wallet_essentials")]
impl ReadableWriteable for SyncConfig {
    const VERSION: u8 = 2;

    fn read<R: Read>(mut reader: R, _input: ()) -> std::io::Result<Self> {
        let version = Self::get_version(&mut reader)?;
        if version < Self::VERSION {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("sync config version {version} is no longer readable"),
            ));
        }

        let gap_limit = reader.read_u8()?;
        let scopes = reader.read_u8()?;
        if scopes & !0b111 != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown transparent address discovery scopes {scopes:#b}"),
            ));
        }
        let performance_level = PerformanceLevel::read(&mut reader, ())?;
        let event_channel_capacity = usize::try_from(reader.read_u64::<LittleEndian>()?)
            .ok()
            .filter(|capacity| *capacity > 0)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "event channel capacity must be at least 1",
                )
            })?;
        Ok(Self {
            transparent_address_discovery: TransparentAddressDiscovery {
                gap_limit,
                scopes: TransparentAddressDiscoveryScopes {
                    external: scopes & 0b1 != 0,
                    internal: scopes & 0b10 != 0,
                    refund: scopes & 0b100 != 0,
                },
            },
            performance_level,
            event_channel_capacity,
        })
    }

    fn write<W: Write>(&self, mut writer: W, _input: ()) -> std::io::Result<()> {
        writer.write_u8(Self::VERSION)?;
        writer.write_u8(self.transparent_address_discovery.gap_limit)?;
        let mut scopes = 0;
        if self.transparent_address_discovery.scopes.external {
            scopes |= 0b1;
        }
        if self.transparent_address_discovery.scopes.internal {
            scopes |= 0b10;
        }
        if self.transparent_address_discovery.scopes.refund {
            scopes |= 0b100;
        }
        writer.write_u8(scopes)?;
        self.performance_level.write(&mut writer, ())?;
        writer.write_u64::<LittleEndian>(self.event_channel_capacity as u64)?;

        Ok(())
    }
}

/// Transparent address configuration.
///
/// Sets which `scopes` will be searched for addresses in use, scanning relevant transactions, up to a given `gap_limit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransparentAddressDiscovery {
    /// Sets the gap limit for transparent address discovery.
    pub gap_limit: u8,
    /// Sets the scopes for transparent address discovery.
    pub scopes: TransparentAddressDiscoveryScopes,
}

impl Default for TransparentAddressDiscovery {
    fn default() -> Self {
        Self {
            gap_limit: 10,
            scopes: TransparentAddressDiscoveryScopes::default(),
        }
    }
}

impl TransparentAddressDiscovery {
    /// Constructs a transparent address discovery config with a gap limit of 1 and ignoring the internal scope.
    #[must_use]
    pub fn minimal() -> Self {
        Self {
            gap_limit: 1,
            scopes: TransparentAddressDiscoveryScopes::default(),
        }
    }

    /// Constructs a transparent address discovery config with a gap limit of 20 for all scopes.
    #[must_use]
    pub fn recovery() -> Self {
        Self {
            gap_limit: 20,
            scopes: TransparentAddressDiscoveryScopes::recovery(),
        }
    }

    /// Disables transparent address discovery. Sync will only scan transparent outputs for addresses already in the
    /// wallet in transactions that also contain shielded inputs or outputs relevant to the wallet.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            gap_limit: 0,
            scopes: TransparentAddressDiscoveryScopes {
                external: false,
                internal: false,
                refund: false,
            },
        }
    }
}

/// Sets the active scopes for transparent address recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransparentAddressDiscoveryScopes {
    /// External.
    pub external: bool,
    /// Internal.
    pub internal: bool,
    /// Refund.
    pub refund: bool,
}

impl Default for TransparentAddressDiscoveryScopes {
    fn default() -> Self {
        Self {
            external: true,
            internal: false,
            refund: true,
        }
    }
}

impl TransparentAddressDiscoveryScopes {
    /// Constructor with all all scopes active.
    #[must_use]
    pub fn recovery() -> Self {
        Self {
            external: true,
            internal: true,
            refund: true,
        }
    }
}

#[cfg(all(test, feature = "wallet_essentials"))]
mod tests {
    use super::*;

    fn sync_config_bytes(version: u8, scopes: u8, event_channel_capacity: u64) -> Vec<u8> {
        let mut out = vec![version, 10, scopes, 0, 2];
        out.write_u64::<LittleEndian>(event_channel_capacity)
            .unwrap();
        out
    }

    #[test]
    fn default_config_round_trips() {
        let mut bytes = Vec::new();
        SyncConfig::default().write(&mut bytes, ()).unwrap();
        assert_eq!(
            SyncConfig::read(bytes.as_slice(), ()).unwrap(),
            SyncConfig::default()
        );
    }

    #[test]
    fn versions_before_2_are_rejected() {
        for version in [0, 1] {
            let bytes = sync_config_bytes(version, 0b101, 512);
            assert!(SyncConfig::read(bytes.as_slice(), ()).is_err());
        }
    }

    #[test]
    fn unknown_scope_bits_are_rejected() {
        let bytes = sync_config_bytes(2, 0b1101, 512);
        assert!(SyncConfig::read(bytes.as_slice(), ()).is_err());
    }

    #[test]
    fn event_channel_capacity_of_zero_is_rejected() {
        let bytes = sync_config_bytes(2, 0b101, 0);
        assert!(SyncConfig::read(bytes.as_slice(), ()).is_err());
        let bytes = sync_config_bytes(2, 0b101, 1 << 20);
        assert!(SyncConfig::read(bytes.as_slice(), ()).is_ok());
    }
}
