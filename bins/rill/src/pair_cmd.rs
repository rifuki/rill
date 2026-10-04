//! Prove the configured local signer to an owner-created pairing request. No owner key or grant.
use crate::{http, keystore::Keystore};
use serde_json::{json, Value};

fn data(value: &Value) -> &Value {
    value.get("data").unwrap_or(value)
}

pub async fn pair(
    keystore: &Keystore,
    api: &str,
    request_id: &str,
    network: &str,
) -> Result<Value, String> {
    if request_id.is_empty()
        || !request_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("invalid pairing request ID".into());
    }
    let api = api.trim_end_matches('/');
    let response = http::get_json(&format!("{api}/api/pairing/{request_id}")).await?;
    let challenge = data(&response);
    validate_challenge(
        challenge,
        api,
        request_id,
        &keystore.address().to_string(),
        network,
    )?;
    let signature = keystore
        .sign_personal_message(
            challenge["message"]
                .as_str()
                .ok_or("no message")?
                .as_bytes(),
        )
        .map_err(|e| e.to_string())?
        .to_base64();
    let response = http::post_json(
        &format!("{api}/api/pairing/prove"),
        &json!({
            "requestId": request_id, "agent": keystore.address().to_string(), "signature": signature
        }),
    )
    .await?;
    Ok(data(&response).clone())
}

pub fn validate_challenge(
    challenge: &Value,
    api: &str,
    request_id: &str,
    agent: &str,
    network: &str,
) -> Result<(), String> {
    let text = |field: &str| {
        challenge[field]
            .as_str()
            .ok_or_else(|| format!("pairing challenge lacks {field}"))
    };
    if text("requestId")? != request_id
        || text("agent")? != agent
        || text("network")? != network
        || text("domain")? != api
    {
        return Err(
            "pairing challenge does not match this API, request, signer and network".into(),
        );
    }
    let expires = challenge["expiresAt"]
        .as_u64()
        .ok_or("pairing challenge lacks expiry")?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_millis() as u64;
    if expires <= now {
        return Err("pairing challenge expired".into());
    }
    let expected = format!("Rill signer pairing v1\nPurpose: prove agent signer; no spend authorization\nDomain: {}\nRequest: {}\nNetwork: {}\nOwner: {}\nAgent: {}\nNonce: {}\nExpiresAt: {}", api, request_id, network, text("owner")?, agent, text("nonce")?, expires);
    if text("message")? != expected {
        return Err("pairing message does not match its metadata".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn refuses_wrong_signer_and_arbitrary_message() {
        let mut value = json!({"requestId":"one","agent":"agent","network":"mainnet","domain":"https://example.org","owner":"owner","nonce":"nonce","expiresAt":u64::MAX,"message":"sign anything"});
        assert!(
            validate_challenge(&value, "https://example.org", "one", "other", "mainnet").is_err()
        );
        assert!(
            validate_challenge(&value, "https://example.org", "one", "agent", "mainnet").is_err()
        );
        value["message"] = json!(format!("Rill signer pairing v1\nPurpose: prove agent signer; no spend authorization\nDomain: https://example.org\nRequest: one\nNetwork: mainnet\nOwner: owner\nAgent: agent\nNonce: nonce\nExpiresAt: {}",u64::MAX));
        assert!(
            validate_challenge(&value, "https://example.org", "one", "agent", "mainnet").is_ok()
        );
    }
}
