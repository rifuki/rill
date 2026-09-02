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

use std::io::{BufRead, Write};

use serde_json::{json, Value};

use crate::keystore::Keystore;
use crate::runset::RunSet;

/// Protocol versions this signer speaks.
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
const LATEST_PROTOCOL_VERSION: &str = "2025-06-18";

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
}

impl WalletContext {
    pub fn new(keystore: Option<Keystore>, network: String, mainnet_allowed: bool) -> Self {
        Self {
            keystore,
            run_set: None,
            network,
            mainnet_allowed,
            last_rejection: None,
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
    rpc_result(
        id,
        json!({
            "content": [{ "type": "text", "text": message }],
            "structuredContent": { "code": code, "message": message },
            "isError": true
        }),
    )
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
            let version = requested
                .filter(|v| SUPPORTED_PROTOCOL_VERSIONS.contains(v))
                .unwrap_or(LATEST_PROTOCOL_VERSION);
            Some(rpc_result(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": {} },
                    "serverInfo": {
                        "name": "rill",
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
        "rill_status" => status(context, id, &params),
        "rill_spend" => spend(context, id, &params),
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
/// Submission is not wired: the validation chain ends at [`rill_policy::Simulated`], which is the
/// only type that can be signed, and going further needs a live fullnode this function does not
/// have. What is proven here is that a bad envelope never reaches that type.
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

/// Everything read-only, in one answer.
///
/// Four tools used to say this — readiness, capabilities, a wallet's limits, the last refusal — and
/// they all answered the same question. Four names for one question is four things an agent has to
/// learn before it can ask, and three of them return most of the same fields.
///
/// The write is deliberately still separate; see the note in `rill-mcp`.
fn status(context: &mut WalletContext, id: Value, params: &Value) -> Value {
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

    // The live read, only when a wallet was named. It costs a round trip, so it is not done for a
    // caller that only asked whether the signer is up.
    if let Some(wallet) = argument(params, "wallet") {
        let package = std::env::var("AGENT_WALLET_PACKAGE_ID")
            .unwrap_or_else(|_| rill_ptb::deployments::TESTNET_AGENT_WALLET.to_string());
        match block_on(crate::wallet_read::read_limits(
            &endpoint(context),
            &package,
            wallet,
        )) {
            Ok(Ok(limits)) => answer["wallet"] = limits,
            Ok(Err(e)) | Err(e) => {
                context.last_rejection = Some(e.clone());
                return tool_error(id, "read_failed", &e);
            }
        }
    }

    tool_ok(id, answer)
}

/// Read a wallet's limits from the chain that enforces them.
///
/// Not from the run-set, and not from anything this process was told at startup. A limit reported
/// from a local copy is a limit an agent could be shown after it had already changed.
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
        let reason = "Refusing to sign on mainnet without RILL_ALLOW_MAINNET=true.".to_string();
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

    let package = std::env::var("AGENT_WALLET_PACKAGE_ID")
        .unwrap_or_else(|_| rill_ptb::deployments::TESTNET_AGENT_WALLET.to_string());
    let version = std::env::var("AGENT_WALLET_VERSION_ID").unwrap_or_else(|_| {
        "0xd4f88a6dc271f923f0e55dd96eb8f8762ed4d45199c6719ae92365694478fd65".to_string()
    });

    let args = crate::spend_cmd::SpendArgs {
        package_id: package,
        version_id: version,
        wallet_id: wallet.to_string(),
        cap_id: cap.to_string(),
        amount: amount.to_string(),
        recipient: argument(params, "to").map(str::to_owned),
        gas_budget: 100_000_000,
        dry_run: false,
    };

    match block_on(crate::spend_cmd::spend_json(
        &endpoint(context),
        keystore,
        &args,
    )) {
        Ok(Ok(result)) => tool_ok(id, result),
        Ok(Err(e)) | Err(e) => {
            context.last_rejection = Some(e.clone());
            tool_error(id, "refused", &e)
        }
    }
}

fn execute(context: &mut WalletContext, id: Value, params: &Value) -> Value {
    let Some(run_set) = context.run_set.as_ref() else {
        let reason = "No run-set is loaded, so there are no pinned limits to validate against. \
                      Refusing to sign rather than signing against limits nobody set.";
        context.last_rejection = Some(reason.to_string());
        return tool_error(id, "no_run_set", reason);
    };
    if context.keystore.is_none() {
        let reason = "No signing key is configured.";
        context.last_rejection = Some(reason.to_string());
        return tool_error(id, "no_key", reason);
    }
    // Mainnet needs an explicit opt-in, and it is checked before anything is parsed — the cheapest
    // possible place to stop.
    if run_set.network == rill_core::envelope::Network::Mainnet && !context.mainnet_allowed {
        let reason = "Refusing to sign on mainnet without RILL_ALLOW_MAINNET=true.";
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
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let validated = match rill_policy::RawEnvelope::new(envelope).validate(&policy, now_ms) {
        Ok(v) => v,
        Err(rejection) => {
            let reason = rejection.to_string();
            context.last_rejection = Some(reason.clone());
            return tool_error(id, "policy_rejection", &reason);
        }
    };
    let pinned = match validated.pin_bytes() {
        Ok(p) => p,
        Err(rejection) => {
            let reason = rejection.to_string();
            context.last_rejection = Some(reason.clone());
            return tool_error(id, "policy_rejection", &reason);
        }
    };

    tool_ok(
        id,
        json!({
            "validated": true,
            "digest": pinned.pinned_digest(),
            "spendBaseUnits": pinned.envelope().resolved_params.as_ref()
                .map(|p| p.spend_amount_mist.clone()),
            "note": "The envelope passed every local check and is byte-pinned. Submission is not \
                     wired on this build: the next step re-simulates against a live fullnode, and \
                     only the type produced by that step can be signed."
        }),
    )
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
        assert_eq!(out[0]["result"]["serverInfo"]["name"], "rill");
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
            "sender": "0xagent",
            "actionId": "skill_hero",
            "walletPackageId": "0xpkg",
            "walletId": "0xwallet",
            "agentCapId": "0xcap",
            "versionId": "0xversion",
            "capabilityManifest": {
                "walletCoinType": "0x2::sui::SUI",
                "rules": [{ "kind": "budget", "totalMist": "5000000000" }]
            },
            "allowedTargets": ["0xpkg::agent_wallet::request_spend"],
            "allowedObjectIds": ["0xwallet"],
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

    const PTB: &str = "AAA=";

    fn envelope(overrides: serde_json::Value) -> Value {
        let mut base = json!({
            "version": "1",
            "actionId": "skill_hero",
            "actionDigest": rill_core::envelope::digest_unsigned_ptb(PTB),
            "network": "testnet",
            "sender": "0xagent",
            "walletPackageId": "0xpkg",
            "walletId": "0xwallet",
            "agentCapId": "0xcap",
            "balanceManagerId": "0xbm",
            "tradeCapId": "0xtc",
            "resolvedParams": {
                "poolKey": "DEEP_SUI", "poolId": "0xpool", "clientOrderId": "1",
                "spendAmountMist": "1000000000", "price": "2.5", "quantity": "1",
                "depositSui": "1", "isBid": true, "payWithDeep": false
            },
            "allowedTargets": ["0xpkg::agent_wallet::request_spend"],
            "requiredObjectIds": ["0xwallet"],
            "requiredGuards": [],
            "unsignedPtb": PTB,
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

    #[test]
    fn a_good_envelope_passes_every_local_check_and_is_byte_pinned() {
        let mut ctx = context_with_run_set();
        let out = execute_with(&mut ctx, envelope(json!({})));
        assert_eq!(out["result"]["isError"], false, "{out}");
        assert_eq!(out["result"]["structuredContent"]["validated"], true);
        assert!(out["result"]["structuredContent"]["digest"].is_string());
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
