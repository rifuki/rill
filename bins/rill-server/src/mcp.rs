//! The owner-scoped MCP endpoint.
//!
//! One URL a user pastes once. What it serves is every action the address behind the access token
//! has published — so publishing another action later needs no reconnection.
//!
//! # Why an empty catalogue is not an error
//!
//! A user who has connected but published nothing must still complete the MCP handshake and see an
//! empty `list_actions`. Failing here would make their agent report the connector itself as broken
//! when nothing is wrong, and "it says it can't connect" is a much worse first experience than
//! "there's nothing here yet".
//!
//! # Why an unknown action and someone else's action look identical
//!
//! `tools/call` on an id belonging to another address answers exactly as it does for an id that
//! does not exist. Distinguishing them would turn this endpoint into a way to discover which ids
//! are real.

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use rill_auth::tokens::{bearer_from_header, verify_token, Expectation, TokenKind};
use rill_store::SkillStore;
use serde_json::{json, Value};

use crate::state::AppState;

/// Protocol versions this server speaks. A client's version is echoed when recognised; otherwise
/// the newest is offered, which is what the transport expects.
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
const LATEST_PROTOCOL_VERSION: &str = "2025-06-18";

/// The `WWW-Authenticate` value every 401 must carry, so a client can discover where to
/// authenticate instead of reporting a dead connector.
pub fn discovery_header(public_base_url: &str) -> String {
    format!(
        "Bearer realm=\"rill\", resource_metadata=\"{}/.well-known/oauth-protected-resource\"",
        public_base_url.trim_end_matches('/')
    )
}

fn unauthorized(state: &AppState, description: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(
            header::WWW_AUTHENTICATE,
            discovery_header(&state.config.public_base_url),
        )],
        Json(json!({ "error": "invalid_token", "error_description": description })),
    )
        .into_response()
}

/// Who is calling, established from the bearer token alone.
///
/// The error side is a fully-formed `Response` rather than an error code, because a 401 here has
/// to carry the `WWW-Authenticate` discovery header and only this function knows the base URL to
/// build it from. Boxed to keep the `Result` small.
fn authenticate(state: &AppState, authorization: Option<&str>) -> Result<String, Box<Response>> {
    let Some(token) = bearer_from_header(authorization) else {
        return Err(Box::new(unauthorized(
            state,
            "An OAuth 2.1 access token is required.",
        )));
    };
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let claims = verify_token(
        token,
        &state.config.oauth_secret,
        Expectation {
            // Access only. A refresh token replayed here fails on the signed `t` claim.
            kind: TokenKind::Access,
            audience: &state.config.resource(),
            now_secs,
        },
    )
    .map_err(|e| Box::new(unauthorized(state, &e.to_string())))?;

    if !claims.scope.split_whitespace().any(|s| s == "mcp") {
        return Err(Box::new(
            (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "error": "insufficient_scope",
                    "error_description": "The \"mcp\" scope is required."
                })),
            )
                .into_response(),
        ));
    }
    Ok(claims.sub)
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// Handle one JSON-RPC message. Returns `None` for a notification, which gets no response at all.
async fn handle_one(state: &AppState, owner: &str, message: &Value) -> Option<Value> {
    let method = message.get("method").and_then(Value::as_str)?;
    // A message with no `id` is a notification. Not answered — not even with a null id, which is
    // what a spec-violating implementation does and what makes a client wait for a reply that
    // never comes.
    let has_id = message.get("id").is_some();
    if !has_id && !method.starts_with("notifications/") {
        return Some(rpc_error(
            Value::Null,
            -32600,
            "Invalid Request: \"id\" is required for a non-notification request.",
        ));
    }
    if !has_id {
        return None;
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
                        "name": "rill-actions",
                        "version": env!("CARGO_PKG_VERSION"),
                        "description": "Keyless action builder — returns an unsigned ExecutionEnvelope for local signing."
                    }
                }),
            ))
        }
        "ping" => Some(rpc_result(id, json!({}))),
        "tools/list" => {
            let tools: Vec<Value> = rill_mcp::tools(rill_mcp::Surface::Actions)
                .into_iter()
                .map(|t| serde_json::to_value(t).unwrap_or(Value::Null))
                .collect();
            Some(rpc_result(id, json!({ "tools": tools })))
        }
        "tools/call" => Some(handle_tool_call(state, owner, id, message).await),
        other => Some(rpc_error(id, -32601, &format!("Method not found: {other}"))),
    }
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

