//! The stdio MCP transport.
//!
//! One JSON message per line in, one per line out. That is the whole protocol at this layer, and
//! writing it out is cheaper than the dependency that would hide it.
//!
//! # stdout is the wire
//!
//! Every diagnostic goes to stderr. A single stray `println!` corrupts the stream and the client
//! reports a parse error with no indication of where it came from — which is why the readiness
//! banner this binary prints on startup goes to stderr too, even though it is meant for a human.
//!
//! # Notifications get nothing
//!
//! A message with no `id` is answered with silence, not with a null-id response. Answering one is
//! a spec violation that some clients tolerate and others hang on.

use std::collections::HashMap;
use std::io::{BufRead, Write};

use serde_json::{json, Value};

use crate::keystore::Keystore;
use crate::runset::{RunSet, RUN_SET_VAR};
use crate::verdict::Failure;

/// Protocol versions this signer speaks.
// The list and the negotiation live in rill-mcp, so the two transports cannot drift apart on
// which revisions of the protocol they speak. See its module note.
use rill_mcp::negotiate_protocol_version;

/// What the signer knows about itself. Everything here is public.
pub struct WalletContext {
    pub keystore: Option<Keystore>,
    /// Loaded at startup and never written by any tool. An agent that could widen its own limits
    /// has no limits — the Move contract makes the same choice by reserving `add_rule` to the owner.
    pub run_set: Option<RunSet>,
    pub network: String,
    /// Whether signing on mainnet has been explicitly opted into.
    pub mainnet_allowed: bool,
    /// The last policy refusal, so `rill_explain_rejection` can answer without re-running anything.
    pub last_rejection: Option<String>,
    /// Every envelope this signer has already handed to the chain, by its pinned byte digest.
    ///
    /// # Why a second call is refused rather than obeyed
    ///
    /// `rill_execute` submits, and the tool used to say in its own answer that calling it again
    /// with the same envelope submits a second transaction. That is a footgun described rather
    /// than removed: an agent whose reply was lost, or which retried on a timeout, would move the
    /// money twice, and it cannot tell its retry from its first attempt because the envelope is
    /// identical both times. The signer can.
    ///
    /// The key is the digest pinned from the bytes about to be signed, so it matches the same
    /// transaction and nothing else. It lives for the life of this process, which is the honest
    /// scope and is stated in the refusal: a freshly started signer does not know, and a replay
    /// would then fail on chain anyway, as an unexplained stale-object error rather than a named
    /// refusal.
    pub submitted: HashMap<String, Submission>,
}

/// One envelope this signer has already handed to the chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submission {
    /// The chain's digest, when the node answered. `None` means the response was lost and whether
    /// it landed is unknown, which is the case where a blind retry is most dangerous: the first
    /// submission may already be on chain.
    pub digest: Option<String>,
}

impl WalletContext {
    pub fn new(keystore: Option<Keystore>, network: String, mainnet_allowed: bool) -> Self {
        Self {
            keystore,
            run_set: None,
            network,
            mainnet_allowed,
            last_rejection: None,
            submitted: HashMap::new(),
        }
    }

    pub fn with_run_set(mut self, run_set: Option<RunSet>) -> Self {
        self.run_set = run_set;
        self
    }
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn tool_ok(id: Value, data: Value) -> Value {
    rpc_result(
        id,
        json!({
            "content": [{ "type": "text", "text": serde_json::to_string_pretty(&data).unwrap_or_default() }],
            "structuredContent": data,
            "isError": false
        }),
    )
}

fn tool_error(id: Value, code: &str, message: &str) -> Value {
    tool_error_with(id, code, message, Value::Null)
}

/// A refusal with structured detail beside the message.
///
/// The message is what a person reads; the fields are what the agent acts on. A refusal that put
/// the name of the rule that refused only into prose left that decision to a substring match, and
/// an agent that cannot tell "the cap stopped you" from "the node is down" retries the same amount
/// forever.
fn tool_error_with(id: Value, code: &str, message: &str, detail: Value) -> Value {
    let mut structured = json!({ "code": code, "message": message });
    if let (Some(target), Some(fields)) = (structured.as_object_mut(), detail.as_object()) {
        for (key, value) in fields {
            target.insert(key.clone(), value.clone());
        }
    }
    rpc_result(
        id,
        json!({
            "content": [{ "type": "text", "text": message }],
            "structuredContent": structured,
            "isError": true
        }),
    )
}

/// A refusal the chain made by name, with the rule in a field of its own.
///
/// `rule` is what a caller acts on: `per_tx` means spend less, `agent_wallet` means the wrong key
/// signed, and neither is legible from `"code": "refused"` with the name buried in a sentence. The
/// name comes from [`rill_chain::aborts::classify_rule_abort`], which reads it out of the Move
/// abort, so it is the module the chain really aborted in rather than a guess made here.
fn rule_refusal(id: Value, refusal: &rill_chain::aborts::RuleRefusal) -> Value {
    let message = Failure::Refused(refusal.clone()).to_string();
    tool_error_with(
        id,
        "rule_refused",
        &message,
        json!({
            "rule": refusal.module,
            "abortCode": refusal.code,
            "advice": refusal.advice(),
        }),
    )
}

/// Render a command's failure, keeping a named refusal named.
///
/// `code` is for the failures that are not refusals: which tool could not finish. A refusal never
/// takes it, because "the per-transaction cap refused this" is not a failure of the tool and
/// reporting it as one is how a caller concludes the signer is broken.
///
/// Public because this mapping is the contract, not an implementation detail: every path that
/// refuses goes through it, and `tests/execute_flow.rs` asserts on what it produces. The paths that
/// call it all need a node, so a test that could only reach it through one would not run in CI.
pub fn failure_response(
    context: &mut WalletContext,
    id: Value,
    code: &str,
    failure: &Failure,
) -> Value {
    context.last_rejection = Some(failure.to_string());
    match failure {
        Failure::Refused(refusal) => rule_refusal(id, refusal),
        Failure::Failed(message) => tool_error(id, code, message),
    }
}

/// Handle one message. `None` means a notification, which gets no reply at all.
pub fn handle(context: &mut WalletContext, message: &Value) -> Option<Value> {
    let method = message.get("method").and_then(Value::as_str)?;
    let has_id = message.get("id").is_some();
    if !has_id {
        if method.starts_with("notifications/") {
            return None;
        }
        return Some(rpc_error(
            Value::Null,
            -32600,
            "Invalid Request: \"id\" is required for a non-notification request.",
        ));
    }
    let id = message.get("id").cloned().unwrap_or(Value::Null);

    match method {
        "initialize" => {
            let requested = message
                .get("params")
                .and_then(|p| p.get("protocolVersion"))
                .and_then(Value::as_str);
            let version = negotiate_protocol_version(requested);
            Some(rpc_result(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": {} },
                    "serverInfo": {
                        "name": crate::BINARY_NAME,
                        "version": env!("CARGO_PKG_VERSION"),
                        "description": "Local signer. Holds the key, validates independently, and is the only thing here that can submit."
                    }
                }),
            ))
        }
        "ping" => Some(rpc_result(id, json!({}))),
        "tools/list" => {
            let tools: Vec<Value> = rill_mcp::tools(rill_mcp::Surface::Wallet)
                .into_iter()
                .map(|t| serde_json::to_value(t).unwrap_or(Value::Null))
                .collect();
            Some(rpc_result(id, json!({ "tools": tools })))
        }
        "tools/call" => Some(call(context, id, message)),
        other => Some(rpc_error(id, -32601, &format!("Method not found: {other}"))),
    }
}

