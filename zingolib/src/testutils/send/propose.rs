//! Proposals: which notes to spend and what to pay, before anything is built or signed.

use zcash_address::ZcashAddress;
use zcash_client_backend::{
    data_api::wallet::{
        ConfirmationsPolicy,
        input_selection::{GreedyInputSelector, SpendPolicy},
    },
    fees::{DustAction, DustOutputPolicy},
    zip321::TransactionRequest,
};
use zcash_protocol::{
    ShieldedPool,
    consensus::Parameters,
    memo::{Memo, MemoBytes},
    value::Zatoshis,
};

use pepper_sync::keys::transparent::TransparentScope;

use super::error::{ProposeSendError, ProposeShieldError};
use super::proposal::{ProportionalFeeProposal, ProportionalFeeShieldProposal, ZingoProposal};
use super::receivers::{Receiver, transaction_request_from_receivers};
use crate::ZENNIES_FOR_ZINGO_AMOUNT;
use crate::config::ChainType;
use crate::get_zennies_for_zingo_address;
use crate::lightclient::LightClient;
use crate::wallet::LightWallet;
use crate::wallet::error::WalletError;

impl LightWallet {
    /// Creates a proposal from a transaction request.
    pub(crate) fn create_send_proposal(
        &mut self,
        request: TransactionRequest,
        account_id: zip32::AccountId,
    ) -> Result<ProportionalFeeProposal, ProposeSendError> {
        let memo = self.change_memo_from_transaction_request(&request);
        let input_selector = GreedyInputSelector::new();
        let change_strategy = zcash_client_backend::fees::zip317::SingleOutputChangeStrategy::new(
            zcash_primitives::transaction::fees::zip317::FeeRule::standard(),
            Some(memo),
            ShieldedPool::Orchard,
            DustOutputPolicy::new(DustAction::AllowDustChange, None),
        );
        let chain_type = self.chain_type();

        zcash_client_backend::data_api::wallet::propose_transfer::<
            LightWallet,
            ChainType,
            GreedyInputSelector<LightWallet>,
            zcash_client_backend::fees::zip317::SingleOutputChangeStrategy<
                zcash_primitives::transaction::fees::zip317::FeeRule,
                LightWallet,
            >,
            WalletError,
        >(
            self,
            &chain_type,
            account_id,
            &input_selector,
            &change_strategy,
            request,
            // TODO: replace wallet min_confirmations field with confirmation policy to unify for all proposals
            ConfirmationsPolicy::new_symmetrical(self.wallet_settings.min_confirmations, false),
            &SpendPolicy::default(),
            None,
        )
        .map_err(ProposeSendError::Proposal)
    }

    /// The shield operation consumes a proposal that transfers value
    /// into the Orchard pool.
    ///
    /// The proposal is generated with this method, which operates on
    /// the balance transparent pool, without other input.
    /// In other words, shield does not take a user-specified amount
    /// to shield, rather it consumes all transparent value in the wallet that
    /// can be consumed without costing more in zip317 fees than is being transferred.
    pub(crate) fn create_shield_proposal(
        &mut self,
        account_id: zip32::AccountId,
    ) -> Result<ProportionalFeeShieldProposal, ProposeShieldError> {
        let input_selector = GreedyInputSelector::new();
        let change_strategy = zcash_client_backend::fees::zip317::SingleOutputChangeStrategy::new(
            zcash_primitives::transaction::fees::zip317::FeeRule::standard(),
            None,
            ShieldedPool::Orchard,
            DustOutputPolicy::new(DustAction::AllowDustChange, None),
        );
        let chain_type = self.chain_type();

        // TODO: store t addrs as concrete types instead of encoded
        let transparent_addresses = self
            .transparent_addresses()
            .values()
            .map(|address| {
                Ok(zcash_address::ZcashAddress::try_from_encoded(address)?
                    .convert_if_network::<zcash_transparent::address::TransparentAddress>(
                        chain_type.network_type(),
                    )
                    .expect("incorrect network should be checked on wallet load"))
            })
            .collect::<Result<Vec<_>, zcash_address::ParseError>>()?;

        let proposed_shield = zcash_client_backend::data_api::wallet::propose_shielding::<
            LightWallet,
            ChainType,
            GreedyInputSelector<LightWallet>,
            zcash_client_backend::fees::zip317::SingleOutputChangeStrategy<
                zcash_primitives::transaction::fees::zip317::FeeRule,
                LightWallet,
            >,
            WalletError,
        >(
            self,
            &chain_type,
            &input_selector,
            &change_strategy,
            Zatoshis::const_from_u64(10_000),
            &transparent_addresses,
            account_id,
            // TODO: replace wallet min_confirmations field with confirmation policy to unify for all proposals
            ConfirmationsPolicy::new_symmetrical(self.wallet_settings.min_confirmations, false),
            zcash_client_backend::data_api::CoinbaseFilter::AllTransparentOutputs,
        )
        .map_err(ProposeShieldError::Component)?;

        for step in proposed_shield.steps().iter() {
            if step
                .balance()
                .proposed_change()
                .iter()
                .fold(0, |total_out, output| total_out + output.value().into_u64())
                == 0
            {
                return Err(ProposeShieldError::InsufficientFunds);
            }
        }

        Ok(proposed_shield)
    }

