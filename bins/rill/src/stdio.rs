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
use std::path::PathBuf;

use serde_json::{json, Value};

use crate::keystore::Keystore;
use crate::runset::{RunSet, RUN_SET_VAR};
use crate::verdict::Failure;
use rill_core::manifest::SignerTool;

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
    ///
    /// Durable deduplication lives in the receipt store instead (`workflow::once`, keyed by the
    /// caller's idempotencyKey), because the tools that rebuild on every call produce new bytes on
    /// a retry and would never match here. This map only names a same-process duplicate envelope
    /// for `rill_execute`.
    pub submitted: HashMap<String, Submission>,
    /// Whether the owner's two steps are offered: `rill mcp --owner`. Off for an agent's launch,
    /// which is what a plugin starts, so an agent is never handed a tool that would make it the
    /// owner of a wallet it funds itself.
    pub owner_tools: bool,
    /// Gas references consumed by successful submissions in this process. Builder indexes may lag.
    pub consumed_gas: HashMap<sui_sdk_types::Address, u64>,
    /// Where this signer keeps what must outlive the process: grant revision floors and workflow
    /// receipts. Set once at startup from the config directory, so every piece of local state has
    /// one root and a test can point it at a scratch directory. `None` means grants are refused:
    /// without it the signer could not remember which grant revisions it has already verified.
    pub state_dir: Option<PathBuf>,
    /// The node this signer talks to, when it is not the one `SUI_RPC_URL` or the network names.
    /// A test points it at a closed local port, so a tool that reaches the network before a check
    /// it should have made fails on this machine instead of reaching a real node.
    pub rpc_url: Option<String>,
    /// The Rill API grants are read from, when it is not the one `RILL_API_URL` names. Like
    /// `rpc_url`, it lets a test pin the API instead of inheriting whatever the shell exports.
    pub api_url: Option<String>,
    /// Transactions above an owner's step-up threshold, pinned and held for the owner's approval,
    /// by the approval id the Rill API issued. Process-scoped like `submitted`: a restarted signer
    /// has forgotten them, and the agent runs the action again. Expired ones are dropped on every
    /// `rill_run_action`.
    pub pending_step_up: HashMap<String, crate::step_up::PendingStepUp>,
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
            state_dir: None,
            rpc_url: None,
            api_url: None,
            pending_step_up: HashMap::new(),
        }
    }

    /// The Rill API this signer reads grants from and builds actions with: the pinned one, else
    /// the one `RILL_API_URL` names.
    pub fn api_url(&self) -> Option<String> {
        self.api_url.clone().or_else(api_url_from_env)
    }

    /// The directory local state lives under; see [`WalletContext::state_dir`].
    pub fn with_state_dir(mut self, state_dir: Option<PathBuf>) -> Self {
        self.state_dir = state_dir;
        self
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
    if rill_mcp::KEYED_TOOLS.contains(&name) {
        return keyed(context, id, name, &params);
    }
    dispatch(context, id, name, &params)
}

/// The tool itself, once every gate in [`call`] has passed.
fn dispatch(context: &mut WalletContext, id: Value, name: &str, params: &Value) -> Value {
    match name {
        "rill_status" => status(context, id),
        "rill_pair" => pair_signer(context, id, params),
        "rill_wallet" => wallet(context, id, params),
        "rill_quote" => quote(context, id, params),
        "rill_create_wallet" => create_wallet(context, id, params),
        "rill_attach_rules" => attach_rules(context, id, params),
        "rill_spend" => spend(context, id, params),
        "rill_swap" => swap(context, id, params),
        "rill_stake" => stake(context, id, params),
        "rill_portfolio" => portfolio(context, id, params),
        "rill_unstake" => unstake(context, id, params),
        "rill_execute" => execute(
            context,
            id,
            params,
            EnvelopeSource::HandSupplied,
            StepUp::None,
        ),
        "rill_actions" => actions(context, id),
        "rill_run_action" => run_action(context, id, params),
        "rill_run_workflow" => run_workflow(context, id, params),
        other => rpc_error(id, -32602, &format!("Unknown tool: {other}")),
    }
}

/// What a keyed tool's outcome adds to its note.
const KEYED_NOTE: &str = "Under this idempotencyKey a repeat returns this outcome and submits \
    nothing; calling again means calling with a new key.";

/// A tool that builds afresh on every call, run at most once per caller-chosen key.
///
/// Every check here comes before any network read, and a call that cannot be claimed on disk is
/// refused rather than run unkeyed: a key the signer did not record protects nothing. The key is
/// stripped before the tool sees it, so no handler has to know it exists.
fn keyed(context: &mut WalletContext, id: Value, name: &str, params: &Value) -> Value {
    let arguments = params.get("arguments");
    let mut stripped = params.clone();
    if let Some(arguments) = stripped.get_mut("arguments").and_then(Value::as_object_mut) {
        arguments.remove("idempotencyKey");
    }
    // A dry run submits nothing, so there is nothing to deduplicate and no key to demand.
    if name == "rill_unstake" && arguments.and_then(|a| a.get("dryRun")) == Some(&Value::Bool(true))
    {
        return dispatch(context, id, name, &stripped);
    }
    let Some(key) = arguments
        .and_then(|a| a.get("idempotencyKey"))
        .and_then(Value::as_str)
    else {
        return tool_error(
            id,
            "missing_idempotency_key",
            "idempotencyKey is required: choose one per intended transaction and send the same \
             value on every retry of it. Nothing was submitted.",
        );
    };
    if let Err(why) = crate::workflow::validate_id("idempotencyKey", key) {
        return tool_error(
            id,
            "invalid_arguments",
            &format!("{why}. Nothing was submitted."),
        );
    }
    // The signer's address names the receipt directory, so two keys loaded on one machine never
    // share a key space. With no key nothing can be signed, so there is nothing to claim and the
    // tool's own `no_key` refusal, which says whose key is missing, answers unrecorded.
    let Some(signer) = context.keystore.as_ref().map(|k| k.address().to_string()) else {
        return dispatch(context, id, name, &stripped);
    };
    let Some(state_dir) = context.state_dir.clone() else {
        return tool_error(
            id,
            "not_configured",
            "This signer has no state directory (neither RILL_CONFIG nor HOME is set), so it \
             cannot record an idempotencyKey and refuses to submit without one. Set HOME or \
             RILL_CONFIG in the launch environment and restart the client. Nothing was submitted.",
        );
    };
    let directory = state_dir
        .join("requests")
        .join(&context.network)
        .join(&signer);
    let request = json!({
        "tool": name,
        "arguments": stripped.get("arguments").cloned().unwrap_or(Value::Null),
    });
    // A repeat is answered here, before any tool's own checks, `mainnet_not_opted_in` among them.
    // That is safe because a replay submits nothing: it hands back what the first call already
    // did, and the first call ran every check.
    let outcome = crate::workflow::once(&directory, key, &request, || {
        dispatch(context, id.clone(), name, &stripped)["result"].clone()
    });
    let receipt = match outcome {
        Ok(receipt) => receipt,
        Err(crate::workflow::ReceiptError::InvalidKey(why)) => {
            return tool_error(id, "invalid_arguments", &why)
        }
        Err(crate::workflow::ReceiptError::Conflict { stored }) => {
            return tool_error_with(
                id,
                "idempotency_key_conflict",
                "This idempotencyKey already belongs to a different call; no transaction \
                 submitted. Use a new key only for a new transaction the user authorized.",
                json!({ "idempotencyKey": key, "storedRequest": stored }),
            );
        }
        Err(crate::workflow::ReceiptError::Unreadable(why)) => {
            return tool_error_with(
                id,
                "receipt_unreadable",
                &format!(
                    "The receipt for this idempotencyKey exists and cannot be read ({why}), so \
                     whether its call ran is unknown. Nothing was submitted now. Inspect the \
                     chain for a transaction from this signer before choosing a new key."
                ),
                json!({ "idempotencyKey": key, "network": context.network, "signer": signer }),
            );
        }
        Err(crate::workflow::ReceiptError::Storage(why)) => {
            return tool_error_with(
                id,
                "receipt_unavailable",
                &format!(
                    "This call could not be recorded under its idempotencyKey ({why}), so it was \
                     not attempted. Nothing was submitted."
                ),
                json!({ "idempotencyKey": key }),
            );
        }
    };
    let replayed = receipt["replayed"].clone();
    if receipt["status"] == "interrupted" {
        let note = receipt["note"]
            .as_str()
            .unwrap_or(crate::workflow::INTERRUPTED_NOTE);
        return tool_error_with(
            id,
            "request_interrupted",
            note,
            json!({
                "idempotencyKey": key,
                "request": receipt["request"],
                "network": context.network,
                "signer": signer,
                "claimedAtMs": receipt["claimedAtMs"],
                "replayed": true,
            }),
        );
    }
    let mut result = receipt["result"].clone();
    if let Some(structured) = result
        .get_mut("structuredContent")
        .and_then(Value::as_object_mut)
    {
        // The command notes these tools share with the CLI say "calling again sends a second
        // payment", which is true of a new key and not of this one. Said here, once, for every
        // keyed note that does not already name the key.
        if let Some(note) = structured.get_mut("note") {
            if let Some(text) = note.as_str().filter(|t| !t.contains("idempotencyKey")) {
                *note = json!(format!("{text} {KEYED_NOTE}"));
            }
        }
        structured.insert("idempotencyKey".into(), json!(key));
        structured.insert("replayed".into(), replayed);
        if let Some(warning) = receipt.get("receiptWarning") {
            structured.insert("receiptWarning".into(), warning.clone());
        }
    }
    // A success's text is its structured content printed, as `tool_ok` writes it, so it is printed
    // again to carry the fields added above. A refusal's text is its message and stays as it is.
    if result["isError"] == json!(false) {
        let text = serde_json::to_string_pretty(&result["structuredContent"]).unwrap_or_default();
        result["content"] = json!([{ "type": "text", "text": text }]);
    }
    rpc_result(id, result)
}

fn pair_signer(context: &WalletContext, id: Value, params: &Value) -> Value {
    let Some(api) = context.api_url() else {
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
    if let Some(url) = &context.rpc_url {
        return url.clone();
    }
    std::env::var("SUI_RPC_URL")
        .unwrap_or_else(|_| format!("https://fullnode.{}.sui.io:443", context.network))
}

fn argument<'a>(params: &'a Value, name: &str) -> Option<&'a str> {
    params
        .get("arguments")
        .and_then(|a| a.get(name))
        .and_then(Value::as_str)
}

/// The caller's runtime `params`, where null counts as none: `rill_workflow` builds each step's
/// call with `json!`, which writes a step without `params` as null, and such a step runs on the
/// grant's own params exactly as before bounds were checked.
fn runtime_overrides(params: &Value) -> Option<Value> {
    params
        .get("arguments")
        .and_then(|a| a.get("params"))
        .filter(|overrides| !overrides.is_null())
        .cloned()
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
/// the loaded run-set, and only when that run-set is for this wallet: another wallet's scopes are
/// not this wallet's, and listing them here told an agent this wallet was bound by rules nothing
/// would check against it. See [`crate::wallet_read`] for why the two are labelled separately.
///
/// Its own tool rather than an argument to `rill_status`: it answers a different question, needs an
/// argument that one does not, and costs a round trip a caller asking about the signer should not
/// pay for.
fn wallet(context: &mut WalletContext, id: Value, params: &Value) -> Value {
    let Some(wallet) = argument(params, "wallet") else {
        return tool_error(id, "invalid_arguments", "wallet is required.");
    };
    let package = package_id();

    let local = run_set_for(context, wallet).map(|run_set| &run_set.capability_manifest);
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

/// The loaded run set, when it is for `wallet`.
///
/// Compared as addresses, because `0x1` and its 64-digit spelling are one wallet. A run set for
/// another wallet says nothing about this one, so it is neither listed nor checked against it.
fn run_set_for<'a>(context: &'a WalletContext, wallet: &str) -> Option<&'a RunSet> {
    let wallet = wallet.parse::<sui_sdk_types::Address>().ok()?;
    context
        .run_set
        .as_ref()
        .filter(|run_set| run_set.wallet_id.parse::<sui_sdk_types::Address>().ok() == Some(wallet))
}

/// The packages a protocol scope never has to list: the framework, the wallet's own package and
/// the guard. The same set the Rill builder allows, so an action it compiled under a scope is not
/// refused here for calling the wallet it spends from.
fn infrastructure(wallet_package: &str, guard_package: Option<&str>) -> Vec<String> {
    let mut packages = vec![
        "0x1".to_string(),
        "0x2".into(),
        "0x3".into(),
        wallet_package.into(),
    ];
    packages.extend(guard_package.map(str::to_owned));
    packages
}

/// The pre-flight rules of the run set for `wallet`, checked before anything is built.
///
/// Pure and first: a local build reads the chain before it can sign, so a refusal that waited for
/// the build would cost round trips and could only be tested against a node. Which rules apply to
/// which tool is [`RuleKind::signer_tools`](rill_core::manifest::RuleKind::signer_tools), not a
/// decision made here.
fn local_pre_flight(
    context: &mut WalletContext,
    id: &Value,
    wallet: &str,
    proposal: &rill_policy::preflight::Proposal,
) -> Option<Value> {
    let run_set = run_set_for(context, wallet)?;
    let infrastructure = infrastructure(&run_set.wallet_package_id, None);
    let refusal =
        rill_policy::preflight::check(&run_set.capability_manifest, &infrastructure, proposal)
            .err()?;
    Some(pre_flight_refused(context, id.clone(), &refusal))
}

/// A pre-flight refusal, named by its rule so an agent reads which limit held without parsing.
fn pre_flight_refused(
    context: &mut WalletContext,
    id: Value,
    refusal: &rill_policy::preflight::PreFlightRefusal,
) -> Value {
    let reason = format!(
        "{refusal}. This limit is the owner's, set in the run set this signer holds for the \
         wallet; report it rather than retrying or looking for another tool to do the same."
    );
    context.last_rejection = Some(reason.clone());
    tool_error_with(
        id,
        "pre_flight_refused",
        &reason,
        json!({ "rule": refusal.kind.module(), "enforcement": refusal.kind.enforcement().as_str() }),
    )
}

/// The coin every agent wallet releases.
const SUI: &str = "0x2::sui::SUI";