fn call(context: &mut WalletContext, id: Value, message: &Value) -> Value {
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return rpc_error(id, -32602, "tools/call requires a tool name.");
    };

    match name {
        "rill_status" => status(context, id),
        "rill_wallet" => wallet(context, id, &params),
        "rill_create_wallet" => create_wallet(context, id, &params),
        "rill_attach_rules" => attach_rules(context, id, &params),
        "rill_spend" => spend(context, id, &params),
        "rill_swap" => swap(context, id, &params),
        "rill_execute" => execute(context, id, &params),
        other => rpc_error(id, -32602, &format!("Unknown tool: {other}")),
    }
}

/// Validate an envelope against the pinned run-set.
///
/// Every refusal is remembered so `rill_explain_rejection` can answer without re-running anything,
/// and every refusal names which check failed rather than saying "policy violation" — an operator
/// reading the latter learns nothing about what to fix.
///
/// The chain runs to the end: validate, pin the bytes and read them, re-simulate against a live
/// fullnode, sign, submit. Only [`rill_policy::Simulated`] can be signed, and only a re-simulation
/// this signer ran itself produces one — the build-time simulation belongs to the server, which is
/// the party this whole path exists to not trust.
/// A single-threaded runtime, built per call.
///
/// The transport is synchronous by design — it reads lines and writes lines — and the chain client
/// is not. Building a runtime per call costs microseconds and keeps the transport free of an
/// executor it would otherwise have to own.
fn block_on<F: std::future::Future>(future: F) -> Result<F::Output, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())
        .map(|rt| rt.block_on(future))
}

fn endpoint(context: &WalletContext) -> String {
    std::env::var("SUI_RPC_URL")
        .unwrap_or_else(|_| format!("https://fullnode.{}.sui.io:443", context.network))
}

fn argument<'a>(params: &'a Value, name: &str) -> Option<&'a str> {
    params
        .get("arguments")
        .and_then(|a| a.get(name))
        .and_then(Value::as_str)
}

/// A count, which is the one kind of argument that is honestly a number.
///
/// Amounts are never read here. They are decimal text all the way to the chain, because a JSON
/// number on the money path is a float, and a float is how 0.1 becomes 0.09999999999999999.
fn count_argument(params: &Value, name: &str) -> Option<u64> {
    params
        .get("arguments")
        .and_then(|a| a.get(name))
        .and_then(Value::as_u64)
}

/// The deployed package and the shared `Version` object it gates itself on.
///
/// Three tools need the same pair, and a capability minted against one package cannot authorise a
/// call in another, so this is read in one place from the environment that a deployment sets.
fn package_id() -> String {
    std::env::var("AGENT_WALLET_PACKAGE_ID")
        .unwrap_or_else(|_| rill_ptb::deployments::TESTNET_AGENT_WALLET.to_string())
}

fn version_id() -> String {
    std::env::var("AGENT_WALLET_VERSION_ID")
        .unwrap_or_else(|_| rill_ptb::deployments::TESTNET_AGENT_WALLET_VERSION.to_string())
}

/// The deployed `rill_guard` package, which carries the slippage floor every swap passes through.
///
/// Defaulted like the pair above rather than asked for: a caller that had to supply it could supply
/// nothing, and a swap with no floor is exactly the outcome the floor exists to prevent.
fn guard_package_id() -> String {
    std::env::var("RILL_GUARD_PACKAGE_ID")
        .unwrap_or_else(|_| rill_ptb::deployments::TESTNET_RILL_GUARD.to_string())
}

/// Milliseconds since the epoch, for an expiry the contract compares against its own clock.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The gas budget every tool here builds with, in mist.
///
/// A ceiling the gas coins have to cover, not a fee, and the same value the commands use, so a
/// wallet minted over MCP costs what one minted by hand costs and fails the same way when the
/// account is too thin to cover it.
const TOOL_GAS_BUDGET: u64 = 100_000_000;

/// Whether this signer can act. Says nothing about any particular wallet.
fn status(context: &WalletContext, id: Value) -> Value {
    let mut answer = match &context.keystore {
        Some(keystore) => json!({
            "ready": true,
            "address": keystore.address().to_string(),
            "network": context.network,
            // Stated rather than assumed: an operator should be able to see that mainnet signing
            // is off without reading the launch environment.
            "mainnetSigningAllowed": context.mainnet_allowed,
        }),
        None => json!({
            "ready": false,
            "network": context.network,
            "reason": "No signing key is configured. Set RILL_SUI_PRIVATE_KEY in the shell or \
                       secret manager that launches this process, or run \
                       `sui client new-address ed25519`."
        }),
    };

    if let Some(reason) = &context.last_rejection {
        answer["lastRejection"] = Value::String(reason.clone());
    }

    // A run-set, when one is loaded, says what this run is pinned to. Reported as null rather than
    // omitted: an absent field reads as "no limits", which is the opposite of what it means.
    answer["runSet"] = match &context.run_set {
        Some(run_set) => json!({
            "label": run_set.label,
            "network": run_set.network,
            "actionId": run_set.action_id,
            "walletId": run_set.wallet_id,
            "allowedTargets": run_set.allowed_targets,
            "maxAmountBaseUnits": run_set.max_amount_base_units,
            "minimumRemainingBaseUnits": run_set.minimum_remaining_base_units,
            // Which layer holds each limit, so a reader is not left assuming the chain enforces
            // all of them.
            "declaration": rill_core::manifest::to_declaration(&run_set.capability_manifest)
                .map(|d| serde_json::to_value(d).unwrap_or(Value::Null))
                .unwrap_or(Value::Null),
        }),
        None => Value::Null,
    };

    tool_ok(id, answer)
}

