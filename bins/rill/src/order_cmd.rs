//! `rill order` — the hero path, in one transaction.
//!
//! Release funds from an agent wallet under its on-chain rules, deposit them into a DeepBook
//! BalanceManager with a delegated capability, prove the right to trade, and place a limit order.
//! Every step is signed by the agent, and no step needs the owner's key.

use rill_chain::{grpc::GrpcSui, SuiRead, SuiWrite};
use rill_ptb::book::parse_u64_return;
use rill_ptb::book_params::BookParams;
use rill_ptb::deepbook::{place_limit_order, LimitOrder, FLOAT_SCALAR};
use rill_ptb::policy_read::{attached_modules, parse_type_names, policy_rules_transaction};
use rill_ptb::registry::{pool_spec, DeepBookNetwork};
use rill_ptb::shared::SharedObjects;
use rill_ptb::spend::{build_gated_spend_for_modules, WalletBinding};
use sui_sdk_types::{Address, Digest};
use sui_transaction_builder::{ObjectInput, TransactionBuilder};

use crate::keystore::Keystore;

const SUI_COIN_TYPE: &str =
    "0x0000000000000000000000000000000000000000000000000000000000000002::coin::Coin<0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI>";

pub struct OrderArgs {
    pub package_id: String,
    pub version_id: String,
    pub wallet_id: String,
    pub cap_id: String,
    pub deepbook_package: String,
    pub pool_key: String,
    pub network: DeepBookNetwork,
    pub balance_manager_id: String,
    pub trade_cap_id: String,
    pub deposit_cap_id: String,
    /// Decimal SUI released from the wallet and deposited.
    pub spend: String,
    /// Decimal price, as text.
    pub price: String,
    /// Decimal quantity, as text.
    pub quantity: String,
    pub is_bid: bool,
    pub gas_budget: u64,
    pub dry_run: bool,
}

async fn owned_input(chain: &GrpcSui, id: &str, label: &str) -> Result<ObjectInput, String> {
    let o = chain
        .get_object(id)
        .await
        .map_err(|e| format!("reading the {label}: {e}"))?;
    Ok(ObjectInput::owned(
        o.reference
            .id
            .parse()
            .map_err(|_| format!("{id} is not an address"))?,
        o.reference.version,
        o.reference
            .digest
            .parse::<Digest>()
            .map_err(|_| format!("the {label} digest did not parse"))?,
    ))
}

/// Ask the pool for its tick, lot and minimum.
async fn read_book_params(
    chain: &GrpcSui,
    deepbook_package: &str,
    pool: &rill_ptb::deepbook::PoolSpec,
    shared: &SharedObjects,
) -> Result<BookParams, String> {
    use sui_sdk_types::Identifier;
    use sui_transaction_builder::Function;

    let mut tx = TransactionBuilder::new();
    tx.set_sender(Address::ZERO);
    tx.set_gas_budget(10_000_000);
    tx.set_gas_price(1_000);
    let pool_arg = tx.object(
        shared
            .input(pool.pool_id, false)
            .map_err(|e| e.to_string())?,
    );
    tx.move_call(
        Function::new(
            deepbook_package
                .parse()
                .map_err(|_| "bad deepbook package id")?,
            Identifier::new("pool").map_err(|_| "pool")?,
            Identifier::new("pool_book_params").map_err(|_| "pool_book_params")?,
        )
        .with_type_args(vec![
            pool.base_coin_type
                .parse()
                .map_err(|_| "bad base coin type")?,
            pool.quote_coin_type
                .parse()
                .map_err(|_| "bad quote coin type")?,
        ]),
        vec![pool_arg],
    );
    tx.add_gas_objects([ObjectInput::owned(
        "0x1".parse().expect("valid"),
        1,
        Digest::ZERO,
    )]);
    let mut built = tx.try_build().map_err(|e| e.to_string())?;
    built.gas_payment.objects.clear();

    let b64 = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .encode(bcs::to_bytes(&built).map_err(|e| e.to_string())?)
    };
    let outcome = chain
        .simulate_read(&b64)
        .await
        .map_err(|e| format!("reading the pool's parameters: {e}"))?;
    let values: Vec<u64> = outcome
        .command_returns
        .iter()
        .flatten()
        .filter_map(|b| parse_u64_return(b).ok())
        .collect();
    if values.len() != 3 {
        return Err(format!(
            "pool_book_params returned {} values, expected 3 (tick, lot, min)",
            values.len()
        ));
    }
    Ok(BookParams {
        tick_size: values[0],
        lot_size: values[1],
        min_size: values[2],
    })
}

