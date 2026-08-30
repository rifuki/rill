//! Compile a stored flow into an unsigned, strictly-simulated `ExecutionEnvelope`.
//!
//! This is the keyless half of the money path, and the whole of what this server contributes to a
//! transaction: it assembles commands, asks a fullnode whether they would work, and hands back
//! bytes nobody has signed. No key is reachable from here.
//!
//! # A refusal is not an envelope
//!
//! When simulation fails, this returns a [`BuildOutcome::Refused`] carrying the reason — never an
//! envelope with a flag set. An envelope is the thing a signer accepts; if a failed build could
//! produce one, the only thing standing between a broken transaction and a signature would be a
//! boolean that someone has to remember to check. A refusal has no `unsigned_ptb`, no digest, and
//! no version, so it is not merely rejected downstream — it cannot be presented as signable.

use rill_chain::{ChainError, SuiRead};
use rill_core::envelope::{
    digest_unsigned_ptb, Amount, DeepBookResolvedParams, ExecutionEnvelope, Network,
    StrictSimulationResult, Verification, EXECUTION_ENVELOPE_VERSION,
};
use rill_core::manifest::CapabilityManifest;
use rill_ptb::deepbook::{expected_order_targets, place_limit_order, LimitOrder, PoolSpec};
use rill_ptb::shared::SharedObjects;
use rill_ptb::spend::{build_manifest_gated_spend, expected_spend_targets, WalletBinding};
use sui_sdk_types::{Address, Digest};
use sui_transaction_builder::{ObjectInput, TransactionBuilder};

/// How long a built envelope stays signable. Short by construction — it is minted, carried to the
/// signer, and used within seconds, and the signer independently refuses anything longer.
pub const ENVELOPE_TTL_MS: u64 = 5 * 60 * 1000;

/// What the caller must supply that the stored flow cannot know: which wallet is funding this, and
/// with which objects.
pub struct BuildRequest {
    pub action_id: String,
    pub sender: Address,
    pub network: Network,
    pub wallet_package_id: Address,
    pub wallet_id: Address,
    pub agent_cap: ObjectInput,
    pub agent_cap_id: String,
    pub version_id: Address,
    pub manifest: CapabilityManifest,
    pub deepbook_package_id: Address,
    pub pool: PoolSpec,
    pub balance_manager_id: Address,
    pub trade_cap: ObjectInput,
    pub trade_cap_id: String,
    pub client_order_id: u64,
    /// Decimal strings. They never become floats anywhere in this path.
    pub price: String,
    pub quantity: String,
    pub is_bid: bool,
    pub pay_with_deep: bool,
    pub spend_base_units: u64,
    pub gas_budget: u64,
    pub gas_price: u64,
    pub gas_objects: Vec<ObjectInput>,
}

/// Either something a signer can act on, or a named reason it cannot.
pub enum BuildOutcome {
    Built(Box<ExecutionEnvelope>),
    /// Deliberately shaped so it can never be mistaken for the other. See the module note.
    Refused {
        code: &'static str,
        reason: String,
    },
}

impl BuildOutcome {
    fn refuse(code: &'static str, reason: impl Into<String>) -> Self {
        Self::Refused {
            code,
            reason: reason.into(),
        }
    }
}

