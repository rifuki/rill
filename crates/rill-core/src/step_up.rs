//! The owner's out-of-band approval of one transaction above their step-up threshold.
//!
//! A grant lets an agent run an action without asking anyone, up to its per-transaction cap. An
//! owner who wants to see the large ones first sets a threshold inside the grant's run set, and
//! the signer then stops above it: it pins the exact bytes, asks the owner to sign this approval in
//! Studio, and submits only after it has checked that signature itself.
//!
//! Like a grant, the text is built here and only here, by the signer that verifies it and by the
//! server that shows it, so there is one canonical form. Studio signs the exact text the server
//! returns; the signer never reads that text back, it checks the signature against its own.
//!
//! What this binds and what it does not: the approval names the pinned digest and Sui's own
//! transaction digest, so it is good for exactly one set of bytes, and it expires with the
//! envelope (at most five minutes). It is checked by the signer, so it binds the MCP path and a
//! compromised builder. It does not bind someone who can read the agent's key: they can sign a
//! transaction themselves, bounded only by the wallet's on-chain rules.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::grant::{utc_minute, write_canonical};

/// The first line of the text the owner signs.
pub const APPROVAL_HEADER: &str = "Rill step-up approval v1";

/// The first line of the text the agent signs to open a request. A different header from the
/// approval, so a signature over one can never be presented as the other.
pub const REQUEST_HEADER: &str = "Rill step-up request v1";

/// One transaction the signer paused on, as the owner is asked to approve it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StepUpApproval {
    /// `testnet` or `mainnet`.
    pub network: String,
    pub agent: String,
    pub wallet_id: String,
    pub wallet_package_id: String,
    pub action_id: String,
    pub action_name: String,
    /// The grant this run used, so a revoke or a new revision in the meantime is noticed.
    pub grant_revision: u64,
    /// [`crate::grant::Grant::digest`] of that grant.
    pub grant_digest: String,
    /// [`crate::envelope::digest_unsigned_ptb`] of the bytes the signer pinned.
    pub pinned_digest: String,
    /// Sui's base58 digest of the unsigned transaction, which Studio recomputes from the bytes.
    pub transaction_digest: String,
    /// What the transaction spends, as the signer read it: the larger of the envelope's declared
    /// spend and the amount the bytes request.
    pub spend_base_units: String,
    pub threshold_base_units: String,
    pub coin_type: String,
    /// The envelope's own expiry, in milliseconds since the epoch, as decimal text.
    pub expires_at_ms: String,
}

