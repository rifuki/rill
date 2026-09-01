//! Minting an agent wallet and the capability that drives it.
//!
//! # Why this is the first thing, not the last
//!
//! Every other path in this repo assumes a wallet and an `AgentCap` that already exist. On testnet
//! they do — but they belong to the superseded package (see [`crate::deployments`]), and a
//! capability minted by one package cannot authorise a call in another. So nothing downstream can
//! be demonstrated end to end until a wallet is created against the current package.
//!
//! `create_wallet` mints the cap to the agent and shares the wallet in one call, which is what
//! makes this a single command rather than a setup procedure.
//!
//! # The empty policy is the trap
//!
//! The contract shares the wallet with **no rules attached**, and `confirm_spend` on an empty
//! policy requires zero receipts — so a wallet created and handed over as-is is a wallet with no
//! limits at all, holding real funds. The contract's own note says composing at least one
//! restriction is the owner's responsibility.
//!
//! That is a responsibility a type can carry instead of a comment, so it does:
//! [`build_create_wallet`] takes the manifest that will govern the wallet and refuses an empty one
//! before emitting anything. The rule calls themselves must follow in the same transaction, which
//! is why this returns the arguments needed to attach them rather than a finished transaction.

use rill_core::manifest::{to_on_chain_rule_params, CapabilityManifest, ManifestError};
use sui_sdk_types::{Address, Identifier};
use sui_transaction_builder::{Argument, Function, TransactionBuilder};

use crate::shared::{SharedObjects, UnknownSharedVersion};

/// Everything `create_wallet` needs that the manifest does not carry.
#[derive(Clone)]
pub struct NewWallet {
    pub package_id: Address,
    /// The shared `Version` object the package gates itself on.
    pub version_id: Address,
    /// Who receives the `AgentCap` — the agent, not the owner.
    pub agent: Address,
    /// When the wallet stops working, in milliseconds since the epoch.
    pub expires_at_ms: u64,
    /// The coin type the wallet holds. Must match the manifest's.
    pub coin_type: String,
    /// What will govern it. Refused if empty; see the module note.
    pub manifest: CapabilityManifest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateError {
    BadIdentifier(String),
    Manifest(ManifestError),
    UnknownShared(UnknownSharedVersion),
    /// The wallet's coin type and the manifest's disagree.
    CoinTypeMismatch {
        wallet: String,
        manifest: String,
    },
    /// An expiry already in the past mints a wallet that cannot be used.
    AlreadyExpired {
        expires_at_ms: u64,
        now_ms: u64,
    },
}

impl std::fmt::Display for CreateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadIdentifier(s) => write!(f, "\"{s}\" is not a valid Move identifier or type"),
            Self::Manifest(e) => write!(f, "{e}"),
            Self::UnknownShared(e) => write!(f, "{e}"),
            Self::CoinTypeMismatch { wallet, manifest } => write!(
                f,
                "the wallet holds {wallet} but its manifest governs {manifest}; the limits would \
                 be denominated in a coin the wallet does not hold"
            ),
            Self::AlreadyExpired {
                expires_at_ms,
                now_ms,
            } => write!(
                f,
                "expiry {expires_at_ms} is already past ({now_ms}); request_spend would abort on \
                 the first use, after the funds are already in the wallet"
            ),
        }
    }
}

impl std::error::Error for CreateError {}

impl From<UnknownSharedVersion> for CreateError {
    fn from(e: UnknownSharedVersion) -> Self {
        Self::UnknownShared(e)
    }
}

fn ident(s: &str) -> Result<Identifier, CreateError> {
    Identifier::new(s).map_err(|_| CreateError::BadIdentifier(s.to_owned()))
}

/// Emit `create_wallet`, returning the arguments its rule calls need.
///
/// The wallet is shared by the call, so it cannot be referenced by a later command in the same
/// transaction — the rule modules' `add` take `&mut AgentWallet`, and a freshly shared object has
/// no known initial version yet. Attaching rules is therefore a second transaction, and the
/// returned value is what the caller carries forward to build it.
pub fn build_create_wallet(
    tx: &mut TransactionBuilder,
    wallet: &NewWallet,
    funds: Argument,
    shared: &SharedObjects,
    now_ms: u64,
) -> Result<(), CreateError> {
    // An empty manifest is refused here rather than by the contract, which accepts one. See the
    // module note: `confirm_spend` on an empty policy requires zero receipts.
    let _ = to_on_chain_rule_params(&wallet.manifest).map_err(CreateError::Manifest)?;

    if wallet.coin_type != wallet.manifest.wallet_coin_type {
        return Err(CreateError::CoinTypeMismatch {
            wallet: wallet.coin_type.clone(),
            manifest: wallet.manifest.wallet_coin_type.clone(),
        });
    }

    if wallet.expires_at_ms <= now_ms {
        return Err(CreateError::AlreadyExpired {
            expires_at_ms: wallet.expires_at_ms,
            now_ms,
        });
    }

    let coin_type = wallet
        .coin_type
        .parse()
        .map_err(|_| CreateError::BadIdentifier(wallet.coin_type.clone()))?;

    let version = tx.object(shared.input(wallet.version_id, false)?);
    let agent = tx.pure(&wallet.agent);
    let expires_at = tx.pure(&wallet.expires_at_ms);

    tx.move_call(
        Function::new(
            wallet.package_id,
            ident("agent_wallet")?,
            ident("create_wallet")?,
        )
        .with_type_args(vec![coin_type]),
        vec![version, funds, agent, expires_at],
    );
    Ok(())
}

/// The target this emits, for a pinned sequence.
pub fn expected_create_targets(package_id: Address) -> Vec<String> {
    vec![format!("{package_id}::agent_wallet::create_wallet")]
}
