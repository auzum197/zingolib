//! Transparent address derivation and encoding.

use zcash_address::{ToAddress as _, ZcashAddress};
use zcash_protocol::consensus;
use zcash_transparent::address::TransparentAddress;

#[cfg(not(feature = "darkside_test"))]
use zcash_transparent::keys::{AccountPubKey, IncomingViewingKey as _, NonHardenedChildIndex};

pub use ekdromoi_common::keys::{TransparentAddressId, TransparentScope};

#[cfg(not(feature = "darkside_test"))]
pub(crate) fn derive_address(
    consensus_parameters: &impl consensus::Parameters,
    account_pubkey: &AccountPubKey,
    address_id: TransparentAddressId,
) -> Result<String, bip32::Error> {
    let address = match address_id.scope() {
        TransparentScope::External => {
            derive_external_address(account_pubkey, address_id.address_index())?
        }
        TransparentScope::Internal => {
            derive_internal_address(account_pubkey, address_id.address_index())?
        }
        TransparentScope::Refund => {
            derive_refund_address(account_pubkey, address_id.address_index())?
        }
    };

    Ok(encode_address(consensus_parameters, address))
}

#[cfg(not(feature = "darkside_test"))]
fn derive_external_address(
    account_pubkey: &AccountPubKey,
    address_index: NonHardenedChildIndex,
) -> Result<TransparentAddress, bip32::Error> {
    account_pubkey
        .derive_external_ivk()?
        .derive_address(address_index)
}

#[cfg(not(feature = "darkside_test"))]
fn derive_internal_address(
    account_pubkey: &AccountPubKey,
    address_index: NonHardenedChildIndex,
) -> Result<TransparentAddress, bip32::Error> {
    account_pubkey
        .derive_internal_ivk()?
        .derive_address(address_index)
}

#[cfg(not(feature = "darkside_test"))]
fn derive_refund_address(
    account_pubkey: &AccountPubKey,
    address_index: NonHardenedChildIndex,
) -> Result<TransparentAddress, bip32::Error> {
    account_pubkey
        .derive_ephemeral_ivk()?
        .derive_ephemeral_address(address_index)
}

/// Encodes transparent address
pub fn encode_address(
    consensus_parameters: &impl consensus::Parameters,
    address: TransparentAddress,
) -> String {
    let zcash_address = match address {
        TransparentAddress::PublicKeyHash(data) => {
            ZcashAddress::from_transparent_p2pkh(consensus_parameters.network_type(), data)
        }
        TransparentAddress::ScriptHash(data) => {
            ZcashAddress::from_transparent_p2sh(consensus_parameters.network_type(), data)
        }
    };
    zcash_address.to_string()
}
