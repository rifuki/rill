//! The idempotency key over the MCP transport: required before anything is read, and a stored
//! outcome answered without touching the network.
//!
//! Every context here is mainnet without the opt-in, with the node and the Rill API pinned to a
//! closed local port. Before the key existed each of these calls stopped at a check of its own
//! (`mainnet_not_opted_in`, an unknown argument, an unreachable API); now the key is checked and a
//! receipt answered first, and still nothing leaves this machine.

use rill_cli::keystore::Keystore;
use rill_cli::stdio::{handle, WalletContext};
use serde_json::{json, Value};
use sui_crypto::ed25519::Ed25519PrivateKey;

/// Nothing listens here, so a network call fails at once and stays on this machine.
const CLOSED: &str = "http://127.0.0.1:1";
const WALLET: &str = "0x0000000000000000000000000000000000000000000000000000000000000abc";
const CAP: &str = "0x0000000000000000000000000000000000000000000000000000000000000cab";

fn key() -> Keystore {
    let encoded = Ed25519PrivateKey::new([71u8; 32]).to_suiprivkey().unwrap();
    Keystore::from_suiprivkey(&encoded).unwrap()
}

fn state_dir() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "rill-idempotency-transport-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ))
}

fn context(state_dir: Option<std::path::PathBuf>) -> WalletContext {
    let mut context = WalletContext::new(Some(key()), "mainnet".into(), false)
        .with_owner_tools(true)
        .with_state_dir(state_dir);
    context.rpc_url = Some(CLOSED.into());
    context.api_url = Some(CLOSED.into());
    context
}

/// Where a key's receipt lives for this signer on mainnet.
fn receipts(state_dir: &std::path::Path) -> std::path::PathBuf {
    state_dir
        .join("requests")
        .join("mainnet")
        .join(key().address().to_string())
}

/// A receipt written the way the store writes one, without going through the store, so these
/// tests say what is on disk rather than trusting the code under test to have put it there.
fn plant(state_dir: &std::path::Path, key: &str, request: Value, status: &str, result: Value) {
    let dir = receipts(state_dir);
    std::fs::create_dir_all(&dir).unwrap();
    let receipt = json!({
        "version": 1, "idempotencyKey": key, "request": request, "status": status,
        "claimedAtMs": 1_700_000_000_000u64, "replayed": false, "result": result
    });
    std::fs::write(
        dir.join(format!("{key}.json")),
        serde_json::to_vec(&receipt).unwrap(),
    )
    .unwrap();
}

fn landed(digest: &str) -> Value {
    let data = json!({
        "submitted": true, "digest": digest,
        "note": "Submitted and confirmed. This cannot be undone, and calling again sends a second payment."
    });
    json!({
        "content": [{"type": "text", "text": data.to_string()}],
        "structuredContent": data,
        "isError": false
    })
}

fn call(context: &mut WalletContext, tool: &str, arguments: Value) -> Value {
    handle(
        context,
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": { "name": tool, "arguments": arguments }
        }),
    )
    .expect("a request gets a reply")["result"]
        .clone()
}

fn with_key(mut arguments: Value, key: &str) -> Value {
    arguments["idempotencyKey"] = json!(key);
    arguments
}

fn spend_arguments(amount: &str) -> Value {
    json!({"wallet": WALLET, "cap": CAP, "amount": amount})
}

#[test]
fn spend_replays_a_stored_receipt_without_touching_the_network() {
    let dir = state_dir();
    plant(
        &dir,
        "pay-1",
        json!({"tool": "rill_spend", "arguments": spend_arguments("0.01")}),
        "completed",
        landed("STORED"),
    );
    let mut context = context(Some(dir.clone()));
    let result = call(
        &mut context,
        "rill_spend",
        with_key(spend_arguments("0.01"), "pay-1"),
    );
    assert_eq!(result["isError"], false, "{result}");
    let out = &result["structuredContent"];
    assert_eq!(out["replayed"], true, "{result}");
    assert_eq!(out["digest"], "STORED", "{result}");
    assert_eq!(out["idempotencyKey"], "pay-1", "{result}");
    // The command's own note says what a new call does; the answer also says what this key does.
    let note = out["note"].as_str().unwrap();
    assert!(
        note.contains("Under this idempotencyKey a repeat returns"),
        "{note}"
    );
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("\"replayed\": true"),
        "the text carries the fields: {text}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn spend_without_an_idempotency_key_is_refused_before_anything() {
    let dir = state_dir();
    let mut context = context(Some(dir.clone()));
    let result = call(&mut context, "rill_spend", spend_arguments("0.01"));
    assert_eq!(result["isError"], true, "{result}");
    assert_eq!(
        result["structuredContent"]["code"], "missing_idempotency_key",
        "{result}"
    );
    assert!(!dir.exists(), "a refused call must not create a receipt");
}