/// What a local build would do, as the pre-flight checker reads it. One per tool, each naming
/// only the facts that tool knows before it reads the chain. A spend moves SUI to its recipient,
/// which is the signer itself when the caller names none.
fn spend_proposal(recipient: &str) -> rill_policy::preflight::Proposal {
    let mut proposal = rill_policy::preflight::Proposal::new(SignerTool::Spend);
    proposal.coin_types = Some(vec![SUI.into()]);
    proposal.recipients = Some(vec![recipient.into()]);
    proposal
}

/// `bought` is the coin the swap outputs. `min_out` is the caller's floor as `swap_cmd` reads it:
/// empty is zero, and zero emits no guard. Text that is not a u64 is a floor nobody can name, so it
/// is `None`, which a slippage floor refuses rather than guesses.
fn swap_proposal(
    coin_type_a: &str,
    coin_type_b: &str,
    bought: &str,
    min_out: &str,
) -> rill_policy::preflight::Proposal {
    let floor = if min_out.trim().is_empty() {
        Some(0)
    } else {
        rill_core::amounts::parse_u64_string(min_out).ok()
    };
    let mut proposal = rill_policy::preflight::Proposal::new(SignerTool::Swap);
    proposal.coin_types = Some(vec![coin_type_a.into(), coin_type_b.into()]);
    proposal.swap_floors = Some(vec![(bought.into(), floor)]);
    proposal
}

/// A stake moves SUI in and haSUI out. `hasui` is `None` when the Haedal package was overridden
/// from the environment, because then this signer cannot name the coin the stake mints.
fn stake_proposal(hasui: Option<&str>) -> rill_policy::preflight::Proposal {
    let mut proposal = rill_policy::preflight::Proposal::new(SignerTool::Stake);
    proposal.coin_types = hasui.map(|hasui| vec![SUI.into(), hasui.into()]);
    proposal
}

/// Action ids of the grants this signer could use for `wallet` right now: verified against the
/// chain, unexpired and not superseded, which is what an `Ok` verdict after the floor means.
fn usable_grants_for(checked: &[CheckedGrant], wallet: &str) -> Vec<String> {
    let Ok(wallet) = wallet.parse::<sui_sdk_types::Address>() else {
        return Vec::new();
    };
    checked
        .iter()
        .filter_map(|(_, verdict)| verdict.as_ref().ok())
        .filter(|verified| {
            verified
                .grant
                .wallet_id
                .parse::<sui_sdk_types::Address>()
                .ok()
                == Some(wallet)
        })
        .map(|verified| verified.grant.action_id.clone())
        .collect()
}

/// What the grants this signer fetched say about a local build on one wallet.
#[derive(Debug, PartialEq, Eq)]
enum WalletGate {
    /// No grant names the wallet, or every one that does is definitely unusable.
    Open,
    /// The owner granted these actions on the wallet, so it is spent through them.
    Granted(Vec<String>),
    /// Grants for these actions name the wallet and the node could not say whether they are
    /// usable.
    Unknown(Vec<String>),
}

/// Pure, so the decision is tested without a server or a node.
fn wallet_gate(checked: &[CheckedGrant], wallet: &str) -> WalletGate {
    let granted = usable_grants_for(checked, wallet);
    if !granted.is_empty() {
        return WalletGate::Granted(granted);
    }
    let Ok(wallet) = wallet.parse::<sui_sdk_types::Address>() else {
        return WalletGate::Open;
    };
    let unknown: Vec<String> = checked
        .iter()
        .filter(|(_, verdict)| verdict.as_ref().is_err_and(|r| r.is_indeterminate()))
        .filter(|(signed, _)| {
            signed
                .grant
                .wallet_id
                .parse::<sui_sdk_types::Address>()
                .ok()
                == Some(wallet)
        })
        .map(|(signed, _)| signed.grant.action_id.clone())
        .collect();
    if unknown.is_empty() {
        WalletGate::Open
    } else {
        WalletGate::Unknown(unknown)
    }
}

/// The refusal for a local build on a wallet the owner granted actions on.
fn use_granted_action(
    context: &mut WalletContext,
    id: Value,
    wallet: &str,
    actions: &[String],
) -> Value {
    let reason = format!(
        "The owner of {wallet} granted this agent actions on it ({}), so this signer does not \
         build its own transactions for that wallet: they would spend the same wallet and cap \
         outside the grant's limits. Run the granted action through rill_run_action instead; \
         rill_actions lists them. Do not look for another route.",
        actions.join(", ")
    );
    context.last_rejection = Some(reason.clone());
    tool_error_with(
        id,
        "use_granted_action",
        &reason,
        json!({ "walletId": wallet, "actionIds": actions }),
    )
}

/// Refuses a local build on a wallet this signer holds a usable grant for.
///
/// A grant's scopes bind only the envelope its action builds. Without this, an agent holding the
/// grant could spend the same wallet and cap through `rill_spend`, `rill_swap`, `rill_stake` or a
/// hand-supplied envelope, where only the on-chain rules apply. So a wallet with a grant is spent
/// through the grant, and a wallet without one keeps working as before.
///
/// `api` is the Rill API grants are read from. With none configured the signer runs in pure local
/// mode, has no grants to read, and the check is skipped. With one configured and unreachable, or
/// when a grant naming the wallet could not be checked because the node did not answer, it cannot
/// know whether the wallet is granted, so it refuses with `grants_unavailable` rather than assume
/// the wallet is not. A grant the node did answer about and that failed (forged, expired,
/// superseded, a revoked wallet) is not a grant, and leaves the wallet open. The check is as
/// current as the API's answer: the keyless server can withhold a grant, and then this signer
/// sees a wallet without one.
fn granted_wallet_gate(
    context: &mut WalletContext,
    id: &Value,
    wallet: &str,
    api: Option<&str>,
) -> Option<Value> {
    let api = api?;
    let signer = context.keystore.as_ref()?.address().to_string();
    let checked = match checked_grants(context, &signer, api) {
        Ok(checked) => checked,
        Err(e) => {
            let reason = format!(
                "This signer could not read its grants, so it cannot tell whether {wallet} must be \
                 spent through one, and refuses rather than guess: {e}"
            );
            context.last_rejection = Some(reason.clone());
            return Some(tool_error(id.clone(), "grants_unavailable", &reason));
        }
    };
    gate_refusal(context, id, wallet, wallet_gate(&checked, wallet))
}

