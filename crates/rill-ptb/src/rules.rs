//! Attaching a manifest's rules to a wallet.
//!
//! # The step between creating a wallet and using one
//!
//! `create_wallet` shares the wallet with an empty policy, and `confirm_spend` on an empty policy
//! requires zero receipts. So between minting a wallet and handing over its capability there is a
//! step that is easy to skip and expensive to skip: attaching the rules that make the capability
//! mean anything.
//!
//! # It cannot be the same transaction
//!
//! `create_wallet` shares the wallet, and every `add` takes `&mut AgentWallet`. A shared object is
//! referenced by the version it was shared at, and an object created by the transaction in hand has
//! no such version yet — there is nothing to reference it by until the transaction lands. So this is
//! necessarily a second transaction, and the wallet id it needs comes from the first one's effects.
//!
//! # One shape, four rules
//!
//! Each `add` takes `(wallet, version, ...config)` and nothing else, with the config values in the
//! order Move's constructor declares them. `rill-core`'s manifest already produces them in exactly
//! that order — it is the single declaration producer, and this is one of the three projections it
//! exists to keep in agreement. So this emits them positionally rather than matching on rule kind,
//! which means a new rule needs no change here at all.

use rill_core::manifest::{to_on_chain_rule_params, CapabilityManifest, ManifestError};
use sui_sdk_types::{Address, Identifier};
use sui_transaction_builder::{Function, TransactionBuilder};

use crate::shared::{SharedObjects, UnknownSharedVersion};

