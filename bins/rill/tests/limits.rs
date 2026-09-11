//! What a wallet would allow right now, and which rule says so.
//!
//! Every case here is one an agent hits in practice: a per-transaction cap below the balance, a
//! budget almost used up, a rate-limit window that has rolled over, a time window not yet open. The
//! number is useless if any of them is wrong, because the whole point is that an agent can trust it
//! instead of attempting a spend to find out.

use rill_cli::limits::{largest_spend_now, rule_config, wallet_facts, RuleConfig, WalletFacts};
use serde_json::json;

const HOUR: u64 = 3_600_000;
const NOW: u64 = 1_700_000_000_000;

fn wallet() -> WalletFacts {
    WalletFacts {
        balance: 149_000_000,
        spent: 51_000_000,
        expires_at_ms: NOW + HOUR,
        revoked: false,
    }
}

/// The real wallet this was built against: 0.149 left, a 0.2 budget with 0.051 gone, a 0.05 cap.
///
/// `per_tx` is the binding rule and the balance is not, which is the ordinary case and the one a
/// report that only showed the balance would get wrong.
#[test]
fn the_tightest_rule_is_the_answer_and_it_is_named() {
    let h = largest_spend_now(
        &wallet(),
        &[
            RuleConfig::Budget {
                total_mist: 200_000_000,
                spent: 51_000_000,
            },
            RuleConfig::PerTx {
                max_mist: 50_000_000,
            },
        ],
        0,
        NOW,
    );
    assert_eq!(h.base_units, 50_000_000);
    assert_eq!(h.bound_by, "per_tx");
    assert!(h.certain);
}

/// With no rules at all the balance is the only bound, and it is named as such rather than left
/// blank. A wallet in this state is the hole v0.5.0 refuses to spend from, and the read still has to
/// describe it honestly.
#[test]
fn with_nothing_attached_the_balance_is_the_bound() {
    let h = largest_spend_now(&wallet(), &[], 0, NOW);
    assert_eq!(h.base_units, 149_000_000);
    assert_eq!(h.bound_by, "balance");
}

/// A budget nearly exhausted beats a generous per-transaction cap.
#[test]
fn a_nearly_spent_budget_binds_below_a_large_per_tx_cap() {
    let h = largest_spend_now(
        &wallet(),
        &[
            RuleConfig::Budget {
                total_mist: 60_000_000,
                spent: 51_000_000,
            },
            RuleConfig::PerTx {
                max_mist: 50_000_000,
            },
        ],
        0,
        NOW,
    );
    assert_eq!(h.base_units, 9_000_000, "60 minus 51, not the cap");
    assert_eq!(h.bound_by, "budget");
}

/// A budget spent past its total does not wrap around into an enormous allowance.
#[test]
fn an_overspent_budget_reports_zero_rather_than_underflowing() {
    let h = largest_spend_now(
        &wallet(),
        &[RuleConfig::Budget {
            total_mist: 10,
            spent: 99,
        }],
        0,
        NOW,
    );
    assert_eq!(h.base_units, 0);
    assert_eq!(h.bound_by, "budget");
}

/// Inside a rate-limit window, what is left of the window binds.
#[test]
fn inside_a_rate_limit_window_the_remainder_binds() {
    let h = largest_spend_now(
        &wallet(),
        &[RuleConfig::RateLimit {
            window_ms: HOUR,
            window_max: 20_000_000,
            window_start_ms: NOW - 60_000,
            spent_in_window: 15_000_000,
        }],
        0,
        NOW,
    );
    assert_eq!(h.base_units, 5_000_000);
    assert_eq!(h.bound_by, "rate_limit");
}

/// Once the window has elapsed the whole allowance is available again.
///
/// The contract resets the window on the next spend, so reporting the spent-down remainder of a
/// window that has already closed would tell an agent it could spend less than it can. That is the
/// direction of error that makes an agent ask its owner to raise a limit that was never reached.
#[test]
fn an_elapsed_rate_limit_window_reports_the_full_allowance() {
    let h = largest_spend_now(
        &wallet(),
        &[RuleConfig::RateLimit {
            window_ms: HOUR,
            window_max: 20_000_000,
            window_start_ms: NOW - HOUR - 1,
            spent_in_window: 20_000_000,
        }],
        0,
        NOW,
    );
    assert_eq!(h.base_units, 20_000_000, "the window rolled over");
    assert_eq!(h.bound_by, "rate_limit");
}

