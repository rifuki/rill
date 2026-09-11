//! What a swap would actually return, and the `minOut` to ask for.
//!
//! # Why a signer answers this at all
//!
//! `rill_swap` requires `minOut`, and requiring it was right: without a floor the wallet's rules cap
//! what goes into a swap and nothing caps what comes back. But a required field a caller cannot
//! compute is a field it will fill with a guess or escape with `acceptAnyOutput`, and both defeat the
//! floor. An agent has no price source of its own.
//!
//! # The quote is a simulation, not arithmetic
//!
//! The first version of this computed the output from the pool's `current_sqrt_price` with a
//! constant-price formula. Measured against real testnet fills it was 11% high, and 36% high once the
//! pool had moved, and a floor derived from it was refused by the floor's own guard. A
//! concentrated-liquidity pool prices each tick separately and the price moves during the swap, so
//! predicting a fill means reimplementing Cetus.
//!
//! So it does not predict. It builds the real gated swap, asks the node to run it, and reports the
//! balance change the node reports. That figure matched a real fill exactly, to the base unit, on the
//! same transaction. Nothing is signed and nothing is submitted.
//!
//! A consequence worth having: because the simulation includes the gated spend, a quote also answers
//! whether the wallet's rules permit the swap at all. A quote that comes back refused by `per_tx` has
//! told the agent something no price could.

use rill_chain::{SuiRead, SuiWrite};
use rill_ptb::cetus::{pool_coin_types, pool_state};
use serde_json::{json, Value};

use crate::keystore::Keystore;
use crate::swap_cmd::{swap_json_on, SwapArgs};

/// The SUI type, which is what an agent wallet releases.
const SUI: &str = "0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI";

pub struct QuoteArgs {
    pub package_id: String,
    pub version_id: String,
    pub wallet_id: String,
    pub cap_id: String,
    pub integrate_package_id: String,
    pub global_config_id: String,
    pub pool_id: String,
    /// Decimal SUI to be released from the wallet and swapped.
    pub spend: String,
    /// How far below the simulated output the floor should sit, in basis points. 100 is one percent.
    pub slippage_bps: u64,
    pub gas_budget: u64,
}

pub async fn quote_json_on(
    chain: &(impl SuiRead + SuiWrite),
    keystore: &Keystore,
    args: &QuoteArgs,
) -> Result<Value, String> {
    // The pool first, for the two coin types and the direction. Reading them from the pool is what
    // lets a caller pass a pool id and nothing else: a transposed pair aborts inside Cetus with a
    // type mismatch that names neither coin, and that class of mistake is now unreachable rather
    // than documented.
    let pool = chain
        .get_object(&args.pool_id)
        .await
        .map_err(|e| format!("reading the pool: {e}"))?;
    let object_type = pool
        .object_type
        .as_deref()
        .ok_or("the node returned the pool without its type")?;
    let (coin_a, coin_b) = pool_coin_types(object_type)
        .ok_or_else(|| format!("{} is not a Cetus pool with two coin types", args.pool_id))?;

    // `a2b` means "spend A, buy B", so a wallet releasing SUI spends whichever side SUI is on.
    let a2b = if coin_a == SUI {
        true
    } else if coin_b == SUI {
        false
    } else {
        return Err(format!(
            "neither side of this pool is SUI ({coin_a} against {coin_b}), and an agent wallet \
             releases SUI, so it cannot fund a swap here"
        ));
    };
    let bought = if a2b { &coin_b } else { &coin_a };

    let state = match pool.fields.as_ref() {
        Some(fields) => pool_state(fields)?,
        None => return Err("the node returned the pool without its fields".to_string()),
    };

    // The real swap, simulated. `min_out` is zero with the acknowledgement set, because a floor here
    // would be a floor on the measurement rather than on a trade: nothing is submitted, and the
    // figure being measured is the one a floor would be derived from.
    let probe = SwapArgs {
        package_id: args.package_id.clone(),
        version_id: args.version_id.clone(),
        wallet_id: args.wallet_id.clone(),
        cap_id: args.cap_id.clone(),
        integrate_package_id: args.integrate_package_id.clone(),
        global_config_id: args.global_config_id.clone(),
        pool_id: args.pool_id.clone(),
        coin_type_a: coin_a.clone(),
        coin_type_b: coin_b.clone(),
        a2b,
        spend: args.spend.clone(),
        min_out_base_units: "0".into(),
        guard_package_id: String::new(),
        accept_any_output: true,
        gas_budget: args.gas_budget,
        dry_run: true,
    };
    let simulated = swap_json_on(chain, keystore, &probe)
        .await
        .map_err(|e| e.to_string())?;

    // The bought coin's delta. Positive by definition: it is the side the swap produced.
    let out = simulated["simulation"]["balanceChanges"]
        .as_array()
        .and_then(|changes| {
            changes.iter().find_map(|c| {
                let coin = c["coinType"].as_str()?;
                let amount: i128 = c["amount"].as_str()?.parse().ok()?;
                (coin == bought && amount > 0).then_some(amount as u128)
            })
        })
        .ok_or_else(|| {
            format!(
                "the simulation reported no gain of {bought}, so there is nothing to quote. The \
                 swap may be routed the wrong way for this pool, or the pool may be unable to fill \
                 it."
            )
        })?;

    let bps = u128::from(args.slippage_bps.min(10_000));
    // Rounded down, so the floor is never above what the simulation implies.
    let floor = out * (10_000 - bps) / 10_000;

    Ok(json!({
        "pool": args.pool_id,
        "coinTypeA": coin_a,
        "coinTypeB": coin_b,
        "a2b": a2b,
        "boughtCoinType": bought,
        "spendBaseUnits": simulated["spendBaseUnits"].clone(),
        "feeRateMillionths": state.fee_rate.to_string(),
        "liquidity": state.liquidity.to_string(),
        "poolPaused": state.is_paused,
        "expectedOut": out.to_string(),
        "slippageBps": args.slippage_bps.to_string(),
        "minOut": floor.to_string(),
        "gasEstimate": simulated["simulation"]["gasEstimate"].clone(),
        "rulesProved": simulated["rulesProved"].clone(),
        // Everything rill_swap needs except the wallet and cap, which the caller already has.
        "swapArguments": {
            "pool": args.pool_id,
            "coinTypeA": coin_a,
            "coinTypeB": coin_b,
            "a2b": a2b,
            "amount": args.spend,
            "minOut": floor.to_string(),
        },
        "note": "`expectedOut` is what the node says this exact swap produces, from running Cetus's \
                 own code, not an estimate computed here. `minOut` is that figure less slippageBps, \
                 and `swapArguments` is the rest of the rill_swap call ready to pass through. The \
                 pool can still move between this and the swap, which is what the floor is for, so \
                 quote immediately before swapping rather than reusing an old quote. A refusal with \
                 E_SLIPPAGE means it moved further than slippageBps allowed: quote again and widen \
                 it, rather than removing the floor. Because this simulates the gated spend too, a \
                 quote refused by a rule has told you the wallet would not permit the swap at all."
    }))
}

/// The same, against a real node.
pub async fn quote_json(
    endpoint: &str,
    keystore: &Keystore,
    args: &QuoteArgs,
) -> Result<Value, String> {
    let chain = rill_chain::grpc::GrpcSui::new(endpoint).map_err(|e| e.to_string())?;
    quote_json_on(&chain, keystore, args).await
}
