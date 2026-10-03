/// Aggregate spend ceiling for one transaction. Reservations are keyed by its digest.
///
/// Abort codes:
/// - `E_OVER_PER_TX` (1): the request's amount exceeds the configured `max_mist`.
module agent_wallet::per_tx;

use agent_wallet::agent_wallet::{Self as aw, AgentWallet, SpendRequest};
use agent_wallet::version::Version;

const E_OVER_PER_TX: u64 = 1;

public struct Rule has drop {}

public struct Config has store, drop {
    max_mist: u64,
    tx_digest: vector<u8>,
    spent_in_tx: u64,
}

/// Owner-only: attach the per-tx rule with a ceiling of `max_mist` per `request_spend`.
public fun add<T>(wallet: &mut AgentWallet<T>, version: &Version, max_mist: u64, ctx: &TxContext) {
    aw::add_rule<T, Rule, Config>(Rule {}, wallet, version, Config { max_mist, tx_digest: vector[], spent_in_tx: 0 }, ctx);
}

/// Owner-only: detach the per-tx rule.
public fun remove<T>(wallet: &mut AgentWallet<T>, version: &Version, ctx: &TxContext) {
    aw::remove_rule<T, Rule, Config>(wallet, version, ctx);
}

/// Check the invariant and stamp a receipt onto `req`. Aborts `E_OVER_PER_TX` if the request's amount
/// exceeds the configured ceiling.
public fun prove<T>(req: &mut SpendRequest, wallet: &mut AgentWallet<T>, version: &Version, ctx: &TxContext) {
    version.check_is_valid();
    let amount = req.request_amount();
    let digest = *ctx.digest();
    let cfg: &mut Config = aw::rule_config_mut<T, Rule, Config>(Rule {}, wallet);
    if (cfg.tx_digest != digest) {
        cfg.tx_digest = digest;
        cfg.spent_in_tx = 0;
    };
    assert!(amount <= cfg.max_mist, E_OVER_PER_TX);
    assert!(cfg.spent_in_tx <= cfg.max_mist - amount, E_OVER_PER_TX);
    cfg.spent_in_tx = cfg.spent_in_tx + amount;
    aw::add_receipt(Rule {}, wallet, req);
}

public fun max_mist(cfg: &Config): u64 { cfg.max_mist }
