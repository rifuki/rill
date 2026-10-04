//! The whole owner-to-agent flow, against a live network, in one run that cleans up after itself.
//!
//! Run it through `scripts/e2e.sh`, which builds both binaries, starts a throwaway server and stops
//! it again. What runs here is what a person does in Studio and then asks their agent to do, with
//! the owner's browser wallet replaced by a key from the local Sui keystore, so no step waits on a
//! popup:
//!
//! 1. the owner signs in and publishes a guarded Cetus swap (floor and asset scope included);
//! 2. the server prepares an empty wallet, then the rules and funding, and the owner signs both;
//! 3. the owner signs a grant, and a tampered copy of it is refused;
//! 4. the agent's signer, started exactly as the plugin starts it, lists the grant and runs it once;
//! 5. a second run exceeds what the budget has left and is refused before anything is signed;
//! 6. the owner revokes, the remaining budget comes back, and the agent is refused after that.
//!
//! Whatever fails after step 2, the wallet is revoked on the way out, so a failed run leaves no
//! funds behind. Mainnet costs real SUI (one 0.005 SUI swap plus gas) and needs
//! `RILL_E2E_ALLOW_MAINNET=1` on top of the signer's own mainnet consent.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use base64::{engine::general_purpose::STANDARD, Engine};
use rill_chain::gas::{affordable_budget, sui_gas_coins};
use rill_chain::grpc::GrpcSui;
use rill_chain::{SuiRead, SuiWrite};
use rill_cli::grant_cmd::{self, GrantArgs};
use rill_cli::http;
use rill_cli::keystore::Keystore;
use rill_cli::revoke_cmd::{self, RevokeArgs};
use rill_core::envelope::Network;
use serde_json::{json, Value};
use sui_sdk_types::{
    Address, Digest, GasPayment, ObjectReference, Transaction, TransactionExpiration,
    TransactionKind,
};

/// Every scenario funds one run and a remainder too small for a second, so the second run is the
/// wallet's own refusal rather than a limit the test chose to trip.
const SUI: u64 = 1_000_000_000;

struct Setup {
    network: Network,
    network_name: &'static str,
    api: String,
    rpc: String,
    owner: Keystore,
    agent: Address,
    wallet_bin: String,
    work_dir: std::path::PathBuf,
}

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn setup() -> Setup {
    let (network, network_name, default_rpc) = match var("RILL_E2E_NETWORK").as_deref() {
        Some("mainnet") => {
            assert_eq!(
                var("RILL_E2E_ALLOW_MAINNET").as_deref(),
                Some("1"),
                "mainnet spends real SUI: set RILL_E2E_ALLOW_MAINNET=1 to mean it"
            );
            (
                Network::Mainnet,
                "mainnet",
                "https://fullnode.mainnet.sui.io:443",
            )
        }
        _ => (
            Network::Testnet,
            "testnet",
            "https://fullnode.testnet.sui.io:443",
        ),
    };
    let owner: Address = var("RILL_E2E_OWNER")
        .expect("RILL_E2E_OWNER: an address in the local Sui keystore")
        .parse()
        .expect("RILL_E2E_OWNER is an address");
    let agent: Address = var("RILL_E2E_AGENT")
        .expect("RILL_E2E_AGENT: the agent's address, also in the local Sui keystore")
        .parse()
        .expect("RILL_E2E_AGENT is an address");
    assert_ne!(
        owner, agent,
        "an agent that owns its wallet can lift its own limits"
    );
    Setup {
        network,
        network_name,
        api: var("RILL_E2E_API").expect("RILL_E2E_API: the server scripts/e2e.sh started"),
        rpc: var("RILL_E2E_RPC").unwrap_or_else(|| default_rpc.into()),
        owner: Keystore::load_for(owner).expect("the owner's key is in the local keystore"),
        agent,
        wallet_bin: var("RILL_E2E_WALLET_BIN").expect("RILL_E2E_WALLET_BIN: the signer binary"),
        work_dir: var("RILL_E2E_DIR")
            .map(Into::into)
            .unwrap_or_else(std::env::temp_dir),
    }
}

fn step(name: &str) {
    eprintln!("\n== {name}");
}

fn data(value: Value) -> Value {
    match value.get("data") {
        Some(inner) => inner.clone(),
        None => value,
    }
}