/// What one wallet permits, and which layer holds each limit.
///
/// The on-chain rules are read from the chain that enforces them. The pre-flight rules come from
/// the loaded run-set, because they exist only where this signer has something to refuse against;
/// see [`crate::wallet_read`] for why the two are labelled separately.
///
/// Its own tool rather than an argument to `rill_status`: it answers a different question, needs an
/// argument that one does not, and costs a round trip a caller asking about the signer should not
/// pay for.
fn wallet(context: &mut WalletContext, id: Value, params: &Value) -> Value {
    let Some(wallet) = argument(params, "wallet") else {
        return tool_error(id, "invalid_arguments", "wallet is required.");
    };
    let package = std::env::var("AGENT_WALLET_PACKAGE_ID")
        .unwrap_or_else(|_| rill_ptb::deployments::TESTNET_AGENT_WALLET.to_string());

    let local = context
        .run_set
        .as_ref()
        .map(|run_set| &run_set.capability_manifest);
    let outcome = block_on(crate::wallet_read::read_limits(
        &endpoint(context),
        &package,
        wallet,
        local,
    ));
    match outcome {
        Ok(Ok(limits)) => tool_ok(id, limits),
        Ok(Err(e)) | Err(e) => {
            context.last_rejection = Some(e.clone());
            tool_error(id, "read_failed", &e)
        }
    }
}

/// Release funds from an agent wallet, gated by the rules the wallet carries on chain.
///
/// # A refusal here is the wallet working
///
/// The rules live in a Move contract, so this process cannot widen them and neither can the agent
/// calling it. When the contract refuses, that is reported as a refusal naming the rule — not as an
/// error, and never as something to retry with the same amount.
fn spend(context: &mut WalletContext, id: Value, params: &Value) -> Value {
    let Some(keystore) = context.keystore.as_ref() else {
        let reason = "No signing key is configured, so nothing can be signed.".to_string();
        context.last_rejection = Some(reason.clone());
        return tool_error(id, "no_key", &reason);
    };
    if context.network == "mainnet" && !context.mainnet_allowed {
        let reason = rill_core::mainnet::mainnet_refusal();
        context.last_rejection = Some(reason.clone());
        return tool_error(id, "mainnet_not_opted_in", &reason);
    }

    let (Some(wallet), Some(cap), Some(amount)) = (
        argument(params, "wallet"),
        argument(params, "cap"),
        argument(params, "amount"),
    ) else {
        return tool_error(
            id,
            "invalid_arguments",
            "wallet, cap and amount are all required. amount is decimal text, never a number.",
        );
    };

    let args = crate::spend_cmd::SpendArgs {
        package_id: package_id(),
        version_id: version_id(),
        wallet_id: wallet.to_string(),
        cap_id: cap.to_string(),
        amount: amount.to_string(),
        recipient: argument(params, "to").map(str::to_owned),
        gas_budget: TOOL_GAS_BUDGET,
        dry_run: false,
    };

    // A rule that refused arrives as a refusal naming the rule, not as `"code": "refused"` with
    // `per_tx` somewhere in the prose. That distinction is the whole of R3 on this path.
    match block_on(crate::spend_cmd::spend_json(
        &endpoint(context),
        keystore,
        &args,
    )) {
        Ok(Ok(result)) => tool_ok(id, result),
        Ok(Err(failure)) => failure_response(context, id, "spend_failed", &failure),
        Err(e) => failure_response(context, id, "spend_failed", &Failure::Failed(e)),
    }
}

