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

const SWAP_MIST: u64 = 5_000_000;
const PER_TX_MIST: u64 = 5_000_000;
/// One swap fits, a second does not: what is left after the first is below the per-run spend.
const BUDGET_MIST: u64 = 7_500_000;

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

#[tokio::test]
#[ignore = "spends on a live network; run through scripts/e2e.sh"]
async fn an_owner_grants_an_action_and_the_agent_runs_it_within_its_limits() {
    let setup = setup();
    let chain = GrpcSui::new(&setup.rpc).expect("a fullnode client");
    let mut receipts = json!({ "network": setup.network_name });
    let mut wallet: Option<String> = None;

    let result = run(&setup, &chain, &mut receipts, &mut wallet).await;

    // Whatever happened, a wallet this run funded is revoked before the run ends.
    if let (Err(_), Some(id)) = (&result, &wallet) {
        let revoked = matches!(
            wallet_fields(&chain, id).await,
            Ok(fields) if fields["revoked"] == Value::Bool(true)
        );
        if !revoked {
            eprintln!("\n== cleanup: revoking {id}");
            let _ = revoke(&setup, id).await;
        }
    }
    let report = setup.work_dir.join("e2e-receipts.json");
    std::fs::write(&report, serde_json::to_vec_pretty(&receipts).unwrap()).unwrap();
    eprintln!("\nreceipts: {}", report.display());
    if let Err(e) = result {
        panic!("{e}");
    }
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

async fn run(
    setup: &Setup,
    chain: &GrpcSui,
    receipts: &mut Value,
    wallet_slot: &mut Option<String>,
) -> Result<(), String> {
    let api = setup.api.trim_end_matches('/');
    let owner = setup.owner.address().to_string();
    let agent = setup.agent.to_string();

    step("owner signs in");
    let token = grant_cmd::sign_in(api, &setup.owner).await?;

    step("publish a guarded swap");
    let protocols = data(http::get_json(&format!("{api}/api/protocols")).await?);
    let cetus = &protocols["cetus_swap"];
    let pool = cetus["defaultPoolId"]
        .as_str()
        .ok_or("no default Cetus pool")?;
    let usdc = cetus["tokens"]
        .as_array()
        .and_then(|t| t.iter().find(|t| t["symbol"] == "USDC"))
        .and_then(|t| t["coinType"].as_str())
        .ok_or("no USDC in the Cetus registry")?
        .to_owned();
    let flow_with = |min_out: u64| {
        json!({"nodes":[{"id":"swap","type":"cetus_swap","config":{
            "amount_in": SWAP_MIST.to_string(), "pool": pool, "inputCoinType": "0x2::sui::SUI",
            "min_amount_out": min_out.to_string()
        }}],"edges":[]})
    };
    // Quote by simulating, then floor at 90% of it: tight enough to mean something, loose enough
    // that the price moving between here and the swap does not fail the run.
    let simulated = data(
        http::post_json(
            &format!("{api}/api/simulate"),
            &json!({"flow": flow_with(1)}),
        )
        .await?,
    );
    let quoted = simulated["simulation"]["balanceChanges"]
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
        .ok_or_else(|| format!("the simulation quoted no USDC: {simulated}"))?;
    let min_out = (quoted as u64) * 9 / 10;
    let manifest = json!({"walletCoinType":"0x2::sui::SUI","rules":[
        {"kind":"budget","totalMist": BUDGET_MIST.to_string()},
        {"kind":"per_tx","maxMist": PER_TX_MIST.to_string()},
        {"kind":"slippage_floor","minOutMist": min_out.to_string(),"coinType": usdc},
        {"kind":"asset_scope","allowedCoinTypes":["0x2::sui::SUI", usdc]}
    ]});
    let published = data(
        http::post_json_as(
            &format!("{api}/api/publish"),
            &json!({"flow": flow_with(min_out), "manifest": manifest}),
            &token,
        )
        .await?,
    );
    let action = published["skillId"]
        .as_str()
        .ok_or("publishing returned no id")?
        .to_owned();
    receipts["action"] =
        json!({"id": action, "quotedUsdc": quoted.to_string(), "minOut": min_out.to_string()});
    eprintln!("action {action}, quoted {quoted} USDC base units, floor {min_out}");

    step("create the empty wallet");
    let expires = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
        + 2 * 3_600_000)
        .to_string();
    let input = json!({
        "skillId": action, "sender": owner, "agent": agent,
        "budgetMist": BUDGET_MIST.to_string(), "perTxMist": PER_TX_MIST.to_string(),
        "expiresAtMs": expires,
    });
    let plan = data(http::post_json_as(&format!("{api}/api/setup/prepare"), &input, &token).await?);
    let created = sign_and_run(
        chain,
        &setup.owner,
        plan["setupPtb"].as_str().ok_or("no setupPtb")?,
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
    wallet_until(chain, &wallet, "its funding", |f| {
        number(f, "budget") == BUDGET_MIST
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
            budget_mist: BUDGET_MIST.to_string(),
            per_tx_mist: PER_TX_MIST.to_string(),
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

    step("the agent lists and runs the grant");
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
    wallet_until(chain, &wallet, "one run's spend", |f| {
        number(f, "spent") == SWAP_MIST
    })
    .await?;
    receipts["swap"] = json!({"digest": digest, "spentMist": SWAP_MIST.to_string()});
    eprintln!("swap {digest}");

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
    if number(&wallet_fields(chain, &wallet).await?, "spent") != SWAP_MIST {
        return Err("the refused run still spent".into());
    }
    receipts["overBudget"] = json!({"code": refused["code"], "rule": refused["rule"], "abortCode": refused["abortCode"], "message": refused["message"]});
    eprintln!("refused: {}", refused["code"]);

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
    receipts["passed"] = json!(true);
    eprintln!("\nall steps passed");
    Ok(())
}
