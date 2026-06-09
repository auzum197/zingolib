//! Encrypt a zingolib wallet at rest with a passphrase, save it, and load it back.
//!
//! Run with:
//!
//! ```sh
//! cargo run -p zingolib --example encrypted_wallet
//! ```
//!
//! Everything here is offline: no server, no proving parameters, no wallet file is written to
//! disk (the encrypted bytes are kept in memory so the example is self-contained).

use std::io::Cursor;
use std::num::NonZeroU32;

use pepper_sync::config::{PerformanceLevel, SyncConfig, TransparentAddressDiscovery};
use zingolib::config::{ChainType, WalletConfig};
use zingolib::wallet::LightWallet;
use zingolib::wallet::WalletSettings;
use zingolib::wallet::encryption::{self, EncryptionConfig};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let network = ChainType::Mainnet;
    let wallet_settings = WalletSettings {
        sync_config: SyncConfig {
            transparent_address_discovery: TransparentAddressDiscovery::minimal(),
            performance_level: PerformanceLevel::High,
        },
        min_confirmations: NonZeroU32::MIN,
    };

    // The passphrase is a plain String; callers don't depend on the `secrecy` crate.
    let passphrase = "a strong passphrase".to_string();

    // A fresh, encrypted wallet. Encryption is a construction input: Argon2id runs once during
    // construction, and the derived key is cached so each later save only pays for the fast
    // symmetric step. To use less memory on a constrained device, swap `EncryptionConfig::new`
    // for `EncryptionConfig::with_params(passphrase, Argon2Params::with_memory_mib(32))`.
    let mut wallet = LightWallet::new(
        network,
        WalletConfig::NewSeed {
            no_of_accounts: NonZeroU32::MIN,
            chain_height: 419_300,
            wallet_settings,
        },
        Some(EncryptionConfig::new(passphrase.clone())),
    )?;
    let seed_phrase = wallet
        .mnemonic_phrase()
        .ok_or("a fresh wallet should have a seed")?;
    let seed_entropy =
        bip0039::Mnemonic::<bip0039::English>::from_phrase(&seed_phrase)?.into_entropy();

    // `save` returns an encrypted envelope (or `None` when nothing changed since the last
    // save). Persist these bytes wherever the wallet file lives.
    let Some(encrypted) = wallet.save()? else {
        return Err("a freshly created wallet should need saving".into());
    };
    assert!(encryption::is_encrypted(&encrypted));
    assert!(
        !bytes_contain(&encrypted, &seed_entropy),
        "the seed must not appear in the encrypted bytes"
    );

    // Reload from those bytes with the same passphrase and confirm the seed round-trips.
    let reloaded = LightWallet::read_encrypted(Cursor::new(&encrypted), network, Some(passphrase))?;
    assert_eq!(reloaded.mnemonic_phrase(), Some(seed_phrase));

    // A wrong passphrase is rejected rather than returning a corrupt wallet.
    assert!(
        LightWallet::read_encrypted(
            Cursor::new(&encrypted),
            network,
            Some("not the passphrase".to_string()),
        )
        .is_err()
    );

    println!(
        "encrypted wallet is {} bytes; seed recovered after reload, wrong passphrase rejected",
        encrypted.len()
    );
    Ok(())
}

/// True if `needle` occurs as a contiguous run within `haystack`.
fn bytes_contain(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && needle.len() <= haystack.len()
        && haystack.windows(needle.len()).any(|w| w == needle)
}