async fn handle_tool_call(state: &AppState, owner: &str, id: Value, message: &Value) -> Value {
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return rpc_error(id, -32602, "tools/call requires a tool name.");
    };
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

    // Runs on every call, before anything is dispatched. This server holds no key, and must never
    // be talked into behaving as though it does.
    if let Err(reason) = rill_mcp::assert_keyless_arguments(&arguments) {
        return tool_error(id, "forbidden_arguments", &reason);
    }

    let catalogue = state.skills.list_by_owner(owner);
    match name {
        "rill_list_actions" => {
            let actions: Vec<Value> = catalogue
                .iter()
                .map(|s| {
                    json!({
                        "actionId": s.id,
                        "name": s.name,
                        "description": s.description,
                        "network": state.config.network.as_str(),
                    })
                })
                .collect();
            tool_ok(id, json!({ "actions": actions }))
        }
        "rill_describe_action" => {
            let wanted = arguments.get("actionId").and_then(Value::as_str);
            match wanted.and_then(|w| catalogue.iter().find(|s| s.id == w)) {
                Some(skill) => tool_ok(
                    id,
                    json!({
                        "actionId": skill.id,
                        "name": skill.name,
                        "description": skill.description,
                        "network": state.config.network.as_str(),
                        "simulationRule": "Rill Cloud and rill-wallet both require a verified, successful simulation.",
                        "signingRule": "Only the local rill-wallet may validate, re-simulate, sign, and submit."
                    }),
                ),
                // Deliberately the same answer as for an id that does not exist.
                None => tool_error(
                    id,
                    "action_unavailable",
                    "Action is not available from this endpoint.",
                ),
            }
        }
        "rill_build_action" => build_action(state, &catalogue, id, &arguments).await,
        other => rpc_error(id, -32602, &format!("Unknown tool: {other}")),
    }
}

/// `POST /mcp`.
///
/// Authentication comes **before** parsing, deliberately. Parsing an unauthenticated caller's body
/// first would hand them a free error oracle — they would learn that this endpoint speaks
/// JSON-RPC, and could tell a malformed request from an unauthorized one. Someone without a token
/// should learn exactly one thing: that they need one, and where to get it.
pub async fn post(
    state: AppState,
    authorization: Option<&str>,
    body: axum::body::Bytes,
) -> Response {
    let owner = match authenticate(&state, authorization) {
        Ok(o) => o,
        Err(response) => return *response,
    };

    // Only now is the body worth looking at. A parse failure is JSON-RPC 2.0 §5.1's parse error.
    let body: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": { "code": -32700, "message": "Parse error" }
                })),
            )
                .into_response()
        }
    };

    // A JSON-RPC batch is an array. Responses come back in an array too, omitting notifications —
    // and a batch of nothing but notifications gets no body at all, same as a single one.
    if let Value::Array(messages) = &body {
        if messages.is_empty() {
            return (
                StatusCode::BAD_REQUEST,
                Json(rpc_error(
                    Value::Null,
                    -32600,
                    "Invalid Request: batch must not be empty.",
                )),
            )
                .into_response();
        }
        let mut responses: Vec<Value> = Vec::new();
        for message in messages {
            if let Some(response) = handle_one(&state, &owner, message).await {
                responses.push(response);
            }
        }
        if responses.is_empty() {
            return StatusCode::ACCEPTED.into_response();
        }
        return Json(Value::Array(responses)).into_response();
    }

    match handle_one(&state, &owner, &body).await {
        Some(response) => Json(response).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

/// Compile and simulate one action, returning an unsigned envelope or a named refusal.
///
/// A refusal is surfaced as an MCP tool error rather than as content, so an agent cannot mistake
/// `structuredContent` for something signable. That is the same reason the build path returns a
/// distinct type rather than an envelope with a flag.
async fn build_action(
    state: &AppState,
    catalogue: &[rill_store::PublishedSkill],
    id: Value,
    arguments: &Value,
) -> Value {
    let Some(action_id) = arguments.get("actionId").and_then(Value::as_str) else {
        return tool_error(id, "invalid_arguments", "actionId is required.");
    };
    // Same answer as for an id that does not exist — see the module note.
    if !catalogue.iter().any(|s| s.id == action_id) {
        return tool_error(
            id,
            "action_unavailable",
            "Action is not available from this endpoint.",
        );
    }

    let Some(deepbook) = state.deepbook_package_id.as_deref() else {
        return tool_error(
            id,
            "not_configured",
            "DEEPBOOK_PACKAGE_ID is not set on this deployment, so there is no DeepBook to build \
             against. Refusing rather than guessing an address.",
        );
    };
    let Ok(deepbook_package_id) = deepbook.parse() else {
        return tool_error(
            id,
            "not_configured",
            "DEEPBOOK_PACKAGE_ID is set but is not a Sui address.",
        );
    };

    let request = match crate::request::parse_build_request(
        arguments,
        action_id,
        state.config.network.into(),
        deepbook_package_id,
        DEFAULT_GAS_BUDGET,
        DEFAULT_GAS_PRICE,
    ) {
        Ok(r) => r,
        Err(e) => return tool_error(id, "invalid_arguments", &e.to_string()),
    };

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    match crate::build::build(&request, state.chain.as_ref(), now_ms).await {
        crate::build::BuildOutcome::Built(envelope) => match serde_json::to_value(&*envelope) {
            Ok(value) => tool_ok(id, value),
            Err(e) => tool_error(id, "serialize_failed", &e.to_string()),
        },
        crate::build::BuildOutcome::Refused { code, reason } => tool_error(id, code, &reason),
    }
}

/// Deliberately generous; the signer enforces its own ceiling and refuses anything above it, so
/// the binding limit is the one held by whoever owns the key rather than one chosen here.
const DEFAULT_GAS_BUDGET: u64 = 50_000_000;
const DEFAULT_GAS_PRICE: u64 = 1_000;
