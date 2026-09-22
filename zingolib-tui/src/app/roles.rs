//! Gives each output of a transaction its role.
//!
//! zingolib reports an output the wallet sent to itself twice: once as outgoing, decrypted with
//! its outgoing viewing key, and once as received. Both share a position within their pool, the
//! identity zingolib itself uses when it totals what was sent. The outgoing copy is dropped here
//! so every output appears once.
//!
//! The rules for what was sent on purpose and what is change mirror the ones zingolib uses to
//! build wallet events, so the two views agree.

use super::state::{OutputRole, OutputRow, Pool, TxKind};

pub fn assign_roles(
    kind: TxKind,
    input_pools: &[Pool],
    mut received: Vec<OutputRow>,
    outgoing: Vec<OutputRow>,
) -> Vec<OutputRow> {
    let own = |o: &OutputRow| {
        received
            .iter()
            .any(|r| r.pool == o.pool && r.index == o.index)
    };
    let mut sent: Vec<OutputRow> = outgoing.into_iter().filter(|o| !own(o)).collect();
    sent.iter_mut().for_each(|o| o.role = OutputRole::Sent);

    let external = |o: &OutputRow| {
        o.scope
            .as_deref()
            .is_none_or(|s| s.eq_ignore_ascii_case("external"))
    };
    let any_external = received.iter().any(external);
    let moved_into: Vec<Pool> = received
        .iter()
        .filter(|o| o.value > 0 && !input_pools.contains(&o.pool))
        .map(|o| o.pool)
        .collect();
    for o in &mut received {
        o.role = match kind {
            TxKind::Received => OutputRole::Received,
            TxKind::Shield => OutputRole::ToSelf,
            TxKind::Moved(_) if moved_into.contains(&o.pool) => OutputRole::ToSelf,
            TxKind::Moved(_) => OutputRole::Change,
            TxKind::SentToSelf if external(o) || !any_external => OutputRole::ToSelf,
            TxKind::Sent if external(o) => OutputRole::ToSelf,
            TxKind::SentToSelf | TxKind::Sent => OutputRole::Change,
        };
    }
    sent.extend(received);
    sent
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::state::SpendState;

    fn output(pool: Pool, value: u64, index: u32, scope: &str) -> OutputRow {
        OutputRow {
            pool,
            role: OutputRole::Received,
            value,
            index,
            address: None,
            memo: None,
            spend: SpendState::Unspent,
            scope: Some(scope.to_string()),
        }
    }

    fn roles(outputs: &[OutputRow]) -> Vec<(Pool, u64, OutputRole)> {
        outputs.iter().map(|o| (o.pool, o.value, o.role)).collect()
    }

    #[test]
    fn a_move_into_ironwood_lists_each_output_once() {
        // the reported mainnet transaction: 0.0102 ZEC of Orchard in, 0.01 ZEC to the wallet's own
        // Ironwood address, and a zero-value Orchard output as padding
        let received = vec![
            output(Pool::Orchard, 0, 1, "internal"),
            output(Pool::Ironwood, 1_000_000, 1, "internal"),
        ];
        let outgoing = vec![output(Pool::Ironwood, 1_000_000, 1, "internal")];
        let out = assign_roles(
            TxKind::Moved(Pool::Ironwood),
            &[Pool::Orchard],
            received,
            outgoing,
        );
        assert_eq!(
            roles(&out),
            vec![
                (Pool::Orchard, 0, OutputRole::Change),
                (Pool::Ironwood, 1_000_000, OutputRole::ToSelf),
            ],
            "the outgoing copy of the Ironwood output is gone, and nothing is tagged sent"
        );
    }

    #[test]
    fn a_payment_keeps_its_recipient_and_calls_the_rest_change() {
        let received = vec![output(Pool::Ironwood, 150_000, 1, "internal")];
        let outgoing = vec![output(Pool::Sapling, 10_000, 0, "external")];
        let out = assign_roles(TxKind::Sent, &[Pool::Ironwood], received, outgoing);
        assert_eq!(
            roles(&out),
            vec![
                (Pool::Sapling, 10_000, OutputRole::Sent),
                (Pool::Ironwood, 150_000, OutputRole::Change),
            ]
        );
    }

    #[test]
    fn a_payment_to_one_of_the_wallets_own_addresses_is_to_self() {
        let received = vec![
            output(Pool::Transparent, 17_000, 2, "external"),
            output(Pool::Ironwood, 150_000, 1, "internal"),
        ];
        let outgoing = vec![
            output(Pool::Transparent, 17_000, 2, "external"),
            output(Pool::Sapling, 23_000, 0, "external"),
        ];
        let out = assign_roles(TxKind::Sent, &[Pool::Ironwood], received, outgoing);
        assert_eq!(
            roles(&out),
            vec![
                (Pool::Sapling, 23_000, OutputRole::Sent),
                (Pool::Transparent, 17_000, OutputRole::ToSelf),
                (Pool::Ironwood, 150_000, OutputRole::Change),
            ]
        );
    }

    #[test]
    fn a_same_pool_send_to_self_splits_purpose_from_change() {
        let received = vec![
            output(Pool::Orchard, 40_000, 0, "internal"),
            output(Pool::Orchard, 0, 1, "external"),
        ];
        let out = assign_roles(TxKind::SentToSelf, &[Pool::Orchard], received, vec![]);
        assert_eq!(
            roles(&out),
            vec![
                (Pool::Orchard, 40_000, OutputRole::Change),
                (Pool::Orchard, 0, OutputRole::ToSelf),
            ]
        );
    }

    #[test]
    fn consolidating_into_change_is_all_to_self() {
        let received = vec![output(Pool::Sapling, 90_000, 0, "internal")];
        let out = assign_roles(TxKind::SentToSelf, &[Pool::Sapling], received, vec![]);
        assert_eq!(
            roles(&out),
            vec![(Pool::Sapling, 90_000, OutputRole::ToSelf)]
        );
    }

    #[test]
    fn everything_in_a_received_transaction_is_received() {
        let received = vec![output(Pool::Orchard, 5, 0, "external")];
        let out = assign_roles(TxKind::Received, &[], received, vec![]);
        assert_eq!(roles(&out), vec![(Pool::Orchard, 5, OutputRole::Received)]);
    }
}
