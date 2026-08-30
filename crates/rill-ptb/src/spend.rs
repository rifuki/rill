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

fn ident(s: &str) -> Result<Identifier, SpendError> {
    Identifier::new(s).map_err(|_| SpendError::BadIdentifier(s.to_owned()))
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
    let wallet = tx.object(ObjectInput::shared(binding.wallet_id, 0, true));
    let version = tx.object(ObjectInput::shared(binding.version_id, 0, false));
    let clock = tx.object(ObjectInput::shared(
        CLOCK_ID.parse().expect("0x6 is a valid address"),
        0,
        false,
    ));
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
        tx.move_call(
            Function::new(binding.package_id, ident(module)?, ident("prove")?)
                .with_type_args(vec![coin_type_arg]),
            vec![request, wallet, version],
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