/// Read the version each shared object was first shared at.
///
/// A shared object is referenced by the version it was *shared* at, not its current version and not
/// zero — a wrong one is rejected by the node before execution, with a message about the object
/// being missing that points at the address rather than the version. So it is read here, and an
/// object that turns out not to be shared is refused rather than entered anyway.
async fn resolve_shared(
    request: &BuildRequest,
    chain: &impl SuiRead,
) -> Result<SharedObjects, BuildOutcome> {
    let mut shared = SharedObjects::new();
    for id in [
        request.wallet_id,
        request.version_id,
        request.pool.pool_id,
        request.balance_manager_id,
    ] {
        if shared.get(id).is_ok() {
            continue;
        }
        let summary = chain
            .get_object(&id.to_string())
            .await
            .map_err(|e| match e {
                ChainError::NotFound(_) => BuildOutcome::refuse(
                    "shared_object_missing",
                    format!("{id} does not exist on this network"),
                ),
                other => BuildOutcome::refuse("chain_unavailable", other.to_string()),
            })?;
        let version = summary.shared_initial_version.ok_or_else(|| {
            BuildOutcome::refuse(
                "not_a_shared_object",
                format!(
                    "{id} is not a shared object, so it cannot be referenced as one; check the \
                     address before anything is signed"
                ),
            )
        })?;
        shared.insert(id, version);
    }
    Ok(shared)
}

