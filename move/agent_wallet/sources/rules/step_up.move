/// Above an owner-set amount per transaction, the owner must pay the transaction's gas.
///
/// Every other rule bounds what the agent key may do on its own. This one makes the owner a party to
/// the large ones: Sui lets only the gas owner produce the sponsor signature, so a transaction whose
/// sponsor is the wallet's owner carries proof that the owner signed those exact bytes. Whoever holds
/// the agent key cannot produce that, with or without Rill's signer in the way.
///
/// The total is per transaction and keyed by its digest, as `per_tx` keeps it, so splitting one payout
/// into several requests under the threshold still needs the owner once they add up past it.
///
/// Shipped in an upgrade, so the attached rule's type name carries the upgraded package's id. A
/// builder that does not know the rule leaves it out of the prove list and `confirm_spend` aborts
/// `E_RULE_NOT_SATISFIED`: an old client fails closed rather than spending past it.
///
/// Abort codes:
/// - `E_NEEDS_OWNER` (1): the transaction's total is above the threshold and its gas sponsor is not
///   the wallet's owner.
module agent_wallet::step_up;

use agent_wallet::agent_wallet::{Self as aw, AgentWallet, SpendRequest};
use agent_wallet::version::Version;

const E_NEEDS_OWNER: u64 = 1;

public struct Rule has drop {}

public struct Config has store, drop {
    threshold: u64,
    tx_digest: vector<u8>,
    spent_in_tx: u64,
}

/// Owner-only: attach the rule. A transaction whose released total is above `threshold` must be
/// gas-sponsored by the owner; at or under it the agent pays its own gas as before.
public fun add<T>(wallet: &mut AgentWallet<T>, version: &Version, threshold: u64, ctx: &TxContext) {
    aw::add_rule<T, Rule, Config>(Rule {}, wallet, version, Config { threshold, tx_digest: vector[], spent_in_tx: 0 }, ctx);
}

/// Owner-only: detach the rule.
public fun remove<T>(wallet: &mut AgentWallet<T>, version: &Version, ctx: &TxContext) {
    aw::remove_rule<T, Rule, Config>(wallet, version, ctx);
}

/// Add this request to the transaction's running total and stamp a receipt onto `req`. Aborts
/// `E_NEEDS_OWNER` once the total is above the threshold and the owner is not the gas sponsor.
public fun prove<T>(req: &mut SpendRequest, wallet: &mut AgentWallet<T>, version: &Version, ctx: &TxContext) {
    version.check_is_valid();
    let amount = req.request_amount();
    let digest = *ctx.digest();
    let owner = aw::owner(wallet);
    let cfg: &mut Config = aw::rule_config_mut<T, Rule, Config>(Rule {}, wallet);
    if (cfg.tx_digest != digest) {
        cfg.tx_digest = digest;
        cfg.spent_in_tx = 0;
    };
    // An overflow here aborts the transaction, which is the closed side of this check.
    cfg.spent_in_tx = cfg.spent_in_tx + amount;
    if (cfg.spent_in_tx > cfg.threshold) {
        assert!(ctx.sponsor() == option::some(owner), E_NEEDS_OWNER);
    };
    aw::add_receipt(Rule {}, wallet, req);
}

/// The attached config, for clients that report the threshold. Aborts if the rule is not attached.
public fun config<T>(wallet: &AgentWallet<T>): &Config {
    aw::rule_config<T, Rule, Config>(Rule {}, wallet)
}

public fun threshold(cfg: &Config): u64 { cfg.threshold }
