use bip0039::Mnemonic;
use zcash_keys::keys::Era;
use zingo_test_vectors::seeds::CHIMNEY_BETTER_SEED;

use crate::{
    config::{ChainType, ClientConfig, WalletConfig},
    lightclient::LightClient,
    testutils::default_test_wallet_settings,
    wallet::{LightWallet, keys::unified::UnifiedKeyStore},
};

#[tokio::test]
async fn reload_wallet_from_file() {
    let wallet_dir = tempfile::tempdir().unwrap();
    let chain_type = ChainType::Testnet;

    let config = ClientConfig::builder()
        .set_chain_type(chain_type)
        .set_wallet_dir(wallet_dir.path().to_path_buf())
        .set_wallet_config(WalletConfig::MnemonicPhrase {
            mnemonic_phrase: CHIMNEY_BETTER_SEED.to_string(),
            no_of_accounts: 1.try_into().unwrap(),
            birthday: 2_000_000,
            wallet_settings: default_test_wallet_settings(),
        })
        .build();
    let mut mid_client = LightClient::new(config, true, None).await.unwrap();

    mid_client.save_task().await;
    mid_client.wait_for_save().await;
    mid_client.shutdown_save_task().await.unwrap();

    let config = ClientConfig::builder()
        .set_chain_type(chain_type)
        .set_wallet_dir(wallet_dir.path().to_path_buf())
        .set_wallet_config(WalletConfig::Read)
        .build();
    let loaded_client = LightClient::new(config, true, None).await.unwrap();
    let loaded_wallet = loaded_client.wallet().read().await;

    let expected_mnemonic = Mnemonic::from_phrase(CHIMNEY_BETTER_SEED.to_string()).unwrap();

    let expected_keys =
        UnifiedKeyStore::new_from_mnemonic(chain_type, &expected_mnemonic, zip32::AccountId::ZERO)
            .unwrap();

    let UnifiedKeyStore::Spend(usk) = &loaded_wallet
        .unified_key_store
        .get(&zip32::AccountId::ZERO)
        .unwrap()
    else {
        panic!("should be spending key!")
    };
    let UnifiedKeyStore::Spend(expected_usk) = &expected_keys else {
        panic!("should be spending key!")
    };

    assert_eq!(
        usk.to_bytes(Era::Orchard),
        expected_usk.to_bytes(Era::Orchard)
    );
    assert_eq!(usk.orchard().to_bytes(), expected_usk.orchard().to_bytes());
    assert_eq!(usk.sapling().to_bytes(), expected_usk.sapling().to_bytes());
    assert_eq!(
        usk.transparent().to_bytes(),
        expected_usk.transparent().to_bytes()
    );

    assert_eq!(loaded_wallet.unified_addresses.len(), 1);
    for addr in loaded_wallet.unified_addresses.values() {
        assert!(addr.orchard().is_some());
        assert!(addr.sapling().is_none());
        assert!(addr.transparent().is_none());
    }

    let ufvk = usk.to_unified_full_viewing_key();
    let ufvk_string = ufvk.encode(&chain_type);
    let wallet_config = WalletConfig::Ufvk {
        ufvk: ufvk_string.clone(),
        birthday: loaded_client.birthday(),
        wallet_settings: loaded_wallet.wallet_settings.clone(),
    };
    let view_wallet = LightWallet::new(chain_type, wallet_config, None).unwrap();
    let UnifiedKeyStore::View(v_ufvk) = &view_wallet
        .unified_key_store
        .get(&zip32::AccountId::ZERO)
        .unwrap()
    else {
        panic!("should be viewing key!");
    };
    let v_ufvk_string = v_ufvk.encode(&view_wallet.chain_type);
    assert_eq!(ufvk_string, v_ufvk_string);
}

