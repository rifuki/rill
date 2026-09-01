//! The funding chokepoint: how a coin leaves an agent wallet.
//!
//! This is a hot potato, and the shape is what makes it safe. `request_spend` mints a
//! `SpendRequest` that has no abilities — it cannot be dropped, stored, or copied — so the only
//! way for the transaction to type-check at all is to route it through every attached rule's
//! `prove` and hand it to `confirm_spend`. Miss one rule and the transaction does not fail a
//! check; it fails to compile as a transaction.
//!
//! The reference's signer still expects a `spend()` entry point that the deployed contract no
//! longer has, so the run-sets it generates can never validate. Nothing here emits that call.
//!
//! ## Rule order
//!
//! `confirm_spend` compares receipts as a set, so order does not matter on-chain. It is emitted in
//! manifest order anyway, because a deterministic sequence is what lets the signer compare the
//! transaction it received against the one it expected, byte for byte.

use rill_core::manifest::{to_on_chain_rule_params, CapabilityManifest, ManifestError};
use sui_sdk_types::{Address, Identifier};
use sui_transaction_builder::{Argument, Function, ObjectInput, TransactionBuilder};

use crate::shared::{SharedObjects, UnknownSharedVersion};

/// Sui's shared `Clock`. Always at this address, on every network.
pub const CLOCK_ID: &str = "0x6";

/// Everything needed to spend from one agent wallet.
#[derive(Clone)]
pub struct WalletBinding {
    /// The published `agent_wallet` package.
    pub package_id: Address,
    /// The shared `AgentWallet<T>` object.
    pub wallet_id: Address,
    /// The `AgentCap` the agent holds.
    pub cap: ObjectInput,
    /// The shared `Version` object that gates upgrades.
    pub version_id: Address,
    /// The coin the wallet holds, as a type tag string (e.g. `0x2::sui::SUI`).
    pub coin_type: String,
    /// The rules attached on-chain. Drives which `prove` calls must be emitted.
    pub manifest: CapabilityManifest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpendError {
    /// A shared object was referenced before its initial version was known.
    UnknownShared(UnknownSharedVersion),
    ZeroAmount,
    Manifest(ManifestError),
    /// A rule is attached on-chain but this builder does not know how to prove it. Refusing is the
    /// only safe answer: emitting the sequence without that rule's receipt produces a transaction
    /// that cannot be confirmed, and guessing at the call shape would produce one that is wrong.
    UnprovableRule(&'static str),
    BadIdentifier(String),
}

impl std::fmt::Display for SpendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownShared(e) => write!(f, "{e}"),
            Self::ZeroAmount => write!(f, "refusing to build a spend of zero"),
            Self::Manifest(e) => write!(f, "capability manifest is invalid: {e}"),
            Self::UnprovableRule(module) => write!(
                f,
                "the wallet has a \"{module}\" rule attached, but this builder cannot emit its \
                 proof — the resulting transaction could never be confirmed"
            ),
            Self::BadIdentifier(s) => write!(f, "\"{s}\" is not a valid Move identifier"),
        }
    }
}

impl std::error::Error for SpendError {}

impl From<UnknownSharedVersion> for SpendError {
    fn from(e: UnknownSharedVersion) -> Self {
        Self::UnknownShared(e)
    }
}

fn ident(s: &str) -> Result<Identifier, SpendError> {
    Identifier::new(s).map_err(|_| SpendError::BadIdentifier(s.to_owned()))
}
/// Emit the gated sequence for an explicit list of rule modules.
///
/// # The module list is the thing that must be right
///
/// `confirm_spend` compares the receipts on the hot potato against the wallet's *live* policy, so
/// the sequence must name exactly the modules attached to that wallet — no more, no fewer. Emitting
/// a `prove` for a rule that is not attached aborts inside `df::borrow_mut`, with no abort code of
/// its own to explain it. Omitting one that is aborts `E_RULE_NOT_SATISFIED` at the last command,
/// after every other check has passed.
///
/// A manifest is one way to obtain that list, and [`crate::policy_read`] — reading the chain — is
/// the better one. This takes the list itself so the caller can use either, and so the two never
/// silently disagree inside this function.
pub fn build_gated_spend_for_modules(
    tx: &mut TransactionBuilder,
    binding: &WalletBinding,
    amount_base_units: u64,
    modules: &[&str],
    shared: &SharedObjects,
) -> Result<Argument, SpendError> {
    if amount_base_units == 0 {
        return Err(SpendError::ZeroAmount);
    }

    let coin_type: sui_sdk_types::TypeTag = binding
        .coin_type
        .parse()
        .map_err(|_| SpendError::BadIdentifier(binding.coin_type.clone()))?;

    // Shared objects. The wallet and version are mutated by the sequence; the clock is read.
    // Each carries the version it was first shared at, read from the chain — see `shared`.
    let wallet = tx.object(shared.input(binding.wallet_id, true)?);
    let version = tx.object(shared.input(binding.version_id, false)?);
    let clock = tx.object(shared.input(CLOCK_ID.parse().expect("0x6 is a valid address"), false)?);
    let cap = tx.object(binding.cap.clone());
    let amount = tx.pure(&amount_base_units);

    let request = tx.move_call(
        Function::new(
            binding.package_id,
            ident("agent_wallet")?,
            ident("request_spend")?,
        )
        .with_type_args(vec![coin_type.clone()]),
        vec![wallet, cap, version, amount, clock],
    );

    for module in modules {
        // `rate_limit` and `time_window` decide against the current time and take the clock;
        // `budget` and `per_tx` do not. Emitting three arguments for all four builds a call with
        // the wrong arity, which the node rejects.
        let mut args = vec![request, wallet, version];
        if matches!(*module, "rate_limit" | "time_window") {
            args.push(clock);
        }
        tx.move_call(
            Function::new(binding.package_id, ident(module)?, ident("prove")?)
                .with_type_args(vec![coin_type.clone()]),
            args,
        );
    }

    Ok(tx.move_call(
        Function::new(
            binding.package_id,
            ident("agent_wallet")?,
            ident("confirm_spend")?,
        )
        .with_type_args(vec![coin_type]),
        vec![wallet, request, version, clock],
    ))
}