    /// Stores a proposal in the `send_proposal` field.
    /// This field must be populated in order to then construct and transmit transactions.
    pub(crate) fn store_proposal(&mut self, proposal: ZingoProposal) {
        self.send_proposal = Some(proposal);
    }

    /// Takes the proposal from the `send_proposal` field, leaving the field empty.
    pub(crate) fn take_proposal(&mut self) -> Option<ZingoProposal> {
        self.send_proposal.take()
    }

    fn change_memo_from_transaction_request(&self, request: &TransactionRequest) -> MemoBytes {
        let chain_type = self.chain_type();
        let mut recipient_uas = Vec::new();
        let mut refund_address_indexes = Vec::new();
        let mut refund_address_count = self
            .transparent_addresses()
            .keys()
            .filter(|&address_id| address_id.scope() == TransparentScope::Refund)
            .count() as u32;
        for payment in request.payments().values() {
            if let Ok(address) = payment
                .recipient_address()
                .clone()
                .convert_if_network::<zcash_keys::address::Address>(chain_type.network_type())
            {
                match address {
                    zcash_keys::address::Address::Unified(unified_address) => {
                        recipient_uas.push(unified_address);
                    }
                    zcash_keys::address::Address::Tex(_) => {
                        refund_address_indexes.push(refund_address_count);
                        refund_address_count += 1;
                    }
                    _ => (),
                }
            }
        }
        let uas_bytes = match zingolib_common::memo::create_wallet_internal_memo_version_1(
            &chain_type,
            recipient_uas.as_slice(),
            refund_address_indexes.as_slice(),
        ) {
            Ok(bytes) => bytes,
            Err(e) => {
                log::error!(
                    "Could not write uas to memo field: {e}\n\
        Your wallet will display an incorrect sent-to address. This is a visual error only.\n\
        The correct address was sent to."
                );
                [0; 511]
            }
        };
        MemoBytes::from(Memo::Arbitrary(Box::new(uas_bytes)))
    }
}

impl LightClient {
    fn append_zingo_zenny_receiver(&self, receivers: &mut Vec<Receiver>) {
        let zfz_address = get_zennies_for_zingo_address(self.chain_type());
        let dev_donation_receiver = Receiver::new(
            crate::utils::conversion::address_from_str(zfz_address).expect("Hard coded str"),
            Zatoshis::from_u64(ZENNIES_FOR_ZINGO_AMOUNT).expect("Hard coded u64."),
            None,
        );
        receivers.push(dev_donation_receiver);
    }

    /// Creates and stores a proposal from a transaction request.
    pub async fn propose_send(
        &mut self,
        request: TransactionRequest,
        account_id: zip32::AccountId,
    ) -> Result<ProportionalFeeProposal, ProposeSendError> {
        let _ignore_error = self.pause_sync();
        let mut wallet = self.wallet().write().await;
        let proposal = wallet.create_send_proposal(request, account_id)?;
        wallet.store_proposal(ZingoProposal::Send {
            proposal: proposal.clone(),
            sending_account: account_id,
        });

        Ok(proposal)
    }

