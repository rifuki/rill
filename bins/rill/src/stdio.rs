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
    /// Whether the owner's two steps are offered: `rill mcp --owner`. Off for an agent's launch,
    /// which is what a plugin starts, so an agent is never handed a tool that would make it the
    /// owner of a wallet it funds itself.
    pub owner_tools: bool,
    /// Gas references consumed by successful submissions in this process. Builder indexes may lag.
    pub consumed_gas: HashMap<sui_sdk_types::Address, u64>,
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
            owner_tools: false,
            consumed_gas: HashMap::new(),
        }
    }

    /// Offer the owner's steps as well: `rill_create_wallet` and `rill_attach_rules`.
    pub fn with_owner_tools(mut self, owner_tools: bool) -> Self {
        self.owner_tools = owner_tools;
        self
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
            let surface = if context.owner_tools {
                rill_mcp::Surface::Owner
            } else {
                rill_mcp::Surface::Wallet
            };
            let mut tools: Vec<Value> = rill_mcp::tools(surface)
                .into_iter()
                .map(|t| serde_json::to_value(t).unwrap_or(Value::Null))
                .collect();
            tools.push(json!({"name":"rill_pair","description":"Prove this local signer to an owner-created pairing request. Gives no spend authority; owner confirms in Studio.","inputSchema":{"type":"object","properties":{"requestId":{"type":"string"}},"required":["requestId"],"additionalProperties":false}}));
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

    // Not offered on an agent's launch, and not reachable by name either: a tool that is merely
    // unlisted is one a client that cached an older list can still call.
    if !context.owner_tools && rill_mcp::OWNER_TOOLS.contains(&name) {
        return tool_error(
            id,
            "owner_only",
            &format!(
                "{name} is an owner's step, and this signer was started for an agent. The wallet's \
                 owner creates it, sets its limits and funds it from their own wallet: in Rill \
                 Studio, or with `rill-wallet mcp --owner` run under the owner's key. Ask the \
                 owner; do not try another route."
            ),
        );
    }

    // A named pair is checked here, before any chain read, so a testnet id on mainnet is refused by
    // name. With none named, the published mainnet pair and guard are the defaults.
    if context.network == "mainnet"
        && !matches!(
            name,
            "rill_status" | "rill_execute" | "rill_pair" | "rill_portfolio" | "rill_unstake"
        )
    {
        let package = std::env::var("AGENT_WALLET_PACKAGE_ID").ok();
        let version = std::env::var("AGENT_WALLET_VERSION_ID").ok();
        if let Err(reason) = rill_ptb::deployments::wallet_deployment(
            rill_core::envelope::Network::Mainnet,
            package.as_deref(),
            version.as_deref(),
        ) {
            return tool_error(id, "not_configured", &reason);
        }
    }
    match name {
        "rill_status" => status(context, id),
        "rill_pair" => pair_signer(context, id, &params),
        "rill_wallet" => wallet(context, id, &params),
        "rill_quote" => quote(context, id, &params),
        "rill_create_wallet" => create_wallet(context, id, &params),
        "rill_attach_rules" => attach_rules(context, id, &params),
        "rill_spend" => spend(context, id, &params),
        "rill_swap" => swap(context, id, &params),
        "rill_stake" => stake(context, id, &params),
        "rill_portfolio" => portfolio(context, id, &params),
        "rill_unstake" => unstake(context, id, &params),
        "rill_execute" => execute(context, id, &params),
        "rill_actions" => actions(context, id),
        "rill_run_action" => run_action(context, id, &params),
        "rill_run_workflow" => run_workflow(context, id, &params),
        other => rpc_error(id, -32602, &format!("Unknown tool: {other}")),
    }
}

fn pair_signer(context: &WalletContext, id: Value, params: &Value) -> Value {
    let Some(api) = api_url() else {
        return tool_error(id, "not_configured", NO_API);
    };
    let Some(key) = context.keystore.as_ref() else {
        return tool_error(id, "no_key", "No signing key is configured.");
    };
    let Some(request) = argument(params, "requestId") else {
        return tool_error(id, "invalid_arguments", "requestId is required.");
    };
    match block_on(crate::pair_cmd::pair(key, &api, request, &context.network)) {
        Ok(Ok(report)) => tool_ok(id, report),
        Ok(Err(error)) | Err(error) => tool_error(id, "pairing_refused", &error),
    }
}