/// Emit `request_spend` → one `prove` per attached rule → `confirm_spend`, returning the released
/// coin.
///
/// The returned `Argument` is a `Coin<T>` that the caller **must** fully consume. Leaving any of
/// it unused aborts execution with `UnusedValueWithoutDrop` — a failure mode the compiler's settle
/// sweep exists to prevent, and one that a simulation does catch (verified against a live node).
pub fn build_manifest_gated_spend(
    tx: &mut TransactionBuilder,
    binding: &WalletBinding,
    amount_base_units: u64,
    // Initial shared versions read from the chain; a missing one refuses the build.
    shared: &SharedObjects,
) -> Result<Argument, SpendError> {
    if amount_base_units == 0 {
        return Err(SpendError::ZeroAmount);
    }
    let rule_params = to_on_chain_rule_params(&binding.manifest).map_err(SpendError::Manifest)?;

    let coin_type = binding
        .coin_type
        .parse()
        .map_err(|_| SpendError::BadIdentifier(binding.coin_type.clone()))?;

    // Shared objects. The wallet and version are mutated by the sequence; the clock is read.
    // Each carries the version it was first shared at, read from the chain — see `shared`.
    let wallet = tx.object(shared.input(binding.wallet_id, true)?);
    let version = tx.object(shared.input(binding.version_id, false)?);
    let clock = tx.object(shared.input(CLOCK_ID.parse().expect("0x6 is a valid address"), false)?);
    let cap = tx.object(binding.cap.clone());
    let amount = tx.pure(&amount_base_units);

    let request = tx.move_call(
        Function::new(
            binding.package_id,
            ident("agent_wallet")?,
            ident("request_spend")?,
        )
        .with_type_args(vec![coin_type]),
        vec![wallet, cap, version, amount, clock],
    );

    // One prove per rule, in manifest order. A rule this builder does not recognise is refused
    // rather than skipped — see `UnprovableRule`.
    for params in &rule_params {
        let module = params.module;
        let coin_type_arg = binding
            .coin_type
            .parse()
            .map_err(|_| SpendError::BadIdentifier(binding.coin_type.clone()))?;
        // `rate_limit` and `time_window` decide against the current time and take the clock;
        // `budget` and `per_tx` do not. Emitting three arguments for all four builds a call with
        // the wrong arity, which the node rejects — so the shape comes from the manifest rather
        // than from an assumption that the rules are uniform. They are not.
        let mut args = vec![request, wallet, version];
        if params.prove_takes_clock {
            args.push(clock);
        }
        tx.move_call(
            Function::new(binding.package_id, ident(module)?, ident("prove")?)
                .with_type_args(vec![coin_type_arg]),
            args,
        );
    }

    let coin_type_arg = binding
        .coin_type
        .parse()
        .map_err(|_| SpendError::BadIdentifier(binding.coin_type.clone()))?;
    Ok(tx.move_call(
        Function::new(
            binding.package_id,
            ident("agent_wallet")?,
            ident("confirm_spend")?,
        )
        .with_type_args(vec![coin_type_arg]),
        vec![wallet, request, version, clock],
    ))
}

/// The Move call targets this sequence emits, in order.
///
/// The signer pins the transaction against exactly this list, so it is derived from the same
/// manifest rather than written out twice — two hand-maintained lists would drift, and the drift
/// would surface as a signer refusing a transaction the compiler considered correct.
pub fn expected_spend_targets(binding: &WalletBinding) -> Result<Vec<String>, SpendError> {
    let rule_params = to_on_chain_rule_params(&binding.manifest).map_err(SpendError::Manifest)?;
    let pkg = binding.package_id;
    let mut targets = vec![format!("{pkg}::agent_wallet::request_spend")];
    for params in &rule_params {
        targets.push(format!("{pkg}::{}::prove", params.module));
    }
    targets.push(format!("{pkg}::agent_wallet::confirm_spend"));
    Ok(targets)
}