    /// Creates and stores a proposal for sending all shielded funds from a specified account to a given `address`.
    pub async fn propose_send_all(
        &mut self,
        address: ZcashAddress,
        zennies_for_zingo: bool,
        memo: Option<zcash_protocol::memo::MemoBytes>,
        account_id: zip32::AccountId,
    ) -> Result<ProportionalFeeProposal, ProposeSendError> {
        let max_send_value = self
            .max_send_value(address.clone(), zennies_for_zingo, account_id)
            .await?;
        if max_send_value == Zatoshis::ZERO {
            return Err(ProposeSendError::ZeroValueSendAll);
        }
        let mut receivers = vec![Receiver::new(address, max_send_value, memo)];
        if zennies_for_zingo {
            self.append_zingo_zenny_receiver(&mut receivers);
        }
        let request = transaction_request_from_receivers(receivers)
            .map_err(ProposeSendError::TransactionRequestFailed)?;
        let _ignore_error = self.pause_sync();
        let mut wallet = self.wallet().write().await;
        let proposal = wallet.create_send_proposal(request, account_id)?;
        wallet.store_proposal(ZingoProposal::Send {
            proposal: proposal.clone(),
            sending_account: account_id,
        });

        Ok(proposal)
    }

    /// Creates and stores a proposal for shielding all transparent funds..
    pub async fn propose_shield(
        &mut self,
        account_id: zip32::AccountId,
    ) -> Result<ProportionalFeeShieldProposal, ProposeShieldError> {
        let mut wallet = self.wallet().write().await;
        let proposal = wallet.create_shield_proposal(account_id)?;
        wallet.store_proposal(ZingoProposal::Shield {
            proposal: proposal.clone(),
            shielding_account: account_id,
        });

        Ok(proposal)
    }

    /// Returns the maximum value that can be sent from the given `account_id`.
    ///
    /// This value is calculated from the shielded spendable balance minus any fees required to send those funds to
    /// the given `address`. If the wallet is still syncing, the spendable balance may be less than the confirmed
    /// balance - minus the fee - due to notes being above the minimum confirmation threshold or not being able to
    /// construct a witness from the current state of the wallet's note commitment tree.
    /// If `zennies_for_zingo` is set true, an additional payment of `1_000_000` ZAT to the `ZingoLabs` developer address
    /// will be taken into account.
    ///
    /// # Error
    ///
    /// Will return an error if this method fails to calculate the total wallet balance or create the
    /// proposal needed to calculate the fee
    pub async fn max_send_value(
        &self,
        address: ZcashAddress,
        zennies_for_zingo: bool,
        account_id: zip32::AccountId,
    ) -> Result<Zatoshis, ProposeSendError> {
        let mut wallet = self.wallet().write().await;
        let confirmed_balance = wallet.shielded_spendable_balance(account_id, false)?;
        let mut spendable_balance = confirmed_balance;

        loop {
            let mut receivers = vec![Receiver::new(address.clone(), spendable_balance, None)];
            if zennies_for_zingo {
                self.append_zingo_zenny_receiver(&mut receivers);
            }
            let request = transaction_request_from_receivers(receivers)?;
            let trial_proposal = wallet.create_send_proposal(request, account_id);

            match trial_proposal {
                Err(ProposeSendError::Proposal(
                    zcash_client_backend::data_api::error::Error::InsufficientFunds {
                        available,
                        required,
                    },
                )) => {
                    if let Some(shortfall) = required - confirmed_balance {
                        match spendable_balance - shortfall {
                            Some(updated_spendable) => {
                                spendable_balance = updated_spendable;
                            }
                            None => {
                                return Err(ProposeSendError::Proposal(
                                zcash_client_backend::data_api::error::Error::InsufficientFunds {
                                    available: confirmed_balance,
                                    required,
                                },
                            ));
                            }
                        }
                    } else {
                        // bugged underflow case, required should always be larger than confirmed shielded balance to cause
                        // insufficient funds error.
                        // returns insufficient funds error with same values from original error for debugging
                        return Err(ProposeSendError::Proposal(
                            zcash_client_backend::data_api::error::Error::InsufficientFunds {
                                available,
                                required,
                            },
                        ));
                    }
                }
                Err(e) => {
                    return Err(e);
                }
                Ok(_) => {
                    break;
                }
            }
        }

        Ok(spendable_balance)
    }
}