/// Release funds under the wallet's rules and swap them, in one transaction.
///
/// The gated prefix is the same builder `rill spend` and `rill order` use, so the SUI that enters the
/// swap was released by the contract against its own rules: a swap above a cap is refused by the
/// chain, and the refusal names the rule. A swap from this signer's own coins would be an ordinary
/// swap with extra steps, and would prove nothing the product claims.
fn swap(context: &mut WalletContext, id: Value, params: &Value) -> Value {
    let Some(keystore) = context.keystore.as_ref() else {
        let reason = "No signing key is configured, so nothing can be signed.".to_string();
        context.last_rejection = Some(reason.clone());
        return tool_error(id, "no_key", &reason);
    };
    if context.network == "mainnet" && !context.mainnet_allowed {
        let reason = rill_core::mainnet::mainnet_refusal();
        context.last_rejection = Some(reason.clone());
        return tool_error(id, "mainnet_not_opted_in", &reason);
    }

    let required = [
        "wallet",
        "cap",
        "amount",
        "pool",
        "integratePackage",
        "globalConfig",
        "coinTypeA",
        "coinTypeB",
    ];
    let mut missing = Vec::new();
    for name in required {
        if argument(params, name).is_none() {
            missing.push(name);
        }
    }
    let a2b = params
        .get("arguments")
        .and_then(|a| a.get("a2b"))
        .and_then(Value::as_bool);
    if a2b.is_none() {
        missing.push("a2b");
    }
    if !missing.is_empty() {
        return tool_error(
            id,
            "invalid_arguments",
            &format!(
                "missing: {}. amount is decimal text, never a number, and a2b is a boolean saying \
                 which side the wallet's SUI funds.",
                missing.join(", ")
            ),
        );
    }
    let get = |name: &str| argument(params, name).unwrap_or_default().to_string();

    let args = crate::swap_cmd::SwapArgs {
        package_id: package_id(),
        version_id: version_id(),
        wallet_id: get("wallet"),
        cap_id: get("cap"),
        integrate_package_id: get("integratePackage"),
        global_config_id: get("globalConfig"),
        pool_id: get("pool"),
        coin_type_a: get("coinTypeA"),
        coin_type_b: get("coinTypeB"),
        a2b: a2b.unwrap_or(false),
        spend: get("amount"),
        min_out_base_units: get("minOut"),
        guard_package_id: guard_package_id(),
        // Absent is false, so an unprotected swap needs the caller to have said the word.
        accept_any_output: params
            .get("arguments")
            .and_then(|a| a.get("acceptAnyOutput"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        gas_budget: TOOL_GAS_BUDGET,
        dry_run: false,
    };

    match block_on(crate::swap_cmd::swap_json(
        &endpoint(context),
        keystore,
        &args,
    )) {
        Ok(Ok(result)) => tool_ok(id, result),
        Ok(Err(failure)) => failure_response(context, id, "swap_failed", &failure),
        Err(e) => failure_response(context, id, "swap_failed", &Failure::Failed(e)),
    }
}

/// Mint an agent wallet and the capability that drives it. Owner-side, and the first step of a flow
/// an agent has to be able to drive on its own.
///
/// # Why an owner's tool sits on the agent's surface
///
/// Everything before the spend was a command somebody typed, so the flow an agent could drive
/// started halfway through: it could spend from a wallet and could not get one, and every
/// demonstration of the claim began with a human at a terminal. Offering this here widens nothing,
/// because what an agent may do is decided by the contract and not by this list: the cap goes to
/// the agent address named here, `add_rule` asserts the owner, and a signer launched with the
/// agent's key is refused by name the moment it reaches for either.
///
/// The key this process holds becomes the wallet's owner. That is stated in the tool's description
/// rather than inferred, because it is the one consequence a caller cannot see from the arguments.
fn create_wallet(context: &mut WalletContext, id: Value, params: &Value) -> Value {
    let Some(keystore) = context.keystore.as_ref() else {
        let reason = "No signing key is configured, so no wallet can be created. The key this \
                      signer holds is what becomes the wallet's owner."
            .to_string();
        context.last_rejection = Some(reason.clone());
        return tool_error(id, "no_key", &reason);
    };
    if context.network == "mainnet" && !context.mainnet_allowed {
        let reason = rill_core::mainnet::mainnet_refusal();
        context.last_rejection = Some(reason.clone());
        return tool_error(id, "mainnet_not_opted_in", &reason);
    }

    let (Some(agent), Some(amount), Some(budget), Some(per_tx)) = (
        argument(params, "agent"),
        argument(params, "amount"),
        argument(params, "budget"),
        argument(params, "perTx"),
    ) else {
        return tool_error(
            id,
            "invalid_arguments",
            "agent, amount, budget and perTx are all required. agent is the address that receives \
             the AgentCap, amount is decimal SUI as text, and budget and perTx are mist as text, \
             never numbers.",
        );
    };
    if let Some(refusal) = bad_mist(id.clone(), &[("budget", budget), ("perTx", per_tx)]) {
        return refusal;
    }

    let args = crate::wallet::CreateArgs {
        package_id: package_id(),
        version_id: version_id(),
        agent: Some(agent.to_string()),
        amount: amount.to_string(),
        expires_in_days: count_argument(params, "days").unwrap_or(30),
        manifest: bounded_manifest(budget, per_tx),
        gas_budget: TOOL_GAS_BUDGET,
        // Not a dry run. A tool whose whole purpose is to mint a wallet would otherwise answer
        // with a simulation and leave the agent with nothing to attach rules to, and there is no
        // second call for it to come back with: `--submit` is a flag a person types.
        dry_run: false,
    };

    match block_on(crate::wallet::create_json(
        &endpoint(context),
        keystore,
        &args,
        now_ms(),
    )) {
        Ok(Ok(report)) => tool_ok(id, report),
        Ok(Err(failure)) => failure_response(context, id, "create_failed", &failure),
        Err(e) => failure_response(context, id, "create_failed", &Failure::Failed(e)),
    }
}

/// Attach the rules that bound a wallet. The step that is easy to skip and expensive to skip.
///
/// Owner-only, enforced on chain: `add_rule` asserts the sender is the wallet's owner, so a signer
/// holding the agent's key is refused with `E_NOT_OWNER`, named, rather than by this process
/// declining to try. An agent driving this tool cannot widen its own limits, and the refusal it
/// gets says which rule module said no.
fn attach_rules(context: &mut WalletContext, id: Value, params: &Value) -> Value {
    let Some(keystore) = context.keystore.as_ref() else {
        let reason = "No signing key is configured, so no rules can be attached. Attaching is \
                      owner-only and needs the owner's key."
            .to_string();
        context.last_rejection = Some(reason.clone());
        return tool_error(id, "no_key", &reason);
    };
    if context.network == "mainnet" && !context.mainnet_allowed {
        let reason = rill_core::mainnet::mainnet_refusal();
        context.last_rejection = Some(reason.clone());
        return tool_error(id, "mainnet_not_opted_in", &reason);
    }

    let (Some(wallet), Some(budget), Some(per_tx)) = (
        argument(params, "wallet"),
        argument(params, "budget"),
        argument(params, "perTx"),
    ) else {
        return tool_error(
            id,
            "invalid_arguments",
            "wallet, budget and perTx are all required. wallet is the AgentWallet id from \
             rill_create_wallet, and budget and perTx are mist as text, never numbers.",
        );
    };
    if let Some(refusal) = bad_mist(id.clone(), &[("budget", budget), ("perTx", per_tx)]) {
        return refusal;
    }

    let args = crate::rules_cmd::RulesArgs {
        package_id: package_id(),
        version_id: version_id(),
        wallet_id: wallet.to_string(),
        manifest: bounded_manifest(budget, per_tx),
        gas_budget: TOOL_GAS_BUDGET,
        dry_run: false,
    };

    match block_on(crate::rules_cmd::attach_json(
        &endpoint(context),
        keystore,
        &args,
    )) {
        Ok(Ok(report)) => tool_ok(id, report),
        Ok(Err(failure)) => failure_response(context, id, "attach_failed", &failure),
        Err(e) => failure_response(context, id, "attach_failed", &Failure::Failed(e)),
    }
}

/// The two on-chain rules these tools offer: a total and a per-transaction cap.
///
/// Both kinds the contract proves against the real transaction, and nothing pre-flight, because a
/// rule this signer would have to enforce itself is not something a tool can attach to a wallet.
fn bounded_manifest(budget: &str, per_tx: &str) -> rill_core::manifest::CapabilityManifest {
    rill_core::manifest::CapabilityManifest {
        wallet_coin_type: "0x2::sui::SUI".into(),
        rules: vec![
            rill_core::manifest::CapabilityRule::Budget {
                total_mist: budget.to_owned(),
            },
            rill_core::manifest::CapabilityRule::PerTx {
                max_mist: per_tx.to_owned(),
            },
        ],
    }
}

/// Refuse an amount that is not whole mist, here rather than four round trips later.
///
/// The manifest projection would catch it, after the Version object has been read and the sender's
/// objects listed, and report it as a field name from a layer the caller never mentioned. Named
/// here, the answer is the argument the caller typed.
fn bad_mist(id: Value, fields: &[(&str, &str)]) -> Option<Value> {
    fields.iter().find_map(|(name, value)| {
        value.parse::<u64>().err().map(|_| {
            tool_error(
                id.clone(),
                "invalid_arguments",
                &format!(
                    "{name} must be a whole number of mist written as text, and \"{value}\" is \
                     not. One SUI is 1000000000 mist."
                ),
            )
        })
    })
}

fn execute(context: &mut WalletContext, id: Value, params: &Value) -> Value {
    let Some(run_set) = context.run_set.as_ref() else {
        // Names what is missing and where it goes. "No run-set is loaded" alone tells an agent it
        // cannot proceed without telling whoever launched the signer what to fix.
        let reason = format!(
            "No run-set is loaded, so there are no pinned limits to validate against. Refusing to \
             sign rather than signing against limits nobody set. Point {RUN_SET_VAR} at a run-set \
             file and start this signer again."
        );
        context.last_rejection = Some(reason.clone());
        return tool_error(id, "no_run_set", &reason);
    };
    if context.keystore.is_none() {
        let reason = "No signing key is configured.";
        context.last_rejection = Some(reason.to_string());
        return tool_error(id, "no_key", reason);
    }
    // Mainnet needs an explicit opt-in, and it is checked before anything is parsed — the cheapest
    // possible place to stop.
    if run_set.network == rill_core::envelope::Network::Mainnet && !context.mainnet_allowed {
        let reason = rill_core::mainnet::mainnet_refusal();
        let reason = reason.as_str();
        context.last_rejection = Some(reason.to_string());
        return tool_error(id, "mainnet_not_opted_in", reason);
    }

    let Some(envelope_value) = params.get("arguments").and_then(|a| a.get("envelope")) else {
        return tool_error(id, "invalid_arguments", "envelope is required.");
    };
    let envelope: rill_core::envelope::ExecutionEnvelope =
        match serde_json::from_value(envelope_value.clone()) {
            Ok(e) => e,
            Err(e) => {
                let reason = format!("the envelope did not parse: {e}");
                context.last_rejection = Some(reason.clone());
                return tool_error(id, "malformed_envelope", &reason);
            }
        };

    let policy = match run_set.to_policy() {
        Ok(p) => p,
        Err(e) => return tool_error(id, "bad_run_set", &e.to_string()),
    };
    let validated = match rill_policy::RawEnvelope::new(envelope).validate(&policy, now_ms()) {
        Ok(v) => v,
        Err(rejection) => {
            let reason = rejection.to_string();
            context.last_rejection = Some(reason.clone());
            return tool_error(id, "policy_rejection", &reason);
        }
    };
    let pinned = match validated.pin_bytes(&policy) {
        Ok(p) => p,
        Err(rejection) => {
            let reason = rejection.to_string();
            context.last_rejection = Some(reason.clone());
            return tool_error(id, "policy_rejection", &reason);
        }
    };

    let digest = pinned.pinned_digest().to_string();
    let targets = pinned.decoded().targets.clone();

    // Re-issuing this call does not start a second operation. See `WalletContext::submitted`: the
    // envelope is identical on a retry, so the agent cannot tell one from the other and the signer
    // is the only party that can. Checked after the bytes are pinned, because the digest is
    // re-derived from them rather than taken from what the envelope claims about itself.
    if let Some(prior) = context.submitted.get(&digest) {
        let reason = match &prior.digest {
            Some(landed) => format!(
                "This envelope was already submitted by this signer, and landed as {landed}. \
                 Submitting it again would be a second transaction moving the same funds, so it is \
                 refused. Build a new action if another spend is intended."
            ),
            None => {
                "This envelope was already handed to the node, and the node's answer was lost, \
                     so whether it landed is unknown. Look for it on chain before sending anything \
                     again: a blind retry is how one intended spend becomes two."
                    .to_string()
            }
        };
        context.last_rejection = Some(reason.clone());
        return tool_error_with(
            id,
            "already_submitted",
            &reason,
            json!({ "pinnedDigest": digest, "priorDigest": prior.digest }),
        );
    }

    // The client is built inside the runtime, and everything that uses it stays inside the same
    // one. That is not a style choice.
    //
    // `block_on` builds a fresh current-thread runtime per call and drops it on return, taking
    // the tonic channel's connection task with it. An earlier version created the client in one
    // `block_on` for the simulation, returned it, and then handed it to a second `block_on` for
    // the submission, where the channel was already dead: every `rill_execute` ended in
    // `Service was not ready: transport error, Closed`. It simulated, it signed, and it could
    // never submit, while every CLI command on the same code worked because each does its work
    // inside one runtime. The two-runtime shape dates from the commit whose message says this
    // tool submits, so the claim was never true.
    let endpoint = endpoint(context);
    let Some(keystore) = context.keystore.as_ref() else {
        return tool_error(id, "no_key", "No signing key is configured.");
    };

    /// Which refusal to report, decided inside the runtime and rendered outside it.
    enum Failed {
        Chain(String),
        Malformed(String),
        Signing(String),
        Submit(String),
    }

    let outcome = block_on(async {
        let chain =
            rill_chain::grpc::GrpcSui::new(&endpoint).map_err(|e| Failed::Chain(e.to_string()))?;

        // Our own simulation, against live state. The server's proves only that the server
        // thought so.
        let simulated = pinned
            .simulate(&chain, &policy)
            .await
            .map_err(|r| Failed::Chain(r.to_string()))?;

        let transaction =
            decode_for_signing(simulated.signable_bytes()).map_err(Failed::Malformed)?;
        let signature = keystore
            .sign(&transaction)
            .map_err(|e| Failed::Signing(e.to_string()))?;

        let outcome = rill_chain::SuiWrite::execute(
            &chain,
            simulated.signable_bytes(),
            &[signature.to_base64()],
        )
        .await
        .map_err(|e| Failed::Submit(e.to_string()))?;

        Ok::<_, Failed>((outcome, simulated.spend_base_units().to_string()))
    });

    let (outcome, spend_base_units) = match outcome {
        Ok(Ok(pair)) => pair,
        Ok(Err(failed)) => {
            return match failed {
                Failed::Chain(reason) => {
                    context.last_rejection = Some(reason.clone());
                    // A rule that refused is not a chain problem, and reporting it as one tells an
                    // agent to retry a spend the wallet will refuse every time. The abort text
                    // survives the re-simulation inside the rejection, so it can still be named.
                    if let Some(refusal) = rill_chain::aborts::classify_rule_abort(&reason) {
                        return rule_refusal(id, &refusal);
                    }
                    // A node that answered "this object moved" was reached. Calling that
                    // unavailable sends an agent to retry against a network that is fine,
                    // holding an envelope that will never work; what it needs is a fresh build.
                    match rill_chain::stale::classify_stale_object(&reason) {
                        Some(stale) => tool_error(
                            id,
                            "object_changed",
                            &format!(
                                "{stale}. Build the action again so every object is read afresh."
                            ),
                        ),
                        None => tool_error(id, "chain_unavailable", &reason),
                    }
                }
                Failed::Malformed(reason) => {
                    context.last_rejection = Some(reason.clone());
                    tool_error(id, "malformed_envelope", &reason)
                }
                Failed::Signing(reason) => tool_error(id, "signing_failed", &reason),
                Failed::Submit(reason) => {
                    // The node may have taken it. Remembered with no digest, so a retry is met
                    // with "look on chain first" rather than quietly submitting a second time.
                    // That is the reasoning `verdict::submit_failed` states for the command path.
                    context
                        .submitted
                        .insert(digest.clone(), Submission { digest: None });
                    context.last_rejection = Some(reason.clone());
                    tool_error(id, "submit_failed", &reason)
                }
            };
        }
        Err(reason) => return tool_error(id, "chain_unavailable", &reason),
    };

    if let Some(error) = &outcome.error {
        // A rule can still refuse here, when the wallet's state moved between the simulation and
        // the submission. Named, exactly as it would have been before signing.
        let failure = crate::verdict::did_fail(error);
        return failure_response(context, id, "execution_failed", &failure);
    }

    context.submitted.insert(
        digest.clone(),
        Submission {
            digest: Some(outcome.digest.clone()),
        },
    );

    tool_ok(
        id,
        json!({
            "submitted": true,
            "digest": outcome.digest,
            "pinnedDigest": digest,
            "callSequence": targets,
            "gasUsed": outcome.gas_used_mist,
            "spendBaseUnits": spend_base_units,
            // What the code does, said in the answer. This used to read "calling again with the
            // same envelope submits a second transaction", which described a footgun instead of
            // removing it; the second call is now refused and answered with this digest.
            "note": "Submitted and confirmed. This cannot be undone. Calling again with this same \
                     envelope is refused and answered with this digest, so a retry cannot become a \
                     second transaction; build a new action if another spend is intended."
        }),
    )
}

/// Turn the signable bytes back into a transaction.
///
/// `sign` takes a [`Transaction`] rather than bytes on purpose — signing arbitrary bytes is how a
/// "sign this message" flow becomes a transaction signature. So the bytes are decoded here, and a
/// failure to decode is a refusal rather than something to sign anyway.
fn decode_for_signing(base64_bytes: &str) -> Result<sui_sdk_types::Transaction, String> {
    use base64::Engine as _;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(base64_bytes.trim())
        .map_err(|_| "the signable bytes are not base64".to_string())?;
    bcs::from_bytes(&raw).map_err(|e| format!("the signable bytes are not a transaction: {e}"))
}

/// Read messages from `input`, write replies to `output`.
///
/// Separated from stdin/stdout so it can be driven from a test with ordinary buffers — a transport
/// that can only be exercised by spawning a process tends not to be exercised.
pub fn serve(
    context: &mut WalletContext,
    input: impl BufRead,
    mut output: impl Write,
) -> std::io::Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(message) => handle(context, &message),
            Err(_) => Some(json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": { "code": -32700, "message": "Parse error" }
            })),
        };
        if let Some(response) = response {
            writeln!(output, "{response}")?;
            output.flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> WalletContext {
        WalletContext::new(None, "testnet".into(), false)
    }

    fn drive(input: &str) -> Vec<Value> {
        let mut ctx = context();
        let mut out = Vec::new();
        serve(&mut ctx, input.as_bytes(), &mut out).unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn the_handshake_completes() {
        let out = drive(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#);
        assert_eq!(out.len(), 1);
        // The literal, not the constant: this is the name an MCP client sees, and it must match
        // what the client downloaded.
        assert_eq!(out[0]["result"]["serverInfo"]["name"], "rill-wallet");
    }

    #[test]
    fn a_notification_produces_no_line_at_all() {
        let out = drive(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        assert!(
            out.is_empty(),
            "answering a notification is a spec violation some clients hang on"
        );
    }

    #[test]
    fn a_request_missing_its_id_is_reported_rather_than_swallowed() {
        let out = drive(r#"{"jsonrpc":"2.0","method":"tools/list"}"#);
        assert_eq!(out[0]["error"]["code"], -32600);
    }

    #[test]
    fn a_malformed_line_is_a_parse_error_and_does_not_stop_the_loop() {
        let out = drive("{ not json\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}");
        assert_eq!(out[0]["error"]["code"], -32700);
        assert_eq!(out[1]["id"], 2, "the transport must survive one bad line");
    }

    #[test]
    fn blank_lines_are_skipped() {
        let out = drive("\n\n{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"ping\"}\n\n");
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn the_wallet_surface_is_advertised_with_annotations() {
        let out = drive(r#"{"jsonrpc":"2.0","id":4,"method":"tools/list"}"#);
        let tools = out[0]["result"]["tools"].as_array().unwrap();
        let execute = tools
            .iter()
            .find(|t| t["name"] == "rill_execute")
            .expect("the wallet must offer execution");
        assert_eq!(
            execute["annotations"]["destructiveHint"], true,
            "the one tool that submits must say so"
        );
    }

    #[test]
    fn status_without_a_key_says_so_plainly() {
        let out = drive(
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"rill_status","arguments":{}}}"#,
        );
        assert_eq!(out[0]["result"]["structuredContent"]["ready"], false);
        assert!(out[0]["result"]["structuredContent"]["reason"]
            .as_str()
            .unwrap()
            .contains("RILL_SUI_PRIVATE_KEY"));
    }

    #[test]
    fn status_with_a_key_reports_the_address_and_nothing_secret() {
        use sui_crypto::ed25519::Ed25519PrivateKey;
        let encoded = Ed25519PrivateKey::new([5u8; 32]).to_suiprivkey().unwrap();
        let keystore = Keystore::from_suiprivkey(&encoded).unwrap();
        let expected = keystore.address().to_string();

        let mut ctx = WalletContext::new(Some(keystore), "testnet".into(), false);
        let mut out = Vec::new();
        serve(
            &mut ctx,
            r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"rill_status","arguments":{}}}"#.as_bytes(),
            &mut out,
        )
        .unwrap();
        let rendered = String::from_utf8(out).unwrap();
        assert!(rendered.contains(&expected));
        assert!(
            !rendered.contains("suiprivkey"),
            "the key must never reach the wire"
        );
    }

    /// Refusing to sign is a state worth reporting, so the refusal is remembered.
    #[test]
    fn a_refusal_is_remembered_and_can_be_explained() {
        let mut ctx = context();
        let mut out = Vec::new();
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"rill_execute","arguments":{"envelope":{}}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"rill_status","arguments":{}}}"#
        );
        serve(&mut ctx, input.as_bytes(), &mut out).unwrap();
        let lines: Vec<Value> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines[0]["result"]["isError"], true);
        assert_eq!(
            lines[0]["result"]["structuredContent"]["code"],
            "no_run_set"
        );
        assert!(
            lines[1]["result"]["structuredContent"]["lastRejection"]
                .as_str()
                .unwrap()
                .to_lowercase()
                .contains("run-set"),
            "the refusal must survive into explain_rejection"
        );
    }

    #[test]
    fn an_unknown_tool_is_refused() {
        let out = drive(
            r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"rill_do_anything","arguments":{}}}"#,
        );
        assert_eq!(out[0]["error"]["code"], -32602);
    }
}

