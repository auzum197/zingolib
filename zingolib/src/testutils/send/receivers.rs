//! transforming data related to the destination of a send.

use zcash_address::ZcashAddress;
use zcash_client_backend::zip321::Payment;
use zcash_client_backend::zip321::PaymentError;
use zcash_client_backend::zip321::TransactionRequest;
use zcash_client_backend::zip321::Zip321Error;
use zcash_protocol::memo::MemoBytes;
use zcash_protocol::value::Zatoshis;

/// A list of Receivers
pub type Receivers = Vec<Receiver>;

/// The superficial representation of the the consumer's intended receiver
#[derive(Clone, Debug, PartialEq)]
pub struct Receiver {
    /// Where the value goes.
    pub recipient_address: ZcashAddress,
    /// How much.
    pub amount: Zatoshis,
    /// Attached to the output when the receiver is shielded.
    pub memo: Option<MemoBytes>,
}
impl Receiver {
    /// Create a new Receiver
    pub(crate) fn new(
        recipient_address: ZcashAddress,
        amount: Zatoshis,
        memo: Option<MemoBytes>,
    ) -> Self {
        Self {
            recipient_address,
            amount,
            memo,
        }
    }
}
impl TryFrom<Receiver> for Payment {
    type Error = PaymentError;

    fn try_from(receiver: Receiver) -> Result<Self, Self::Error> {
        Payment::new(
            receiver.recipient_address,
            Some(receiver.amount),
            receiver.memo,
            None,
            None,
            vec![],
        )
    }
}

/// Creates a [`zcash_client_backend::zip321::TransactionRequest`] from receivers.
/// Note this fn is called to calculate the `spendable_shielded` balance
/// shielding and TEX should be handled mutually exclusively
pub fn transaction_request_from_receivers(
    receivers: Receivers,
) -> Result<TransactionRequest, Zip321Error> {
    let payments = receivers
        .into_iter()
        .enumerate()
        .map(|(i, receiver)| {
            Payment::try_from(receiver).map_err(|e| match e {
                PaymentError::TransparentMemo => Zip321Error::TransparentMemo(i),
                PaymentError::ZeroValuedTransparentOutput => {
                    Zip321Error::ZeroValuedTransparentOutput(i)
                }
            })
        })
        .collect::<Result<Vec<_>, Zip321Error>>()?;

    TransactionRequest::new(payments)
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use zcash_address::ZcashAddress;
    use zcash_client_backend::zip321::TransactionRequest;
    use zcash_protocol::memo::{Memo, MemoBytes};
    use zcash_protocol::value::Zatoshis;

    use super::{Receiver, Receivers, transaction_request_from_receivers};

    #[test]
    fn test_build_request() {
        let amount_1 = Zatoshis::const_from_u64(20000);
        let recipient_address_1 =
            ZcashAddress::try_from_encoded("utest17wwv8nuvdnpjsxtu6ndz6grys5x8wphcwtzmg75wkx607c7cue9qz5kfraqzc7k9dfscmylazj4nkwazjj26s9rhyjxm0dcqm837ykgh2suv0at9eegndh3kvtfjwp3hhhcgk55y9d2ys56zkw8aaamcrv9cy0alj0ndvd0wll4gxhrk9y4yy9q9yg8yssrencl63uznqnkv7mk3w05").unwrap();
        let memo_1 = None;

        let amount_2 = Zatoshis::const_from_u64(20000);
        let recipient_address_2 =
            ZcashAddress::try_from_encoded("utest17wwv8nuvdnpjsxtu6ndz6grys5x8wphcwtzmg75wkx607c7cue9qz5kfraqzc7k9dfscmylazj4nkwazjj26s9rhyjxm0dcqm837ykgh2suv0at9eegndh3kvtfjwp3hhhcgk55y9d2ys56zkw8aaamcrv9cy0alj0ndvd0wll4gxhrk9y4yy9q9yg8yssrencl63uznqnkv7mk3w05").unwrap();
        let memo_2 = Some(MemoBytes::from(
            Memo::from_str("the lake wavers along the beach").expect("string can memofy"),
        ));

        let rec: Receivers = vec![
            Receiver {
                recipient_address: recipient_address_1,
                amount: amount_1,
                memo: memo_1,
            },
            Receiver {
                recipient_address: recipient_address_2,
                amount: amount_2,
                memo: memo_2,
            },
        ];
        let request: TransactionRequest =
            transaction_request_from_receivers(rec).expect("rec can requestify");

        assert_eq!(
            request.total().expect("total").expect("amounts present"),
            (amount_1 + amount_2).expect("add")
        );
    }
}
