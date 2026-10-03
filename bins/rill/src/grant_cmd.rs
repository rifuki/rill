//! `rill grant`: an owner without a browser wallet grants an action to their wallet's agent.
//!
//! The round trip Studio makes in a browser, made from the terminal with the owner's key: sign in
//! with a personal-message challenge, ask the server to prepare the grant, sign it, store it.
//!
//! The message signed is the one this binary derives from the grant, never the text the server
//! sent. They must be identical or nothing is signed: an owner approves what the grant says, and
//! a server that showed one thing and stored another would be caught here.

use rill_core::grant::Grant;
use serde_json::{json, Value};

use crate::http;
use crate::keystore::Keystore;

pub struct GrantArgs {
    pub api: String,
    pub action_id: String,
    pub wallet_id: String,
    pub budget_mist: String,
    pub per_tx_mist: String,
    pub expires_at_ms: Option<String>,
    /// Sign and store. Without it the grant is prepared and shown, and nothing is signed.
    pub submit: bool,
}

fn data(value: &Value) -> &Value {
    value.get("data").unwrap_or(value)
}

/// Sign in to the Rill API as `keystore`'s address and return the session token.
pub async fn sign_in(api: &str, keystore: &Keystore) -> Result<String, String> {
    let challenge = http::get_json(&format!("{api}/oauth/wallet-challenge")).await?;
    let challenge = data(&challenge);
    let message = challenge["message"]
        .as_str()
        .ok_or("the sign-in challenge carried no message")?;
    let id = challenge["challengeId"]
        .as_str()
        .ok_or("the sign-in challenge carried no id")?;
    let signature = keystore
        .sign_personal_message(message.as_bytes())
        .map_err(|e| e.to_string())?
        .to_base64();
    let token = http::post_json(
        &format!("{api}/oauth/wallet-token"),
        &json!({ "challengeId": id, "signature": signature }),
    )
    .await?;
    data(&token)["access_token"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "signing in returned no token".to_owned())
}

pub async fn grant(keystore: &Keystore, args: &GrantArgs) -> Result<Value, String> {
    let api = args.api.trim_end_matches('/');
    let token = sign_in(api, keystore).await?;

    let mut request = json!({
        "actionId": args.action_id,
        "walletId": args.wallet_id,
        "budgetMist": args.budget_mist,
        "perTxMist": args.per_tx_mist,
    });
    if let Some(expiry) = &args.expires_at_ms {
        request["expiresAtMs"] = json!(expiry);
    }
    let prepared =
        http::post_json_as(&format!("{api}/api/grants/prepare"), &request, &token).await?;
    let prepared = data(&prepared);
    let grant: Grant = serde_json::from_value(prepared["grant"].clone())
        .map_err(|e| format!("the prepared grant did not parse: {e}"))?;
    let message = grant.message();
    if prepared["message"].as_str() != Some(message.as_str()) {
        return Err(
            "the server's message is not the one this grant produces, so nothing was signed".into(),
        );
    }

    let mut report = json!({
        "actionId": grant.action_id,
        "walletId": grant.wallet_id,
        "agent": grant.agent,
        "revision": grant.revision,
        "expiresAtMs": grant.expires_at_ms,
        "message": message,
        "submitted": false,
    });
    if !args.submit {
        report["note"] = json!(
            "Prepared only. Read the message, then run again with --submit to sign and store it."
        );
        return Ok(report);
    }

    let signature = keystore
        .sign_personal_message(message.as_bytes())
        .map_err(|e| e.to_string())?
        .to_base64();
    let stored = http::post_json_as(
        &format!("{api}/api/grants"),
        &json!({ "grant": grant, "signature": signature }),
        &token,
    )
    .await?;
    report["submitted"] = json!(true);
    report["stored"] = data(&stored).clone();
    report["note"] = json!("Signed and stored. The agent's signer lists it with rill_actions.");
    Ok(report)
}