#[cfg(test)]
mod execution_tests {
    use super::*;
    use crate::runset::RunSet;

    fn run_set() -> RunSet {
        serde_json::from_value(serde_json::json!({
            "label": "hero-testnet",
            "network": "testnet",
            "sender": WALLET,
            "actionId": "skill_hero",
            "walletPackageId": PKG,
            "walletId": WALLET,
            "agentCapId": "0xcap",
            "versionId": "0xversion",
            "capabilityManifest": {
                "walletCoinType": "0x2::sui::SUI",
                "rules": [{ "kind": "budget", "totalMist": "5000000000" }]
            },
            "allowedTargets": [format!("{PKG}::agent_wallet::request_spend")],
            "allowedObjectIds": [WALLET],
            "maxAmountBaseUnits": "2000000000",
            "declaredSpendBaseUnits": "2000000000",
            "minimumRemainingBaseUnits": "0",
            "gasCeilingBaseUnits": "50000000"
        }))
        .unwrap()
    }

    fn context_with_run_set() -> WalletContext {
        use sui_crypto::ed25519::Ed25519PrivateKey;
        let encoded = Ed25519PrivateKey::new([11u8; 32]).to_suiprivkey().unwrap();
        WalletContext::new(
            Some(Keystore::from_suiprivkey(&encoded).unwrap()),
            "testnet".into(),
            false,
        )
        .with_run_set(Some(run_set()))
    }