pub async fn order(endpoint: &str, keystore: &Keystore, args: &OrderArgs) -> Result<(), String> {
    let chain = GrpcSui::new(endpoint).map_err(|e| e.to_string())?;
    let sender = keystore.address();

    let wallet_id: Address = args.wallet_id.parse().map_err(|_| "bad wallet id")?;
    let version_id: Address = args.version_id.parse().map_err(|_| "bad version id")?;
    let manager_id: Address = args
        .balance_manager_id
        .parse()
        .map_err(|_| "bad balance manager id")?;

    // Three shared objects, each entered by the version it was first shared at.
    let mut shared = SharedObjects::new();
    for (label, id, raw) in [
        ("wallet", wallet_id, args.wallet_id.as_str()),
        ("version", version_id, args.version_id.as_str()),
        (
            "balance manager",
            manager_id,
            args.balance_manager_id.as_str(),
        ),
    ] {
        let summary = chain
            .get_object(raw)
            .await
            .map_err(|e| format!("reading the {label}: {e}"))?;
        shared.insert(
            id,
            summary
                .shared_initial_version
                .ok_or_else(|| format!("the {label} {raw} is not shared"))?,
        );
    }
    let pool = pool_spec(args.network, &args.pool_key)
        .ok_or_else(|| format!("{} is not a listed pool", args.pool_key))?;
    let pool_summary = chain
        .get_object(&pool.pool_id.to_string())
        .await
        .map_err(|e| format!("reading the pool: {e}"))?;
    shared.insert(
        pool.pool_id,
        pool_summary
            .shared_initial_version
            .ok_or("the pool is not shared")?,
    );

    // The prove list, read from the wallet rather than assumed.
    let read_tx = policy_rules_transaction(
        args.package_id.parse().map_err(|_| "bad package id")?,
        wallet_id,
        "0x2::sui::SUI",
        &shared,
    )
    .map_err(|e| e.to_string())?;
    let read_b64 = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .encode(bcs::to_bytes(&read_tx).map_err(|e| e.to_string())?)
    };
    let read = chain
        .simulate_read(&read_b64)
        .await
        .map_err(|e| format!("reading the wallet's rules: {e}"))?;
    let names = read
        .command_returns
        .iter()
        .flatten()
        .next()
        .ok_or("the wallet did not report its rules")
        .and_then(|b| parse_type_names(b).map_err(|_| "the rule list did not decode"))?;
    let modules: Vec<String> = attached_modules(&names)
        .into_iter()
        .map(str::to_owned)
        .collect();

    let gas: Vec<_> = chain
        .list_owned_objects(&sender.to_string())
        .await
        .map_err(|e| format!("listing the sender's objects: {e}"))?
        .into_iter()
        .filter(|o| o.object_type.as_deref() == Some(SUI_COIN_TYPE))
        .collect();
    if gas.is_empty() {
        return Err(format!("{sender} holds no SUI to pay for this"));
    }

    let mut tx = TransactionBuilder::new();
    tx.set_sender(sender);
    tx.set_gas_budget(args.gas_budget);
    tx.set_gas_price(
        chain
            .reference_gas_price()
            .await
            .map_err(|e| format!("reading the reference gas price: {e}"))?,
    );
    tx.add_gas_objects(gas.iter().map(|c| {
        ObjectInput::owned(
            c.reference.id.parse().expect("an id from the chain"),
            c.reference.version,
            c.reference.digest.parse::<Digest>().expect("a digest"),
        )
    }));

    let binding = WalletBinding {
        package_id: args.package_id.parse().map_err(|_| "bad package id")?,
        wallet_id,
        cap: owned_input(&chain, &args.cap_id, "AgentCap").await?,
        version_id,
        coin_type: "0x2::sui::SUI".into(),
        manifest: rill_core::manifest::CapabilityManifest {
            wallet_coin_type: "0x2::sui::SUI".into(),
            rules: Vec::new(),
        },
    };

    let spend_mist = rill_core::amounts::decimal_to_base_units(&args.spend, 9)
        .map_err(|e| format!("the spend amount: {e}"))?;
    let module_refs: Vec<&str> = modules.iter().map(String::as_str).collect();
    let coin = build_gated_spend_for_modules(&mut tx, &binding, spend_mist, &module_refs, &shared)
        .map_err(|e| e.to_string())?;

    // What the pool will accept, read from the pool. Checked before anything is built: a miss
    // aborts in order_info::validate_inputs with a bare code that names neither the number that was
    // wrong nor the one it should have been.
    let params = read_book_params(&chain, &args.deepbook_package, &pool, &shared).await?;
    let price_scaled = rill_core::amounts::deepbook_price_to_base_units(
        &args.price,
        FLOAT_SCALAR,
        pool.quote_scalar,
        pool.base_scalar,
    )
    .map_err(|e| format!("the price: {e}"))?;
    let quantity_base =
        rill_core::amounts::deepbook_quantity_to_base_units(&args.quantity, pool.base_scalar)
            .map_err(|e| format!("the quantity: {e}"))?;
    println!(
        "pool takes: min {} · lot {} · tick {}",
        params.min_size, params.lot_size, params.tick_size
    );
    params
        .check(price_scaled, quantity_base)
        .map_err(|e| e.to_string())?;

    let order = LimitOrder {
        pool: pool.clone(),
        balance_manager_id: manager_id,
        trade_cap: owned_input(&chain, &args.trade_cap_id, "TradeCap").await?,
        deposit_cap: owned_input(&chain, &args.deposit_cap_id, "DepositCap").await?,
        client_order_id: 1,
        price: args.price.clone(),
        quantity: args.quantity.clone(),
        is_bid: args.is_bid,
        pay_with_deep: false,
    };

    place_limit_order(
        &mut tx,
        args.deepbook_package
            .parse()
            .map_err(|_| "bad deepbook package id")?,
        &order,
        coin,
        &shared,
    )
    .map_err(|e| e.to_string())?;

    println!("wallet  : {wallet_id}");
    println!("manager : {manager_id}");
    println!("pool    : {} ({})", args.pool_key, pool.pool_id);
    println!("rules   : {} (read from chain)", modules.join(", "));
    println!(
        "order   : {} {} @ {}  funded with {} SUI",
        if args.is_bid { "BUY" } else { "SELL" },
        args.quantity,
        args.price,
        args.spend
    );

    let built = tx.try_build().map_err(|e| format!("compiling: {e}"))?;
    let b64 = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .encode(bcs::to_bytes(&built).map_err(|e| e.to_string())?)
    };

    let outcome = chain
        .simulate(&b64)
        .await
        .map_err(|e| format!("the node did not answer, so there is no verdict: {e}"))?;
    println!(
        "\nsimulation: ok={} verification={:?} gas={}",
        outcome.ok, outcome.verification, outcome.gas_used_mist
    );
    if !outcome.ok {
        let error = outcome.error.unwrap_or_else(|| "no reason given".into());
        return Err(match rill_chain::aborts::classify_rule_abort(&error) {
            Some(refusal) => format!("{refusal}.\n\nThe limit is on chain. Spend less."),
            None => format!("the chain refused it: {error}"),
        });
    }
    if args.dry_run {
        println!("\ndry run — nothing signed. Re-run with --submit.");
        return Ok(());
    }

    let signature = keystore.sign(&built).map_err(|e| e.to_string())?;
    let outcome = chain
        .execute(&b64, &[signature.to_base64()])
        .await
        .map_err(|e| format!("submitting: {e}"))?;
    if let Some(error) = &outcome.error {
        return Err(format!("the transaction failed on chain: {error}"));
    }
    println!("\ndigest: {}", outcome.digest);
    println!("gas   : {}", outcome.gas_used_mist);
    for d in &outcome.balance_changes {
        println!("balance: {} {}", d.amount, d.coin_type);
    }
    Ok(())
}
