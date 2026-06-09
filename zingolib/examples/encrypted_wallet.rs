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

use zcash_protocol::consensus::BlockHeight;

use pepper_sync::config::{PerformanceLevel, SyncConfig, TransparentAddressDiscovery};
use zingolib::config::ChainType;
use zingolib::wallet::encryption::{self, EncryptionConfig};
use zingolib::wallet::{LightWallet, WalletBase, WalletSettings};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let network = ChainType::Mainnet;
    let settings = WalletSettings {
        sync_config: SyncConfig {
            transparent_address_discovery: TransparentAddressDiscovery::minimal(),
            performance_level: PerformanceLevel::High,
        },
        min_confirmations: NonZeroU32::MIN,
    };

    // The passphrase is a plain String; callers don't depend on the `secrecy` crate.
    let passphrase = "a strong passphrase".to_string();

    // A fresh, encrypted wallet. The birthday must be at or after the network's Sapling
    // activation height (419_200 on mainnet). Encryption is a construction input: Argon2id
    // runs once during `build`, and the derived key is cached so each later save only pays for
    // the fast symmetric step. To use less memory on a constrained device, swap in
    // `EncryptionConfig::with_params(passphrase.clone(), Argon2Params::with_memory_mib(32))`.
    let mut wallet = LightWallet::builder(
        network,
        WalletBase::FreshEntropy {
            no_of_accounts: NonZeroU32::MIN,
        },
        BlockHeight::from_u32(419_200),
        settings,
    )
    .encryption(EncryptionConfig::new(passphrase.clone()))
    .build()?;
    let seed_phrase = wallet
        .mnemonic_phrase()
        .ok_or("a fresh wallet should have a seed")?;

    // `save` returns an encrypted envelope (or `None` when nothing changed since the last
    // save). Persist these bytes wherever the wallet file lives.
    let Some(encrypted) = wallet.save()? else {
        return Err("a freshly created wallet should need saving".into());
    };
    assert!(encryption::is_encrypted(&encrypted));
    assert!(
        !bytes_contain(&encrypted, seed_phrase.as_bytes()),
        "the seed must not appear in the encrypted bytes"
    );

    // Reload from those bytes with the same passphrase and confirm the seed round-trips.
    let reloaded = LightWallet::read_encrypted(Cursor::new(&encrypted), network, Some(passphrase))?;
    assert_eq!(
        reloaded.mnemonic_phrase().as_deref(),
        Some(seed_phrase.as_str())
    );

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
