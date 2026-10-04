//! An owner-signed action grant: the run set an agent's signer may execute an action under.
//!
//! A run set is what keeps a compromised builder from getting the signer to sign something the
//! wallet's owner never intended, so it cannot simply be fetched from the server that builds the
//! transactions. A grant is the run set and its build arguments, signed by the owner as a personal
//! message. The server stores and serves it; the signer uses it only after checking, against the
//! chain, that the signature is from the address the wallet names as its owner.
//!
//! The message is built here, from the grant alone, by both the server that asks the owner to sign
//! and the signer that checks the signature. Nothing outside Rust computes it: a browser signs the
//! exact text the server returns, so one canonical form exists and it is this one.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// The first line of every grant message. A version in the text means a later format can never be
/// mistaken for this one, and a signature over this one can never be replayed as a later one.
pub const MESSAGE_HEADER: &str = "Rill action grant v1";

/// What the owner signs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Grant {
    /// `testnet` or `mainnet`.
    pub network: String,
    pub action_id: String,
    /// Shown in the message so the owner reads what they are allowing, not only an id.
    pub action_name: String,
    /// The address whose key may run the action.
    pub agent: String,
    pub wallet_id: String,
    /// The `agent_wallet` deployment the wallet belongs to, so a grant for one deployment cannot be
    /// presented to a signer configured for another.
    pub wallet_package_id: String,
    /// Milliseconds since the epoch after which the signer refuses this grant, as decimal text.
    pub expires_at_ms: String,
    /// Increases each time the owner signs a changed grant for the same action and wallet, so the
    /// latest approval is identifiable and an older one can be told apart from it.
    pub revision: u64,
    /// The signer's run set, verbatim: allowed targets, object ids and limits.
    pub run_set: Value,
    /// What the signer passes to the builder to produce the action's transaction.
    pub build_arguments: Value,
}

/// A grant with the owner's signature over [`message`]: base64 of Sui's serialized signature.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SignedGrant {
    pub grant: Grant,
    pub signature: String,
}

impl Grant {
    /// The grant as compact JSON with every object's keys sorted, recursively.
    ///
    /// Written out rather than left to `serde_json`'s map ordering: whether its `Map` sorts depends
    /// on a crate feature any dependency can switch on for the whole build, and a digest that moved
    /// when an unrelated crate was added would invalidate every grant already signed.
    pub fn canonical_json(&self) -> String {
        let value = serde_json::to_value(self).expect("a grant serializes");
        let mut out = String::new();
        write_canonical(&value, &mut out);
        out
    }

    /// Lowercase hex sha256 of [`Grant::canonical_json`].
    pub fn digest(&self) -> String {
        let hash = Sha256::digest(self.canonical_json().as_bytes());
        hash.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// A string field of the run set, for the message. `?` when absent, so a malformed grant still
    /// renders and is refused by the signer's validation rather than by a panic here.
    fn run_set_text(&self, key: &str) -> String {
        self.run_set
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_owned()
    }

    /// The wallet's coin type, from the run set's capability manifest.
    fn coin_type(&self) -> String {
        self.run_set
            .get("capabilityManifest")
            .and_then(|m| m.get("walletCoinType"))
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_owned()
    }

    /// The exact personal message the owner signs and the signer verifies.
    ///
    /// Written for the person approving it: the action, the wallet, the asset and the ceiling in
    /// words, then the digest that binds every other detail of the run set.
    pub fn message(&self) -> String {
        format!(
            "{MESSAGE_HEADER}\n\
             Agent {agent} may run \"{name}\" ({id})\n\
             from wallet {wallet} on {network},\n\
             spending {coin}, at most {per_tx} base units per transaction,\n\
             until {expiry}. Revision {revision}.\n\
             Digest: {digest}",
            agent = self.agent,
            name = self.action_name,
            id = self.action_id,
            wallet = self.wallet_id,
            network = self.network,
            coin = self.coin_type(),
            per_tx = self.run_set_text("maxAmountBaseUnits"),
            expiry = self
                .expires_at_ms
                .parse::<u64>()
                .map(utc_minute)
                .unwrap_or_else(|_| "?".into()),
            revision = self.revision,
            digest = self.digest()
        )
    }

    /// Whether the grant has expired at `now_ms`. Unparseable is expired: a grant whose expiry
    /// cannot be read must not be treated as one that never expires.
    pub fn is_expired(&self, now_ms: u64) -> bool {
        self.expires_at_ms
            .parse::<u64>()
            .map_or(true, |expiry| now_ms >= expiry)
    }
}

/// `YYYY-MM-DD HH:MM UTC` for milliseconds since the epoch, without a date crate in the pure core.
fn utc_minute(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Howard Hinnant's days-to-civil, for the proleptic Gregorian calendar.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        rem / 3_600,
        (rem % 3_600) / 60
    )
}