    /// The package and wallet the fixture's transaction really calls.
    const PKG: &str = "0x000000000000000000000000000000000000000000000000000000000000cafe";
    const WALLET: &str = "0x0000000000000000000000000000000000000000000000000000000000000001";

    /// A real transaction, built the way the server builds one.
    ///
    /// This used to be `"AAA="` — three zero bytes, which is not a transaction. Every local check
    /// passed on it, because nothing read the bytes. Once `pin_bytes` started decoding them, the
    /// fixture failed, which is the whole point: the signer's own tests were exercising a signer
    /// that never looked at what it was about to sign.
    fn real_ptb() -> String {
        use sui_sdk_types::{Address, Digest, Identifier};
        use sui_transaction_builder::{Function, ObjectInput, TransactionBuilder};

        let mut tx = TransactionBuilder::new();
        tx.set_sender(WALLET.parse::<Address>().unwrap());
        tx.set_gas_budget(50_000_000);
        tx.set_gas_price(1_000);
        tx.add_gas_objects([ObjectInput::owned(
            "0x000000000000000000000000000000000000000000000000000000000000000a"
                .parse()
                .unwrap(),
            1,
            Digest::ZERO,
        )]);
        let wallet = tx.object(ObjectInput::shared(WALLET.parse().unwrap(), 400_001, true));
        tx.move_call(
            Function::new(
                PKG.parse().unwrap(),
                Identifier::new("agent_wallet").unwrap(),
                Identifier::new("request_spend").unwrap(),
            ),
            vec![wallet],
        );
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .encode(bcs::to_bytes(&tx.try_build().unwrap()).unwrap())
    }