impl StepUpApproval {
    /// Lowercase hex sha256 of the approval as sorted-key compact JSON, the same canonical form a
    /// grant's digest uses.
    pub fn digest(&self) -> String {
        let value = serde_json::to_value(self).expect("an approval serializes");
        let mut canonical = String::new();
        write_canonical(&value, &mut canonical);
        Sha256::digest(canonical.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// The exact personal message the owner signs and the signer verifies.
    pub fn message(&self) -> String {
        self.text(APPROVAL_HEADER)
    }

    /// The same transaction, under the header the agent signs to open a request. Only the agent's
    /// key can open one, so a stranger cannot fill the owner's Studio with requests.
    pub fn request_message(&self) -> String {
        self.text(REQUEST_HEADER)
    }

    fn text(&self, header: &str) -> String {
        format!(
            "{header}\n\
             I approve agent {agent} running \"{name}\" ({id}) once\n\
             from wallet {wallet} on {network},\n\
             spending {spend} base units of {coin} (my threshold is {threshold}),\n\
             under grant revision {revision}.\n\
             Transaction: {transaction}\n\
             Valid until {expiry}.\n\
             Digest: {digest}",
            agent = self.agent,
            name = self.action_name,
            id = self.action_id,
            wallet = self.wallet_id,
            network = self.network,
            spend = self.spend_base_units,
            coin = self.coin_type,
            threshold = self.threshold_base_units,
            revision = self.grant_revision,
            transaction = self.transaction_digest,
            expiry = self
                .expires_at_ms
                .parse::<u64>()
                .map(utc_minute)
                .unwrap_or_else(|_| "?".into()),
            digest = self.digest()
        )
    }

    /// Whether the approval has expired at `now_ms`. Unparseable is expired, as for a grant.
    pub fn is_expired(&self, now_ms: u64) -> bool {
        self.expires_at_ms
            .parse::<u64>()
            .map_or(true, |expiry| now_ms >= expiry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approval() -> StepUpApproval {
        StepUpApproval {
            network: "mainnet".into(),
            agent: "0x3e".into(),
            wallet_id: "0x49".into(),
            wallet_package_id: "0xb8".into(),
            action_id: "skill_abc".into(),
            action_name: "cetus swap".into(),
            grant_revision: 7,
            grant_digest: "aa".repeat(32),
            pinned_digest: "bb".repeat(32),
            transaction_digest: "4vJ9JU1bJJE96FWSJKvHsmmFADCg4gpZQff4P3bkLKi".into(),
            spend_base_units: "70000000".into(),
            threshold_base_units: "10000000".into(),
            coin_type: "0x2::sui::SUI".into(),
            expires_at_ms: "1791143096704".into(),
        }
    }

    #[test]
    fn the_approval_message_is_exact_and_binds_the_pinned_transaction() {
        let a = approval();
        // Studio rebuilds this text to check the server's copy before the owner signs it
        // (rill-frontend src/lib/step-up.test.ts pins the same value), so the digest is a literal.
        assert_eq!(
            a.digest(),
            "38952458b5914b38bc7246d3352f2c03fd7cc86735437242f69b975736696c54"
        );
        assert_eq!(
            a.message(),
            format!(
                "Rill step-up approval v1\n\
                 I approve agent 0x3e running \"cetus swap\" (skill_abc) once\n\
                 from wallet 0x49 on mainnet,\n\
                 spending 70000000 base units of 0x2::sui::SUI (my threshold is 10000000),\n\
                 under grant revision 7.\n\
                 Transaction: 4vJ9JU1bJJE96FWSJKvHsmmFADCg4gpZQff4P3bkLKi\n\
                 Valid until 2026-10-04 19:44 UTC.\n\
                 Digest: {}",
                a.digest()
            )
        );

        type Change = fn(&mut StepUpApproval);
        let changes: [(&str, Change); 4] = [
            ("pinnedDigest", |a| a.pinned_digest = "cc".repeat(32)),
            ("transactionDigest", |a| {
                a.transaction_digest = "other".into()
            }),
            ("spendBaseUnits", |a| a.spend_base_units = "70000001".into()),
            ("grantDigest", |a| a.grant_digest = "dd".repeat(32)),
        ];
        for (field, change) in changes {
            let mut changed = approval();
            change(&mut changed);
            assert_ne!(
                changed.digest(),
                a.digest(),
                "{field} must change the digest"
            );
            assert_ne!(
                changed.message(),
                a.message(),
                "{field} must change the text"
            );
        }
    }

    #[test]
    fn a_request_is_never_mistaken_for_an_approval() {
        let a = approval();
        let request = a.request_message();
        assert!(
            request.starts_with("Rill step-up request v1\n"),
            "{request}"
        );
        assert_ne!(request, a.message());
        assert_eq!(
            request.split_once('\n').map(|(_, rest)| rest),
            a.message().split_once('\n').map(|(_, rest)| rest),
            "the request names the same transaction; only the header differs"
        );
    }

    #[test]
    fn expiry_is_checked_and_an_unreadable_expiry_counts_as_expired() {
        assert!(!approval().is_expired(1_791_143_096_703));
        assert!(approval().is_expired(1_791_143_096_704));
        let mut garbled = approval();
        garbled.expires_at_ms = "soon".into();
        assert!(garbled.is_expired(0));
    }

    /// Strict like a grant: a field this signer does not know is a different approval format, and
    /// reading past it would approve something nobody rendered.
    #[test]
    fn an_unknown_field_is_refused() {
        let mut value = serde_json::to_value(approval()).unwrap();
        value["note"] = serde_json::json!("approve everything");
        assert!(serde_json::from_value::<StepUpApproval>(value).is_err());
    }
}