/// Assemble, serialize, simulate, and — only if the simulation succeeded and was conclusive —
/// return an envelope.
pub async fn build(request: &BuildRequest, chain: &impl SuiRead, now_ms: u64) -> BuildOutcome {
    let shared = match resolve_shared(request, chain).await {
        Ok(s) => s,
        Err(refusal) => return refusal,
    };

    let binding = WalletBinding {
        package_id: request.wallet_package_id,
        wallet_id: request.wallet_id,
        cap: request.agent_cap.clone(),
        version_id: request.version_id,
        coin_type: request.manifest.wallet_coin_type.clone(),
        manifest: request.manifest.clone(),
    };

    let mut tx = TransactionBuilder::new();
    tx.set_sender(request.sender);
    tx.set_gas_budget(request.gas_budget);
    tx.set_gas_price(request.gas_price);
    tx.add_gas_objects(request.gas_objects.clone());

    // Funding first: request_spend, one prove per attached rule, confirm_spend. The released coin
    // must be fully consumed, and the order below consumes it.
    let coin =
        match build_manifest_gated_spend(&mut tx, &binding, request.spend_base_units, &shared) {
            Ok(coin) => coin,
            Err(e) => return BuildOutcome::refuse("spend_rejected", e.to_string()),
        };

    let order = LimitOrder {
        pool: request.pool.clone(),
        balance_manager_id: request.balance_manager_id,
        trade_cap: request.trade_cap.clone(),
        client_order_id: request.client_order_id,
        price: request.price.clone(),
        quantity: request.quantity.clone(),
        is_bid: request.is_bid,
        pay_with_deep: request.pay_with_deep,
    };
    if let Err(e) = place_limit_order(&mut tx, request.deepbook_package_id, &order, coin, &shared) {
        return BuildOutcome::refuse("order_rejected", e.to_string());
    }

    let built = match tx.try_build() {
        Ok(t) => t,
        Err(e) => return BuildOutcome::refuse("compile_failed", e.to_string()),
    };
    let bytes = match bcs::to_bytes(&built) {
        Ok(b) => b,
        Err(e) => return BuildOutcome::refuse("serialize_failed", e.to_string()),
    };
    let unsigned_ptb = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(bytes)
    };

    // The strict gate. A transport failure is reported as its own refusal rather than folded into
    // "the transaction would fail" — the two mean opposite things, and a build that quietly
    // downgrades an unreachable node into a verdict is how an unchecked transaction gets built.
    let simulation = match chain.simulate(&unsigned_ptb).await {
        Ok(outcome) => outcome,
        Err(ChainError::Transport(m)) => {
            return BuildOutcome::refuse(
                "simulation_unavailable",
                format!("could not reach a fullnode to simulate: {m}"),
            )
        }
        Err(e) => return BuildOutcome::refuse("simulation_failed", e.to_string()),
    };

    // Inconclusive is checked first, and the order matters. A Cetus package-version abort is both
    // `!ok` and unverified, and reporting it as an ordinary failure would tell an operator the
    // transaction is broken when what actually happened is that we could not find out. "We do not
    // know" and "it would fail" call for different responses.
    if simulation.verification != rill_chain::Verification::Verified {
        return BuildOutcome::refuse(
            "simulation_unverified",
            simulation.error.map_or_else(
                || "the simulation was inconclusive, so nothing here can be signed".to_string(),
                |e| format!("the simulation was inconclusive, so nothing here can be signed: {e}"),
            ),
        );
    }
    if !simulation.ok {
        return BuildOutcome::refuse(
            "simulation_failed",
            simulation
                .error
                .unwrap_or_else(|| "the transaction would not succeed".into()),
        );
    }

    let mut allowed_targets = match expected_spend_targets(&binding) {
        Ok(t) => t,
        Err(e) => return BuildOutcome::refuse("spend_rejected", e.to_string()),
    };
    allowed_targets.extend(expected_order_targets(request.deepbook_package_id));

    let envelope = ExecutionEnvelope {
        version: EXECUTION_ENVELOPE_VERSION.to_string(),
        action_id: request.action_id.clone(),
        // A hash, not a signature. It detects drift between here and the signer; it authorises
        // nothing.
        action_digest: digest_unsigned_ptb(&unsigned_ptb),
        network: request.network,
        sender: request.sender.to_string(),
        wallet_package_id: request.wallet_package_id.to_string(),
        wallet_id: request.wallet_id.to_string(),
        agent_cap_id: request.agent_cap_id.clone(),
        balance_manager_id: Some(request.balance_manager_id.to_string()),
        trade_cap_id: Some(request.trade_cap_id.clone()),
        resolved_params: Some(DeepBookResolvedParams {
            pool_key: request.pool.base_coin_type.clone(),
            pool_id: request.pool.pool_id.to_string(),
            client_order_id: request.client_order_id.to_string(),
            spend_amount_mist: request.spend_base_units.to_string(),
            price: match Amount::parse(&request.price) {
                Ok(a) => a,
                Err(e) => return BuildOutcome::refuse("bad_price", e.to_string()),
            },
            quantity: match Amount::parse(&request.quantity) {
                Ok(a) => a,
                Err(e) => return BuildOutcome::refuse("bad_quantity", e.to_string()),
            },
            deposit_sui: match Amount::parse(&request.price) {
                Ok(a) => a,
                Err(e) => return BuildOutcome::refuse("bad_deposit", e.to_string()),
            },
            is_bid: request.is_bid,
            pay_with_deep: request.pay_with_deep,
        }),
        steps: Vec::new(),
        allowed_targets,
        required_object_ids: vec![
            request.wallet_id.to_string(),
            request.balance_manager_id.to_string(),
            request.pool.pool_id.to_string(),
        ],
        required_guards: Vec::new(),
        unsigned_ptb,
        preview: format!(
            "Place a {} limit order for {} at {} on {}",
            if request.is_bid { "buy" } else { "sell" },
            request.quantity,
            request.price,
            request.pool.base_coin_type,
        ),
        simulation: StrictSimulationResult {
            ok: true,
            verification: Verification::Verified,
            error: None,
            gas_estimate: simulation.gas_used_mist.to_string(),
            balance_changes: Vec::new(),
            object_changes: Vec::new(),
        },
        expires_at: format_rfc3339_ms(now_ms + ENVELOPE_TTL_MS),
    };

    BuildOutcome::Built(Box::new(envelope))
}

/// The inverse of `rill-policy`'s parser, kept in step with it deliberately: the signer refuses an
/// expiry it cannot read, so the two formats must agree exactly.
pub fn format_rfc3339_ms(ms: u64) -> String {
    let days = (ms / 86_400_000) as i64;
    let rem = ms % 86_400_000;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3_600_000,
        (rem / 60_000) % 60,
        (rem / 1_000) % 60,
        rem % 1_000
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// A gas object reference, as the caller supplies it.
pub fn gas_object(id: Address, version: u64, digest: Digest) -> ObjectInput {
    ObjectInput::owned(id, version, digest)
}