/// Complete a transaction kind the server prepared, as a browser wallet would: the owner's gas,
/// a strict simulation first, then the owner's signature and submission.
async fn sign_and_run(
    chain: &GrpcSui,
    owner: &Keystore,
    kind_b64: &str,
) -> Result<rill_chain::ExecutionOutcome, String> {
    // The owner's gas is read from a load-balanced fullnode that can trail the owner's previous
    // transaction: it skips a coin it has not seen yet, so the gas covers less than the funding
    // split from it ("InsufficientCoinBalance"), or names a version already spent ("unavailable
    // for consumption"). Both clear once the node catches up, so they are retried with gas read
    // afresh. A wallet's browser extension picks its own gas and never meets this.
    let mut last = String::new();
    for attempt in 0..8 {
        match sign_and_run_once(chain, owner, kind_b64).await {
            Ok(outcome) => return Ok(outcome),
            Err(e)
                if e.contains("InsufficientCoinBalance")
                    || e.contains("unavailable for consumption")
                    || e.contains("needs to be rebuilt") =>
            {
                eprintln!(
                    "owner gas not caught up (attempt {}), retrying",
                    attempt + 1
                );
                last = e;
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

async fn sign_and_run_once(
    chain: &GrpcSui,
    owner: &Keystore,
    kind_b64: &str,
) -> Result<rill_chain::ExecutionOutcome, String> {
    let kind: TransactionKind = bcs::from_bytes(
        &STANDARD
            .decode(kind_b64)
            .map_err(|e| format!("kind is not base64: {e}"))?,
    )
    .map_err(|e| format!("kind is not a TransactionKind: {e}"))?;
    let sender = owner.address();
    let coins = sui_gas_coins(chain, &sender.to_string())
        .await
        .map_err(|e| format!("reading gas: {e}"))?;
    let objects = coins
        .iter()
        .map(|c| {
            Ok(ObjectReference::new(
                c.reference.id.parse().map_err(|_| "gas id")?,
                c.reference.version,
                c.reference
                    .digest
                    .parse::<Digest>()
                    .map_err(|_| "gas digest")?,
            ))
        })
        .collect::<Result<Vec<_>, &str>>()?;
    let transaction = Transaction {
        kind,
        sender,
        gas_payment: GasPayment {
            objects,
            owner: sender,
            price: chain
                .reference_gas_price()
                .await
                .map_err(|e| e.to_string())?,
            budget: affordable_budget(50_000_000, &coins),
        },
        expiration: TransactionExpiration::None,
    };
    let bytes = STANDARD.encode(bcs::to_bytes(&transaction).map_err(|e| e.to_string())?);
    let simulated = chain.simulate(&bytes).await.map_err(|e| e.to_string())?;
    if !simulated.ok {
        return Err(format!("simulation refused: {:?}", simulated.error));
    }
    let signature = owner.sign(&transaction).map_err(|e| e.to_string())?;
    let outcome = chain
        .execute(&bytes, &[signature.to_base64()])
        .await
        .map_err(|e| e.to_string())?;
    if !outcome.success {
        return Err(format!(
            "{} failed on chain: {:?}",
            outcome.digest, outcome.error
        ));
    }
    Ok(outcome)
}

/// The wallet's fields once `ready` holds, waiting out a fullnode that has not caught up yet.
///
/// Right after a transaction, a load-balanced fullnode can answer with the previous version of the
/// wallet, or with nothing at all for one it has not seen created. Reading once turned that lag
/// into a failed run, and a panic in the cleanup path that should have revoked the wallet.
async fn wallet_until(
    chain: &GrpcSui,
    wallet: &str,
    what: &str,
    ready: impl Fn(&Value) -> bool,
) -> Result<Value, String> {
    let mut last = String::from("never read");
    for _ in 0..40 {
        match chain.get_object(wallet).await {
            Ok(object) => match object.fields {
                Some(fields) if ready(&fields) => return Ok(fields),
                Some(fields) => last = fields.to_string(),
                None => last = "no fields".into(),
            },
            Err(e) => last = e.to_string(),
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    Err(format!(
        "wallet {wallet} never showed {what}: last read {last}"
    ))
}

async fn wallet_fields(chain: &GrpcSui, wallet: &str) -> Result<Value, String> {
    wallet_until(chain, wallet, "its fields", |_| true).await
}

fn number(fields: &Value, name: &str) -> u64 {
    match &fields[name] {
        Value::String(s) => s.parse().expect("a number"),
        Value::Number(n) => n.as_u64().expect("a u64"),
        other => panic!("{name} is {other}"),
    }
}

/// The agent's signer over stdio, launched the way the plugin launches it: its own config file,
/// no Rill or Sui variables inherited from whoever runs the test.
struct Agent {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    next_id: u64,
}

impl Agent {
    fn start(setup: &Setup) -> Agent {
        let config = setup.work_dir.join("agent-config.json");
        std::fs::write(
            &config,
            serde_json::to_vec_pretty(&json!({
                "network": setup.network_name,
                "signAs": setup.agent.to_string(),
                "allowMainnet": setup.network == Network::Mainnet,
                "apiUrl": setup.api,
                "rpcUrl": setup.rpc,
            }))
            .unwrap(),
        )
        .unwrap();
        let mut command = Command::new(&setup.wallet_bin);
        command.arg("mcp");
        for (key, _) in std::env::vars() {
            if key.starts_with("RILL_") || key.starts_with("SUI_") || key.starts_with("AGENT_") {
                command.env_remove(key);
            }
        }
        let mut child = command
            .env("RILL_CONFIG", &config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("the signer starts");
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        let mut agent = Agent {
            child,
            input,
            output,
            next_id: 1,
        };
        agent.request(
            "initialize",
            json!({"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"1"}}),
        );
        agent
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let message = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        writeln!(self.input, "{message}").unwrap();
        self.input.flush().unwrap();
        let mut line = String::new();
        self.output
            .read_line(&mut line)
            .expect("the signer answers");
        let reply: Value = serde_json::from_str(&line).expect("a JSON-RPC reply");
        reply["result"].clone()
    }

    /// A tool's structured answer, and whether it was an error.
    fn call(&mut self, tool: &str, arguments: Value) -> (Value, bool) {
        let result = self.request("tools/call", json!({"name":tool,"arguments":arguments}));
        (
            result["structuredContent"].clone(),
            result["isError"] == Value::Bool(true),
        )
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn grant_entry(actions: &Value, action: &str, wallet: &str) -> Value {
    actions["actions"]
        .as_array()
        .expect("rill_actions lists actions")
        .iter()
        .find(|a| a["actionId"] == action && a["walletId"] == wallet)
        .cloned()
        .unwrap_or_else(|| panic!("no grant for {action} from {wallet} in {actions}"))
}

/// What one scenario publishes and how much it grants. Everything after publishing, from the empty
/// wallet to the refusal after revocation, is the same for every scenario.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    /// A Cetus swap, SUI to USDC, floored at 90% of a simulated quote and scoped to both coins.
    Swap,
    /// A Haedal stake: SUI in, haSUI to the agent. Haedal's minimum is 1 SUI.
    Stake,
    /// A DeepBook ask resting well above the market, from a BalanceManager the owner provisioned
    /// and delegated. The owner cancels it and withdraws the deposit at the end.
    DeepBook,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Swap => "swap",
            Kind::Stake => "stake",
            Kind::DeepBook => "deepbook",
        }
    }
}

struct Plan {
    flow: Value,
    manifest: Value,
    budget: u64,
    per_tx: u64,
    /// What one run takes from the wallet.
    spend: u64,
    /// Extra fields onboarding needs, such as a DeepBook manager and its capabilities.
    attach: Value,
}

/// A BalanceManager this run provisioned, which the owner empties again before the run ends.
#[derive(Clone)]
struct Manager {
    id: String,
    trade_cap: String,
    deposit_cap: String,
}

#[tokio::test]
#[ignore = "spends on a live network; run through scripts/e2e.sh"]
async fn swap_an_owner_grants_and_the_agent_runs_within_its_limits() {
    scenario(Kind::Swap).await;
}

#[tokio::test]
#[ignore = "spends on a live network; run through scripts/e2e.sh"]
async fn stake_an_owner_grants_and_the_agent_runs_within_its_limits() {
    scenario(Kind::Stake).await;
}

#[tokio::test]
#[ignore = "spends on a live network; run through scripts/e2e.sh"]
async fn deepbook_an_owner_grants_and_the_agent_runs_within_its_limits() {
    scenario(Kind::DeepBook).await;
}

async fn scenario(kind: Kind) {
    let setup = setup();
    let chain = GrpcSui::new(&setup.rpc).expect("a fullnode client");
    let mut receipts = json!({ "network": setup.network_name, "scenario": kind.name() });
    let mut wallet: Option<String> = None;
    let mut manager: Option<Manager> = None;

    let result = run(
        kind,
        &setup,
        &chain,
        &mut receipts,
        &mut wallet,
        &mut manager,
    )
    .await;

    // Whatever happened, nothing this run funded is left behind: the wallet is revoked and a
    // DeepBook manager is cancelled and withdrawn to the owner. The one exception is a kept wallet
    // (RILL_E2E_KEEP), handed on live to whoever asked for it, who revokes it.
    let kept = result.is_ok() && keep();
    if let (false, Some(id)) = (kept, &wallet) {
        let revoked = matches!(
            wallet_fields(&chain, id).await,
            Ok(fields) if fields["revoked"] == Value::Bool(true)
        );
        if !revoked {
            eprintln!("\n== cleanup: revoking {id}");
            if let Err(e) = revoke(&setup, id).await {
                eprintln!("cleanup revoke failed: {e}");
            }
        }
    }
    if let Some(m) = &manager {
        if receipts["recovered"].is_null() {
            eprintln!("\n== cleanup: emptying manager {}", m.id);
            match recover_manager(&setup, &chain, m).await {
                Ok((digest, delta)) => {
                    receipts["recovered"] =
                        json!({ "digest": digest, "ownerSuiDelta": delta.to_string() })
                }
                Err(e) => eprintln!("cleanup recovery failed: {e}"),
            }
        }
    }
    let report = setup
        .work_dir
        .join(format!("e2e-{}-receipts.json", kind.name()));
    std::fs::write(&report, serde_json::to_vec_pretty(&receipts).unwrap()).unwrap();
    eprintln!("\nreceipts: {}", report.display());
    if let Err(e) = result {
        panic!("{e}");
    }
}

/// RILL_E2E_KEEP: stop once the grant is stored and leave the wallet live, for a run that drives
/// the agent itself (scripts/agent-redteam.py). Its receipts say what to revoke afterwards.
fn keep() -> bool {
    var("RILL_E2E_KEEP").as_deref() == Some("1")
}

/// RILL_E2E_RUNS: how many granted runs the swap wallet affords before the remainder refuses one.
fn swap_runs() -> u64 {
    var("RILL_E2E_RUNS")
        .and_then(|r| r.parse().ok())
        .filter(|r| (1..=10).contains(r))
        .unwrap_or(1)
}

async fn revoke(setup: &Setup, wallet: &str) -> Result<(), String> {
    let (package, _) = rill_ptb::deployments::wallet_deployment(setup.network, None, None)?;
    revoke_cmd::revoke(
        &setup.rpc,
        &setup.owner,
        &RevokeArgs {
            package_id: package.to_string(),
            wallet_id: wallet.into(),
            recipient: None,
            gas_budget: 50_000_000,
            dry_run: false,
        },
    )
    .await
}

fn deepbook_network(network: Network) -> rill_ptb::registry::DeepBookNetwork {
    match network {
        Network::Mainnet => rill_ptb::registry::DeepBookNetwork::Mainnet,
        Network::Testnet => rill_ptb::registry::DeepBookNetwork::Testnet,
    }
}

fn deepbook_pool_key(network: Network) -> &'static str {
    match network {
        Network::Mainnet => "SUI_USDC",
        Network::Testnet => "SUI_DBUSDC",
    }
}

/// A transaction kind from a builder the test fills in, encoded the way the server sends one.
fn kind_of(
    owner: Address,
    build: impl FnOnce(&mut sui_transaction_builder::TransactionBuilder) -> Result<(), String>,
) -> Result<String, String> {
    let mut tx = sui_transaction_builder::TransactionBuilder::new();
    tx.set_sender(owner);
    tx.set_gas_budget(50_000_000);
    tx.set_gas_price(1_000);
    build(&mut tx)?;
    // The placeholder only satisfies the builder; sign_and_run pays with the owner's real coins.
    tx.add_gas_objects([sui_transaction_builder::ObjectInput::owned(
        "0x1".parse().unwrap(),
        1,
        Digest::ZERO,
    )]);
    let built = tx.try_build().map_err(|e| e.to_string())?;
    Ok(STANDARD.encode(bcs::to_bytes(&built.kind).map_err(|e| e.to_string())?))
}

fn shared_input(
    object: &rill_chain::ObjectSummary,
    mutable: bool,
) -> Result<sui_transaction_builder::ObjectInput, String> {
    let version = object
        .shared_initial_version
        .ok_or_else(|| format!("{} is not shared", object.reference.id))?;
    Ok(sui_transaction_builder::ObjectInput::shared(
        object.reference.id.parse().map_err(|_| "object id")?,
        version,
        mutable,
    ))
}

/// The owner provisions a BalanceManager and gives its trade and deposit capabilities to the agent.
async fn provision_manager(setup: &Setup, chain: &GrpcSui) -> Result<Manager, String> {
    let package: Address = deepbook_network(setup.network)
        .package_id()
        .parse()
        .map_err(|_| "deepbook package")?;
    let agent = setup.agent;
    let kind = kind_of(setup.owner.address(), |tx| {
        rill_ptb::balance_manager::build_provision_manager(tx, package, agent)
            .map_err(|e| e.to_string())
    })?;
    let outcome = sign_and_run(chain, &setup.owner, &kind).await?;
    let find = |suffix: &str| {
        outcome
            .created
            .iter()
            .find(|o| {
                o.object_type
                    .as_deref()
                    .is_some_and(|t| t.ends_with(suffix))
            })
            .map(|o| o.object_id.clone())
            .ok_or_else(|| format!("provisioning created no {suffix}"))
    };
    Ok(Manager {
        id: find("::balance_manager::BalanceManager")?,
        trade_cap: find("::balance_manager::TradeCap")?,
        deposit_cap: find("::balance_manager::DepositCap")?,
    })
}

/// The manager's owner cancels every order and withdraws its SUI, returning the deposit. Answers
/// the digest and the owner's net SUI change, read from the transaction's own effects.
async fn recover_manager(
    setup: &Setup,
    chain: &GrpcSui,
    manager: &Manager,
) -> Result<(String, i128), String> {
    let network = deepbook_network(setup.network);
    let package: Address = network
        .package_id()
        .parse()
        .map_err(|_| "deepbook package")?;
    let pool = rill_ptb::registry::pool_spec(network, deepbook_pool_key(setup.network))
        .ok_or("unknown DeepBook pool")?;
    // Freshly provisioned: wait until this node can read it before referencing it.
    let manager_object = {
        let mut found = None;
        for _ in 0..40 {
            if let Ok(o) = chain.get_object(&manager.id).await {
                found = Some(o);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        found.ok_or("the manager never became readable")?
    };
    let pool_object = chain
        .get_object(&pool.pool_id.to_string())
        .await
        .map_err(|e| e.to_string())?;
    let clock = chain.get_object("0x6").await.map_err(|e| e.to_string())?;
    let owner = setup.owner.address();
    let base: sui_sdk_types::TypeTag = pool.base_coin_type.parse().map_err(|_| "base type")?;
    let quote: sui_sdk_types::TypeTag = pool.quote_coin_type.parse().map_err(|_| "quote type")?;
    let id = |s: &str| sui_sdk_types::Identifier::new(s).map_err(|_| s.to_owned());
    let kind = kind_of(owner, |tx| {
        let m = tx.object(shared_input(&manager_object, true)?);
        let p = tx.object(shared_input(&pool_object, true)?);
        let c = tx.object(shared_input(&clock, false)?);
        let proof = tx.move_call(
            sui_transaction_builder::Function::new(
                package,
                id("balance_manager")?,
                id("generate_proof_as_owner")?,
            ),
            vec![m],
        );
        tx.move_call(
            sui_transaction_builder::Function::new(package, id("pool")?, id("cancel_all_orders")?)
                .with_type_args(vec![base.clone(), quote.clone()]),
            vec![p, m, proof, c],
        );
        let mut coins = Vec::new();
        for coin in [base.clone(), quote.clone()] {
            coins.push(
                tx.move_call(
                    sui_transaction_builder::Function::new(
                        package,
                        id("balance_manager")?,
                        id("withdraw_all")?,
                    )
                    .with_type_args(vec![coin]),
                    vec![m],
                ),
            );
        }
        let recipient = tx.pure(&owner);
        tx.transfer_objects(coins, recipient);
        Ok(())
    })?;
    let outcome = sign_and_run(chain, &setup.owner, &kind).await?;
    let sui_delta = outcome
        .balance_changes
        .iter()
        .filter(|d| {
            d.coin_type.ends_with("::sui::SUI") && d.address.parse::<Address>().ok() == Some(owner)
        })
        .filter_map(|d| d.amount.parse::<i128>().ok())
        .sum();
    Ok((outcome.digest, sui_delta))
}

/// The sum of the coins of `suffix` an address owns, waiting until it differs from `not`.
async fn coin_total_changed(chain: &GrpcSui, owner: &str, suffix: &str, not: u128) -> u128 {
    let mut total = not;
    for _ in 0..40 {
        total = coin_total(chain, owner, suffix).await;
        if total != not {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    total
}

async fn coin_total(chain: &GrpcSui, owner: &str, suffix: &str) -> u128 {
    // An owner listing names the coins but carries no balances, so each is read for its own.
    let listed = chain.list_owned_objects(owner).await.unwrap_or_default();
    let mut total = 0u128;
    for coin in listed.iter().filter(|o| {
        o.object_type
            .as_deref()
            .is_some_and(|t| t.contains("::coin::Coin<") && t.ends_with(&format!("{suffix}>")))
    }) {
        if let Ok(read) = chain.get_object(&coin.reference.id).await {
            total += rill_chain::gas::coin_balance(&read)
                .map(u128::from)
                .unwrap_or(0);
        }
    }
    total
}

/// The swap's USDC quote for `amount` SUI, by simulating it.
async fn quote_usdc(api: &str, pool: &str, amount: u64) -> Result<u64, String> {
    let flow = json!({"nodes":[{"id":"swap","type":"cetus_swap","config":{
        "amount_in": amount.to_string(), "pool": pool, "inputCoinType": "0x2::sui::SUI",
        "min_amount_out": "1"
    }}],"edges":[]});
    let simulated =
        data(http::post_json(&format!("{api}/api/simulate"), &json!({"flow": flow})).await?);
    simulated["simulation"]["balanceChanges"]
        .as_array()
        .and_then(|changes| {
            changes.iter().find(|c| {
                c["coinType"]
                    .as_str()
                    .is_some_and(|t| t.ends_with("::usdc::USDC"))
            })
        })
        .and_then(|c| c["amount"].as_str().and_then(|a| a.parse::<i128>().ok()))
        .filter(|a| *a > 0)
        .map(|a| a as u64)
        .ok_or_else(|| format!("the simulation quoted no USDC: {simulated}"))
}

async fn plan(
    kind: Kind,
    setup: &Setup,
    chain: &GrpcSui,
    api: &str,
    receipts: &mut Value,
    manager_slot: &mut Option<Manager>,
) -> Result<Plan, String> {
    let protocols = data(http::get_json(&format!("{api}/api/protocols")).await?);
    let cetus = &protocols["cetus_swap"];
    let pool = cetus["defaultPoolId"]
        .as_str()
        .ok_or("no default Cetus pool")?
        .to_owned();
    let usdc = cetus["tokens"]
        .as_array()
        .and_then(|t| t.iter().find(|t| t["symbol"] == "USDC"))
        .and_then(|t| t["coinType"].as_str())
        .ok_or("no USDC in the Cetus registry")?
        .to_owned();
    match kind {
        Kind::Swap => {
            let spend = 5_000_000;
            // Floored at 90% of a quote: tight enough to mean something, loose enough that the
            // price moving between here and the swap does not fail the run.
            let quoted = quote_usdc(api, &pool, spend).await?;
            let min_out = quoted * 9 / 10;
            receipts["quote"] = json!({"usdc": quoted.to_string(), "minOut": min_out.to_string()});
            eprintln!("quoted {quoted} USDC base units, floor {min_out}");
            Ok(Plan {
                flow: json!({"nodes":[{"id":"swap","type":"cetus_swap","config":{
                    "amount_in": spend.to_string(), "pool": pool, "inputCoinType": "0x2::sui::SUI",
                    "min_amount_out": min_out.to_string()
                }}],"edges":[]}),
                manifest: json!({"walletCoinType":"0x2::sui::SUI","rules":[
                    {"kind":"budget","totalMist": (spend * swap_runs() + spend / 2).to_string()},
                    {"kind":"per_tx","maxMist": spend.to_string()},
                    {"kind":"slippage_floor","minOutMist": min_out.to_string(),"coinType": usdc},
                    {"kind":"asset_scope","allowedCoinTypes":["0x2::sui::SUI", usdc]}
                ]}),
                budget: spend * swap_runs() + spend / 2,
                per_tx: spend,
                spend,
                attach: json!({}),
            })
        }
        Kind::Stake => {
            let min = protocols["haedal_stake"]["minStakeMist"]
                .as_str()
                .and_then(|m| m.parse::<u64>().ok())
                .unwrap_or(SUI);
            Ok(Plan {
                flow: json!({"nodes":[{"id":"stake","type":"haedal_stake","config":{
                    "amount": min.to_string()
                }}],"edges":[]}),
                manifest: json!({"walletCoinType":"0x2::sui::SUI","rules":[
                    {"kind":"budget","totalMist": (min + min / 2).to_string()},
                    {"kind":"per_tx","maxMist": min.to_string()}
                ]}),
                budget: min + min / 2,
                per_tx: min,
                spend: min,
                attach: json!({}),
            })
        }
        Kind::DeepBook => {
            let manager = provision_manager(setup, chain).await?;
            eprintln!(
                "manager {} (trade cap {}, deposit cap {})",
                manager.id, manager.trade_cap, manager.deposit_cap
            );
            receipts["manager"] = json!({"id": manager.id, "tradeCap": manager.trade_cap, "depositCap": manager.deposit_cap});
            *manager_slot = Some(manager.clone());
            // An ask at three times the market, so it rests on the book instead of filling.
            let per_sui = quote_usdc(api, &pool, 5_000_000).await? as f64 / 5_000.0;
            let price = format!("{:.2}", (per_sui * 3.0).max(0.01));
            receipts["order"] = json!({"marketUsdcPerSui": per_sui, "askPrice": price});
            eprintln!("market about {per_sui:.4} USDC per SUI, resting ask at {price}");
            let deposit = SUI + SUI / 10;
            let config = json!({
                "poolKey": deepbook_pool_key(setup.network), "depositSui": "1.1", "price": price,
                "quantity": "1", "isBid": "false", "payWithDeep": "false", "clientOrderId": "1",
                "balanceManagerId": manager.id, "tradeCapId": manager.trade_cap,
                "depositCapId": manager.deposit_cap
            });
            Ok(Plan {
                flow: json!({"nodes":[{"id":"order","type":"deepbook_limit_order","config": config}],"edges":[]}),
                manifest: json!({"walletCoinType":"0x2::sui::SUI","rules":[
                    {"kind":"budget","totalMist": (deposit + SUI / 10).to_string()},
                    {"kind":"per_tx","maxMist": deposit.to_string()}
                ]}),
                budget: deposit + SUI / 10,
                per_tx: deposit,
                spend: deposit,
                attach: json!({
                    "balanceManagerId": manager.id, "tradeCapId": manager.trade_cap,
                    "depositCapId": manager.deposit_cap, "price": price
                }),
            })
        }
    }
}

async fn run(
    kind: Kind,
    setup: &Setup,
    chain: &GrpcSui,
    receipts: &mut Value,
    wallet_slot: &mut Option<String>,
    manager_slot: &mut Option<Manager>,
) -> Result<(), String> {
    let api = setup.api.trim_end_matches('/');
    let owner = setup.owner.address().to_string();
    let agent = setup.agent.to_string();

    step(&format!("{}: owner signs in", kind.name()));
    let token = grant_cmd::sign_in(api, &setup.owner).await?;

    step(&format!("{}: publish", kind.name()));
    let plan = plan(kind, setup, chain, api, receipts, manager_slot).await?;
    let published = data(
        http::post_json_as(
            &format!("{api}/api/publish"),
            &json!({"flow": plan.flow, "manifest": plan.manifest}),
            &token,
        )
        .await?,
    );
    let action = published["skillId"]
        .as_str()
        .ok_or("publishing returned no id")?
        .to_owned();
    receipts["action"] = json!(action);
    eprintln!("action {action}");

    step("create the empty wallet");
    let expires = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
        + 2 * 3_600_000)
        .to_string();
    let mut input = json!({
        "skillId": action, "sender": owner, "agent": agent,
        "budgetMist": plan.budget.to_string(), "perTxMist": plan.per_tx.to_string(),
        "expiresAtMs": expires,
    });
    if let (Some(target), Some(extra)) = (input.as_object_mut(), plan.attach.as_object()) {
        for (key, value) in extra {
            target.insert(key.clone(), value.clone());
        }
    }
    let prepared =
        data(http::post_json_as(&format!("{api}/api/setup/prepare"), &input, &token).await?);
    let created = sign_and_run(
        chain,
        &setup.owner,
        prepared["setupPtb"].as_str().ok_or("no setupPtb")?,
    )
    .await?;
    let find = |suffix: &str| {
        created
            .created
            .iter()
            .find(|o| o.object_type.as_deref().is_some_and(|t| t.contains(suffix)))
            .map(|o| o.object_id.clone())
    };
    let wallet = find("::agent_wallet::AgentWallet<").ok_or("no wallet was created")?;
    let cap = find("::agent_wallet::AgentCap").ok_or("no agent capability was created")?;
    *wallet_slot = Some(wallet.clone());
    receipts["wallet"] = json!({"id": wallet, "agentCap": cap, "createDigest": created.digest});
    eprintln!("wallet {wallet}, cap {cap}");

    step("attach rules and fund");
    let mut attach_input = input.clone();
    attach_input["walletId"] = json!(wallet);
    attach_input["agentCapId"] = json!(cap);
    let attached =
        data(http::post_json_as(&format!("{api}/api/setup/attach"), &attach_input, &token).await?);
    let funded = sign_and_run(
        chain,
        &setup.owner,
        attached["attachPtb"].as_str().ok_or("no attachPtb")?,
    )
    .await?;
    let budget = plan.budget;
    wallet_until(chain, &wallet, "its funding", |f| {
        number(f, "budget") == budget
    })
    .await?;
    receipts["wallet"]["fundDigest"] = json!(funded.digest);

    step("owner signs the grant");
    let granted = grant_cmd::grant(
        &setup.owner,
        &GrantArgs {
            api: api.into(),
            action_id: action.clone(),
            wallet_id: wallet.clone(),
            budget_mist: plan.budget.to_string(),
            per_tx_mist: plan.per_tx.to_string(),
            expires_at_ms: Some(expires.clone()),
            submit: true,
        },
    )
    .await?;
    receipts["grant"] = json!({"revision": granted["revision"]});

    step("a tampered grant is refused");
    let listed = data(http::get_json(&format!("{api}/api/grants/{agent}")).await?);
    let mut forged = listed["grants"]
        .as_array()
        .and_then(|g| g.iter().find(|g| g["grant"]["walletId"] == wallet.as_str()))
        .cloned()
        .ok_or("the stored grant is not listed")?;
    forged["grant"]["revision"] = json!(2);
    match http::post_json_as(&format!("{api}/api/grants"), &forged, &token).await {
        Ok(v) => return Err(format!("a grant the owner never signed was stored: {v}")),
        Err(e) if e.contains("signature") => eprintln!("refused: {e}"),
        Err(e) => return Err(format!("refused, but not for its signature: {e}")),
    }

    if keep() {
        receipts["kept"] = json!({
            "wallet": wallet, "action": action, "budgetMist": plan.budget.to_string(),
            "perTxMist": plan.per_tx.to_string(), "spendMist": plan.spend.to_string(),
            "agent": agent, "owner": owner, "api": api,
        });
        eprintln!("\nkept live for the caller: wallet {wallet}, action {action}");
        return Ok(());
    }

    step("the agent lists and runs the grant");
    let hasui_before = coin_total(chain, &agent, "::hasui::HASUI").await;
    let mut signer = Agent::start(setup);
    let (actions, _) = signer.call("rill_actions", json!({}));
    let entry = grant_entry(&actions, &action, &wallet);
    if entry["usable"] != Value::Bool(true) {
        return Err(format!("the fresh grant is not usable: {entry}"));
    }
    let (ran, failed) = signer.call(
        "rill_run_action",
        json!({"actionId": action, "walletId": wallet}),
    );
    if failed {
        return Err(format!("the granted run was refused: {ran}"));
    }
    let digest = ran["digest"].as_str().ok_or("the run returned no digest")?;
    let spend = plan.spend;
    wallet_until(chain, &wallet, "one run's spend", |f| {
        number(f, "spent") == spend
    })
    .await?;
    receipts["run"] = json!({"digest": digest, "spentMist": spend.to_string()});
    eprintln!("ran {digest}");

    if kind == Kind::Stake {
        let after = coin_total_changed(chain, &agent, "::hasui::HASUI", hasui_before).await;
        if after <= hasui_before {
            return Err(format!(
                "the stake minted no haSUI to the agent ({hasui_before} -> {after})"
            ));
        }
        receipts["run"]["haSuiMinted"] = json!((after - hasui_before).to_string());
        eprintln!("haSUI to the agent: {}", after - hasui_before);
    }

    step("a second run exceeds what is left and is refused");
    let (refused, failed) = signer.call(
        "rill_run_action",
        json!({"actionId": action, "walletId": wallet}),
    );
    if !failed || refused.get("digest").is_some() {
        return Err(format!("a run past the budget was not refused: {refused}"));
    }
    // Refused by name, not as an abort code an agent would have to parse.
    if refused["code"] != "rule_refused" || refused["rule"].as_str().is_none() {
        return Err(format!("the refusal does not name its rule: {refused}"));
    }
    if number(&wallet_fields(chain, &wallet).await?, "spent") != spend {
        return Err("the refused run still spent".into());
    }
    receipts["overBudget"] = json!({"code": refused["code"], "rule": refused["rule"], "abortCode": refused["abortCode"]});
    eprintln!("refused: {} {}", refused["rule"], refused["abortCode"]);

    step("owner revokes");
    revoke(setup, &wallet).await?;
    wallet_until(chain, &wallet, "revoked and emptied", |f| {
        f["revoked"] == Value::Bool(true) && number(f, "budget") == 0
    })
    .await?;

    step("the agent is refused after revocation");
    let (actions, _) = signer.call("rill_actions", json!({}));
    let entry = grant_entry(&actions, &action, &wallet);
    if entry["usable"] != Value::Bool(false) {
        return Err(format!(
            "a revoked wallet's grant still reads as usable: {entry}"
        ));
    }
    let (after, failed) = signer.call(
        "rill_run_action",
        json!({"actionId": action, "walletId": wallet}),
    );
    if !failed {
        return Err(format!("a revoked wallet still ran: {after}"));
    }
    receipts["afterRevoke"] = json!({"code": after["code"], "reason": entry["refusedBecause"]});

    if let Some(manager) = manager_slot.clone() {
        step("owner cancels the resting order and withdraws the deposit");
        let (digest, delta) = recover_manager(setup, chain, &manager).await?;
        receipts["recovered"] = json!({ "digest": digest, "ownerSuiDelta": delta.to_string() });
        // The deposit, net of this transaction's gas, read from its own effects. An ask that had
        // filled would have come back as USDC, and the SUI returned would fall short.
        if delta < i128::from(SUI) {
            return Err(format!(
                "the manager returned only {delta} mist of SUI; did the ask fill?"
            ));
        }
        eprintln!("recovered {digest}, owner +{delta} mist");
    }

    receipts["passed"] = json!(true);
    eprintln!("\n{}: all steps passed", kind.name());
    Ok(())
}
