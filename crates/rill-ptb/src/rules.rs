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
