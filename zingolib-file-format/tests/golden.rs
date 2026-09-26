//! Pins the exact byte-for-byte encoding of a fresh wallet, per chain, at the current file
//! layout version.
//!
//! A failing assertion here means the layout changed. That's a deliberate version bump: after
//! confirming the new bytes are correct, refresh the constants below with the ignored
//! `print_current_hex` test and bump `WalletFile::VERSION`.

mod support;

use zingo_common_components::protocol::ActivationHeights;
use zingolib_common::chain::ChainType;

use support::{bytes, fresh};

/// Seed for the mnemonic `fresh` derives its keys from. Fixed so the pinned hex below is
/// reproducible.
const SEED: u8 = 1;

const MAINNET_HEX: &str = "29000000000000000020010101010101010101010101010101010101010101010101010101010101010180841e0001000000000002fd1d01b4d0d6c2032035390938690f1a87156a19e4739a04f864bc0f635376ccff9aa84d1e1a5ce1c302a9033a692fd1000000800ad558947245fc5738a00385543d9ace2865a99a9403f696e3d3ac50ecd36651731d1c92e23b20a20007cf0dbd318921e910bb3dd436fc4dcf2ec8f7e9bd7b01a64b51c8e79153d3e2331fde02dc4647e051573054f74da7da3b1d5a9cc51d0bbcabf3de94a6c9b182f3be4d498b19cd4efccc4ee52a15d3bce48ec106c4a4f99c1e9cd058a1cc6d87c9258e2f35220c4ec4060d032bd7fe5a7d6cc0c0ecf8a7004a03e6cfe32c80000000ae78ae1ca342ec366d3ad72366095eada94d35da6d590be1435d59f6147e9aee002b8386a2d4b499f19110bd502cf05fd125741e8ddb5265df71c31bfd8093425c0100000000000000000201010000000000000000000000020000000001000100000000000001000001000000000000010000010000000000000100040000000000020a05000200020000000000000300000000000000";
const TESTNET_HEX: &str = "29000000000000000120010101010101010101010101010101010101010101010101010101010101010180841e0001000000000002fd1d01b4d0d6c2032051cfd76dbf3925ba445b70b95b1ac18be789cdc85208888a6c695650d3253cdb02a9031bf75f8400000080687581090839e49aa9d9f2cd6b63eda2b27fba90b7b3ae568109cfd16a31bfa3e2ea04dad5586280777703b3c3079a3062eb426452c94e48d18610979cce7209ebc77d56ed302152d1a8ec0278b8459e16c6e1ef717153d75e2c0cedca601c0a71808465435c13b92de876edd728d9972c14356f7d8acbaefa91bd3918babfd2ce6efc7c3a3f9a395b295c2e09fddfeb510b87c71b27049327b6fc5f0ffb1fd0004a039f41f435800000004a70dade26d394e8aadaf8a1e7607f989f7beda82af9529cf2da2c4cc13afc63000668436847c6f11584dfaf4ad70eed95e9c64fe354602aa19208550c726718360100000000000000000201010000000000000000000000020000000001000100000000000001000001000000000000010000010000000000000100040000000000020a05000200020000000000000300000000000000";
const REGTEST_HEX: &str = "29000000000000000220010101010101010101010101010101010101010101010101010101010101010180841e0001000000000002fd1d01b4d0d6c2032051cfd76dbf3925ba445b70b95b1ac18be789cdc85208888a6c695650d3253cdb02a9031bf75f8400000080687581090839e49aa9d9f2cd6b63eda2b27fba90b7b3ae568109cfd16a31bfa3e2ea04dad5586280777703b3c3079a3062eb426452c94e48d18610979cce7209ebc77d56ed302152d1a8ec0278b8459e16c6e1ef717153d75e2c0cedca601c0a71808465435c13b92de876edd728d9972c14356f7d8acbaefa91bd3918babfd2ce6efc7c3a3f9a395b295c2e09fddfeb510b87c71b27049327b6fc5f0ffb1fd0004a039f41f435800000004a70dade26d394e8aadaf8a1e7607f989f7beda82af9529cf2da2c4cc13afc63000668436847c6f11584dfaf4ad70eed95e9c64fe354602aa19208550c726718360100000000000000000201010000000000000000000000020000000001000100000000000001000001000000000000010000010000000000000100040000000000020a05000200020000000000000300000000000000";

fn chains_with_expected_hex() -> [(ChainType, &'static str); 3] {
    [
        (ChainType::Mainnet, MAINNET_HEX),
        (ChainType::Testnet, TESTNET_HEX),
        (
            ChainType::Regtest(ActivationHeights::default()),
            REGTEST_HEX,
        ),
    ]
}

#[test]
fn fresh_wallet_bytes_are_pinned() {
    for (chain_type, expected_hex) in chains_with_expected_hex() {
        let written = bytes(&fresh(chain_type, SEED));

        assert_eq!(
            hex::encode(&written),
            expected_hex,
            "{chain_type} wallet bytes changed; if this is a deliberate layout bump, refresh \
             this constant with `print_current_hex`"
        );

        let version = u64::from_le_bytes(written[0..8].try_into().unwrap());
        assert_eq!(version, 41u64, "{chain_type} version prefix");

        let expected_tag: u8 = match chain_type {
            ChainType::Mainnet => 0,
            ChainType::Testnet => 1,
            ChainType::Regtest(_) => 2,
        };
        assert_eq!(written[8], expected_tag, "{chain_type} chain tag byte");
    }
}

#[test]
#[ignore = "prints the current encoding so the pinned constants can be refreshed after a \
            deliberate layout bump"]
fn print_current_hex() {
    for (chain_type, _) in chains_with_expected_hex() {
        let written = bytes(&fresh(chain_type, SEED));
        println!("{chain_type}: {}", hex::encode(&written));
    }
}