#[test]
fn every_keyed_tool_requires_the_key() {
    let mut context = context(Some(state_dir()));
    for tool in [
        "rill_create_wallet",
        "rill_spend",
        "rill_swap",
        "rill_stake",
        "rill_unstake",
        "rill_run_action",
    ] {
        let result = call(&mut context, tool, json!({"amount": "1"}));
        assert_eq!(
            result["structuredContent"]["code"], "missing_idempotency_key",
            "{tool}: {result}"
        );
    }
    for invalid in [json!("../escape"), json!(7), json!("")] {
        let result = call(
            &mut context,
            "rill_spend",
            json!({"wallet": WALLET, "cap": CAP, "amount": "0.01", "idempotencyKey": invalid}),
        );
        assert!(
            ["missing_idempotency_key", "invalid_arguments"]
                .contains(&result["structuredContent"]["code"].as_str().unwrap()),
            "{invalid}: {result}"
        );
    }
}

/// With no state directory there is nowhere to keep the claim, and a call that cannot be claimed
/// is refused rather than submitted unkeyed.
#[test]
fn a_signer_without_a_state_directory_refuses_keyed_calls() {
    let mut context = context(None);
    let result = call(
        &mut context,
        "rill_spend",
        with_key(spend_arguments("0.01"), "pay-none"),
    );
    assert_eq!(
        result["structuredContent"]["code"], "not_configured",
        "{result}"
    );
}

#[test]
fn a_running_receipt_answers_request_interrupted() {
    let dir = state_dir();
    plant(
        &dir,
        "pay-run",
        json!({"tool": "rill_spend", "arguments": spend_arguments("0.01")}),
        "running",
        Value::Null,
    );
    let mut context = context(Some(dir.clone()));
    let result = call(
        &mut context,
        "rill_spend",
        with_key(spend_arguments("0.01"), "pay-run"),
    );
    let out = &result["structuredContent"];
    assert_eq!(result["isError"], true, "{result}");
    assert_eq!(out["code"], "request_interrupted", "{result}");
    assert_eq!(out["signer"], key().address().to_string(), "{result}");
    assert_eq!(out["network"], "mainnet", "{result}");
    assert_eq!(out["claimedAtMs"], 1_700_000_000_000u64, "{result}");
    assert_eq!(out["replayed"], true, "{result}");
    let _ = std::fs::remove_dir_all(dir);
}