    fn envelope(overrides: serde_json::Value) -> Value {
        let mut base = json!({
            "version": "1",
            "actionId": "skill_hero",
            "actionDigest": rill_core::envelope::digest_unsigned_ptb(&real_ptb()),
            "network": "testnet",
            "sender": WALLET,
            "walletPackageId": PKG,
            "walletId": WALLET,
            "agentCapId": "0xcap",
            "balanceManagerId": "0xbm",
            "tradeCapId": "0xtc",
            "resolvedParams": {
                "poolKey": "DEEP_SUI", "poolId": "0xpool", "clientOrderId": "1",
                "spendAmountMist": "1000000000", "price": "2.5", "quantity": "1",
                "depositSui": "1", "isBid": true, "payWithDeep": false
            },
            "allowedTargets": [format!("{PKG}::agent_wallet::request_spend")],
            "requiredObjectIds": ["0xwallet"],
            "requiredGuards": [],
            "unsignedPtb": real_ptb(),
            "preview": "place a limit order",
            "simulation": {
                "ok": true, "verification": "verified", "gasEstimate": "2000000",
                "balanceChanges": [], "objectChanges": []
            },
            "expiresAt": far_future()
        });
        if let Some(map) = overrides.as_object() {
            for (k, v) in map {
                base[k] = v.clone();
            }
        }
        base
    }

    fn far_future() -> String {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 60_000;
        let secs = ms / 1000;
        let days = (secs / 86_400) as i64;
        let rem = secs % 86_400;
        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = (z - era * 146_097) as u64;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
        let y = yoe as i64 + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        let y = if m <= 2 { y + 1 } else { y };
        format!(
            "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
            rem / 3600,
            (rem / 60) % 60,
            rem % 60,
            ms % 1000
        )
    }