/// SHA-256 of deterministic JSON, for immutable workflow publication metadata.
pub fn content_digest(value: &Value) -> String {
    let mut canonical = String::new();
    write_canonical(value, &mut canonical);
    Sha256::digest(canonical.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key).expect("a string serializes"));
                out.push(':');
                write_canonical(&map[key.as_str()], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&serde_json::to_string(scalar).expect("a scalar serializes")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn grant() -> Grant {
        Grant {
            network: "mainnet".into(),
            action_id: "skill_abc".into(),
            action_name: "cetus swap".into(),
            agent: "0x3e".into(),
            wallet_id: "0x49".into(),
            wallet_package_id: "0xb8".into(),
            expires_at_ms: "1791143096704".into(),
            revision: 1,
            run_set: json!({
                "maxAmountBaseUnits": "50000000",
                "allowedTargets": ["b", "a"],
                "capabilityManifest": {"walletCoinType": "0x2::sui::SUI"}
            }),
            build_arguments: json!({"sender": "0x3e", "agentWallet": {"walletId": "0x49"}}),
        }
    }

    #[test]
    fn canonical_json_sorts_keys_at_every_depth_and_keeps_array_order() {
        assert_eq!(
            grant().canonical_json(),
            r#"{"actionId":"skill_abc","actionName":"cetus swap","agent":"0x3e","buildArguments":{"agentWallet":{"walletId":"0x49"},"sender":"0x3e"},"expiresAtMs":"1791143096704","network":"mainnet","revision":1,"runSet":{"allowedTargets":["b","a"],"capabilityManifest":{"walletCoinType":"0x2::sui::SUI"},"maxAmountBaseUnits":"50000000"},"walletId":"0x49","walletPackageId":"0xb8"}"#
        );
    }

    #[test]
    fn the_digest_does_not_depend_on_the_order_fields_arrived_in() {
        let mut reordered = grant();
        reordered.build_arguments = json!({"agentWallet": {"walletId": "0x49"}, "sender": "0x3e"});
        assert_eq!(grant().digest(), reordered.digest());
    }

    #[test]
    fn any_change_to_the_run_set_changes_the_message() {
        let mut widened = grant();
        widened.run_set["maxAmountBaseUnits"] = json!("90000000");
        assert_ne!(grant().message(), widened.message());
        let mut retargeted = grant();
        retargeted.run_set["allowedTargets"] = json!(["a", "b"]);
        assert_ne!(grant().message(), retargeted.message());
    }

    #[test]
    fn the_message_names_what_the_owner_is_allowing() {
        assert_eq!(
            grant().message(),
            format!(
                "Rill action grant v1\n\
                 Agent 0x3e may run \"cetus swap\" (skill_abc)\n\
                 from wallet 0x49 on mainnet,\n\
                 spending 0x2::sui::SUI, at most 50000000 base units per transaction,\n\
                 until 2026-10-04 19:44 UTC. Revision 1.\n\
                 Digest: {}",
                grant().digest()
            )
        );
    }

    #[test]
    fn a_new_revision_or_expiry_is_a_new_digest() {
        let mut later = grant();
        later.revision = 2;
        assert_ne!(grant().digest(), later.digest());
        let mut extended = grant();
        extended.expires_at_ms = "1791229496704".into();
        assert_ne!(grant().digest(), extended.digest());
    }

    #[test]
    fn expiry_is_checked_and_an_unreadable_expiry_counts_as_expired() {
        assert!(!grant().is_expired(1_791_143_096_703));
        assert!(grant().is_expired(1_791_143_096_704));
        let mut garbled = grant();
        garbled.expires_at_ms = "soon".into();
        assert!(garbled.is_expired(0));
    }

    #[test]
    fn utc_minute_matches_known_instants() {
        assert_eq!(utc_minute(0), "1970-01-01 00:00 UTC");
        assert_eq!(utc_minute(951_782_400_000), "2000-02-29 00:00 UTC");
        assert_eq!(utc_minute(1_791_143_096_704), "2026-10-04 19:44 UTC");
    }
}
