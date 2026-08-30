//! Response shapes, and why there are two of them.
//!
//! The frontend reads errors from **different fields** depending on the path: `/api/*` responses
//! carry `error`, while `/oauth/*` responses carry `error_description`. That is not a tidy design
//! — it is what the deployed client already parses, and R4 says it runs against this server
//! unchanged. Unifying the two would be a nicer API and a broken product.
//!
//! One more rule the client depends on: it rejects any response where `success` is true but `data`
//! is missing or empty-falsy. So a success must always carry data, and "succeeded with nothing to
//! say" is not expressible — it would read to the client as a failure.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use serde_json::{json, Value};

/// A successful `/api/*` response. Always carries `data`.
pub fn api_ok<T: Serialize>(data: T) -> Response {
    Json(json!({ "success": true, "data": data })).into_response()
}

/// A failed `/api/*` response. The error text goes in `error`.
pub fn api_err(status: StatusCode, message: impl Into<String>) -> Response {
    let body = json!({ "success": false, "error": message.into() });
    (status, Json(body)).into_response()
}

/// A failed `/api/*` response carrying a machine-readable `type`, which the client uses to
/// distinguish a validation refusal from a server fault.
///
/// Unused until `/api/compile` and `/api/simulate` land — kept because those endpoints must use
/// this exact shape, and re-deriving it later from the client's parsing code is how the two drift.
#[allow(dead_code)]
pub fn api_err_typed(
    status: StatusCode,
    message: impl Into<String>,
    kind: &'static str,
) -> Response {
    let body = json!({ "success": false, "error": message.into(), "type": kind });
    (status, Json(body)).into_response()
}

/// A successful `/oauth/*` response.
///
/// Unused until the consent endpoints land; see the note on `api_err_typed`.
#[allow(dead_code)]
pub fn oauth_ok<T: Serialize>(data: T) -> Response {
    Json(json!({ "success": true, "data": data })).into_response()
}

/// A failed `/oauth/*` response. The error text goes in `error_description`, and `error` carries
/// the OAuth error code — this is RFC 6749 §5.2's shape, not an inconsistency.
pub fn oauth_err(status: StatusCode, code: &str, description: impl Into<String>) -> Response {
    let body = json!({ "error": code, "error_description": description.into() });
    (status, Json(body)).into_response()
}

/// A bare JSON object with no envelope at all — `/health` only, matching what the client expects.
pub fn bare(value: Value) -> Response {
    Json(value).into_response()
}