/// The wallet whose policy is being filled in.
#[derive(Clone)]
pub struct RuleTarget {
    pub package_id: Address,
    /// The shared wallet, from the creating transaction's effects.
    pub wallet_id: Address,
    pub version_id: Address,
    /// The wallet's coin type — the `T` every `add` is generic over.
    pub coin_type: String,
    pub manifest: CapabilityManifest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleError {
    BadIdentifier(String),
    Manifest(ManifestError),
    UnknownShared(UnknownSharedVersion),
}

impl std::fmt::Display for RuleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadIdentifier(s) => write!(f, "\"{s}\" is not a valid Move identifier or type"),
            Self::Manifest(e) => write!(f, "{e}"),
            Self::UnknownShared(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RuleError {}

impl From<UnknownSharedVersion> for RuleError {
    fn from(e: UnknownSharedVersion) -> Self {
        Self::UnknownShared(e)
    }
}

fn ident(s: &str) -> Result<Identifier, RuleError> {
    Identifier::new(s).map_err(|_| RuleError::BadIdentifier(s.to_owned()))
}

/// Emit one `add` per on-chain rule, in manifest order.
///
/// Called by the wallet's owner, not the agent: every `add` asserts owner-only inside
/// `agent_wallet::add_rule`. Pre-flight rules project nothing here — they are enforced before a
/// transaction is built, and `to_on_chain_rule_params` already leaves them out.
pub fn build_attach_rules(
    tx: &mut TransactionBuilder,
    target: &RuleTarget,
    shared: &SharedObjects,
) -> Result<usize, RuleError> {
    // Validates the manifest too — an empty one is refused here as it is at creation.
    let rules = to_on_chain_rule_params(&target.manifest).map_err(RuleError::Manifest)?;

    let coin_type: sui_sdk_types::TypeTag = target
        .coin_type
        .parse()
        .map_err(|_| RuleError::BadIdentifier(target.coin_type.clone()))?;

    let wallet = tx.object(shared.input(target.wallet_id, true)?);
    let version = tx.object(shared.input(target.version_id, false)?);

    for rule in &rules {
        // (wallet, version, ...config) — the config values in Move's own constructor order, which
        // is the order the manifest produces them in. See the module note.
        let mut args = vec![wallet, version];
        for (_field, value) in &rule.config {
            args.push(tx.pure(value));
        }

        tx.move_call(
            Function::new(target.package_id, ident(rule.module)?, ident("add")?)
                .with_type_args(vec![coin_type.clone()]),
            args,
        );
    }

    Ok(rules.len())
}

/// Reconcile a wallet's attached rules to a manifest.
///
/// # Attaching is not idempotent, and there is no "set"
///
/// `add_rule` aborts `E_RULE_ALREADY_SET` (11) when the witness type is already attached, so
/// re-running an attach fails, and adding a third rule later would re-emit the first two and fail
/// with it. There is no update call either: changing a cap means `remove` then `add`.
///
/// So the operation a caller actually wants is a reconciliation against what is on chain — which is
/// why this takes the live module list rather than assuming the wallet is empty. Read it with
/// [`crate::policy_read`]; do not pass a guess.
///
/// Removes come first. A rule that is being replaced must be gone before its `add` runs, and
/// ordering them the other way is the same abort with a more confusing cause.
pub fn build_reconcile_rules(
    tx: &mut TransactionBuilder,
    target: &RuleTarget,
    attached: &[&str],
    shared: &SharedObjects,
) -> Result<Reconciliation, RuleError> {
    let wanted = to_on_chain_rule_params(&target.manifest).map_err(RuleError::Manifest)?;

    let coin_type: sui_sdk_types::TypeTag = target
        .coin_type
        .parse()
        .map_err(|_| RuleError::BadIdentifier(target.coin_type.clone()))?;

    // A rule whose config is unchanged is still removed and re-added: the config is not readable
    // from `policy_rules`, which reports types only. Re-attaching an identical limit is a no-op in
    // effect and costs one command; guessing that it is unchanged would silently keep an old cap.
    let to_remove: Vec<&str> = attached
        .iter()
        .copied()
        .filter(|m| wanted.iter().any(|w| w.module == *m))
        .collect();
    let orphaned: Vec<String> = attached
        .iter()
        .filter(|m| !wanted.iter().any(|w| w.module == **m))
        .map(|m| (*m).to_owned())
        .collect();

    let wallet = tx.object(shared.input(target.wallet_id, true)?);
    let version = tx.object(shared.input(target.version_id, false)?);

    for module in to_remove.iter().chain(
        orphaned
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .iter(),
    ) {
        tx.move_call(
            Function::new(target.package_id, ident(module)?, ident("remove")?)
                .with_type_args(vec![coin_type.clone()]),
            vec![wallet, version],
        );
    }

    for rule in &wanted {
        let mut args = vec![wallet, version];
        for (_field, value) in &rule.config {
            args.push(tx.pure(value));
        }
        tx.move_call(
            Function::new(target.package_id, ident(rule.module)?, ident("add")?)
                .with_type_args(vec![coin_type.clone()]),
            args,
        );
    }

    Ok(Reconciliation {
        removed: to_remove.iter().map(|m| (*m).to_owned()).collect(),
        orphaned,
        added: wanted.iter().map(|r| r.module.to_owned()).collect(),
    })
}

/// What a reconciliation did, so a caller can say it rather than guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reconciliation {
    /// Attached, wanted, and re-attached with the manifest's current values.
    pub removed: Vec<String>,
    /// Attached but not in the manifest — detached and not restored.
    pub orphaned: Vec<String>,
    pub added: Vec<String>,
}

impl Reconciliation {
    /// True when the wallet already carried exactly this manifest's rules.
    pub fn is_no_change(&self) -> bool {
        self.orphaned.is_empty() && self.removed.len() == self.added.len()
    }
}

/// The targets attaching a manifest's rules emits, in order, for a pinned sequence.
pub fn expected_attach_targets(
    package_id: Address,
    manifest: &CapabilityManifest,
) -> Result<Vec<String>, RuleError> {
    Ok(to_on_chain_rule_params(manifest)
        .map_err(RuleError::Manifest)?
        .iter()
        .map(|r| format!("{package_id}::{}::add", r.module))
        .collect())
}
