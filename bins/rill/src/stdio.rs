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
                Failed::Submit(reason) => tool_error(id, "submit_failed", &reason),
            };
        }
        Err(reason) => return tool_error(id, "chain_unavailable", &reason),
    };

    if let Some(error) = &outcome.error {
        context.last_rejection = Some(error.clone());
        return tool_error(id, "execution_failed", error);
    }

    tool_ok(
        id,
        json!({
            "submitted": true,
            "digest": outcome.digest,
            "pinnedDigest": digest,
            "callSequence": targets,
            "gasUsed": outcome.gas_used_mist,
            "spendBaseUnits": spend_base_units,
            "note": "Submitted and confirmed. This cannot be undone, and calling again with the \
                     same envelope submits a second transaction."
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