/// The tool error a gate decision calls for, if any.
fn gate_refusal(
    context: &mut WalletContext,
    id: &Value,
    wallet: &str,
    gate: WalletGate,
) -> Option<Value> {
    match gate {
        WalletGate::Open => None,
        WalletGate::Granted(actions) => {
            Some(use_granted_action(context, id.clone(), wallet, &actions))
        }
        WalletGate::Unknown(actions) => {
            let reason = format!(
                "The node could not say whether the owner's grants on {wallet} ({}) are usable, so \
                 this signer cannot tell whether that wallet must be spent through one, and \
                 refuses rather than guess. Read rill_actions again once the node answers.",
                actions.join(", ")
            );
            context.last_rejection = Some(reason.clone());
            Some(tool_error(id.clone(), "grants_unavailable", &reason))
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

    // Both before anything is built or read: see `local_pre_flight` and `granted_wallet_gate`.
    let signer = keystore.address().to_string();
    let recipient = argument(params, "to").unwrap_or(&signer);
    if let Some(refused) = local_pre_flight(context, &id, wallet, &spend_proposal(recipient)) {
        return refused;
    }
    let api = context.api_url();
    if let Some(refused) = granted_wallet_gate(context, &id, wallet, api.as_deref()) {
        return refused;
    }

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

    let Some(keystore) = context.keystore.as_ref() else {
        return tool_error(id, "no_key", "No signing key is configured.");
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
    if context.keystore.is_none() {
        let reason = "No signing key is configured, so nothing can be signed.".to_string();
        context.last_rejection = Some(reason.clone());
        return tool_error(id, "no_key", &reason);
    }
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

    // The coin the swap buys is the side the wallet's SUI does not fund, as `swap_cmd` reads it.
    let a2b = a2b.unwrap_or(false);
    let bought = if a2b {
        get("coinTypeB")
    } else {
        get("coinTypeA")
    };
    let proposal = swap_proposal(
        &get("coinTypeA"),
        &get("coinTypeB"),
        &bought,
        &get("minOut"),
    );
    let wallet = get("wallet");
    if let Some(refused) = local_pre_flight(context, &id, &wallet, &proposal) {
        return refused;
    }
    let api = context.api_url();
    if let Some(refused) = granted_wallet_gate(context, &id, &wallet, api.as_deref()) {
        return refused;
    }

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
        a2b,
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

    let Some(keystore) = context.keystore.as_ref() else {
        return tool_error(id, "no_key", "No signing key is configured.");
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
    if context.keystore.is_none() {
        let reason = "No signing key is configured, so nothing can be signed.".to_string();
        context.last_rejection = Some(reason.clone());
        return tool_error(id, "no_key", &reason);
    }
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
    // The coin a stake mints is fixed by the Haedal package, so it can be named only for the
    // package this signer defaults to. An overridden package mints a coin nobody here can name.
    let hasui = match std::env::var("HAEDAL_PACKAGE_ID") {
        Ok(_) => None,
        Err(_) if context.network == "mainnet" => Some(crate::unstake_cmd::MAINNET_HASUI),
        Err(_) => Some(rill_ptb::deployments::TESTNET_HASUI_TYPE),
    };
    let wallet = get("wallet");
    if let Some(refused) = local_pre_flight(context, &id, &wallet, &stake_proposal(hasui)) {
        return refused;
    }
    let api = context.api_url();
    if let Some(refused) = granted_wallet_gate(context, &id, &wallet, api.as_deref()) {
        return refused;
    }
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
    let Some(keystore) = context.keystore.as_ref() else {
        return tool_error(id, "no_key", "No signing key is configured.");
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

/// Every gas reference this signer has consumed: this process's memory plus what earlier
/// processes recorded under the state directory, so a restart keeps the stale-gas fence.
fn gas_fence(context: &WalletContext) -> HashMap<sui_sdk_types::Address, u64> {
    let mut fence = context.consumed_gas.clone();
    if let (Some(state_dir), Some(keystore)) = (&context.state_dir, &context.keystore) {
        for (object, version) in
            crate::gas_fence::load(state_dir, &context.network, &keystore.address())
        {
            let held = fence.entry(object).or_insert(version);
            *held = (*held).max(version);
        }
    }
    fence
}

/// Remember the gas a landed submission consumed, in memory and on disk. Returns a warning when
/// the disk copy could not be written; the in-memory fence still holds for this process.
fn remember_consumed_gas(
    context: &mut WalletContext,
    objects: &[sui_sdk_types::ObjectReference],
) -> Option<String> {
    let mut failures = Vec::new();
    for object in objects {
        let held = context
            .consumed_gas
            .entry(*object.object_id())
            .or_insert(object.version());
        *held = (*held).max(object.version());
        if let (Some(state_dir), Some(keystore)) = (&context.state_dir, &context.keystore) {
            if let Err(e) = crate::gas_fence::record(
                state_dir,
                &context.network,
                &keystore.address(),
                object.object_id(),
                object.version(),
            ) {
                failures.push(e);
            }
        }
    }
    (!failures.is_empty()).then(|| {
        format!(
            "the consumed gas could not be recorded on disk ({}); a restart before the builder \
             catches up may build with a stale gas reference, which the chain refuses",
            failures.join("; ")
        )
    })
}

/// The Rill API `RILL_API_URL` names. Read through [`WalletContext::api_url`], never directly.
fn api_url_from_env() -> Option<String> {
    std::env::var(crate::config::API_URL_VAR)
        .ok()
        .map(|url| url.trim().trim_end_matches('/').to_owned())
        .filter(|url| !url.is_empty())
}

const NO_API: &str = "No Rill API is configured, so there are no granted actions to read. Run \
                      `~/.rill/bin/rill-wallet setup --api <url>` and restart the client.";

/// Said when the signer has no state directory. The fix is the launch environment, which only the
/// person who configured the signer can change.
pub const NO_STATE_DIR: &str = "This signer has no state directory (neither RILL_CONFIG nor HOME \
     is set), so it cannot remember which grant revisions it has already verified and refuses to \
     use grants. Set HOME or RILL_CONFIG in the launch environment and restart the client.";

/// One fetched grant and what the chain said about it.
type CheckedGrant = (
    rill_core::grant::SignedGrant,
    Result<crate::grants::VerifiedGrant, crate::grants::Refusal>,
);

/// Every grant for this signer's address, each checked against the chain. A grant that fails a
/// check is kept with the reason, so `rill_actions` can say why an action is not available.
///
/// Every grant use goes through here, so this is the one place the revision floor is applied: a
/// grant below a revision this signer has already verified comes back refused as superseded.
fn checked_grants(
    context: &WalletContext,
    signer: &str,
    api: &str,
) -> Result<Vec<CheckedGrant>, String> {
    // Checked before anything is fetched: a floor that silently did nothing would be no floor.
    let Some(state_dir) = context.state_dir.clone() else {
        return Err(NO_STATE_DIR.into());
    };
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
        floor_checked(&state_dir, &network, signer, checked)
    })?
}

/// Applies this signer's revision floor to a fetched and verified grant list, raising the floor
/// to the newest usable revision first. Kept apart from the fetch so the step that carries the
/// L6 guarantee is exercised by a test without a server or a chain.
fn floor_checked(
    state_dir: &std::path::Path,
    network: &str,
    signer: &str,
    mut checked: Vec<CheckedGrant>,
) -> Result<Vec<CheckedGrant>, String> {
    let floors = crate::grant_floor::GrantFloors::open(state_dir, network, signer)?;
    crate::grants::apply_floor(&mut checked, &floors)?;
    Ok(checked)
}

/// One `rill_actions` entry. The refusal is given as a code beside the prose, so an agent tells a
/// superseded grant from any other refusal without matching text.
fn action_entry((signed, verdict): &CheckedGrant) -> Value {
    let grant = &signed.grant;
    let mut entry = json!({
        "actionId": grant.action_id,
        "name": grant.action_name,
        "walletId": grant.wallet_id,
        "revision": grant.revision,
        "expiresAtMs": grant.expires_at_ms,
        "maxPerTransactionBaseUnits": grant.run_set.get("maxAmountBaseUnits"),
        // So an agent knows ahead of a run which ones will pause for the owner.
        "stepUpAboveBaseUnits": grant.run_set.get(rill_core::grant::STEP_UP_KEY),
        // When true the wallet's on-chain step_up threshold also pauses a run, at whichever of the
        // two is lower, and above the chain's the owner signs the transaction as its gas sponsor.
        // Read from the grant's manifest rather than the chain, so listing actions costs no extra
        // read; rill_wallet reads the threshold itself.
        "stepUpOnChain": grant.run_set["capabilityManifest"]["rules"]
            .as_array()
            .is_some_and(|rules| rules.iter().any(|rule| rule["kind"] == "step_up")),
        "usable": verdict.is_ok(),
    });
    match verdict {
        Ok(verified) => entry["owner"] = json!(verified.owner),
        Err(refusal) => {
            entry["refusedBecause"] = json!(refusal.to_string());
            entry["refusalCode"] = json!(refusal.code());
        }
    }
    entry
}

/// The tool error code when no matching grant is usable. `grant_superseded` only when every
/// candidate was superseded: then the owner's newer approval exists and the server did not serve
/// it, which is a different conversation with the owner than an expired or revoked grant.
fn refusal_code(refusals: &[&crate::grants::Refusal]) -> &'static str {
    if !refusals.is_empty()
        && refusals
            .iter()
            .all(|r| matches!(r, crate::grants::Refusal::Superseded { .. }))
    {
        "grant_superseded"
    } else {
        "grant_refused"
    }
}

/// Why a workflow step cannot run, or `None` when it has its exact usable grant.
///
/// The exact-revision rule is unchanged. When the pinned revision was served but is below this
/// signer's floor, the refusal says so, because "no exact usable revision" alone sends the owner
/// looking for a grant that was in fact replaced.
fn preflight_refusal(
    checked: &[CheckedGrant],
    expected: &crate::workflow::Step,
    owner: &str,
) -> Option<Value> {
    let equal_address = |a: &str, b: &str| {
        a.parse::<sui_sdk_types::Address>().ok() == b.parse::<sui_sdk_types::Address>().ok()
    };
    let pinned = |signed: &rill_core::grant::SignedGrant| {
        signed.grant.action_id == expected.action_id
            && signed.grant.revision == expected.revision
            && equal_address(&signed.grant.wallet_id, &expected.wallet_id)
    };
    let valid = checked.iter().any(|(signed, verdict)| {
        pinned(signed)
            && verdict
                .as_ref()
                .is_ok_and(|grant| equal_address(&grant.owner, owner))
    });
    if valid {
        return None;
    }
    let mut refused = json!({"error":"workflow_preflight_refused", "actionId":expected.action_id,
        "reason":"Every step must have the exact usable revision, vault, signer and owner. No workflow transaction submitted."});
    if let Some(refusal) = checked.iter().find_map(|(signed, verdict)| match verdict {
        Err(refusal @ crate::grants::Refusal::Superseded { .. }) if pinned(signed) => Some(refusal),
        _ => None,
    }) {
        refused["refusalCode"] = json!(refusal.code());
        refused["refusedBecause"] = json!(refusal.to_string());
    }
    Some(refused)
}

/// `rill_actions`: what this agent may run, and for each grant that cannot be used, why.
fn actions(context: &mut WalletContext, id: Value) -> Value {
    let Some(api) = context.api_url() else {
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
    let actions: Vec<Value> = checked.iter().map(action_entry).collect();
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
    let Some(api) = context.api_url() else {
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
    let now = now_ms();
    context.pending_step_up.retain(|approval_id, pending| {
        !pending.approval.is_expired(now) || argument(params, "approvalId") == Some(approval_id)
    });
    if let Some(approval_id) = argument(params, "approvalId") {
        let approval_id = approval_id.to_owned();
        return continue_step_up(context, id, params, &action_id, &approval_id, &api, owner);
    }
    let wallet = argument(params, "walletId").map(str::to_owned);
    let revision_value = params.get("arguments").and_then(|a| a.get("revision"));
    let revision = revision_value.and_then(Value::as_u64);
    if revision_value.is_some() && revision.is_none_or(|revision| revision == 0) {
        return tool_error(
            id,
            "invalid_arguments",
            "revision must be a positive integer.",
        );
    }
    let overrides = runtime_overrides(params);
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
    let refusals: Vec<&crate::grants::Refusal> = matching
        .iter()
        .filter_map(|(_, verdict)| verdict.as_ref().err())
        .collect();
    if matching.iter().all(|(_, verdict)| verdict.is_err()) {
        let reason = format!(
            "The grant for {action_id} cannot be used: {}. Ask the wallet's owner; do not look \
             for another route.",
            refusals
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        );
        let code = refusal_code(&refusals);
        context.last_rejection = Some(reason.clone());
        return tool_error(id, code, &reason);
    }
    let mut usable: Vec<crate::grants::VerifiedGrant> = matching
        .into_iter()
        .filter_map(|(_, verdict)| verdict.ok())
        .collect();
    let wallets: std::collections::BTreeSet<String> =
        usable.iter().map(|v| v.grant.wallet_id.clone()).collect();
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
    // the caller. Each one is checked here against the bounds the owner signed, before the builder
    // sees it, and the builder checks it again against the published action.
    if let Some(overrides) = &overrides {
        if let Err(why) = crate::grants::check_overrides(&verified.grant.build_arguments, overrides)
        {
            let reason = format!(
                "The grant for {action_id} does not allow this runtime value: {why}. Run it \
                 without that value, or ask the wallet's owner to publish and grant a new one."
            );
            context.last_rejection = Some(reason.clone());
            return tool_error(id, rill_core::runtime_bounds::REFUSED, &reason);
        }
    }
    let mut arguments = verified.grant.build_arguments.clone();
    if let Some(object) = arguments.as_object_mut() {
        object.remove("actionId");
        // The bounds are the signer's to enforce; the builder enforces its own from the action.
        object.remove("runtimeBounds");
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
    let consumed_gas = gas_fence(context);
    let consumed_gas = &consumed_gas;
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

    let step_up_grant = crate::step_up::StepUpGrant {
        action_name: verified.grant.action_name.clone(),
        revision: verified.grant.revision,
        digest: verified.grant.digest(),
    };
    // A grant that names the on-chain rule also pauses at the chain's threshold, with the owner as
    // the sponsor above it. One that does not keeps the signer-only threshold, and nothing more is
    // read for it.
    let on_chain = if crate::step_up::names_on_chain_rule(&verified.run_set.capability_manifest) {
        let endpoint = endpoint(context);
        block_on(async {
            let chain = rill_chain::grpc::GrpcSui::new(&endpoint).map_err(|e| e.to_string())?;
            crate::step_up::on_chain_threshold(
                &chain,
                &verified.run_set.capability_manifest,
                &verified.grant.wallet_id,
            )
            .await
        })
    } else {
        Ok(Ok(None))
    };
    let step_up = match on_chain {
        Ok(Ok(on_chain)) => {
            match step_up_gate(&verified.run_set, on_chain, &verified.owner, step_up_grant) {
                Ok(step_up) => step_up,
                Err(e) => return tool_error(id, "bad_run_set", &e.to_string()),
            }
        }
        Ok(Err(why)) | Err(why) => {
            let reason = format!(
                "The owner's grant names the on-chain step-up rule, and the wallet's threshold \
                 could not be read ({why}). Nothing was signed or submitted."
            );
            context.last_rejection = Some(reason.clone());
            return tool_error(id, "step_up_unavailable", &reason);
        }
    };

    // The owner-signed run set stands in for a file for this one call, then the signer's own
    // configuration is put back whatever happened.
    let saved = context.run_set.replace(verified.run_set.clone());
    let mut response = execute(
        context,
        id.clone(),
        &json!({ "arguments": { "envelope": envelope } }),
        EnvelopeSource::Grant,
        step_up,
    );
    context.run_set = saved;
    let paused = &response["result"]["structuredContent"];
    if paused["code"] == "step_up_required" {
        response = open_step_up(
            context,
            id,
            &paused.clone(),
            envelope,
            verified.run_set.clone(),
            &api,
        );
    }
    with_grant(response, &verified)
}

/// The step-up gate one granted run goes through, from the run set's threshold and the wallet's
/// on-chain one when the grant names the rule.
///
/// Both bind, and neither replaces the other. The run set's threshold is the owner's own word, in
/// the grant they signed, about where they want to be asked; the chain's is where the chain aborts
/// without them. So the run pauses at the lower of the two, and the owner pays the gas only above
/// the chain's, the one place the chain requires it. Between them the pause is the signer-only
/// kind, as it is on a wallet without the rule.
fn step_up_gate(
    run_set: &RunSet,
    on_chain: Option<u64>,
    owner: &str,
    grant: crate::step_up::StepUpGrant,
) -> Result<StepUp, crate::runset::RunSetError> {
    let own = run_set.step_up_threshold()?;
    let Some(threshold) = own.into_iter().chain(on_chain).min() else {
        return Ok(StepUp::None);
    };
    Ok(StepUp::Gate {
        threshold,
        grant,
        on_chain: on_chain.map(|threshold| OnChainStepUp {
            owner: owner.to_owned(),
            threshold,
        }),
    })
}

/// The grant a run used, added to its answer so the agent can report which approval it ran under.
fn with_grant(mut response: Value, verified: &crate::grants::VerifiedGrant) -> Value {
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

/// Open the owner's approval request for a paused run and hold the pinned envelope until then.
///
/// The request is signed by the agent's key over [`StepUpApproval::request_message`], so only
/// this agent can put a request in front of its owner. That is a personal message under its own
/// header, never a transaction signature. If the API cannot take the request, nothing is held and
/// nothing was signed: the agent can run the action again.
///
/// [`StepUpApproval::request_message`]: rill_core::step_up::StepUpApproval::request_message
fn open_step_up(
    context: &mut WalletContext,
    id: Value,
    paused: &Value,
    envelope: Value,
    run_set: RunSet,
    api: &str,
) -> Value {
    let unavailable = |why: String| {
        tool_error(
            id.clone(),
            "step_up_unavailable",
            &format!(
                "This transaction is above the owner's step-up threshold, and the owner's \
                 approval could not be requested ({why}). Nothing was signed or submitted. Run \
                 the action again with a new idempotencyKey once the Rill API answers."
            ),
        )
    };
    let approval: rill_core::step_up::StepUpApproval =
        match serde_json::from_value(paused["approval"].clone()) {
            Ok(approval) => approval,
            Err(e) => return unavailable(format!("the approval did not form: {e}")),
        };
    let Some(keystore) = context.keystore.as_ref() else {
        return tool_error(id, "no_key", "No signing key is configured.");
    };
    let agent_signature =
        match keystore.sign_personal_message(approval.request_message().as_bytes()) {
            Ok(signature) => signature.to_base64(),
            Err(e) => return unavailable(format!("the request could not be signed: {e}")),
        };
    let body = json!({
        "approval": approval,
        "unsignedPtb": envelope["unsignedPtb"],
        "agentSignature": agent_signature,
    });
    let url = format!("{api}/api/step-up");
    let opened = match block_on(crate::http::post_json(&url, &body)) {
        Ok(Ok(opened)) => opened,
        Ok(Err(e)) | Err(e) => return unavailable(e),
    };
    let data = opened.get("data").unwrap_or(&opened);
    let (Some(approval_id), Some(approval_url)) =
        (data["id"].as_str(), data["approvalUrl"].as_str())
    else {
        return unavailable(format!(
            "the API answered without an id and a link: {opened}"
        ));
    };
    // The id comes back as an argument on the agent's next call and names a path on the API, so
    // it is held to the alphabet every caller-chosen id here is.
    if let Err(why) = crate::workflow::validate_id("approvalId", approval_id) {
        return unavailable(why);
    }
    let approval_origin = match crate::step_up::approval_link_origin(approval_url, approval_id) {
        Ok(origin) => origin,
        Err(why) => return unavailable(why),
    };
    context.pending_step_up.insert(
        approval_id.to_owned(),
        crate::step_up::PendingStepUp {
            envelope,
            approval,
            run_set,
        },
    );
    let mut answer = paused.clone();
    answer["approvalId"] = json!(approval_id);
    answer["approvalUrl"] = json!(approval_url);
    // Named on its own so the owner can see which site they are about to connect a wallet to.
    answer["approvalOrigin"] = json!(approval_origin);
    tool_ok(id, answer)
}

/// `rill_run_action` with an `approvalId`: run the transaction the owner approved, and only it.
///
/// Every check is made again here rather than remembered from the paused call: the approval is
/// still live, the owner has signed it, the grant it names is still the latest usable one, and the
/// signature is the current owner's over this signer's own text. Then the held envelope runs
/// through every check `execute` makes, with the approved digest pinned, so different bytes or a
/// lapsed envelope are refused there.
fn continue_step_up(
    context: &mut WalletContext,
    id: Value,
    params: &Value,
    action_id: &str,
    approval_id: &str,
    api: &str,
    owner: Option<&str>,
) -> Value {
    let refuse = |context: &mut WalletContext, code: &str, reason: String| {
        context.last_rejection = Some(reason.clone());
        tool_error(id.clone(), code, &reason)
    };
    let Some(pending) = context.pending_step_up.get(approval_id).cloned() else {
        return refuse(
            context,
            "unknown_approval",
            format!(
                "This signer holds no transaction under approvalId {approval_id}: it was never \
                 opened here, or this signer restarted since. Nothing was submitted. Run the \
                 action again with a new idempotencyKey; the owner is asked again."
            ),
        );
    };
    if pending.approval.action_id != action_id {
        return refuse(
            context,
            "unknown_approval",
            format!(
                "approvalId {approval_id} is for {}, not {action_id}. Nothing was submitted.",
                pending.approval.action_id
            ),
        );
    }
    let arguments = params.get("arguments");
    if ["params", "revision", "walletId"].iter().any(|key| {
        arguments
            .and_then(|a| a.get(*key))
            .is_some_and(|v| !v.is_null())
    }) {
        return refuse(
            context,
            "invalid_arguments",
            "With approvalId, pass only actionId, approvalId and a new idempotencyKey: the owner \
             approved one exact transaction, and nothing about it can change now."
                .to_string(),
        );
    }
    if pending.approval.is_expired(now_ms()) {
        context.pending_step_up.remove(approval_id);
        return refuse(
            context,
            "step_up_expired",
            "The owner did not approve this transaction within the 5 minutes it was valid for, \
             so it can no longer be submitted. Nothing was submitted. Run the action again with a \
             new idempotencyKey to ask the owner again."
                .to_string(),
        );
    }

    let url = format!("{api}/api/step-up/{approval_id}");
    let shown = match block_on(crate::http::get_json(&url)) {
        Ok(Ok(shown)) => shown,
        Ok(Err(e)) | Err(e) => {
            return refuse(
                context,
                "step_up_unavailable",
                format!(
                    "Whether the owner approved could not be read ({e}). Nothing was submitted; \
                     call again with the same approvalId and a new idempotencyKey."
                ),
            )
        }
    };
    let shown = shown.get("data").unwrap_or(&shown);
    let Some(owner_signature) = shown["ownerSignature"].as_str().map(str::to_owned) else {
        let seconds_left = pending
            .approval
            .expires_at_ms
            .parse::<u64>()
            .unwrap_or(0)
            .saturating_sub(now_ms())
            / 1000;
        let mut waiting = json!({
            "code": "step_up_pending",
            "status": "pending_owner_approval",
            "submitted": false,
            "approvalId": approval_id,
            "secondsLeft": seconds_left,
            "spendBaseUnits": pending.approval.spend_base_units,
            "expiresAtMs": pending.approval.expires_at_ms,
            "note": "The owner has not approved this transaction yet; nothing was signed or \
                     submitted. Once they approve it in Studio, call again with this approvalId \
                     and a new idempotencyKey.",
        });
        // Held to the same shape as when the request was opened; a link that fails it is dropped.
        if let Some(url) = shown["approvalUrl"].as_str() {
            if let Ok(origin) = crate::step_up::approval_link_origin(url, approval_id) {
                waiting["approvalUrl"] = json!(url);
                waiting["approvalOrigin"] = json!(origin);
            }
        }
        return tool_ok(id, waiting);
    };

    // The grant the approval names must still be the one the owner would run now.
    let Some(signer) = context.keystore.as_ref().map(|k| k.address().to_string()) else {
        return tool_error(id, "no_key", "No signing key is configured.");
    };
    let checked = match checked_grants(context, &signer, api) {
        Ok(checked) => checked,
        Err(e) => return tool_error(id, "grants_unavailable", &e),
    };
    let wallet = pending
        .approval
        .wallet_id
        .parse::<sui_sdk_types::Address>()
        .ok();
    let current = checked
        .iter()
        .filter_map(|(_, verdict)| verdict.as_ref().ok())
        .filter(|verified| {
            verified.grant.action_id == action_id
                && verified
                    .grant
                    .wallet_id
                    .parse::<sui_sdk_types::Address>()
                    .ok()
                    == wallet
        })
        .max_by_key(|verified| verified.grant.revision)
        .cloned();
    let current_digest = current.as_ref().map(|verified| verified.grant.digest());
    let endpoint = endpoint(context);
    let verdict = block_on(async {
        let chain = rill_chain::grpc::GrpcSui::new(&endpoint)
            .map_err(|e| crate::step_up::ApprovalRefusal::Unavailable(e.to_string()))?;
        crate::step_up::verify_owner_approval(
            &chain,
            &pending.approval,
            pending.envelope["unsignedPtb"].as_str().unwrap_or_default(),
            &owner_signature,
            current_digest.as_deref(),
        )
        .await
    });
    let approved_by = match verdict {
        Ok(Ok(approved_by)) => approved_by,
        Ok(Err(refusal)) => return refuse(context, refusal.code(), refusal.to_string()),
        Err(e) => return refuse(context, "step_up_unavailable", e),
    };
    let Some(verified) = current else {
        return refuse(
            context,
            "grant_changed",
            crate::step_up::ApprovalRefusal::GrantChanged.to_string(),
        );
    };
    let same = |a: &str, b: &str| {
        a.parse::<sui_sdk_types::Address>().ok() == b.parse::<sui_sdk_types::Address>().ok()
    };
    if owner.is_some_and(|owner| !same(owner, &approved_by)) {
        return tool_error(
            id,
            "owner_changed",
            "The vault owner changed since workflow preflight; no transaction submitted for this step.",
        );
    }

    let saved = context.run_set.replace(pending.run_set.clone());
    let response = execute(
        context,
        id,
        &json!({ "arguments": { "envelope": pending.envelope } }),
        EnvelopeSource::Grant,
        StepUp::Approved {
            pinned_digest: pending.approval.pinned_digest.clone(),
            sponsor_signature: pending
                .approval
                .is_sponsored()
                .then(|| owner_signature.clone()),
        },
    );
    context.run_set = saved;
    if response["result"]["structuredContent"]["submitted"] == true {
        context.pending_step_up.remove(approval_id);
    }
    let mut response = with_grant(response, &verified);
    if let Some(structured) = response
        .get_mut("result")
        .and_then(|r| r.get_mut("structuredContent"))
        .and_then(Value::as_object_mut)
    {
        structured.insert("approvalId".into(), json!(approval_id));
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
    let Some(state_dir) = context.state_dir.clone() else {
        return tool_error(id, "not_configured", NO_STATE_DIR);
    };
    let directory = state_dir
        .join("workflow-runs")
        .join(&workflow.network)
        .join(&signer);
    // A repeated run must remain readable even if its grants have since been revoked.
    // A new run preflights before its first submission inside the durable claim.
    let mut preflight_done = false;
    let report = crate::workflow::run(&directory, &workflow, |step| {
        if !preflight_done {
            let Some(api) = context.api_url() else {
                return json!({"error":"not_configured", "reason":NO_API});
            };
            let checked = match checked_grants(context, &signer, &api) {
                Ok(grants) => grants,
                Err(error) => return json!({"error":"grants_unavailable", "reason":error}),
            };
            for expected in &workflow.steps {
                if let Some(refused) = preflight_refusal(&checked, expected, &workflow.owner) {
                    return refused;
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

/// Whether a confirmed transaction placed a DeepBook limit order. Its digest proves the order was
/// placed, not that it filled, and "Submitted and confirmed" alone reads as a completed trade.
pub(crate) fn places_order(targets: &[String]) -> bool {
    targets
        .iter()
        .any(|target| target.ends_with("::pool::place_limit_order"))
}

/// No retry rule here: the envelope cache lives in one process and granted actions are rebuilt on
/// every call, so a promise that a second call is refused would not hold across them. A granted
/// order gets [`GRANTED_RETRY_NOTE`] appended instead, which names the key that does hold.
const ORDER_PLACED_NOTE: &str = "Order placed on DeepBook and confirmed. This digest proves the \
    placement, not a fill: the order may rest unfilled, fill in part or fill in full. This signer \
    has no tool that reads order status or cancels an order; cancelling and withdrawing are the \
    owner's, in Studio.";

/// How a granted run is retried. `rill_run_action` builds a new transaction on every call, so the
/// envelope guard in [`execute`] never matches a retry of it; the caller's key, or a workflow's
/// runId, is what turns a retry into a replay.
const GRANTED_RETRY_NOTE: &str = "A retry with the same idempotencyKey (or, inside a workflow, \
    the same runId) returns this outcome and never submits again; a new key is a new transaction.";

/// Where an envelope came from, which decides whether a grant has to be its route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnvelopeSource {
    /// Passed to `rill_execute` and checked against the run set loaded from a file.
    HandSupplied,
    /// Built for an owner-signed grant by `rill_run_action`, under that grant's run set.
    Grant,
}

/// What an owner's step-up threshold asks of one execution.
#[derive(Debug, Clone, PartialEq, Eq)]
enum StepUp {
    /// The run set names no threshold.
    None,
    /// Above `threshold` base units, pause for the owner instead of signing.
    Gate {
        /// The lower of the run set's threshold and the on-chain one, where the pause starts.
        threshold: u64,
        grant: crate::step_up::StepUpGrant,
        /// The wallet's on-chain `step_up` rule, when the grant names it.
        on_chain: Option<OnChainStepUp>,
    },
    /// The owner approved exactly the bytes with this pinned digest, and the signer checked it.
    Approved {
        pinned_digest: String,
        /// The owner's signature as gas sponsor, submitted beside the agent's, for a sponsored
        /// approval. `None` for the signer-only kind, which the agent's signature alone submits.
        sponsor_signature: Option<String>,
    },
}

/// The wallet's on-chain `step_up` rule as one run meets it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OnChainStepUp {
    /// The wallet's owner. Above `threshold` the bytes must name them as gas sponsor, because the
    /// chain aborts anything else, and the owner approves by signing the transaction as sponsor.
    owner: String,
    /// The rule's threshold as the chain holds it now.
    threshold: u64,
}

/// Said beside every step-up pause, so the agent knows what the owner has to do and what it may
/// not do meanwhile.
const STEP_UP_NOTE: &str = "Above the owner's step-up threshold: nothing was signed or submitted. \
    Give the owner approvalUrl; they approve this exact transaction in Rill Studio with their own \
    wallet, within the 5 minutes the transaction is valid for. Then call rill_run_action again with \
    the same actionId, this approvalId and a NEW idempotencyKey: this key's outcome is this pause, \
    and replaying it returns the pause again. Do not rebuild or try another tool meanwhile.";

fn execute(
    context: &mut WalletContext,
    id: Value,
    params: &Value,
    source: EnvelopeSource,
    step_up: StepUp,
) -> Value {
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
    // A threshold belongs to an owner-signed grant, whose run is the only one that can pause for
    // the owner. Without the gate this path would sign above it, so it refuses before reading
    // anything: a run set file carries no owner's signature and no route to their approval.
    let threshold = match run_set.step_up_threshold() {
        Ok(threshold) => threshold,
        Err(e) => return tool_error(id, "bad_run_set", &e.to_string()),
    };
    if threshold.is_some() && step_up == StepUp::None {
        let reason = "This run set has an owner's step-up threshold, which only an owner-signed \
                      grant can carry: above it the owner approves each transaction in Studio, \
                      and a run set file has no route to that approval. Run the granted action \
                      with rill_run_action instead. Nothing was signed."
            .to_string();
        context.last_rejection = Some(reason.clone());
        return tool_error(id, "step_up_requires_grant", &reason);
    }
    let coin_type = run_set.capability_manifest.wallet_coin_type.clone();

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
        // The guard this signer trusts, which a protocol scope never has to list.
        Ok(p) => rill_policy::LocalPolicy {
            guard_package_id: Some(guard_package_id()),
            ..p
        },
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
        Err(rill_policy::Rejection::PreFlight(refusal)) => {
            return pre_flight_refused(context, id, &refusal)
        }
        Err(rejection) => {
            let reason = rejection.to_string();
            context.last_rejection = Some(reason.clone());
            return tool_error(id, "policy_rejection", &reason);
        }
    };

    // Last of the local checks, because it is the one that reads the network: a hand-supplied
    // envelope for a wallet the owner granted actions on must go through the grant instead.
    if source == EnvelopeSource::HandSupplied {
        let api = context.api_url();
        if let Some(refused) = granted_wallet_gate(context, &id, &policy.wallet_id, api.as_deref())
        {
            return refused;
        }
    }

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

    // The owner's step-up gate. After the bytes are pinned, so the amount compared is the one the
    // signature would cover, and before the key or a node is touched, so a pause signs nothing.
    let same_address = |a: &str, b: &str| {
        a.parse::<sui_sdk_types::Address>().ok() == b.parse::<sui_sdk_types::Address>().ok()
    };
    match &step_up {
        StepUp::None => {}
        StepUp::Approved {
            pinned_digest,
            sponsor_signature,
        } => {
            if sponsor_signature.is_some() != pinned.decoded().is_sponsored() {
                let reason = "The owner's approval and these bytes disagree about who pays the \
                              gas: a sponsor's signature goes only with bytes the sponsor pays \
                              for. The node would reject the pair, so nothing was signed."
                    .to_string();
                context.last_rejection = Some(reason.clone());
                return tool_error(id, "step_up_sponsor_mismatch", &reason);
            }
            if *pinned_digest != digest {
                let reason = format!(
                    "The owner approved the transaction pinned as {pinned_digest}, and these bytes \
                     pin as {digest}. An approval is for exactly the bytes it names. Nothing was \
                     signed."
                );
                context.last_rejection = Some(reason.clone());
                return tool_error(id, "step_up_digest_mismatch", &reason);
            }
        }
        StepUp::Gate {
            threshold,
            grant,
            on_chain,
        } => {
            // The declared spend is the builder's word about the bytes; the bytes are the bytes.
            // The larger is compared, so a builder that understates the spend still meets the
            // gate.
            let Some(requested) = pinned.decoded().requested_spend_base_units else {
                let reason = "This transaction's spend amount is not a literal in its bytes, so \
                              the owner could not be shown what they would approve. Refusing \
                              rather than asking for an approval nobody can check. Nothing was \
                              signed."
                    .to_string();
                context.last_rejection = Some(reason.clone());
                return tool_error(id, "step_up_amount_unreadable", &reason);
            };
            let spend = pinned.spend_base_units().max(requested);
            let decoded = pinned.decoded();
            // The owner signs as sponsor only above the chain's threshold. Under it an owner who
            // approves signs the text, and sponsored bytes would wait for a signature never asked
            // for.
            let sponsor = on_chain
                .as_ref()
                .filter(|rule| spend > rule.threshold)
                .map(|rule| rule.owner.clone());
            if let Some(rule) = on_chain.as_ref().filter(|_| sponsor.is_none()) {
                if decoded.is_sponsored() {
                    let reason = format!(
                        "This transaction spends {spend}, within the wallet's on-chain step-up \
                         threshold of {}, and its gas is paid by {}: the owner has nothing to sign \
                         as sponsor, so no sponsor's signature will come and the node would reject \
                         it. Nothing was signed. Build the action again.",
                        rule.threshold, decoded.gas_owner
                    );
                    context.last_rejection = Some(reason.clone());
                    return tool_error(id, "step_up_unexpected_sponsor", &reason);
                }
            }
            if spend > *threshold {
                if let (Some(owner), Some(rule)) = (&sponsor, on_chain) {
                    if !same_address(&decoded.gas_owner, owner) {
                        let reason = format!(
                            "This transaction spends {spend}, above the wallet's on-chain step-up \
                             threshold of {}, so the chain aborts it unless the wallet's owner \
                             {owner} pays its gas, and its gas is paid by {}. Nothing was signed, \
                             and the owner was not asked. The builder must make the owner the gas \
                             sponsor; build the action again.",
                            rule.threshold, decoded.gas_owner
                        );
                        context.last_rejection = Some(reason.clone());
                        return tool_error(id, "step_up_needs_owner_gas", &reason);
                    }
                }
                let gas_budget = decoded.gas_budget;
                let envelope = pinned.envelope();
                let (Some(expires_at_ms), Ok(transaction_digest)) = (
                    rill_policy::parse_rfc3339_ms(&envelope.expires_at),
                    rill_policy::decode::transaction_digest(&envelope.unsigned_ptb),
                ) else {
                    return tool_error(
                        id,
                        "policy_rejection",
                        "The pinned envelope could not be described for the owner's approval.",
                    );
                };
                let approval = rill_core::step_up::StepUpApproval {
                    network: context.network.clone(),
                    agent: policy.sender.clone(),
                    wallet_id: policy.wallet_id.clone(),
                    wallet_package_id: policy.wallet_package_id.clone(),
                    action_id: policy.action_id.clone(),
                    action_name: grant.action_name.clone(),
                    grant_revision: grant.revision,
                    grant_digest: grant.digest.clone(),
                    pinned_digest: digest.clone(),
                    transaction_digest: transaction_digest.clone(),
                    spend_base_units: spend.to_string(),
                    threshold_base_units: threshold.to_string(),
                    coin_type,
                    expires_at_ms: expires_at_ms.to_string(),
                    gas_owner: sponsor.clone(),
                };
                let mut paused = json!({
                    "code": "step_up_required",
                    "status": "pending_owner_approval",
                    "submitted": false,
                    "pinnedDigest": digest,
                    "transactionDigest": transaction_digest,
                    "spendBaseUnits": spend.to_string(),
                    "thresholdBaseUnits": threshold.to_string(),
                    "expiresAtMs": expires_at_ms.to_string(),
                    "approval": approval,
                    "sponsored": sponsor.is_some(),
                    "note": STEP_UP_NOTE,
                });
                if let Some(owner) = sponsor {
                    // What the owner pays at most, said before they are asked to.
                    paused["gasOwner"] = json!(owner);
                    paused["gasBudgetBaseUnits"] = json!(gas_budget.to_string());
                }
                return tool_ok(id, paused);
            }
        }
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

        // The sender's signature, then the owner's as gas sponsor when the owner approved this
        // transaction under the on-chain rule. The node checks each against its own role.
        let mut signatures = vec![signature.to_base64()];
        if let StepUp::Approved {
            sponsor_signature: Some(sponsor),
            ..
        } = &step_up
        {
            signatures.push(sponsor.clone());
        }
        let outcome =
            rill_chain::SuiWrite::execute(&chain, simulated.signable_bytes(), &signatures)
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

    if let Some(warning) = remember_consumed_gas(context, &gas_objects) {
        // Stderr, never stdout: stdout is the protocol wire. The submission stands either way.
        eprintln!("rill: {warning}");
    }
    context.submitted.insert(
        digest.clone(),
        Submission {
            digest: Some(outcome.digest.clone()),
        },
    );

    let order = places_order(&targets);
    tool_ok(
        id,
        json!({
            "submitted": true,
            "orderPlaced": order,
            "digest": outcome.digest,
            "pinnedDigest": digest,
            "callSequence": targets,
            "gasUsed": outcome.gas_used_mist,
            "spendBaseUnits": spend_base_units,
            // What the code does, said in the answer. This used to read "calling again with the
            // same envelope submits a second transaction", which described a footgun instead of
            // removing it; the second call is now refused and answered with this digest. A granted
            // run never sees that refusal, since each call is a new build, so it names its key.
            "note": match (order, source) {
                (true, EnvelopeSource::HandSupplied) => ORDER_PLACED_NOTE.to_string(),
                (true, EnvelopeSource::Grant) => format!("{ORDER_PLACED_NOTE} {GRANTED_RETRY_NOTE}"),
                (false, EnvelopeSource::Grant) => format!(
                    "Submitted and confirmed. This cannot be undone. {GRANTED_RETRY_NOTE}"
                ),
                (false, EnvelopeSource::HandSupplied) => "Submitted and confirmed. This cannot be \
                    undone. Calling again with this same envelope is refused and answered with \
                    this digest, so a retry cannot become a second transaction; build a new action \
                    if another spend is intended."
                    .to_string(),
            }
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
    fn a_step_without_params_carries_no_runtime_overrides() {
        let step = crate::workflow::Step {
            action_id: "swap".into(),
            wallet_id: "0x1".into(),
            revision: 2,
            params: None,
        };
        // How rill_workflow hands a step to run_action_for_owner: `json!` writes an absent
        // `params` as null, which must not reach check_overrides as a value to bound.
        let call = json!({"arguments":{
            "actionId":step.action_id,"walletId":step.wallet_id,"revision":step.revision,"params":step.params
        }});
        assert_eq!(runtime_overrides(&call), None);
        assert_eq!(
            runtime_overrides(&json!({"arguments":{"params":null}})),
            None
        );
        assert_eq!(runtime_overrides(&json!({"arguments":{}})), None);
        let given = json!({"swap": {"amount_in": "1"}});
        assert_eq!(
            runtime_overrides(&json!({"arguments":{"params":given}})),
            Some(given)
        );
    }

    #[test]
    fn a_limit_order_placement_is_reported_as_placed_not_filled() {
        let deepbook = "0x2c8d603bc51326b8c13cef9dd07031a408a48dddb541963357661df5d3204809";
        assert!(places_order(&[
            format!("{deepbook}::balance_manager::generate_proof_as_owner"),
            format!("{deepbook}::pool::place_limit_order"),
            "0x1::agent_wallet::confirm_spend".into(),
        ]));
        // A swap moves the same coins through a pool, and is a fill by construction.
        assert!(!places_order(&[
            "0x1eabed72::pool_script_v2::swap_a2b".into(),
            format!("{deepbook}::pool::swap_exact_base_for_quote"),
        ]));
        assert!(!places_order(&[]));

        for needed in ["not a fill", "no tool that reads order status", "Studio"] {
            assert!(
                ORDER_PLACED_NOTE.contains(needed),
                "the placement note must say {needed:?}"
            );
        }
        // Only one process remembers an envelope, and actions are rebuilt per call, so the note
        // must not promise that a retry is refused.
        assert!(!ORDER_PLACED_NOTE.contains("refused"));
    }

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

    // ── the owner's step-up gate ──────────────────────────────────────────────────────────────

    /// Nothing listens here, so a call that reaches the network fails on this machine.
    const CLOSED: &str = "http://127.0.0.1:1";

    /// A transaction that requests `amount` from the wallet the way every builder does:
    /// `request_spend(wallet, cap, version, amount, clock)`. The wallet stands in for the other
    /// objects so the run set's object scope stays the one object it already allows.
    fn ptb_requesting(amount: u64) -> String {
        ptb_requesting_paid_by(amount, WALLET)
    }

    /// The same transaction, its gas paid by `gas_owner`: the owner, when the on-chain rule needs
    /// them as sponsor.
    fn ptb_requesting_paid_by(amount: u64, gas_owner: &str) -> String {
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
        let amount = tx.pure(&amount);
        tx.move_call(
            Function::new(
                PKG.parse().unwrap(),
                Identifier::new("agent_wallet").unwrap(),
                Identifier::new("request_spend").unwrap(),
            ),
            vec![wallet, wallet, wallet, amount, wallet],
        );
        let mut built = tx.try_build().unwrap();
        built.gas_payment.owner = gas_owner.parse().unwrap();
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(bcs::to_bytes(&built).unwrap())
    }

    /// An envelope carrying `ptb` and declaring `declared` as its spend.
    fn envelope_for(ptb: &str, declared: &str) -> Value {
        let mut env = envelope(json!({
            "unsignedPtb": ptb,
            "actionDigest": rill_core::envelope::digest_unsigned_ptb(ptb),
        }));
        env["resolvedParams"]["spendAmountMist"] = json!(declared);
        env
    }

    fn stepped_context(threshold: &str) -> WalletContext {
        let mut value = serde_json::to_value(run_set()).unwrap();
        value["stepUpAboveBaseUnits"] = json!(threshold);
        let mut ctx = context_with_run_set()
            .with_run_set(Some(RunSet::from_value(&value).expect("a stepped run set")));
        ctx.rpc_url = Some(CLOSED.into());
        ctx.api_url = Some(CLOSED.into());
        ctx
    }

    fn granted() -> crate::step_up::StepUpGrant {
        crate::step_up::StepUpGrant {
            action_name: "hero".into(),
            revision: 4,
            digest: "aa".repeat(32),
        }
    }

    fn run(ctx: &mut WalletContext, envelope: Value, step_up: StepUp) -> Value {
        execute(
            ctx,
            json!(1),
            &json!({ "arguments": { "envelope": envelope } }),
            EnvelopeSource::Grant,
            step_up,
        )
    }

    fn gate(threshold: u64) -> StepUp {
        StepUp::Gate {
            threshold,
            grant: granted(),
            on_chain: None,
        }
    }

    /// The wallet's owner in these tests, who sponsors above the on-chain threshold.
    const OWNER: &str = "0x00000000000000000000000000000000000000000000000000000000000000a1";

    /// The gate a wallet carrying the on-chain rule gets when the grant has no threshold of its
    /// own: the chain's threshold, and its owner as the sponsor above it.
    fn on_chain_gate(threshold: u64) -> StepUp {
        StepUp::Gate {
            threshold,
            grant: granted(),
            on_chain: Some(OnChainStepUp {
                owner: OWNER.into(),
                threshold,
            }),
        }
    }

    /// The run set of a grant made for a wallet with the on-chain rule, and no signer-only
    /// threshold of its own.
    fn on_chain_context() -> WalletContext {
        let mut ctx = context_with_run_set();
        ctx.rpc_url = Some(CLOSED.into());
        ctx.api_url = Some(CLOSED.into());
        ctx
    }

    #[test]
    fn the_on_chain_gate_pauses_above_its_threshold_for_the_owner_as_sponsor() {
        let mut ctx = on_chain_context();
        let ptb = ptb_requesting_paid_by(20, OWNER);
        let out = run(&mut ctx, envelope_for(&ptb, "20"), on_chain_gate(10));
        let structured = &out["result"]["structuredContent"];
        assert_eq!(structured["code"], "step_up_required", "{out}");
        assert_eq!(structured["sponsored"], true, "{out}");
        assert_eq!(structured["gasOwner"], OWNER, "{out}");
        assert_eq!(structured["gasBudgetBaseUnits"], "50000000", "{out}");
        let approval: rill_core::step_up::StepUpApproval =
            serde_json::from_value(structured["approval"].clone()).unwrap();
        assert_eq!(approval.gas_owner.as_deref(), Some(OWNER));
        assert_eq!(approval.threshold_base_units, "10");
        assert!(ctx.submitted.is_empty());

        // Above the threshold with the agent paying: the chain would abort it, so nothing is
        // signed and the owner is not asked to approve bytes they could not sponsor.
        let out = run(
            &mut ctx,
            envelope_for(&ptb_requesting(20), "20"),
            on_chain_gate(10),
        );
        assert_eq!(
            out["result"]["structuredContent"]["code"], "step_up_needs_owner_gas",
            "{out}"
        );

        // A sponsor other than the owner is no sponsor at all for the rule.
        let out = run(
            &mut ctx,
            envelope_for(&ptb_requesting_paid_by(20, PKG), "20"),
            on_chain_gate(10),
        );
        assert_eq!(
            out["result"]["structuredContent"]["code"], "step_up_needs_owner_gas",
            "{out}"
        );

        // Within the threshold the owner has nothing to sign, so sponsored bytes cannot land.
        let out = run(
            &mut ctx,
            envelope_for(&ptb_requesting_paid_by(5, OWNER), "5"),
            on_chain_gate(10),
        );
        assert_eq!(
            out["result"]["structuredContent"]["code"], "step_up_unexpected_sponsor",
            "{out}"
        );

        // Within it and paid by the agent, the run goes on as before.
        let out = run(
            &mut ctx,
            envelope_for(&ptb_requesting(5), "5"),
            on_chain_gate(10),
        );
        assert_eq!(
            out["result"]["structuredContent"]["code"], "chain_unavailable",
            "{out}"
        );
        assert!(ctx.submitted.is_empty());
    }

    /// A grant can carry both thresholds: the owner's own `stepUpAboveBaseUnits` and the on-chain
    /// rule's. Neither replaces the other. The run pauses at the lower one, so the owner is asked
    /// wherever they said to be asked; only above the chain's threshold must the owner pay the gas,
    /// because only there does the chain abort without them.
    #[test]
    fn a_grant_with_both_thresholds_pauses_at_the_lower_and_sponsors_only_above_the_chains() {
        let both = |ctx: &WalletContext| {
            step_up_gate(ctx.run_set.as_ref().unwrap(), Some(30), OWNER, granted()).unwrap()
        };

        // Between the two: the owner's own threshold pauses it, as the signer-only kind, with the
        // agent paying the gas the chain still accepts from it.
        let mut ctx = stepped_context("10");
        let step_up = both(&ctx);
        let out = run(&mut ctx, envelope_for(&ptb_requesting(20), "20"), step_up);
        let structured = &out["result"]["structuredContent"];
        assert_eq!(structured["code"], "step_up_required", "{out}");
        assert_eq!(structured["sponsored"], false, "{out}");
        assert_eq!(structured["thresholdBaseUnits"], "10", "{out}");
        assert!(structured.get("gasOwner").is_none(), "{out}");
        let approval: rill_core::step_up::StepUpApproval =
            serde_json::from_value(structured["approval"].clone()).unwrap();
        assert_eq!(approval.gas_owner, None);
        assert!(ctx.submitted.is_empty(), "nothing was signed or submitted");

        // Owner-sponsored bytes between the two have no sponsor's signature coming.
        let mut ctx = stepped_context("10");
        let step_up = both(&ctx);
        let out = run(
            &mut ctx,
            envelope_for(&ptb_requesting_paid_by(20, OWNER), "20"),
            step_up,
        );
        assert_eq!(
            out["result"]["structuredContent"]["code"], "step_up_unexpected_sponsor",
            "{out}"
        );

        // Above both: the owner approves as gas sponsor, and bytes the agent pays for are refused.
        let mut ctx = stepped_context("10");
        let step_up = both(&ctx);
        let out = run(
            &mut ctx,
            envelope_for(&ptb_requesting_paid_by(40, OWNER), "40"),
            step_up,
        );
        let structured = &out["result"]["structuredContent"];
        assert_eq!(structured["code"], "step_up_required", "{out}");
        assert_eq!(structured["sponsored"], true, "{out}");
        assert_eq!(structured["gasOwner"], OWNER, "{out}");
        let mut ctx = stepped_context("10");
        let step_up = both(&ctx);
        let out = run(&mut ctx, envelope_for(&ptb_requesting(40), "40"), step_up);
        assert_eq!(
            out["result"]["structuredContent"]["code"], "step_up_needs_owner_gas",
            "{out}"
        );

        // At or under both, the gate stays out of the way and the run goes on to simulate.
        let mut ctx = stepped_context("10");
        let step_up = both(&ctx);
        let out = run(&mut ctx, envelope_for(&ptb_requesting(10), "10"), step_up);
        assert_eq!(
            out["result"]["structuredContent"]["code"], "chain_unavailable",
            "{out}"
        );

        // An on-chain threshold below the owner's own is the lower one, and it pauses as sponsor.
        let mut ctx = stepped_context("50");
        let step_up = both(&ctx);
        let out = run(
            &mut ctx,
            envelope_for(&ptb_requesting_paid_by(40, OWNER), "40"),
            step_up,
        );
        let structured = &out["result"]["structuredContent"];
        assert_eq!(structured["code"], "step_up_required", "{out}");
        assert_eq!(structured["sponsored"], true, "{out}");
        assert_eq!(structured["thresholdBaseUnits"], "30", "{out}");

        // Without an on-chain threshold the run set's alone decides, exactly as before the rule.
        let ctx = stepped_context("10");
        assert_eq!(
            step_up_gate(ctx.run_set.as_ref().unwrap(), None, OWNER, granted()).unwrap(),
            gate(10)
        );
    }

    /// The owner's sponsor signature goes with sponsored bytes only. Submitting one without the
    /// other is a transaction the node rejects, so it is refused before anything is signed.
    #[test]
    fn an_approved_run_carries_the_sponsor_signature_only_for_sponsored_bytes() {
        let mut ctx = on_chain_context();
        let plain = ptb_requesting(20);
        let out = run(
            &mut ctx,
            envelope_for(&plain, "20"),
            StepUp::Approved {
                pinned_digest: rill_core::envelope::digest_unsigned_ptb(&plain),
                sponsor_signature: Some("owner-signature".into()),
            },
        );
        assert_eq!(
            out["result"]["structuredContent"]["code"], "step_up_sponsor_mismatch",
            "{out}"
        );

        let paid = ptb_requesting_paid_by(20, OWNER);
        let out = run(
            &mut ctx,
            envelope_for(&paid, "20"),
            StepUp::Approved {
                pinned_digest: rill_core::envelope::digest_unsigned_ptb(&paid),
                sponsor_signature: None,
            },
        );
        assert_eq!(
            out["result"]["structuredContent"]["code"], "step_up_sponsor_mismatch",
            "{out}"
        );

        let out = run(
            &mut ctx,
            envelope_for(&paid, "20"),
            StepUp::Approved {
                pinned_digest: rill_core::envelope::digest_unsigned_ptb(&paid),
                sponsor_signature: Some("owner-signature".into()),
            },
        );
        assert_eq!(
            out["result"]["structuredContent"]["code"], "chain_unavailable",
            "{out}"
        );
        assert!(ctx.submitted.is_empty());
    }

    #[test]
    fn the_gate_returns_pending_above_threshold_and_never_signs() {
        let mut ctx = stepped_context("10");
        let ptb = ptb_requesting(20);
        let out = run(&mut ctx, envelope_for(&ptb, "20"), gate(10));
        let structured = &out["result"]["structuredContent"];
        assert_eq!(out["result"]["isError"], false, "{out}");
        assert_eq!(structured["code"], "step_up_required", "{out}");
        assert_eq!(structured["status"], "pending_owner_approval", "{out}");
        assert_eq!(structured["submitted"], false);
        assert_eq!(
            structured["pinnedDigest"],
            json!(rill_core::envelope::digest_unsigned_ptb(&ptb))
        );
        assert_eq!(structured["spendBaseUnits"], "20");
        assert_eq!(structured["thresholdBaseUnits"], "10");
        let approval: rill_core::step_up::StepUpApproval =
            serde_json::from_value(structured["approval"].clone()).expect("an approval");
        assert_eq!(approval.grant_revision, 4);
        assert_eq!(approval.grant_digest, "aa".repeat(32));
        assert_eq!(
            approval.transaction_digest,
            rill_policy::decode::transaction_digest(&ptb).unwrap()
        );
        assert!(ctx.submitted.is_empty(), "nothing was signed or submitted");

        // At the threshold the gate stays out of the way: the call goes on to re-simulate, which
        // fails here only because the node is a closed port.
        let mut ctx = stepped_context("10");
        let out = run(&mut ctx, envelope_for(&ptb_requesting(10), "10"), gate(10));
        assert_eq!(
            out["result"]["structuredContent"]["code"], "chain_unavailable",
            "{out}"
        );
    }

    /// The declared spend is the server's word. The bytes say 20, so 20 is what the owner's
    /// threshold is compared with, whatever the envelope claims.
    #[test]
    fn a_server_that_understates_declared_spend_still_meets_the_gate() {
        let mut ctx = stepped_context("10");
        let out = run(&mut ctx, envelope_for(&ptb_requesting(20), "5"), gate(10));
        let structured = &out["result"]["structuredContent"];
        assert_eq!(structured["code"], "step_up_required", "{out}");
        assert_eq!(structured["spendBaseUnits"], "20", "{out}");
        assert!(ctx.submitted.is_empty());
    }

    /// Bytes whose amount cannot be read cannot be shown to the owner, so nobody could approve
    /// them; that is refused by name rather than sent to a page that would refuse it again.
    #[test]
    fn a_spend_the_bytes_do_not_state_is_refused_rather_than_sent_for_approval() {
        let mut ctx = stepped_context("10");
        let out = run(&mut ctx, envelope(json!({})), gate(10));
        assert_eq!(
            out["result"]["structuredContent"]["code"], "step_up_amount_unreadable",
            "{out}"
        );
        assert!(ctx.submitted.is_empty());
    }

    #[test]
    fn an_approved_digest_cannot_be_used_for_different_bytes() {
        let mut ctx = stepped_context("10");
        let out = run(
            &mut ctx,
            envelope_for(&ptb_requesting(20), "20"),
            StepUp::Approved {
                pinned_digest: "00".repeat(32),
                sponsor_signature: None,
            },
        );
        assert_eq!(
            out["result"]["structuredContent"]["code"], "step_up_digest_mismatch",
            "{out}"
        );
        assert!(ctx.submitted.is_empty());

        // The digest the owner approved goes on to simulate and nothing else changes.
        let ptb = ptb_requesting(20);
        let out = run(
            &mut ctx,
            envelope_for(&ptb, "20"),
            StepUp::Approved {
                pinned_digest: rill_core::envelope::digest_unsigned_ptb(&ptb),
                sponsor_signature: None,
            },
        );
        assert_eq!(
            out["result"]["structuredContent"]["code"], "chain_unavailable",
            "{out}"
        );
    }

    /// A run set with a threshold never signs without its gate, whichever path brought it here.
    #[test]
    fn a_threshold_without_its_gate_is_refused() {
        let mut ctx = stepped_context("10");
        let out = run(
            &mut ctx,
            envelope_for(&ptb_requesting(1), "1"),
            StepUp::None,
        );
        assert_eq!(
            out["result"]["structuredContent"]["code"], "step_up_requires_grant",
            "{out}"
        );
    }

    /// The pause is a stopped outcome under its key: replaying that key answers the same pause
    /// and runs nothing, and continuing after the owner approves is a new call with a new key.
    #[test]
    fn a_pending_approval_is_stored_under_its_key_and_replayed_not_rerun() {
        let dir = std::env::temp_dir().join(format!(
            "rill-step-up-receipts-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let request = json!({"tool": "rill_run_action", "arguments": {"actionId": "skill_hero"}});
        let mut ctx = stepped_context("10");
        let first = crate::workflow::once(&dir, "stepped-1", &request, || {
            run(&mut ctx, envelope_for(&ptb_requesting(20), "20"), gate(10))["result"].clone()
        })
        .unwrap();
        assert_eq!(first["status"], "stopped", "{first}");
        assert_eq!(
            first["result"]["structuredContent"]["code"],
            "step_up_required"
        );

        let replay = crate::workflow::once(&dir, "stepped-1", &request, || {
            panic!("a replayed key must not run again")
        })
        .unwrap();
        assert_eq!(replay["replayed"], true);
        assert_eq!(
            replay["result"]["structuredContent"]["code"],
            "step_up_required"
        );

        let follow_up = json!({"tool": "rill_run_action",
            "arguments": {"actionId": "skill_hero", "approvalId": "abc"}});
        assert!(matches!(
            crate::workflow::once(&dir, "stepped-1", &follow_up, || panic!("not run")),
            Err(crate::workflow::ReceiptError::Conflict { .. })
        ));
    }

    fn keyed_call(ctx: &mut WalletContext, arguments: Value) -> Value {
        let mut arguments = arguments;
        arguments["idempotencyKey"] = json!(format!("step-up-{}", rand::random::<u64>()));
        handle(
            ctx,
            &json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": "rill_run_action", "arguments": arguments }
            }),
        )
        .unwrap()["result"]["structuredContent"]
            .clone()
    }

    fn pending(expires_at_ms: u64) -> crate::step_up::PendingStepUp {
        let ptb = ptb_requesting(20);
        let mut approval: rill_core::step_up::StepUpApproval = serde_json::from_value(
            run(
                &mut stepped_context("10"),
                envelope_for(&ptb, "20"),
                gate(10),
            )["result"]["structuredContent"]["approval"]
                .clone(),
        )
        .unwrap();
        approval.expires_at_ms = expires_at_ms.to_string();
        crate::step_up::PendingStepUp {
            envelope: envelope_for(&ptb, "20"),
            approval,
            run_set: stepped_context("10").run_set.unwrap(),
        }
    }

    /// Every refusal on the continuing call is answered before the network is asked anything.
    #[test]
    fn a_continuing_call_names_an_unknown_or_expired_approval() {
        let mut ctx = stepped_context("10").with_state_dir(Some(
            std::env::temp_dir().join(format!("rill-step-up-continue-{}", std::process::id())),
        ));
        let out = keyed_call(
            &mut ctx,
            json!({"actionId": "skill_hero", "approvalId": "nope"}),
        );
        assert_eq!(out["code"], "unknown_approval", "{out}");

        ctx.pending_step_up
            .insert("old".into(), pending(now_ms().saturating_sub(1)));
        let out = keyed_call(
            &mut ctx,
            json!({"actionId": "skill_hero", "approvalId": "old"}),
        );
        assert_eq!(out["code"], "step_up_expired", "{out}");
        assert!(
            out["message"].as_str().unwrap().contains("5 minutes"),
            "{out}"
        );
        assert!(
            !ctx.pending_step_up.contains_key("old"),
            "an expired approval is forgotten"
        );

        ctx.pending_step_up
            .insert("live".into(), pending(now_ms() + 60_000));
        let out = keyed_call(
            &mut ctx,
            json!({"actionId": "skill_other", "approvalId": "live"}),
        );
        assert_eq!(out["code"], "unknown_approval", "{out}");
        let out = keyed_call(
            &mut ctx,
            json!({"actionId": "skill_hero", "approvalId": "live", "params": {"swap": {"amount_in": "1"}}}),
        );
        assert_eq!(out["code"], "invalid_arguments", "{out}");
        assert!(ctx.submitted.is_empty());
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
        assert_eq!(
            pre_flight[0]["enforcedBy"],
            rill_core::manifest::Enforcement::PreFlight.enforced_by()
        );
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

#[cfg(test)]
mod grant_floor_tests {
    use super::*;
    use crate::grants::{Refusal, VerifiedGrant};
    use rill_core::grant::{Grant, SignedGrant};

    const OWNER: &str = "0x00000000000000000000000000000000000000000000000000000000000000a1";
    const SIGNER: &str = "0x00000000000000000000000000000000000000000000000000000000000000a2";
    const WALLET: &str = "0x00000000000000000000000000000000000000000000000000000000000000b1";
    const PACKAGE: &str = "0x00000000000000000000000000000000000000000000000000000000000000c1";

    fn signed(revision: u64) -> SignedGrant {
        let run_set = json!({
            "label": "swap", "network": "mainnet", "sender": SIGNER, "actionId": "skill_x",
            "walletPackageId": PACKAGE, "walletId": WALLET, "agentCapId": PACKAGE,
            "versionId": PACKAGE,
            "capabilityManifest": {"walletCoinType": "0x2::sui::SUI",
                "rules": [{"kind": "budget", "totalMist": "100"}, {"kind": "per_tx", "maxMist": "50"}]},
            "allowedTargets": [format!("{PACKAGE}::agent_wallet::request_spend")],
            "allowedObjectIds": [WALLET], "maxAmountBaseUnits": "50",
            "declaredSpendBaseUnits": "20", "minimumRemainingBaseUnits": "0",
            "gasCeilingBaseUnits": "100000000"
        });
        SignedGrant {
            grant: Grant {
                network: "mainnet".into(),
                action_id: "skill_x".into(),
                action_name: "swap".into(),
                agent: SIGNER.into(),
                wallet_id: WALLET.into(),
                wallet_package_id: PACKAGE.into(),
                expires_at_ms: "9999999999999".into(),
                revision,
                run_set,
                build_arguments: json!({}),
            },
            signature: "sig".into(),
        }
    }

    fn usable(revision: u64) -> CheckedGrant {
        let signed = signed(revision);
        let verified = VerifiedGrant {
            grant: signed.grant.clone(),
            run_set: RunSet::from_value(&signed.grant.run_set).unwrap(),
            owner: OWNER.into(),
        };
        (signed, Ok(verified))
    }

    fn superseded(given: u64, floor: u64) -> CheckedGrant {
        (signed(given), Err(Refusal::Superseded { floor, given }))
    }

    /// Without a state root there is nowhere to keep the floor, so grants are refused outright,
    /// before anything is fetched: a floor that silently did nothing would be no floor.
    #[test]
    fn a_signer_without_a_state_directory_refuses_grants_before_fetching() {
        let context = WalletContext::new(None, "mainnet".into(), true);
        let error = checked_grants(&context, SIGNER, "http://127.0.0.1:1").unwrap_err();
        assert!(error.contains("state directory"), "{error}");
    }

    /// The floor must be applied on the path every grant use takes, not only in `apply_floor`:
    /// once revision 3 was usable, a later fetch that serves only revision 2 is a rollback and
    /// comes back superseded through every surface that reports it.
    #[test]
    fn a_rolled_back_grant_is_superseded_on_the_path_every_use_takes() {
        let state_dir = std::env::temp_dir().join(format!(
            "rill-stdio-floor-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let _ = std::fs::remove_dir_all(&state_dir);

        let first = floor_checked(&state_dir, "mainnet", SIGNER, vec![usable(3)]).unwrap();
        assert!(
            first[0].1.is_ok(),
            "revision 3 is the newest, so it is usable"
        );

        let rolled_back = floor_checked(&state_dir, "mainnet", SIGNER, vec![usable(2)]).unwrap();
        assert!(
            matches!(
                rolled_back[0].1,
                Err(Refusal::Superseded { floor: 3, given: 2 })
            ),
            "{:?}",
            rolled_back[0].1.as_ref().err()
        );
        assert_eq!(
            action_entry(&rolled_back[0])["refusalCode"],
            "grant_superseded"
        );
        let refusals: Vec<&Refusal> = rolled_back
            .iter()
            .filter_map(|(_, verdict)| verdict.as_ref().err())
            .collect();
        assert_eq!(refusal_code(&refusals), "grant_superseded");
        let refused = preflight_refusal(&rolled_back, &step(2), OWNER).unwrap();
        assert_eq!(refused["refusalCode"], "grant_superseded");

        // The floor is per network: the same signer on testnet has no history yet.
        let testnet = floor_checked(&state_dir, "testnet", SIGNER, vec![usable(2)]).unwrap();
        assert!(testnet[0].1.is_ok());

        let _ = std::fs::remove_dir_all(&state_dir);
    }

    #[test]
    fn rill_actions_names_the_refusal_in_a_field() {
        let entry = action_entry(&superseded(2, 3));
        assert_eq!(entry["usable"], false);
        assert_eq!(entry["refusalCode"], "grant_superseded");
        assert!(entry["refusedBecause"]
            .as_str()
            .unwrap()
            .contains("revision 3"));

        let other = action_entry(&(signed(1), Err(Refusal::Expired)));
        assert_eq!(other["refusalCode"], "grant_refused");

        let ok = action_entry(&usable(4));
        assert_eq!(ok["usable"], true);
        assert!(ok.get("refusalCode").is_none());
    }

    /// An agent learns before a run that above the chain's threshold the owner co-signs, so a
    /// pause there is expected rather than a failure to work around.
    #[test]
    fn rill_actions_says_when_the_grant_names_the_on_chain_step_up_rule() {
        assert_eq!(action_entry(&usable(4))["stepUpOnChain"], false);
        let (mut signed, _) = usable(5);
        signed.grant.run_set["capabilityManifest"]["rules"]
            .as_array_mut()
            .unwrap()
            .push(json!({"kind": "step_up", "thresholdMist": "30"}));
        let entry = action_entry(&(signed, Err(Refusal::Expired)));
        assert_eq!(entry["stepUpOnChain"], true, "{entry}");
    }

    #[test]
    fn only_a_wholly_superseded_action_answers_grant_superseded() {
        let superseded = Refusal::Superseded { floor: 3, given: 2 };
        let expired = Refusal::Expired;
        assert_eq!(refusal_code(&[&superseded]), "grant_superseded");
        assert_eq!(
            refusal_code(&[&superseded, &superseded]),
            "grant_superseded"
        );
        assert_eq!(refusal_code(&[&superseded, &expired]), "grant_refused");
        assert_eq!(refusal_code(&[&expired]), "grant_refused");
        assert_eq!(refusal_code(&[]), "grant_refused");
    }

    fn step(revision: u64) -> crate::workflow::Step {
        crate::workflow::Step {
            action_id: "skill_x".into(),
            wallet_id: WALLET.into(),
            revision,
            params: None,
        }
    }

    #[test]
    fn a_workflow_pinned_to_a_superseded_revision_says_why() {
        let refused = preflight_refusal(&[superseded(2, 3)], &step(2), OWNER).unwrap();
        assert_eq!(refused["error"], "workflow_preflight_refused");
        assert_eq!(refused["refusalCode"], "grant_superseded");
        assert!(refused["refusedBecause"]
            .as_str()
            .unwrap()
            .contains("revision 3"));

        // The floor revision itself is still exact-pinnable.
        assert!(preflight_refusal(&[usable(3)], &step(3), OWNER).is_none());

        // A revision the server does not hold is refused as before, with no superseded code.
        let missing = preflight_refusal(&[usable(3)], &step(2), OWNER).unwrap();
        assert_eq!(missing["error"], "workflow_preflight_refused");
        assert!(missing.get("refusalCode").is_none());
    }
}

#[cfg(test)]
mod pre_flight_tests {
    //! The pre-flight rules each signing tool claims, refused on that tool and nowhere else.
    //!
    //! Every context here talks to a closed local port, so a tool that reached the network before
    //! the check it should have made fails on this machine rather than reaching a node. That is
    //! what makes these tests fail offline on code that does not check.

    use super::*;
    use crate::grants::{Refusal, VerifiedGrant};
    use rill_core::grant::{Grant, SignedGrant};
    use rill_core::manifest::{CapabilityRule, Enforcement, RuleKind};

    const PKG: &str = "0x000000000000000000000000000000000000000000000000000000000000cafe";
    const WALLET: &str = "0x0000000000000000000000000000000000000000000000000000000000000001";
    const ANOTHER_WALLET: &str =
        "0x0000000000000000000000000000000000000000000000000000000000000002";
    const ALLOWED: &str = "0x000000000000000000000000000000000000000000000000000000000000beef";
    const STRANGER: &str = "0x000000000000000000000000000000000000000000000000000000000000dead";
    const OFF_SCOPE: &str = "0x0000000000000000000000000000000000000000000000000000000000000bad";
    const USDC: &str =
        "0x00000000000000000000000000000000000000000000000000000000000000c1::usdc::USDC";
    /// Nothing listens here, so a network call fails at once and stays on this machine.
    const CLOSED: &str = "http://127.0.0.1:1";

    fn key() -> Keystore {
        use sui_crypto::ed25519::Ed25519PrivateKey;
        let encoded = Ed25519PrivateKey::new([11u8; 32]).to_suiprivkey().unwrap();
        Keystore::from_suiprivkey(&encoded).unwrap()
    }

    fn signer() -> String {
        key().address().to_string()
    }

    /// A run set for WALLET whose manifest carries a budget and `rule`, pinning `targets`.
    fn run_set(rule: CapabilityRule, targets: &[String]) -> RunSet {
        serde_json::from_value(json!({
            "label": "pre-flight",
            "network": "testnet",
            "sender": signer(),
            "actionId": "skill_hero",
            "walletPackageId": PKG,
            "walletId": WALLET,
            "agentCapId": "0xcap",
            "versionId": "0xversion",
            "capabilityManifest": {
                "walletCoinType": "0x2::sui::SUI",
                "rules": [{ "kind": "budget", "totalMist": "5000000000" }, rule]
            },
            "allowedTargets": targets,
            "allowedObjectIds": [WALLET],
            "maxAmountBaseUnits": "2000000000",
            "declaredSpendBaseUnits": "2000000000",
            "minimumRemainingBaseUnits": "0",
            "gasCeilingBaseUnits": "50000000"
        }))
        .unwrap()
    }

    fn context(rule: CapabilityRule) -> WalletContext {
        // Keyed tools claim their key on disk before anything else, so they need somewhere to.
        // One fixed directory rather than one per process: every run reuses the same tree, so
        // test runs leave a few small receipts behind instead of a new directory each time. Each
        // call takes a fresh key, so runs sharing it never replay each other's outcome.
        let state_dir = std::env::temp_dir().join("rill-pre-flight-tests");
        let mut context = WalletContext::new(Some(key()), "testnet".into(), false)
            .with_run_set(Some(run_set(rule, &[request_spend()])))
            .with_state_dir(Some(state_dir));
        context.rpc_url = Some(CLOSED.into());
        context
    }

    fn request_spend() -> String {
        format!("{PKG}::agent_wallet::request_spend")
    }

    fn call(context: &mut WalletContext, tool: &str, mut arguments: Value) -> Value {
        // A fresh key per call: each of these is a first attempt, never a retry.
        if rill_mcp::KEYED_TOOLS.contains(&tool) {
            arguments["idempotencyKey"] = json!(format!("pre-flight-{}", rand::random::<u64>()));
        }
        let message = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": tool, "arguments": arguments }
        });
        handle(context, &message).expect("a request gets a reply")["result"].clone()
    }

    fn code(result: &Value) -> &str {
        result["structuredContent"]["code"]
            .as_str()
            .unwrap_or_else(|| panic!("no code in {result}"))
    }

    fn assert_refused(result: &Value, kind: RuleKind) {
        assert_eq!(code(result), "pre_flight_refused", "{result}");
        assert_eq!(
            result["structuredContent"]["rule"],
            kind.module(),
            "{result}"
        );
    }

    fn recipients_allowlist() -> CapabilityRule {
        CapabilityRule::RecipientAllowlist {
            addresses: vec![ALLOWED.into()],
        }
    }

    fn usdc_only() -> CapabilityRule {
        CapabilityRule::AssetScope {
            allowed_coin_types: vec![USDC.into()],
        }
    }

    fn spend_to(wallet: &str, to: &str) -> Value {
        json!({ "wallet": wallet, "cap": "0xcap", "amount": "0.01", "to": to })
    }

    fn swap_arguments(min_out: &str, accept_any: bool) -> Value {
        json!({
            "wallet": WALLET, "cap": "0xcap", "amount": "0.01", "pool": "0xpool",
            "coinTypeA": "0x2::sui::SUI", "coinTypeB": USDC, "a2b": true,
            "minOut": min_out, "acceptAnyOutput": accept_any
        })
    }

    #[test]
    fn rill_spend_refuses_a_recipient_off_the_allowlist() {
        let mut context = context(recipients_allowlist());
        let result = call(&mut context, "rill_spend", spend_to(WALLET, STRANGER));
        assert_refused(&result, RuleKind::RecipientAllowlist);
    }

    #[test]
    fn rill_spend_refuses_when_sui_is_outside_the_asset_scope() {
        let mut context = context(usdc_only());
        let result = call(&mut context, "rill_spend", spend_to(WALLET, ALLOWED));
        assert_refused(&result, RuleKind::AssetScope);
    }

    #[test]
    fn rill_swap_refuses_a_minout_below_the_manifest_floor() {
        let mut context = context(CapabilityRule::SlippageFloor {
            min_out_mist: "1000".into(),
            coin_type: Some(USDC.into()),
        });
        let result = call(&mut context, "rill_swap", swap_arguments("999", false));
        assert_refused(&result, RuleKind::SlippageFloor);
    }

    #[test]
    fn rill_swap_refuses_accept_any_output_under_a_floor() {
        let mut context = context(CapabilityRule::SlippageFloor {
            min_out_mist: "1000".into(),
            coin_type: None,
        });
        let result = call(&mut context, "rill_swap", swap_arguments("", true));
        assert_refused(&result, RuleKind::SlippageFloor);
    }

    #[test]
    fn rill_swap_refuses_a_coin_outside_the_asset_scope() {
        let mut context = context(CapabilityRule::AssetScope {
            allowed_coin_types: vec!["0x2::sui::SUI".into()],
        });
        let result = call(&mut context, "rill_swap", swap_arguments("5", false));
        assert_refused(&result, RuleKind::AssetScope);
    }

    #[test]
    fn rill_stake_refuses_when_hasui_is_outside_the_asset_scope() {
        let mut context = context(CapabilityRule::AssetScope {
            allowed_coin_types: vec!["0x2::sui::SUI".into()],
        });
        let result = call(
            &mut context,
            "rill_stake",
            json!({ "wallet": WALLET, "cap": "0xcap", "amount": "1" }),
        );
        assert_refused(&result, RuleKind::AssetScope);
    }

    /// The guarantee is per wallet: a run set for one wallet says nothing about another, so a
    /// spend from another wallet is not checked against it. It reaches the (closed) node instead.
    #[test]
    fn rill_spend_on_another_wallet_is_not_checked_against_this_run_set() {
        let mut context = context(recipients_allowlist());
        let result = call(
            &mut context,
            "rill_spend",
            spend_to(ANOTHER_WALLET, STRANGER),
        );
        assert_ne!(code(&result), "pre_flight_refused", "{result}");
    }

    /// An allowed spend is not refused by the check, and the same wallet spelt short is the same
    /// wallet.
    #[test]
    fn a_spend_inside_every_scope_passes_the_check() {
        let mut context = context(recipients_allowlist());
        let result = call(&mut context, "rill_spend", spend_to("0x1", ALLOWED));
        assert_ne!(code(&result), "pre_flight_refused", "{result}");
    }

    // ── rill_execute: protocol scope, read from the bytes ──────────────────────────────────────

    /// A transaction that requests a spend and then calls `extra`, built the way the server
    /// builds one.
    fn ptb_calling(extra: &str) -> String {
        use sui_sdk_types::{Address, Digest, Identifier};
        use sui_transaction_builder::{Function, ObjectInput, TransactionBuilder};

        let mut tx = TransactionBuilder::new();
        tx.set_sender(signer().parse::<Address>().unwrap());
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
        tx.move_call(
            Function::new(
                extra.parse().unwrap(),
                Identifier::new("m").unwrap(),
                Identifier::new("f").unwrap(),
            ),
            vec![],
        );
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .encode(bcs::to_bytes(&tx.try_build().unwrap()).unwrap())
    }

    fn execute_context(scope: &str, extra: &str) -> (WalletContext, Value) {
        let targets = vec![request_spend(), format!("{extra}::m::f")];
        let rule = CapabilityRule::ProtocolScope {
            allowed_packages: vec![scope.into()],
        };
        let mut context = WalletContext::new(Some(key()), "testnet".into(), false)
            .with_run_set(Some(run_set(rule, &targets)));
        context.rpc_url = Some(CLOSED.into());
        let ptb = ptb_calling(extra);
        let expires = (now_ms() + 60_000) / 1000;
        let envelope = json!({
            "version": "1",
            "actionId": "skill_hero",
            "actionDigest": rill_core::envelope::digest_unsigned_ptb(&ptb),
            "network": "testnet",
            "sender": signer(),
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
            "allowedTargets": targets,
            "requiredObjectIds": [WALLET],
            "requiredGuards": [],
            "unsignedPtb": ptb,
            "preview": "a call outside the scope",
            "simulation": {
                "ok": true, "verification": "verified", "gasEstimate": "2000000",
                "balanceChanges": [], "objectChanges": []
            },
            "expiresAt": iso(expires)
        });
        (context, envelope)
    }

    /// Seconds since the epoch as the ISO instant an envelope carries.
    fn iso(secs: u64) -> String {
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
            "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.000Z",
            rem / 3600,
            (rem / 60) % 60,
            rem % 60
        )
    }

    /// The run set pins the off-scope call, so the sequence check passes: only a comparison with
    /// the manifest's scope refuses it. A hand-written run set is exactly where that can happen.
    #[test]
    fn rill_execute_refuses_a_target_package_outside_the_protocol_scope() {
        let (mut context, envelope) = execute_context(ALLOWED, OFF_SCOPE);
        let result = call(
            &mut context,
            "rill_execute",
            json!({ "envelope": envelope }),
        );
        assert_refused(&result, RuleKind::ProtocolScope);
    }

    /// The same envelope with the package in scope passes every local check and reaches the node,
    /// so the scope is the only thing the refusal above answered to.
    #[test]
    fn rill_execute_passes_a_target_package_inside_the_protocol_scope() {
        let (mut context, envelope) = execute_context(OFF_SCOPE, OFF_SCOPE);
        let result = call(
            &mut context,
            "rill_execute",
            json!({ "envelope": envelope }),
        );
        assert_eq!(code(&result), "chain_unavailable", "{result}");
    }

    // ── the table: every claim refused, every refusal claimed ──────────────────────────────────

    /// A rule of `kind` and a call to `tool` that breaks it, or `None` for a pair with no input.
    fn violation(kind: RuleKind, tool: SignerTool) -> Option<(WalletContext, &'static str, Value)> {
        let (rule, name, arguments) = match (kind, tool) {
            (RuleKind::RecipientAllowlist, SignerTool::Spend) => (
                recipients_allowlist(),
                "rill_spend",
                spend_to(WALLET, STRANGER),
            ),
            (RuleKind::AssetScope, SignerTool::Spend) => {
                (usdc_only(), "rill_spend", spend_to(WALLET, ALLOWED))
            }
            (RuleKind::AssetScope, SignerTool::Swap) => (
                CapabilityRule::AssetScope {
                    allowed_coin_types: vec!["0x2::sui::SUI".into()],
                },
                "rill_swap",
                swap_arguments("5", false),
            ),
            (RuleKind::AssetScope, SignerTool::Stake) => (
                usdc_only(),
                "rill_stake",
                json!({ "wallet": WALLET, "cap": "0xcap", "amount": "1" }),
            ),
            (RuleKind::SlippageFloor, SignerTool::Swap) => (
                CapabilityRule::SlippageFloor {
                    min_out_mist: "1000".into(),
                    coin_type: None,
                },
                "rill_swap",
                swap_arguments("999", false),
            ),
            (RuleKind::ProtocolScope, SignerTool::Execute) => {
                let (context, envelope) = execute_context(ALLOWED, OFF_SCOPE);
                return Some((context, "rill_execute", json!({ "envelope": envelope })));
            }
            _ => return None,
        };
        Some((context(rule), name, arguments))
    }

    /// Iterates the table every claim is rendered from. A pair added to `signer_tools` without a
    /// violating input here fails, so a tool cannot be said to refuse a rule nobody has seen it
    /// refuse. `rill_run_action` and `rill_run_workflow` need a grant from the network; what they
    /// share with `rill_execute` is asserted on the source below.
    #[test]
    fn every_pre_flight_kind_is_refused_on_every_tool_that_claims_it() {
        for kind in RuleKind::all() {
            match kind.enforcement() {
                Enforcement::OnChain => assert!(
                    kind.signer_tools().is_empty(),
                    "{} is the chain's; the signer must not claim it",
                    kind.module()
                ),
                Enforcement::PreFlight => {
                    assert!(
                        !kind.signer_tools().is_empty(),
                        "{} is refused by no signer tool; say so or claim one",
                        kind.module()
                    );
                    for tool in kind.signer_tools() {
                        let (mut context, name, arguments) =
                            violation(kind, *tool).unwrap_or_else(|| {
                                panic!("no violating input for {} on {tool:?}", kind.module())
                            });
                        let result = call(&mut context, name, arguments);
                        assert_refused(&result, kind);
                    }
                }
            }
        }
    }

    const SOURCE: &str = include_str!("stdio.rs");

    fn shipped() -> &'static str {
        SOURCE.split("#[cfg(test)]").next().unwrap()
    }

    /// One function's body, from its signature to the next item at column zero.
    fn body(name: &str) -> &'static str {
        let shipped = shipped();
        let start = shipped
            .find(&format!("\nfn {name}("))
            .unwrap_or_else(|| panic!("no fn {name}"));
        let rest = &shipped[start + 1..];
        &rest[..rest.find("\n}\n").expect("the function closes")]
    }

    /// The two granted tools run the very execution `rill_execute` runs, under the grant's run
    /// set, so every rule it refuses they refuse.
    #[test]
    fn the_granted_tools_run_the_execute_path_under_the_grants_run_set() {
        let action = body("run_action_for_owner");
        assert!(action.contains("context.run_set.replace(verified.run_set.clone())"));
        assert!(action.contains("execute(") && action.contains("EnvelopeSource::Grant"));
        assert!(body("run_workflow").contains("run_action_for_owner("));
        assert!(body("run_action").contains("run_action_for_owner("));
    }

    /// Both checks come before the first network call in each local build, which is what makes a
    /// refusal free and testable without a node.
    #[test]
    fn every_local_build_checks_before_it_reaches_the_network() {
        for tool in ["spend", "swap", "stake"] {
            let body = body(tool);
            let network = body
                .find("block_on(")
                .expect("a local build reaches the chain");
            for check in ["local_pre_flight(", "granted_wallet_gate("] {
                let at = body
                    .find(check)
                    .unwrap_or_else(|| panic!("{tool} never calls {check}"));
                assert!(at < network, "{tool} calls {check} after the network");
            }
        }
        let execute = body("execute");
        let gate = execute
            .find("granted_wallet_gate(")
            .expect("rill_execute checks for a grant");
        assert!(gate < execute.find("block_on(").unwrap());
    }

    // ── the wallet read lists only this wallet's run set ──────────────────────────────────────

    #[test]
    fn rill_wallet_omits_the_run_sets_pre_flight_rules_for_another_wallet() {
        let context = context(recipients_allowlist());
        assert!(run_set_for(&context, ANOTHER_WALLET).is_none());
        assert!(run_set_for(&context, "0x1").is_some(), "0x1 is WALLET");
        assert!(
            body("wallet").contains("run_set_for(context, wallet)"),
            "the wallet read must take only the run set for the wallet it was asked about"
        );
    }

    // ── a wallet the owner granted actions on goes through the grant ──────────────────────────

    fn grant(wallet: &str, action: &str, verdict: Result<(), Refusal>) -> CheckedGrant {
        let run_set =
            serde_json::to_value(run_set(recipients_allowlist(), &[request_spend()])).unwrap();
        let signed = SignedGrant {
            grant: Grant {
                network: "testnet".into(),
                action_id: action.into(),
                action_name: action.into(),
                agent: signer(),
                wallet_id: wallet.into(),
                wallet_package_id: PKG.into(),
                expires_at_ms: "9999999999999".into(),
                revision: 1,
                run_set,
                build_arguments: json!({}),
            },
            signature: "sig".into(),
        };
        let verdict = verdict.map(|()| VerifiedGrant {
            grant: signed.grant.clone(),
            run_set: RunSet::from_value(&signed.grant.run_set).unwrap(),
            owner: ALLOWED.into(),
        });
        (signed, verdict)
    }

    #[test]
    fn only_a_usable_grant_for_this_wallet_routes_it_through_the_grant() {
        let checked = vec![
            grant(WALLET, "skill_a", Ok(())),
            grant(WALLET, "skill_b", Err(Refusal::Expired)),
            grant(
                WALLET,
                "skill_c",
                Err(Refusal::Superseded { floor: 2, given: 1 }),
            ),
            grant(ANOTHER_WALLET, "skill_d", Ok(())),
        ];
        assert_eq!(usable_grants_for(&checked, "0x1"), vec!["skill_a"]);
        assert!(usable_grants_for(&checked[1..3], WALLET).is_empty());
    }

    /// A grant for this wallet the node could not check is not a wallet without a grant. Read as
    /// one, a node that serves object reads and fails signature checks would let a local build
    /// spend the wallet outside the grant's scopes.
    #[test]
    fn a_grant_the_node_could_not_check_leaves_the_wallet_unknown() {
        let unchecked = || Err(Refusal::NodeUnavailable("the node could not answer".into()));
        let checked = vec![
            grant(WALLET, "skill_a", unchecked()),
            grant(WALLET, "skill_b", Err(Refusal::Expired)),
            grant(ANOTHER_WALLET, "skill_c", unchecked()),
        ];
        assert_eq!(
            wallet_gate(&checked, "0x1"),
            WalletGate::Unknown(vec!["skill_a".into()])
        );
        // A usable grant decides it: the wallet is granted whatever else is unknown.
        let mut with_usable = checked.clone();
        with_usable.push(grant(WALLET, "skill_d", Ok(())));
        assert_eq!(
            wallet_gate(&with_usable, WALLET),
            WalletGate::Granted(vec!["skill_d".into()])
        );
        // Definite refusals, and unknowns for another wallet, leave this wallet open.
        assert_eq!(wallet_gate(&checked[1..], WALLET), WalletGate::Open);
    }

    /// Unknown is refused as an unreadable grant list is: this signer cannot tell whether the
    /// wallet must be spent through a grant, so it does not build for it.
    #[test]
    fn an_unknown_grant_refuses_with_grants_unavailable() {
        let mut context = context(recipients_allowlist());
        let refused = gate_refusal(
            &mut context,
            &json!(1),
            WALLET,
            WalletGate::Unknown(vec!["skill_a".into()]),
        )
        .expect("a grant nobody could check is a refusal");
        assert_eq!(code(&refused["result"]), "grants_unavailable", "{refused}");
        let text = refused["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("skill_a"), "{text}");
        assert!(gate_refusal(&mut context, &json!(1), WALLET, WalletGate::Open).is_none());
    }

    #[test]
    fn a_granted_wallet_is_refused_with_use_granted_action() {
        let mut context = context(recipients_allowlist());
        let result = use_granted_action(&mut context, json!(1), WALLET, &["skill_a".into()]);
        assert_eq!(code(&result["result"]), "use_granted_action", "{result}");
        let text = result["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("rill_run_action"), "{text}");
    }

    /// Pure local mode: with no Rill API there are no grants to route through, and the local
    /// builds keep working exactly as before.
    #[test]
    fn without_an_api_the_grant_check_is_skipped() {
        let mut context = context(recipients_allowlist());
        assert!(granted_wallet_gate(&mut context, &json!(1), WALLET, None).is_none());
    }

    /// With an API configured and unreachable, this signer cannot know whether the wallet is
    /// granted, so it refuses rather than guessing that it is not.
    #[test]
    fn an_unreachable_api_refuses_with_grants_unavailable() {
        let state_dir = std::env::temp_dir().join(format!(
            "rill-stdio-gate-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let mut context = context(recipients_allowlist()).with_state_dir(Some(state_dir.clone()));
        let refused = granted_wallet_gate(&mut context, &json!(1), WALLET, Some(CLOSED))
            .expect("an unreachable API is a refusal");
        assert_eq!(code(&refused["result"]), "grants_unavailable", "{refused}");
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    /// A signer restarted right after its own submission must still refuse a build that pays
    /// with the gas reference that submission consumed.
    #[test]
    fn a_restarted_signer_still_knows_the_gas_its_last_submission_consumed() {
        let state_dir = std::env::temp_dir().join(format!(
            "rill-stdio-gas-fence-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let coin: sui_sdk_types::Address = WALLET.parse().unwrap();
        let mut first = context(recipients_allowlist()).with_state_dir(Some(state_dir.clone()));
        let spent = sui_sdk_types::ObjectReference::new(coin, 41, sui_sdk_types::Digest::ZERO);
        let warning = remember_consumed_gas(&mut first, std::slice::from_ref(&spent));
        assert!(warning.is_none(), "{warning:?}");

        let restarted = context(recipients_allowlist()).with_state_dir(Some(state_dir.clone()));
        assert!(
            restarted.consumed_gas.is_empty(),
            "the restart starts with an empty memory"
        );
        let fence = gas_fence(&restarted);
        assert!(crate::workflow::gas_is_stale(&[spent], &fence), "{fence:?}");
        let newer = sui_sdk_types::ObjectReference::new(coin, 42, sui_sdk_types::Digest::ZERO);
        assert!(!crate::workflow::gas_is_stale(&[newer], &fence));
        let _ = std::fs::remove_dir_all(&state_dir);
    }
}