/// A time window that has not opened, or has closed, allows nothing at all.
#[test]
fn a_closed_time_window_allows_nothing() {
    for (not_before, not_after, label) in [
        (NOW + 1, NOW + HOUR, "not yet open"),
        (NOW - HOUR, NOW, "already closed"),
    ] {
        let h = largest_spend_now(
            &wallet(),
            &[RuleConfig::TimeWindow {
                not_before_ms: not_before,
                not_after_ms: not_after,
            }],
            0,
            NOW,
        );
        assert_eq!(h.base_units, 0, "{label}");
        assert_eq!(h.bound_by, "time_window", "{label}");
    }
}

/// Expiry and revocation are absolute and are named, rather than reported as a zero balance.
///
/// An agent told "0, bounded by balance" tops the wallet up. Told "0, because it expired" it asks
/// the owner to extend it. The number is the same and the action is not.
#[test]
fn expiry_and_revocation_are_named_rather_than_shown_as_no_funds() {
    let expired = WalletFacts {
        expires_at_ms: NOW,
        ..wallet()
    };
    let h = largest_spend_now(&expired, &[], 0, NOW);
    assert_eq!((h.base_units, h.bound_by.as_str()), (0, "expiry"),);

    let revoked = WalletFacts {
        revoked: true,
        ..wallet()
    };
    let h = largest_spend_now(&revoked, &[], 0, NOW);
    assert_eq!((h.base_units, h.bound_by.as_str()), (0, "revoked"));
}

/// An unrecognised rule makes the number an upper bound, and it says so.
///
/// This is the one property that cannot be quietly dropped. A rule this binary cannot read might cap
/// the spend below the figure here, and a report that presented the figure as final would be stating
/// a limit it never established.
#[test]
fn an_unreadable_rule_makes_the_answer_uncertain() {
    let h = largest_spend_now(&wallet(), &[RuleConfig::PerTx { max_mist: 1 }], 1, NOW);
    assert_eq!(h.base_units, 1);
    assert!(
        !h.certain,
        "a rule that could not be read must not be reported as accounted for"
    );
}

/// The wallet's own fields parse out of exactly what the node sends: u64 as text.
#[test]
fn the_nodes_string_numbers_parse_without_losing_precision() {
    let facts = wallet_facts(&json!({
        "budget": "18446744073709551615",
        "spent": "9007199254740993",
        "expires_at_ms": "1791656336377",
        "revoked": false
    }))
    .expect("the node's shape parses");
    assert_eq!(facts.balance, u64::MAX, "the top of the range survives");
    assert_eq!(
        facts.spent, 9_007_199_254_740_993,
        "and so does a value a double would have rounded"
    );
}

/// A rule's config parses out of the dynamic field envelope, keyed by the Config type.
#[test]
fn a_configs_numbers_come_out_of_the_dynamic_field_envelope() {
    let field = json!({
        "id": "0x45",
        "name": { "dummy_field": false },
        "value": { "spent": "51000000", "total_mist": "200000000" }
    });
    assert_eq!(
        rule_config("0xcaf::budget::Config", &field),
        Some(RuleConfig::Budget {
            total_mist: 200_000_000,
            spent: 51_000_000
        })
    );

    assert_eq!(
        rule_config(
            "0xcaf::per_tx::Config",
            &json!({ "value": { "max_mist": "50000000" } })
        ),
        Some(RuleConfig::PerTx {
            max_mist: 50_000_000
        })
    );

    // A module this code does not know is not guessed at.
    assert_eq!(
        rule_config(
            "0xcaf::something_new::Config",
            &json!({ "value": { "whatever": "1" } })
        ),
        None
    );
    // Neither is a type that is not a Config at all.
    assert_eq!(rule_config("0xcaf::budget::Rule", &field), None);
}