/// The claim another signer process made and has not written yet: the loser of a race.
#[test]
fn an_empty_receipt_answers_request_interrupted() {
    let dir = state_dir();
    std::fs::create_dir_all(receipts(&dir)).unwrap();
    std::fs::write(receipts(&dir).join("pay-empty.json"), "").unwrap();
    let mut context = context(Some(dir.clone()));
    let result = call(
        &mut context,
        "rill_spend",
        with_key(spend_arguments("0.01"), "pay-empty"),
    );
    assert_eq!(
        result["structuredContent"]["code"], "request_interrupted",
        "{result}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn reusing_a_key_with_other_arguments_is_idempotency_key_conflict() {
    let dir = state_dir();
    plant(
        &dir,
        "pay-5",
        json!({"tool": "rill_spend", "arguments": spend_arguments("0.01")}),
        "completed",
        landed("STORED"),
    );
    let mut context = context(Some(dir.clone()));
    let result = call(
        &mut context,
        "rill_spend",
        with_key(spend_arguments("0.02"), "pay-5"),
    );
    let out = &result["structuredContent"];
    assert_eq!(out["code"], "idempotency_key_conflict", "{result}");
    assert_eq!(
        out["storedRequest"]["arguments"]["amount"], "0.01",
        "{result}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// A key is one call to one tool: the same key and arguments sent to another tool conflict.
#[test]
fn a_key_belongs_to_one_tool() {
    let dir = state_dir();
    plant(
        &dir,
        "pay-6",
        json!({"tool": "rill_spend", "arguments": spend_arguments("1")}),
        "completed",
        landed("STORED"),
    );
    let mut context = context(Some(dir.clone()));
    let result = call(
        &mut context,
        "rill_stake",
        with_key(spend_arguments("1"), "pay-6"),
    );
    assert_eq!(
        result["structuredContent"]["code"], "idempotency_key_conflict",
        "{result}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn unstake_accepts_an_idempotency_key_and_replays() {
    let dir = state_dir();
    let arguments = json!({"amount": "1", "minOut": "0.9"});
    plant(
        &dir,
        "redeem-1",
        json!({"tool": "rill_unstake", "arguments": arguments}),
        "completed",
        landed("REDEEMED"),
    );
    let mut context = context(Some(dir.clone()));
    let result = call(
        &mut context,
        "rill_unstake",
        with_key(arguments, "redeem-1"),
    );
    let out = &result["structuredContent"];
    assert_eq!(out["replayed"], true, "{result}");
    assert_eq!(out["digest"], "REDEEMED", "{result}");
    let _ = std::fs::remove_dir_all(dir);
}

/// A dry run submits nothing, so it needs no key and leaves no receipt.
#[test]
fn an_unstake_dry_run_needs_no_key() {
    let dir = state_dir();
    let mut context = context(Some(dir.clone()));
    let result = call(
        &mut context,
        "rill_unstake",
        json!({"amount": "1", "minOut": "0.9", "dryRun": true}),
    );
    assert_ne!(
        result["structuredContent"]["code"], "missing_idempotency_key",
        "{result}"
    );
    assert!(!dir.exists(), "a dry run must not create a receipt");
}

#[test]
fn run_action_replays_without_reading_grants() {
    let dir = state_dir();
    let arguments = json!({"actionId": "skill_swap"});
    plant(
        &dir,
        "run-1",
        json!({"tool": "rill_run_action", "arguments": arguments}),
        "completed",
        landed("RAN"),
    );
    let mut context = context(Some(dir.clone()));
    let result = call(
        &mut context,
        "rill_run_action",
        with_key(arguments, "run-1"),
    );
    let out = &result["structuredContent"];
    assert_eq!(out["replayed"], true, "{result}");
    assert_eq!(out["digest"], "RAN", "{result}");
    let _ = std::fs::remove_dir_all(dir);
}

/// A first call is claimed on disk before the tool runs, and its outcome, even a refusal, is what
/// the same key answers with afterwards.
#[test]
fn a_first_call_is_claimed_and_its_refusal_replayed() {
    let dir = state_dir();
    let mut context = context(Some(dir.clone()));
    let first = call(
        &mut context,
        "rill_spend",
        with_key(spend_arguments("0.01"), "pay-new"),
    );
    // Mainnet without the opt-in: refused before anything is built.
    assert_eq!(
        first["structuredContent"]["code"], "mainnet_not_opted_in",
        "{first}"
    );
    assert_eq!(first["structuredContent"]["replayed"], false, "{first}");
    let stored: Value =
        serde_json::from_slice(&std::fs::read(receipts(&dir).join("pay-new.json")).unwrap())
            .unwrap();
    assert_eq!(stored["status"], "stopped", "{stored}");
    assert!(
        stored["request"]["arguments"]
            .get("idempotencyKey")
            .is_none(),
        "the key is the receipt's name, not part of the request: {stored}"
    );

    // Opting in afterwards does not turn the stored refusal into a submission under the same key.
    context.mainnet_allowed = true;
    let again = call(
        &mut context,
        "rill_spend",
        with_key(spend_arguments("0.01"), "pay-new"),
    );
    assert_eq!(
        again["structuredContent"]["code"], "mainnet_not_opted_in",
        "{again}"
    );
    assert_eq!(again["structuredContent"]["replayed"], true, "{again}");
    let _ = std::fs::remove_dir_all(dir);
}