/// Returns true if `haystack` contains `needle` as a contiguous subsequence.
fn contains_subsequence(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// End-to-end at-rest encryption. A plaintext wallet still loads with no passphrase (backward
/// compat). Encrypting via the runtime setter and saving produces an envelope whose bytes never
/// contain the raw seed. The encrypted file round-trips with the correct passphrase and rejects
/// a wrong or absent one.
#[test]
fn encrypted_wallet_round_trip_and_backward_compat() {
    use crate::config::{ChainType, WalletConfig};
    use crate::testutils::default_test_wallet_settings;
    use crate::wallet::encryption::is_encrypted;
    use std::num::NonZeroU32;

    let network = ChainType::Testnet;
    let seed = zingo_test_vectors::seeds::CHIMNEY_BETTER_SEED;
    let seed_entropy = Mnemonic::<bip0039::English>::from_phrase(seed.to_string())
        .unwrap()
        .into_entropy();

    let mut wallet = LightWallet::new(
        network,
        WalletConfig::MnemonicPhrase {
            mnemonic_phrase: seed.to_string(),
            no_of_accounts: NonZeroU32::MIN,
            birthday: 2_800_000,
            wallet_settings: default_test_wallet_settings(),
        },
        None,
    )
    .unwrap();

    // Backward compat: a plaintext save still loads with no passphrase.
    let plaintext = wallet.save().unwrap().expect("save produced bytes");
    assert!(!is_encrypted(&plaintext));
    assert!(contains_subsequence(&plaintext, &seed_entropy)); // sanity: plaintext leaks the seed
    let reloaded_plain = LightWallet::read_encrypted(plaintext.as_slice(), network, None).unwrap();
    assert!(!reloaded_plain.is_encrypted());

    // Encrypt an already-open wallet via the runtime setter, then save.
    let passphrase = "correct horse battery staple".to_string();
    wallet.set_passphrase(passphrase.clone()).unwrap();
    assert!(wallet.is_encrypted());
    let encrypted = wallet
        .save()
        .unwrap()
        .expect("encrypted save produced bytes");
    assert!(is_encrypted(&encrypted));
    // The raw seed entropy must not appear anywhere in the ciphertext.
    assert!(!contains_subsequence(&encrypted, &seed_entropy));

    // Round-trip with the correct passphrase. The session is carried forward.
    let reloaded =
        LightWallet::read_encrypted(encrypted.as_slice(), network, Some(passphrase.clone()))
            .expect("decrypt with correct passphrase");
    assert!(reloaded.is_encrypted());
    assert_eq!(
        reloaded.mnemonic().unwrap().clone().into_entropy(),
        seed_entropy
    );

    // Wrong passphrase and missing passphrase both fail.
    assert!(
        LightWallet::read_encrypted(encrypted.as_slice(), network, Some("nope".to_string()))
            .is_err()
    );
    assert!(LightWallet::read_encrypted(encrypted.as_slice(), network, None).is_err());
}

/// Encryption supplied at construction with a custom KDF memory cost is honored and recorded in
/// the header, and the encrypted bytes round-trip (rejecting a wrong passphrase).
#[test]
fn construction_encryption_records_custom_memory_and_round_trips() {
    use crate::config::{ChainType, WalletConfig};
    use crate::testutils::default_test_wallet_settings;
    use crate::wallet::encryption::{Argon2Params, EncryptionConfig, is_encrypted};
    use std::num::NonZeroU32;

    let network = ChainType::Testnet;
    let seed = zingo_test_vectors::seeds::CHIMNEY_BETTER_SEED;
    let passphrase = "a passphrase".to_string();

    // Encrypt at construction with a non-default memory cost (32 MiB) to prove the chosen value
    // is honored.
    let mut wallet = LightWallet::new(
        network,
        WalletConfig::MnemonicPhrase {
            mnemonic_phrase: seed.to_string(),
            no_of_accounts: NonZeroU32::MIN,
            birthday: 2_800_000,
            wallet_settings: default_test_wallet_settings(),
        },
        Some(EncryptionConfig::with_params(
            passphrase.clone(),
            Argon2Params::with_memory_mib(32),
        )),
    )
    .unwrap();

    let bytes = wallet
        .save()
        .unwrap()
        .expect("encrypted save produced bytes");
    assert!(is_encrypted(&bytes));

    // The header records the chosen memory cost (32 MiB = 32*1024 KiB) at bytes [10..14] LE.
    let m_cost = u32::from_le_bytes(bytes[10..14].try_into().unwrap());
    assert_eq!(m_cost, 32 * 1024);

    // Reload reuses the stored params and recovers the seed. A wrong passphrase fails.
    let reloaded = LightWallet::read_encrypted(bytes.as_slice(), network, Some(passphrase))
        .expect("decrypt with correct passphrase");
    assert!(reloaded.mnemonic().is_some());
    assert!(
        LightWallet::read_encrypted(bytes.as_slice(), network, Some("wrong".to_string())).is_err()
    );
}