/// What a swap would return, and the floor to ask for.
///
/// A read: no key, no transaction, nothing submitted. It exists because `rill_swap` requires
/// `minOut` and a caller with no price source would otherwise have to guess it or escape it.
fn quote(context: &mut WalletContext, id: Value, params: &Value) -> Value {
    let pool = match argument(params, "pool") {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => {
            return tool_error(
                id,
                "bad_request",
                "pool is required: the Cetus pool object id to quote against.",
            )
        }
    };
    let amount = match argument(params, "amount") {
        Some(a) if !a.is_empty() => a.to_string(),
        _ => {
            return tool_error(
                id,
                "bad_request",
                "amount is required: decimal SUI to be released and swapped, as text.",
            )
        }
    };
    // One percent unless asked otherwise. A default is right here and would be wrong for `minOut`
    // itself: a default floor would be a number nobody chose applied to a real trade, whereas a
    // default tolerance is a starting point the caller can see in the result and widen.
    let slippage_bps = match argument(params, "slippageBps") {
        Some(raw) if !raw.is_empty() => match raw.parse::<u64>() {
            Ok(v) => v,
            Err(_) => {
                return tool_error(
                    id,
                    "bad_request",
                    &format!(
                        "slippageBps must be a whole number of basis points, as text: {raw:?}"
                    ),
                )
            }
        },
        _ => 100,
    };

    // The signer's own key is the sender of the simulated transaction, so a quote needs one for the
    // same reason a swap does: the gated spend it simulates is the agent's.
    let Some(keystore) = context.keystore.as_ref() else {
        let reason = "No signing key is configured, so the gated swap a quote simulates has no \
                      sender."
            .to_string();
        context.last_rejection = Some(reason.clone());
        return tool_error(id, "no_key", &reason);
    };
    let Some(wallet) = argument(params, "wallet").filter(|w| !w.is_empty()) else {
        return tool_error(
            id,
            "bad_request",
            "wallet is required: a quote simulates the gated spend, so it needs the wallet that \
             would fund it.",
        );
    };
    let Some(cap) = argument(params, "cap").filter(|c| !c.is_empty()) else {
        return tool_error(
            id,
            "bad_request",
            "cap is required: the AgentCap this signer holds for that wallet.",
        );
    };

    let args = crate::quote_cmd::QuoteArgs {
        package_id: package_id(),
        version_id: version_id(),
        wallet_id: wallet.to_string(),
        cap_id: cap.to_string(),
        // Cetus's own ids, defaulted for the same reason the wallet package is: an agent given a
        // pool id cannot invent them, and a caller that had to supply them could supply the wrong
        // ones.
        integrate_package_id: cetus_integrate(context),
        global_config_id: cetus_global_config(context),
        pool_id: pool,
        spend: amount,
        slippage_bps,
        gas_budget: TOOL_GAS_BUDGET,
    };
    match block_on(crate::quote_cmd::quote_json(
        &endpoint(context),
        keystore,
        &args,
    )) {
        Ok(Ok(result)) => tool_ok(id, result),
        Ok(Err(e)) | Err(e) => {
            context.last_rejection = Some(e.clone());
            tool_error(id, "quote_failed", &e)
        }
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
    std::env::var("AGENT_WALLET_PACKAGE_ID").unwrap_or_else(|_| {
        if on_mainnet() {
            rill_ptb::deployments::MAINNET_AGENT_WALLET.to_string()
        } else {
            rill_ptb::deployments::TESTNET_AGENT_WALLET.to_string()
        }
    })
}

fn version_id() -> String {
    std::env::var("AGENT_WALLET_VERSION_ID").unwrap_or_else(|_| {
        if on_mainnet() {
            rill_ptb::deployments::MAINNET_AGENT_WALLET_VERSION.to_string()
        } else {
            rill_ptb::deployments::TESTNET_AGENT_WALLET_VERSION.to_string()
        }
    })
}

/// The network this process was started for, read where every deployment default is chosen so a
/// mainnet signer can never pick up a testnet id by falling through.
fn on_mainnet() -> bool {
    std::env::var("SUI_NETWORK").as_deref() == Ok("mainnet")
}

/// The deployed `rill_guard` package, which carries the slippage floor every swap passes through.
///
/// Defaulted like the pair above rather than asked for: a caller that had to supply it could supply
/// nothing, and a swap with no floor is exactly the outcome the floor exists to prevent.
/// Cetus's router package, where `router::swap` lives.
fn cetus_integrate(context: &WalletContext) -> String {
    std::env::var("CETUS_INTEGRATE_PACKAGE_ID").unwrap_or_else(|_| {
        if context.network == "mainnet" {
            rill_ptb::deployments::MAINNET_CETUS_INTEGRATE.into()
        } else {
            rill_ptb::deployments::TESTNET_CETUS_INTEGRATE.to_string()
        }
    })
}

/// Cetus's `GlobalConfig`, which every swap reads.
fn cetus_global_config(context: &WalletContext) -> String {
    std::env::var("CETUS_GLOBAL_CONFIG_ID").unwrap_or_else(|_| {
        if context.network == "mainnet" {
            rill_ptb::deployments::MAINNET_CETUS_GLOBAL_CONFIG.into()
        } else {
            rill_ptb::deployments::TESTNET_CETUS_GLOBAL_CONFIG.to_string()
        }
    })
}

fn guard_package_id() -> String {
    std::env::var("RILL_GUARD_PACKAGE_ID").unwrap_or_else(|_| {
        if on_mainnet() {
            rill_ptb::deployments::MAINNET_RILL_GUARD.to_string()
        } else {
            rill_ptb::deployments::TESTNET_RILL_GUARD.to_string()
        }
    })
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
    let package = package_id();

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

    // Cetus's two ids are no longer required: they are the same on every call against this network,
    // an agent cannot invent them, and a caller forced to supply them could supply the wrong ones. A
    // caller that does pass them still wins, which is what a non-testnet deployment needs.
    let required = ["wallet", "cap", "amount", "pool", "coinTypeA", "coinTypeB"];
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
        integrate_package_id: match get("integratePackage") {
            given if !given.is_empty() => given,
            _ => cetus_integrate(context),
        },
        global_config_id: match get("globalConfig") {
            given if !given.is_empty() => given,
            _ => cetus_global_config(context),
        },
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

fn portfolio(context: &WalletContext, id: Value, params: &Value) -> Value {
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if arguments
        .as_object()
        .is_none_or(|o| o.keys().any(|k| k != "owner"))
    {
        return tool_error(
            id,
            "bad_request",
            "portfolio accepts only an optional owner address",
        );
    }
    let owner = if let Some(value) = arguments.get("owner") {
        let Some(owner) = value.as_str() else {
            return tool_error(id, "bad_request", "owner must be a string");
        };
        owner.to_owned()
    } else if let Some(key) = context.keystore.as_ref() {
        key.address().to_string()
    } else {
        return tool_error(
            id,
            "bad_request",
            "owner address required when no signer is configured",
        );
    };
    match block_on(crate::portfolio_cmd::portfolio_json(
        &endpoint(context),
        &owner,
    )) {
        Ok(Ok(report)) => tool_ok(id, report),
        Ok(Err(e)) | Err(e) => tool_error(id, "portfolio_failed", &e),
    }
}

fn unstake(context: &mut WalletContext, id: Value, params: &Value) -> Value {
    let Some(key) = context.keystore.as_ref() else {
        return tool_error(id, "no_key", "No signing key configured.");
    };
    if context.network != "mainnet" {
        return tool_error(
            id,
            "bad_request",
            "Haedal redemption currently supports mainnet only.",
        );
    }
    if !context.mainnet_allowed {
        return tool_error(
            id,
            "mainnet_not_opted_in",
            &rill_core::mainnet::mainnet_refusal(),
        );
    }
    let Some(arguments) = params.get("arguments").and_then(Value::as_object) else {
        return tool_error(id, "bad_request", "arguments must be an object");
    };
    if arguments
        .keys()
        .any(|k| !["amount", "minOut", "receiver", "dryRun"].contains(&k.as_str()))
    {
        return tool_error(id, "bad_request", "unknown unstake argument");
    }
    let (Some(amount), Some(min_out)) = (argument(params, "amount"), argument(params, "minOut"))
    else {
        return tool_error(
            id,
            "bad_request",
            "amount and minOut must be decimal strings",
        );
    };
    if arguments.get("receiver").is_some_and(|v| !v.is_string())
        || arguments.get("dryRun").is_some_and(|v| !v.is_boolean())
    {
        return tool_error(
            id,
            "bad_request",
            "receiver must be a string and dryRun a boolean",
        );
    }
    let args = crate::unstake_cmd::UnstakeArgs {
        package_id: crate::unstake_cmd::MAINNET_HAEDAL.into(),
        staking_object_id: crate::unstake_cmd::MAINNET_STAKING.into(),
        coin_type: crate::unstake_cmd::MAINNET_HASUI.into(),
        guard_package_id: guard_package_id(),
        amount: amount.into(),
        min_out: min_out.into(),
        receiver: argument(params, "receiver")
            .map(str::to_owned)
            .unwrap_or_else(|| key.address().to_string()),
        gas_budget: TOOL_GAS_BUDGET,
        dry_run: arguments
            .get("dryRun")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    };
    match block_on(crate::unstake_cmd::unstake_json(
        &endpoint(context),
        key,
        &args,
    )) {
        Ok(Ok(report)) => tool_ok(id, report),
        Ok(Err(failure)) => failure_response(context, id, "unstake_failed", &failure),
        Err(e) => failure_response(context, id, "unstake_failed", &Failure::Failed(e)),
    }
}

/// A Haedal liquid stake the wallet pays for, under its own rules.
fn stake(context: &mut WalletContext, id: Value, params: &Value) -> Value {
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
    let missing: Vec<&str> = ["wallet", "cap", "amount"]
        .into_iter()
        .filter(|name| argument(params, name).is_none_or(str::is_empty))
        .collect();
    if !missing.is_empty() {
        return tool_error(
            id,
            "bad_request",
            &format!("missing: {}", missing.join(", ")),
        );
    }
    let get = |name: &str| argument(params, name).unwrap_or_default().to_string();
    let validator = match get("validator") {
        v if v.is_empty() => "0x0".to_string(),
        v => v,
    };
    let args = crate::stake_cmd::StakeArgs {
        package_id: package_id(),
        version_id: version_id(),
        wallet_id: get("wallet"),
        cap_id: get("cap"),
        haedal_package_id: std::env::var("HAEDAL_PACKAGE_ID").unwrap_or_else(|_| {
            if context.network == "mainnet" {
                "0x126e4cfb051cad744706df590ec399e8c02b6feae195c35b8b496280d5442a62".into()
            } else {
                rill_ptb::deployments::TESTNET_HAEDAL_PACKAGE.to_string()
            }
        }),
        staking_object_id: std::env::var("HAEDAL_STAKING_ID").unwrap_or_else(|_| {
            if context.network == "mainnet" {
                "0x47b224762220393057ebf4f70501b6e657c3e56684737568439a04f80849b2ca".into()
            } else {
                rill_ptb::deployments::TESTNET_HAEDAL_STAKING.to_string()
            }
        }),
        validator,
        spend: get("amount"),
        gas_budget: TOOL_GAS_BUDGET,
        dry_run: false,
    };
    match block_on(crate::stake_cmd::stake_json(
        &endpoint(context),
        keystore,
        &args,
    )) {
        Ok(Ok(result)) => tool_ok(id, result),
        Ok(Err(failure)) => failure_response(context, id, "stake_failed", &failure),
        Err(e) => failure_response(context, id, "stake_failed", &Failure::Failed(e)),
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

    // Create and attach in one runtime over one client: see `create_and_bound_json_on`. This tool
    // required budget and perTx and used to leave the wallet with no rules at all, so a caller who
    // had just passed both limits held an unbounded cap. Found by driving it for a stake, which the
    // no-rules guard then refused.
    let outcome = block_on(crate::wallet::create_and_bound_json(
        &endpoint(context),
        keystore,
        &args,
        now_ms(),
    ));
    match outcome {
        Ok(Ok(crate::wallet::Bounded::Yes { report })) => tool_ok(id, report),
        // Answered as an error, because an agent that reads a success here hands over a cap with no
        // limit on it. The id is in the message: without it the funded wallet cannot be found.
        Ok(Ok(crate::wallet::Bounded::CreatedButUnbounded {
            wallet_id,
            created,
            why,
        })) => {
            tool_error(
                id,
                "created_but_unbounded",
                &format!(
                "The wallet {} was created EMPTY, and the transaction that attaches its rules and \
                 adds its funding did not run, so it holds nothing and has no rules: UNBOUNDED, and \
                 unspendable until it has them. Do not create \
                 another wallet. Call rill_attach_rules with that wallet, budget {budget}, perTx \
                 {per_tx} and amount {amount}: it attaches the rules and funds the wallet in one \
                 transaction. Why: {why}\n\nWhat was created: {created}",
                wallet_id.as_deref().unwrap_or("(id not readable, see below)")
            ),
            )
        }
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

    // With an amount, the first funding goes in with the rules, in one owner-signed transaction:
    // the way an empty wallet from rill_create_wallet is finished when that tool's own attach
    // failed. Without one, this only attaches, and an empty wallet stays empty.
    let funding = match argument(params, "amount") {
        None => None,
        Some(amount) => match rill_core::amounts::decimal_to_base_units(amount, 9) {
            Ok(mist) => Some(mist),
            Err(e) => {
                return tool_error(
                    id,
                    "invalid_arguments",
                    &format!("amount is decimal SUI as text: {e}"),
                )
            }
        },
    };
    let endpoint = endpoint(context);
    let result = block_on(async {
        match funding {
            Some(mist) => {
                crate::rules_cmd::attach_and_fund_json(&endpoint, keystore, &args, mist).await
            }
            None => crate::rules_cmd::attach_json(&endpoint, keystore, &args).await,
        }
    });
    match result {
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

/// The Rill API this signer reads grants from and builds actions with.
fn api_url() -> Option<String> {
    std::env::var(crate::config::API_URL_VAR)
        .ok()
        .map(|url| url.trim().trim_end_matches('/').to_owned())
        .filter(|url| !url.is_empty())
}

const NO_API: &str = "No Rill API is configured, so there are no granted actions to read. Run \
                      `~/.rill/bin/rill-wallet setup --api <url>` and restart the client.";

/// One fetched grant and what the chain said about it.
type CheckedGrant = (
    rill_core::grant::SignedGrant,
    Result<crate::grants::VerifiedGrant, crate::grants::Refusal>,
);

/// Every grant for this signer's address, each checked against the chain. A grant that fails a
/// check is kept with the reason, so `rill_actions` can say why an action is not available.
fn checked_grants(
    context: &WalletContext,
    signer: &str,
    api: &str,
) -> Result<Vec<CheckedGrant>, String> {
    let network = context.network.clone();
    let endpoint = endpoint(context);
    let package = package_id();
    let url = format!("{api}/api/grants/{signer}");
    block_on(async move {
        let response = crate::http::get_json(&url).await?;
        let list = response
            .get("data")
            .unwrap_or(&response)
            .get("grants")
            .cloned()
            .unwrap_or(Value::Array(Vec::new()));
        let grants: Vec<rill_core::grant::SignedGrant> = serde_json::from_value(list)
            .map_err(|e| format!("the grant list did not parse: {e}"))?;
        let chain = rill_chain::grpc::GrpcSui::new(&endpoint).map_err(|e| e.to_string())?;
        let mut checked = Vec::with_capacity(grants.len());
        for grant in grants {
            let verdict =
                crate::grants::verify(&chain, &grant, signer, &network, &package, now_ms()).await;
            checked.push((grant, verdict));
        }
        Ok(checked)
    })?
}

/// `rill_actions`: what this agent may run, and for each grant that cannot be used, why.
fn actions(context: &mut WalletContext, id: Value) -> Value {
    let Some(api) = api_url() else {
        return tool_error(id, "not_configured", NO_API);
    };
    let Some(keystore) = context.keystore.as_ref() else {
        return tool_error(id, "no_key", "No signing key is configured.");
    };
    let signer = keystore.address().to_string();
    let checked = match checked_grants(context, &signer, &api) {
        Ok(checked) => checked,
        Err(e) => return tool_error(id, "grants_unavailable", &e),
    };
    let actions: Vec<Value> = checked
        .iter()
        .map(|(signed, verdict)| {
            let grant = &signed.grant;
            let mut entry = json!({
                "actionId": grant.action_id,
                "name": grant.action_name,
                "walletId": grant.wallet_id,
                "revision": grant.revision,
                "expiresAtMs": grant.expires_at_ms,
                "maxPerTransactionBaseUnits": grant.run_set.get("maxAmountBaseUnits"),
                "usable": verdict.is_ok(),
            });
            match verdict {
                Ok(verified) => entry["owner"] = json!(verified.owner),
                Err(refusal) => entry["refusedBecause"] = json!(refusal.to_string()),
            }
            entry
        })
        .collect();
    tool_ok(
        id,
        json!({
            "signer": signer,
            "actions": actions,
            "note": "Each usable action was granted by the wallet's owner and checked against the \
                     chain just now. Run one with rill_run_action and its actionId. The grant \
                     stops this signer signing anything the owner did not approve; what any \
                     transaction can actually spend is still bounded by the wallet's on-chain rules.",
        }),
    )
}

/// `rill_run_action`: build an action through the Rill API and execute it under the owner-signed
/// run set, through exactly the validation `rill_execute` applies to a run set loaded from a file.
fn run_action(context: &mut WalletContext, id: Value, params: &Value) -> Value {
    run_action_for_owner(context, id, params, None)
}

fn run_action_for_owner(
    context: &mut WalletContext,
    id: Value,
    params: &Value,
    owner: Option<&str>,
) -> Value {
    let Some(api) = api_url() else {
        return tool_error(id, "not_configured", NO_API);
    };
    let Some(keystore) = context.keystore.as_ref() else {
        return tool_error(id, "no_key", "No signing key is configured.");
    };
    let Some(action_id) = argument(params, "actionId") else {
        return tool_error(
            id,
            "invalid_arguments",
            "actionId is required; rill_actions lists them.",
        );
    };
    let action_id = action_id.to_owned();
    let wallet = argument(params, "walletId").map(str::to_owned);
    let revision_value = params.get("arguments").and_then(|a| a.get("revision"));
    let revision = revision_value.and_then(Value::as_u64);
    if revision_value.is_some() && !revision.is_some_and(|revision| revision > 0) {
        return tool_error(
            id,
            "invalid_arguments",
            "revision must be a positive integer.",
        );
    }
    let overrides = params
        .get("arguments")
        .and_then(|a| a.get("params"))
        .cloned();
    let signer = keystore.address().to_string();

    let checked = match checked_grants(context, &signer, &api) {
        Ok(checked) => checked,
        Err(e) => return tool_error(id, "grants_unavailable", &e),
    };
    let matching: Vec<_> = checked
        .into_iter()
        .filter(|(signed, _)| signed.grant.action_id == action_id)
        .filter(|(signed, _)| revision.is_none_or(|revision| signed.grant.revision == revision))
        .filter(|(signed, _)| {
            wallet.as_deref().is_none_or(|w| {
                w.parse::<sui_sdk_types::Address>().ok()
                    == signed
                        .grant
                        .wallet_id
                        .parse::<sui_sdk_types::Address>()
                        .ok()
            })
        })
        .collect();
    if matching.is_empty() {
        let reason = format!(
            "No grant for {action_id} names this agent. The wallet's owner grants actions in Rill \
             Studio; rill_actions lists what is granted now."
        );
        context.last_rejection = Some(reason.clone());
        return tool_error(id, "not_granted", &reason);
    }
    let refusals: Vec<String> = matching
        .iter()
        .filter_map(|(_, verdict)| verdict.as_ref().err().map(ToString::to_string))
        .collect();
    let mut usable: Vec<crate::grants::VerifiedGrant> = matching
        .into_iter()
        .filter_map(|(_, verdict)| verdict.ok())
        .collect();
    let wallets: std::collections::BTreeSet<String> =
        usable.iter().map(|v| v.grant.wallet_id.clone()).collect();
    if usable.is_empty() {
        let reason = format!(
            "The grant for {action_id} cannot be used: {}. Ask the wallet's owner; do not look \
             for another route.",
            refusals.join("; ")
        );
        context.last_rejection = Some(reason.clone());
        return tool_error(id, "grant_refused", &reason);
    }
    if wallets.len() > 1 {
        return tool_error(
            id,
            "ambiguous_wallet",
            &format!(
                "{action_id} is granted from more than one wallet ({}). Pass walletId to choose.",
                wallets.into_iter().collect::<Vec<_>>().join(", ")
            ),
        );
    }
    usable.sort_by_key(|v| v.grant.revision);
    let verified = usable.pop().expect("one usable grant");
    if owner.is_some_and(|owner| {
        owner.parse::<sui_sdk_types::Address>().ok()
            != verified.owner.parse::<sui_sdk_types::Address>().ok()
    }) {
        return tool_error(id, "owner_changed", "The vault owner changed since workflow preflight; no transaction submitted for this step.");
    }

    // The builder is asked for the grant's own build arguments. Only `params` may be supplied by
    // the caller, and the server refuses any that would loosen what was published.
    let mut arguments = verified.grant.build_arguments.clone();
    if let Some(object) = arguments.as_object_mut() {
        object.remove("actionId");
    }
    if let Some(overrides) = overrides {
        let merged = arguments
            .get("params")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut merged = merged;
        if let Some(extra) = overrides.as_object() {
            for (node, values) in extra {
                let entry = merged.entry(node.clone()).or_insert_with(|| json!({}));
                if let (Some(entry), Some(values)) = (entry.as_object_mut(), values.as_object()) {
                    for (k, v) in values {
                        entry.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        arguments["params"] = Value::Object(merged);
    }
    let url = format!("{api}/api/mcp/{action_id}");
    let request = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "build_action", "arguments": arguments }
    });
    let consumed_gas = &context.consumed_gas;
    let built = match block_on(async {
        for attempt in 0..40 {
            let value = crate::http::post_json(&url, &request).await?;
            let text = value.get("data").unwrap_or(&value)["result"]["content"][0]["text"].as_str();
            let transaction = text
                .and_then(|text| serde_json::from_str::<Value>(text).ok())
                .and_then(|envelope| {
                    envelope["unsignedPtb"]
                        .as_str()
                        .and_then(|bytes| decode_for_signing(bytes).ok())
                });
            let stale = transaction.as_ref().is_some_and(|tx| {
                crate::workflow::gas_is_stale(&tx.gas_payment.objects, consumed_gas)
            });
            if !stale {
                return Ok(value);
            }
            if attempt < 39 {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
        Err("The builder has not observed the previous gas transaction yet. No transaction was signed or submitted for this step.".to_string())
    }) {
        Ok(Ok(value)) => value,
        Ok(Err(e)) | Err(e) => return tool_error(id, "build_failed", &e),
    };
    let result = built.get("data").unwrap_or(&built).get("result").cloned();
    let Some(result) = result else {
        return tool_error(
            id,
            "build_failed",
            &format!("the builder answered without a result: {built}"),
        );
    };
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    if result["isError"] == json!(true) {
        // The builder's strict simulation is where a spend past the wallet's means first fails, and
        // it says so as a raw MoveAbort. Named like every other rule refusal, so an agent reads
        // "the wallet does not hold that much" in a field rather than parsing an abort code.
        // The text is the builder's JSON, `{"reason": "...", "refused": true}`, so the abort's
        // quotes arrive escaped; classify the reason itself.
        let reason = serde_json::from_str::<Value>(text)
            .ok()
            .and_then(|v| v["reason"].as_str().map(str::to_owned))
            .unwrap_or_else(|| text.to_owned());
        if let Some(refusal) = rill_chain::aborts::classify_rule_abort(&reason) {
            context.last_rejection = Some(refusal.to_string());
            return rule_refusal(id, &refusal);
        }
        return tool_error(id, "build_refused", text);
    }
    let envelope: Value = match serde_json::from_str(text) {
        Ok(envelope) => envelope,
        Err(e) => {
            return tool_error(
                id,
                "build_failed",
                &format!("the envelope did not parse: {e}"),
            )
        }
    };

    // The owner-signed run set stands in for a file for this one call, then the signer's own
    // configuration is put back whatever happened.
    let saved = context.run_set.replace(verified.run_set.clone());
    let mut response = execute(
        context,
        id,
        &json!({ "arguments": { "envelope": envelope } }),
    );
    context.run_set = saved;
    if let Some(structured) = response
        .get_mut("result")
        .and_then(|r| r.get_mut("structuredContent"))
        .and_then(Value::as_object_mut)
    {
        structured.insert(
            "grant".into(),
            json!({
                "actionId": verified.grant.action_id,
                "walletId": verified.grant.wallet_id,
                "revision": verified.grant.revision,
                "owner": verified.owner,
            }),
        );
    }
    response
}

/// Preflight every exact owner-signed grant, then use the existing action execution path.
fn run_workflow(context: &mut WalletContext, id: Value, params: &Value) -> Value {
    let workflow: crate::workflow::Workflow =
        match serde_json::from_value(params.get("arguments").cloned().unwrap_or(Value::Null)) {
            Ok(workflow) => workflow,
            Err(error) => return tool_error(id, "invalid_workflow", &error.to_string()),
        };
    if let Err(error) = workflow.validate() {
        return tool_error(id, "invalid_workflow", &error);
    }
    let Some(key) = context.keystore.as_ref() else {
        return tool_error(id, "no_key", "No signing key is configured.");
    };
    let signer = key.address().to_string();
    let equal_address = |a: &str, b: &str| {
        a.parse::<sui_sdk_types::Address>().ok() == b.parse::<sui_sdk_types::Address>().ok()
    };
    if workflow.network != context.network || !equal_address(&workflow.signer, &signer) {
        return tool_error(
            id,
            "workflow_context_mismatch",
            "The workflow network and signer must match this signer.",
        );
    }
    let Some(path) = crate::config::path() else {
        return tool_error(
            id,
            "not_configured",
            "A local configuration path is required for workflow receipts.",
        );
    };
    let Some(parent) = path.parent() else {
        return tool_error(
            id,
            "not_configured",
            "The configuration path has no parent directory.",
        );
    };
    let directory = parent
        .join("workflow-runs")
        .join(&workflow.network)
        .join(&signer);
    // A repeated run must remain readable even if its grants have since been revoked.
    // A new run preflights before its first submission inside the durable claim.
    let mut preflight_done = false;
    let report = crate::workflow::run(&directory, &workflow, |step| {
        if !preflight_done {
            let Some(api) = api_url() else {
                return json!({"error":"not_configured", "reason":NO_API});
            };
            let checked = match checked_grants(context, &signer, &api) {
                Ok(grants) => grants,
                Err(error) => return json!({"error":"grants_unavailable", "reason":error}),
            };
            for expected in &workflow.steps {
                let valid = checked.iter().any(|(signed, verdict)| {
                    signed.grant.action_id == expected.action_id
                        && signed.grant.revision == expected.revision
                        && equal_address(&signed.grant.wallet_id, &expected.wallet_id)
                        && verdict
                            .as_ref()
                            .is_ok_and(|grant| equal_address(&grant.owner, &workflow.owner))
                });
                if !valid {
                    return json!({"error":"workflow_preflight_refused", "actionId":expected.action_id,
                        "reason":"Every step must have the exact usable revision, vault, signer and owner. No workflow transaction submitted."});
                }
            }
            preflight_done = true;
        }
        let response = run_action_for_owner(
            context,
            json!(1),
            &json!({"arguments":{
                "actionId":step.action_id,"walletId":step.wallet_id,"revision":step.revision,"params":step.params
            }}),
            Some(&workflow.owner),
        );
        if response["result"]["isError"] == true || response.get("error").is_some() {
            json!({"error":"action_refused", "response":response})
        } else {
            response["result"]["structuredContent"].clone()
        }
    });
    match report {
        Ok(report) => {
            let failed = report["status"] != "completed";
            let mut response = tool_ok(id, report);
            if failed {
                response["result"]["isError"] = json!(true);
            }
            response
        }
        Err(error) => tool_error(id, "workflow_refused", &error),
    }
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

        Ok::<_, Failed>((
            outcome,
            simulated.spend_base_units().to_string(),
            transaction.gas_payment.objects,
        ))
    });

    let (outcome, spend_base_units, gas_objects) = match outcome {
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

    for object in gas_objects {
        context
            .consumed_gas
            .insert(*object.object_id(), object.version());
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

    #[test]
    fn unstake_rejects_unknown_and_numeric_arguments_before_chain_reads() {
        use sui_crypto::ed25519::Ed25519PrivateKey;
        let encoded = Ed25519PrivateKey::new([52; 32]).to_suiprivkey().unwrap();
        let mut ctx = WalletContext::new(
            Some(Keystore::from_suiprivkey(&encoded).unwrap()),
            "mainnet".into(),
            true,
        );
        for args in [
            json!({"amount":1,"minOut":"1"}),
            json!({"amount":"1","minOut":"1","force":true}),
            json!({"amount":"1","minOut":"1","dryRun":"true"}),
        ] {
            let report = unstake(&mut ctx, json!(1), &json!({"arguments":args}));
            assert_eq!(report["result"]["structuredContent"]["code"], "bad_request");
        }
    }

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