#[cfg(test)]
mod tests {
    use zcash_protocol::consensus::Parameters;
    use zcash_protocol::{PoolType, ShieldedPool};
    use zingo_test_vectors::seeds;

    use super::super::error::ProposeShieldError;
    use crate::{
        config::{ClientConfig, WalletConfig},
        lightclient::LightClient,
        testutils::default_test_wallet_settings,
        testutils::lightclient::from_inputs::transaction_request_from_send_inputs,
        wallet::disk::testing::examples,
    };

    async fn create_basic_client() -> LightClient {
        let config = ClientConfig::builder()
            .set_wallet_config(WalletConfig::MnemonicPhrase {
                mnemonic_phrase: seeds::HOSPITAL_MUSEUM_SEED.to_string(),
                no_of_accounts: 1.try_into().unwrap(),
                birthday: 419200,
                wallet_settings: default_test_wallet_settings(),
            })
            .build();
        LightClient::new(config, true, None).await.unwrap()
    }

    #[tokio::test]
    async fn propose_shield_missing_scan_prerequisite() {
        let basic_client = create_basic_client().await;
        let propose_shield_result = basic_client
            .wallet()
            .write()
            .await
            .create_shield_proposal(zip32::AccountId::ZERO);
        match propose_shield_result {
            Err(ProposeShieldError::Component(
                zcash_client_backend::data_api::error::Error::ScanRequired,
            )) => true,
            _ => panic!("Unexpected error state!"),
        };
    }

    #[tokio::test]
    async fn get_transparent_addresses() {
        let basic_client = create_basic_client().await;
        let network = basic_client.chain_type();

        // TODO: store t addrs as concrete types instead of encoded
        let transparent_addresses = basic_client
            .wallet()
            .read()
            .await
            .transparent_addresses()
            .values()
            .map(|address| {
                Ok(zcash_address::ZcashAddress::try_from_encoded(address)?
                    .convert_if_network::<zcash_transparent::address::TransparentAddress>(
                        network.network_type(),
                    )
                    .expect("incorrect network should be checked on wallet load"))
            })
            .collect::<Result<Vec<_>, zcash_address::ParseError>>()
            .unwrap();

        assert_eq!(
            transparent_addresses,
            [
                zcash_transparent::address::TransparentAddress::PublicKeyHash([
                    161, 138, 222, 242, 254, 121, 71, 105, 93, 131, 177, 31, 59, 185, 120, 148,
                    255, 189, 198, 33
                ])
            ]
        );
    }

    /// this test loads an example wallet with existing sapling finds
    #[ignore = "for some reason this is does not work without network, even though it should be possible"]
    #[tokio::test]
    async fn example_mainnet_hhcclaltpcckcsslpcnetblr_80b5594ac_propose_100_000_to_self() {
        let client = examples::NetworkSeedVersion::Mainnet(
            examples::MainnetSeedVersion::HotelHumor(examples::HotelHumorVersion::Latest),
        )
        .load_example_wallet()
        .await;
        let mut wallet = client.wallet().write().await;

        let pool = PoolType::Shielded(ShieldedPool::Orchard);
        let self_address = wallet.get_address(pool);

        let receivers = vec![(self_address.as_str(), 100_000, None)];
        let request = transaction_request_from_send_inputs(receivers)
            .expect("actually all of this logic oughta be internal to propose");

        wallet
            .create_send_proposal(request, zip32::AccountId::ZERO)
            .expect("can propose from existing data");
    }
}
