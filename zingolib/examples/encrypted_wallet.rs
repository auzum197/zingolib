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

use secrecy::SecretString;
use zcash_protocol::consensus::BlockHeight;

use pepper_sync::config::{PerformanceLevel, SyncConfig, TransparentAddressDiscovery};
use zingolib::config::ChainType;
use zingolib::wallet::encryption;
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

    // A fresh wallet. The birthday must be at or after the network's Sapling activation height
    // (419_200 on mainnet).
    let mut wallet = LightWallet::new(
        network,
        WalletBase::FreshEntropy {
            no_of_accounts: NonZeroU32::MIN,
        },
        BlockHeight::from_u32(419_200),
        settings,
    )?;
    let seed_phrase = wallet
        .mnemonic_phrase()
        .ok_or("a fresh wallet should have a seed")?;

    let passphrase = SecretString::new("a strong passphrase".to_string());

    // Turn on at-rest encryption. Argon2id runs once here and the derived key is cached, so
    // each later save only pays for the fast symmetric step. To use less memory on a
    // constrained device, set the cost explicitly instead of the 64 MiB default:
    //   wallet.set_passphrase_with_params(&passphrase, encryption::Argon2Params::with_memory_mib(32))?;
    wallet.set_passphrase(&passphrase)?;

    // `save` now returns an encrypted envelope (or `None` when nothing changed since the last
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
    let reloaded =
        LightWallet::read_encrypted(Cursor::new(&encrypted), network, Some(&passphrase))?;
    assert_eq!(reloaded.mnemonic_phrase().as_deref(), Some(seed_phrase.as_str()));

    // A wrong passphrase is rejected rather than returning a corrupt wallet.
    let wrong = SecretString::new("not the passphrase".to_string());
    assert!(LightWallet::read_encrypted(Cursor::new(&encrypted), network, Some(&wrong)).is_err());

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