    fn execute_with(ctx: &mut WalletContext, envelope: Value) -> Value {
        let message = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": "rill_execute", "arguments": { "envelope": envelope } }
        });
        handle(ctx, &message).expect("a request gets a reply")
    }

    /// A good envelope reaches the chain step, which is as far as a test without a node can see.
    ///
    /// The assertion is on WHICH step failed, not that nothing did. Every local check — freshness,
    /// network, identity, ceilings, the byte pin, the decoded target sequence and object scope —
    /// happens before a node is contacted, so a failure code from the chain step proves all of
    /// them passed. Two codes come from that step and nothing earlier: with a node reachable it
    /// answers that the fixture's objects do not exist, which is `object_changed`; without one it
    /// is `chain_unavailable`. (The first used to be reported as the second, because a definite
    /// refusal from the node was mapped as a transport failure.) A test that only asserted
    /// `isError == true` would pass just as happily if the envelope had been rejected on its
    /// first field.
    #[test]
    fn a_good_envelope_passes_every_local_check_and_reaches_the_chain() {
        let mut ctx = context_with_run_set();
        let out = execute_with(&mut ctx, envelope(json!({})));
        let code = out["result"]["structuredContent"]["code"]
            .as_str()
            .unwrap_or("(none)");
        assert!(
            matches!(code, "chain_unavailable" | "object_changed"),
            "a good envelope must get past every local check; it stopped at {code}: {out}"
        );
    }

    #[test]
    fn an_envelope_for_another_action_is_refused_by_name() {
        let mut ctx = context_with_run_set();
        let out = execute_with(&mut ctx, envelope(json!({ "actionId": "skill_other" })));
        assert_eq!(out["result"]["isError"], true);
        let message = out["result"]["structuredContent"]["message"]
            .as_str()
            .unwrap();
        assert!(message.contains("skill_other"), "{message}");
    }

    /// The gate with no override anywhere in this workspace.
    #[test]
    fn an_unverified_simulation_is_refused_even_with_a_run_set_loaded() {
        let mut ctx = context_with_run_set();
        let mut env = envelope(json!({}));
        env["simulation"]["verification"] = json!("unverified");
        let out = execute_with(&mut ctx, env);
        assert_eq!(out["result"]["isError"], true);
        assert!(out["result"]["structuredContent"]["message"]
            .as_str()
            .unwrap()
            .contains("inconclusive"));
    }

    #[test]
    fn a_spend_above_the_run_sets_ceiling_is_refused() {
        let mut ctx = context_with_run_set();
        let mut env = envelope(json!({}));
        env["resolvedParams"]["spendAmountMist"] = json!("9000000000");
        let out = execute_with(&mut ctx, env);
        assert_eq!(out["result"]["isError"], true);
    }

    #[test]
    fn a_digest_that_does_not_describe_the_bytes_is_refused() {
        let mut ctx = context_with_run_set();
        let out = execute_with(
            &mut ctx,
            envelope(json!({ "actionDigest": "00".repeat(32) })),
        );
        assert_eq!(out["result"]["isError"], true);
    }

    /// Every refusal is remembered, so an operator can ask what happened without re-running it.
    #[test]
    fn a_refusal_is_recoverable_through_explain_rejection() {
        let mut ctx = context_with_run_set();
        execute_with(&mut ctx, envelope(json!({ "actionId": "skill_other" })));
        let out = handle(
            &mut ctx,
            &json!({
                "jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": { "name": "rill_status", "arguments": {} }
            }),
        )
        .unwrap();
        assert!(out["result"]["structuredContent"]["lastRejection"]
            .as_str()
            .unwrap()
            .contains("skill_other"));
    }

    #[test]
    fn capabilities_report_which_layer_holds_each_limit() {
        let mut ctx = context_with_run_set();
        let out = handle(
            &mut ctx,
            &json!({
                "jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": { "name": "rill_status", "arguments": {} }
            }),
        )
        .unwrap();
        let caps = &out["result"]["structuredContent"]["runSet"]["declaration"]["caps"];
        assert_eq!(caps[0]["enforcement"], "on-chain");
    }
}

/// The wallet read merges what the chain reports with what the run-set carries, and labels each
/// rule with the layer that holds it. The labelling is driven directly here, with fixed inputs,
/// because the chain half of the read needs a fullnode; the one test that needs it is ignored.
#[cfg(test)]
mod wallet_read_tests {
    use super::*;
    use crate::wallet_read::label_rules;

    /// A run-set whose manifest carries one rule of each layer.
    fn run_set_with_a_pre_flight_rule() -> RunSet {
        serde_json::from_value(json!({
            "label": "scoped-testnet",
            "network": "testnet",
            "sender": "0x0000000000000000000000000000000000000000000000000000000000000001",
            "actionId": "skill_hero",
            "walletPackageId": "0xcafe",
            "walletId": "0x0000000000000000000000000000000000000000000000000000000000000001",
            "agentCapId": "0xcap",
            "versionId": "0xversion",
            "capabilityManifest": {
                "walletCoinType": "0x2::sui::SUI",
                "rules": [
                    { "kind": "budget", "totalMist": "5000000000" },
                    { "kind": "recipient_allowlist", "addresses": ["0x1"] }
                ]
            },
            "allowedTargets": ["0xcafe::agent_wallet::request_spend"],
            "allowedObjectIds": ["0x1"],
            "maxAmountBaseUnits": "2000000000",
            "declaredSpendBaseUnits": "2000000000",
            "minimumRemainingBaseUnits": "0",
            "gasCeilingBaseUnits": "50000000"
        }))
        .unwrap()
    }

    /// What the chain would report for a wallet carrying budget and per_tx.
    fn chain_reported() -> Vec<String> {
        vec![
            "0xcafe::budget::Rule".to_string(),
            "0xcafe::per_tx::Rule".to_string(),
        ]
    }

    #[test]
    fn a_pre_flight_rule_in_the_loaded_run_set_is_labelled_pre_flight() {
        let run_set = run_set_with_a_pre_flight_rule();
        let out = label_rules(&chain_reported(), Some(&run_set.capability_manifest));
        let pre_flight = out["preFlightRules"].as_array().unwrap();
        assert_eq!(
            pre_flight.len(),
            1,
            "budget is the chain's to report: {out}"
        );
        assert_eq!(pre_flight[0]["module"], "recipient_allowlist");
        assert_eq!(pre_flight[0]["enforcement"], "pre-flight");
        assert_eq!(pre_flight[0]["enforcedBy"], "the signer, before it signs");
        assert!(
            !out["rules"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["module"] == "recipient_allowlist"),
            "a pre-flight rule must not be listed among the rules the chain holds"
        );
    }

    #[test]
    fn a_rule_read_from_chain_is_labelled_on_chain() {
        let run_set = run_set_with_a_pre_flight_rule();
        let out = label_rules(&chain_reported(), Some(&run_set.capability_manifest));
        let rules = out["rules"].as_array().unwrap();
        let labels: Vec<(&str, &str)> = rules
            .iter()
            .map(|r| {
                (
                    r["module"].as_str().unwrap(),
                    r["enforcement"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(labels, vec![("budget", "on-chain"), ("per_tx", "on-chain")]);
    }

    /// The whole tool against the wallet `docs/OVERNIGHT.md` records, through the transport. Needs
    /// a testnet fullnode, so: `cargo test -p rill wallet_read_tests -- --ignored --nocapture`.
    #[test]
    #[ignore = "reads a live testnet wallet"]
    fn rill_wallet_reads_the_recorded_testnet_wallet_and_labels_every_rule() {
        let mut ctx = WalletContext::new(None, "testnet".into(), false)
            .with_run_set(Some(run_set_with_a_pre_flight_rule()));
        let out = handle(
            &mut ctx,
            &json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": "rill_wallet", "arguments": {
                    "wallet": "0x20391fa91aec7a12b6657902af80036e125d1beff6621fe2eb73cfd032a04e5d"
                } }
            }),
        )
        .unwrap();
        eprintln!("{}", serde_json::to_string_pretty(&out).unwrap());
        let limits = &out["result"]["structuredContent"];
        assert_eq!(out["result"]["isError"], false, "{out}");
        let modules: Vec<&str> = limits["rules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["module"].as_str().unwrap())
            .collect();
        assert!(modules.contains(&"budget") && modules.contains(&"per_tx"));
        for rule in limits["rules"].as_array().unwrap() {
            assert_eq!(rule["enforcement"], "on-chain");
        }
        assert_eq!(limits["preFlightRules"][0]["enforcement"], "pre-flight");
    }
}
