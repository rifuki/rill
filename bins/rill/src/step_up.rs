//! The signer's half of the owner's step-up approval: what it remembers while the owner decides,
//! and how it checks the owner's answer.
//!
//! Above the threshold a grant's run set names, `rill_run_action` pins the transaction and stops.
//! It opens a request on the Rill API, signed by the agent, and hands the agent a Studio link for
//! the owner. The owner signs [`StepUpApproval::message`] there with their own wallet. On the
//! agent's next call the signer fetches that signature and checks it here, against its own copy of
//! the approval and the owner the chain names now. The server only relays: nothing it returns is
//! trusted, and the text it showed the owner is never read back.
//!
//! What this binds is the MCP path. Whoever can read the agent's key can sign a transaction without
//! asking this signer at all, and is then bounded only by the wallet's on-chain rules.
//!
//! One of which can be the `step_up` rule. On a wallet that carries it, and for a grant that names
//! it, the chain's threshold pauses a run too, and the grant's own threshold still does: the lower
//! of the two starts the pause. Above the chain's, the transaction is built with the owner as its
//! gas sponsor, and the owner approves by signing that transaction as sponsor. The chain then
//! requires the owner's signature for the spend, so the agent key alone cannot land it on any path.
//! The signer submits the agent's signature as sender beside the owner's, after checking the
//! owner's here over the exact bytes it holds. Between the grant's threshold and the chain's, the
//! approval is the signer-only kind above.

use rill_chain::{SignatureCheck, SuiRead};
use rill_core::step_up::StepUpApproval;
use serde_json::Value;

use crate::runset::RunSet;

/// The grant a paused run used, as far as the approval needs to name it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepUpGrant {
    pub action_name: String,
    pub revision: u64,
    /// [`rill_core::grant::Grant::digest`].
    pub digest: String,
}

/// A transaction this signer pinned and is holding for the owner's approval.
///
/// Kept in the process, like `WalletContext::submitted`, and for the same honest reason: a fresh
/// signer does not know it, and the agent then builds again and the owner is asked again. Nothing
/// is lost by that but a minute, and the envelope itself expires in at most five.
#[derive(Debug, Clone)]
pub struct PendingStepUp {
    /// The envelope exactly as the builder returned it. The approved run executes this, not a new
    /// build, because the owner approved these bytes and no others.
    pub envelope: Value,
    pub approval: StepUpApproval,
    /// The grant's run set the envelope was checked under.
    pub run_set: RunSet,
}

/// Why the owner's answer was not used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalRefusal {
    /// The grant the approval names is no longer the latest usable one: revoked, expired, or
    /// replaced by a revision the owner signed since.
    GrantChanged,
    /// The signature is not the wallet owner's over this signer's own approval text.
    NotOwner(String),
    /// The node could not answer, so whether the owner approved is unknown.
    Unavailable(String),
}

impl ApprovalRefusal {
    pub fn code(&self) -> &'static str {
        match self {
            Self::GrantChanged => "grant_changed",
            Self::NotOwner(_) => "approval_not_owner",
            Self::Unavailable(_) => "step_up_unavailable",
        }
    }
}

impl std::fmt::Display for ApprovalRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GrantChanged => write!(
                f,
                "the grant this approval names is no longer the owner's latest usable grant for \
                 this action and wallet, so the approval is not used. Nothing was submitted. Run \
                 the action again under the grant that is current."
            ),
            Self::NotOwner(why) => write!(
                f,
                "the approval is not signed by the wallet's owner over this exact transaction \
                 ({why}). Nothing was submitted."
            ),
            Self::Unavailable(why) => write!(
                f,
                "the owner's approval could not be checked because the node did not answer \
                 ({why}). Nothing was submitted; call again with the same approvalId and a new \
                 idempotencyKey."
            ),
        }
    }
}

fn text_field(fields: &Value, name: &str) -> Option<String> {
    let value = fields.get(name)?;
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_u64().map(|n| n.to_string()))
}

/// Whether a grant's manifest names the on-chain `step_up` rule.
pub fn names_on_chain_rule(manifest: &rill_core::manifest::CapabilityManifest) -> bool {
    manifest
        .rules
        .iter()
        .any(|rule| matches!(rule, rill_core::manifest::CapabilityRule::StepUp { .. }))
}

/// The wallet's on-chain step-up threshold, for a grant whose manifest names the rule.
///
/// `Ok(None)` for a grant that does not name it, which keeps the signer-only threshold and reads
/// nothing, so a wallet without the rule runs exactly as before it existed. A grant that names it
/// takes the threshold from the chain, never from its own copy, and a wallet that does not carry the
/// rule, or whose rules cannot be read, is an error: running as if the owner had never set it would
/// skip the pause they asked for.
pub async fn on_chain_threshold(
    chain: &impl SuiRead,
    manifest: &rill_core::manifest::CapabilityManifest,
    wallet_id: &str,
) -> Result<Option<u64>, String> {
    if !names_on_chain_rule(manifest) {
        return Ok(None);
    }
    match rill_ptb::policy_read::read_step_up_threshold(chain, wallet_id).await? {
        Some(threshold) => Ok(Some(threshold)),
        None => Err(
            "the grant names the on-chain step_up rule, and the wallet does not carry it now"
                .into(),
        ),
    }
}

fn same_address(a: &str, b: &str) -> bool {
    matches!(
        (a.parse::<sui_sdk_types::Address>(), b.parse::<sui_sdk_types::Address>()),
        (Ok(a), Ok(b)) if a == b
    )
}

/// Check the owner's approval of this signer's own `approval`, and return that owner.
///
/// For the signer-only kind the approval is the owner's signature over [`StepUpApproval::message`].
/// For a sponsored one it is the owner's signature over `unsigned_ptb`, the exact transaction this
/// signer holds, as its gas sponsor; the bytes must name the current owner as that sponsor, since
/// the chain checks the sponsor's signature against the gas owner and nobody else.
///
/// `current_grant_digest` is the digest of the latest usable grant for the approval's action and
/// wallet, verified just now; `None` when there is none. The grant is checked first, so a revoked
/// grant is refused without asking the node anything. The owner is read from the wallet now, not
/// remembered, so an approval signed by a former owner is not one.
pub async fn verify_owner_approval(
    chain: &impl SuiRead,
    approval: &StepUpApproval,
    unsigned_ptb: &str,
    owner_signature: &str,
    current_grant_digest: Option<&str>,
) -> Result<String, ApprovalRefusal> {
    if current_grant_digest != Some(approval.grant_digest.as_str()) {
        return Err(ApprovalRefusal::GrantChanged);
    }
    let wallet = chain
        .get_object(&approval.wallet_id)
        .await
        .map_err(|e| ApprovalRefusal::Unavailable(format!("the wallet could not be read: {e}")))?;
    let owner = wallet
        .fields
        .as_ref()
        .and_then(|fields| text_field(fields, "owner"))
        .ok_or_else(|| ApprovalRefusal::Unavailable("the wallet names no owner".into()))?;
    let check = match approval.gas_owner.as_deref() {
        Some(gas_owner) => {
            if !same_address(gas_owner, &owner) {
                return Err(ApprovalRefusal::NotOwner(format!(
                    "the transaction names {gas_owner} as its gas sponsor, and the wallet's owner \
                     is now {owner}"
                )));
            }
            chain
                .verify_transaction_signature(unsigned_ptb, owner_signature, &owner)
                .await
        }
        None => {
            chain
                .verify_personal_message(approval.message().as_bytes(), owner_signature, &owner)
                .await
        }
    };
    match check {
        Ok(SignatureCheck::Valid) => Ok(owner),
        Ok(SignatureCheck::Invalid(why)) => Err(ApprovalRefusal::NotOwner(why)),
        Err(e) => Err(ApprovalRefusal::Unavailable(e.to_string())),
    }
}

/// The origin of a Studio approval link, when the link is exactly Studio's approve page for this
/// request: `https://<host>/approve?request=<id>`, or plain http on this machine for development.
///
/// The link comes from the Rill API, which this signer does not otherwise trust, and it is the page
/// the owner opens to connect their wallet. So nothing else is passed on: no other path, no extra
/// query or fragment that could carry a redirect, no credentials in the authority. The origin is
/// returned so the answer can name the site the owner is about to open.
pub fn approval_link_origin(link: &str, approval_id: &str) -> Result<String, String> {
    let refused = || {
        format!("the API's approval link {link:?} is not Studio's approve page for this request")
    };
    let (scheme, rest) = link.split_once("://").ok_or_else(refused)?;
    let (authority, path) = rest.split_at(rest.find('/').ok_or_else(refused)?);
    let host = authority.split(':').next().unwrap_or_default();
    let valid_authority = !authority.is_empty()
        && authority
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'));
    let loopback = matches!(host, "localhost" | "127.0.0.1");
    let secure = scheme == "https" || (scheme == "http" && loopback);
    if !valid_authority || host.is_empty() || !secure {
        return Err(refused());
    }
    if path != format!("/approve?request={approval_id}") {
        return Err(refused());
    }
    Ok(format!("{scheme}://{authority}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rill_chain::fake::FakeSui;
    use rill_chain::{ObjectRef, ObjectSummary};
    use serde_json::json;

    const OWNER: &str = "0x00000000000000000000000000000000000000000000000000000000000000a1";
    const AGENT: &str = "0x00000000000000000000000000000000000000000000000000000000000000a2";
    const WALLET: &str = "0x00000000000000000000000000000000000000000000000000000000000000b1";
    const SIGNATURE: &str = "owner-signature";

    /// The server names the page the owner opens to connect their wallet, so its link is held to
    /// one shape: Studio's approve route for this very request, over https or on this machine.
    #[test]
    fn an_approval_link_must_be_studios_approve_page_for_this_request() {
        let id = "abc_DEF-123";
        assert_eq!(
            approval_link_origin("https://rill.rifuki.dev/approve?request=abc_DEF-123", id),
            Ok("https://rill.rifuki.dev".to_owned())
        );
        assert_eq!(
            approval_link_origin("http://localhost:5173/approve?request=abc_DEF-123", id),
            Ok("http://localhost:5173".to_owned())
        );
        for refused in [
            "http://rill.rifuki.dev/approve?request=abc_DEF-123",
            "https://rill.rifuki.dev/connect?request=abc_DEF-123",
            "https://rill.rifuki.dev/approve?request=other",
            "https://rill.rifuki.dev/approve?request=abc_DEF-123&next=https://evil.example",
            "https://user@evil.example/approve?request=abc_DEF-123",
            "https://rill.rifuki.dev/approve?request=abc_DEF-123#x",
            "javascript:alert(1)",
            "https:///approve?request=abc_DEF-123",
            "http://localhost.evil.example/approve?request=abc_DEF-123",
        ] {
            assert!(approval_link_origin(refused, id).is_err(), "{refused}");
        }
    }

    fn approval() -> StepUpApproval {
        StepUpApproval {
            network: "mainnet".into(),
            agent: AGENT.into(),
            wallet_id: WALLET.into(),
            wallet_package_id: "0xc1".into(),
            action_id: "skill_x".into(),
            action_name: "cetus swap".into(),
            grant_revision: 3,
            grant_digest: "aa".repeat(32),
            pinned_digest: "bb".repeat(32),
            transaction_digest: "4vJ9JU1bJJE96FWSJKvHsmmFADCg4gpZQff4P3bkLKi".into(),
            spend_base_units: "70000000".into(),
            threshold_base_units: "10000000".into(),
            coin_type: "0x2::sui::SUI".into(),
            expires_at_ms: "1791143096704".into(),
            gas_owner: None,
        }
    }

    fn wallet() -> ObjectSummary {
        ObjectSummary {
            reference: ObjectRef {
                id: WALLET.into(),
                version: 5,
                digest: sui_sdk_types::Digest::ZERO.to_string(),
            },
            object_type: Some("0xc1::agent_wallet::AgentWallet<0x2::sui::SUI>".into()),
            fields: Some(json!({ "owner": OWNER, "agent": AGENT, "revoked": false })),
            shared_initial_version: Some(4),
        }
    }

    /// The held transaction's bytes, as the signer keeps them beside the approval.
    const HELD: &str = "aGVsZCB0cmFuc2FjdGlvbg==";

    async fn check(
        chain: &FakeSui,
        approval: &StepUpApproval,
        current: Option<&str>,
    ) -> Result<String, ApprovalRefusal> {
        verify_owner_approval(chain, approval, HELD, SIGNATURE, current).await
    }

    fn sponsored() -> StepUpApproval {
        StepUpApproval {
            gas_owner: Some(OWNER.into()),
            ..approval()
        }
    }

    /// Above the on-chain threshold the owner's approval is their signature over the transaction
    /// as gas sponsor, and nothing else: not a text signature, not another key's, and not a former
    /// owner's.
    #[tokio::test]
    async fn a_sponsored_approval_is_only_the_current_owners_signature_over_the_held_bytes() {
        let ours = sponsored();
        let digest = ours.grant_digest.clone();

        let by_owner = FakeSui::new()
            .with_object(None, wallet())
            .with_valid_transaction_signature(HELD, SIGNATURE, OWNER);
        assert_eq!(
            check(&by_owner, &ours, Some(&digest)).await,
            Ok(OWNER.to_string())
        );

        let text = FakeSui::new()
            .with_object(None, wallet())
            .with_valid_signature(ours.message().as_bytes(), SIGNATURE, OWNER);
        assert_eq!(
            check(&text, &ours, Some(&digest)).await.unwrap_err().code(),
            "approval_not_owner"
        );

        let by_agent = FakeSui::new()
            .with_object(None, wallet())
            .with_valid_transaction_signature(HELD, SIGNATURE, AGENT);
        assert_eq!(
            check(&by_agent, &ours, Some(&digest))
                .await
                .unwrap_err()
                .code(),
            "approval_not_owner"
        );

        // The bytes name a sponsor who is no longer the owner: the chain would refuse their
        // signature as the rule's, whoever signed.
        let mut former = sponsored();
        former.gas_owner = Some(AGENT.into());
        let refusal = check(&by_agent, &former, Some(&digest)).await.unwrap_err();
        assert_eq!(refusal.code(), "approval_not_owner", "{refusal}");
    }

    fn manifest(rules: serde_json::Value) -> rill_core::manifest::CapabilityManifest {
        serde_json::from_value(json!({ "walletCoinType": "0x2::sui::SUI", "rules": rules }))
            .unwrap()
    }

    fn with_policy(chain: FakeSui, threshold: Option<u64>) -> FakeSui {
        let mut summary = wallet();
        summary.fields =
            Some(json!({ "owner": OWNER, "agent": AGENT, "policy": { "id": "0xe1" } }));
        let chain = chain.with_object(None, summary);
        match threshold {
            Some(threshold) => chain.with_dynamic_field(
                "0xe1",
                rill_chain::DynamicFieldSummary {
                    field_id: "0xf0".into(),
                    value_type: Some("0xc1::step_up::Config".into()),
                    fields: Some(json!({ "value": { "threshold": threshold.to_string() } })),
                },
            ),
            None => chain,
        }
    }

    /// A grant that does not name the rule keeps the signer-only gate and reads nothing more; one
    /// that names it takes its threshold from the chain, and a wallet that has since lost the rule
    /// is refused rather than run as if the owner had never set it.
    #[tokio::test]
    async fn the_on_chain_threshold_is_read_only_for_a_grant_that_names_the_rule() {
        let without = manifest(json!([{ "kind": "budget", "totalMist": "5" }]));
        assert_eq!(
            on_chain_threshold(&FakeSui::new(), &without, WALLET).await,
            Ok(None),
            "nothing is read for a grant without the rule"
        );

        let with = manifest(json!([
            { "kind": "budget", "totalMist": "5" },
            { "kind": "step_up", "thresholdMist": "999" },
        ]));
        assert_eq!(
            on_chain_threshold(&with_policy(FakeSui::new(), Some(10)), &with, WALLET).await,
            Ok(Some(10)),
            "the chain's threshold, not the grant's copy"
        );
        let missing = on_chain_threshold(&with_policy(FakeSui::new(), None), &with, WALLET)
            .await
            .unwrap_err();
        assert!(missing.contains("step_up"), "{missing}");
        assert!(on_chain_threshold(&FakeSui::new(), &with, WALLET)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn an_approval_is_accepted_only_from_the_current_owner_over_the_signers_own_text() {
        let ours = approval();
        let digest = ours.grant_digest.clone();

        let signed_ours = FakeSui::new()
            .with_object(None, wallet())
            .with_valid_signature(ours.message().as_bytes(), SIGNATURE, OWNER);
        assert_eq!(
            check(&signed_ours, &ours, Some(&digest)).await,
            Ok(OWNER.to_string())
        );

        // The server showed the owner a different amount and the owner signed that text. The
        // signer checks the signature over its own text, so the approval is not for this spend.
        let mut shown = ours.clone();
        shown.spend_base_units = "1".into();
        let signed_shown = FakeSui::new()
            .with_object(None, wallet())
            .with_valid_signature(shown.message().as_bytes(), SIGNATURE, OWNER);
        let refusal = check(&signed_shown, &ours, Some(&digest))
            .await
            .unwrap_err();
        assert_eq!(refusal.code(), "approval_not_owner", "{refusal}");

        // The agent's own key cannot stand in for the owner's.
        let by_agent = FakeSui::new()
            .with_object(None, wallet())
            .with_valid_signature(ours.message().as_bytes(), SIGNATURE, AGENT);
        assert_eq!(
            check(&by_agent, &ours, Some(&digest))
                .await
                .unwrap_err()
                .code(),
            "approval_not_owner"
        );

        // A valid signature after the owner revoked or re-signed the grant is not used.
        for current in [Some("cc".repeat(32)), None] {
            let refusal = check(&signed_ours, &ours, current.as_deref())
                .await
                .unwrap_err();
            assert_eq!(refusal, ApprovalRefusal::GrantChanged, "{refusal}");
            assert_eq!(refusal.code(), "grant_changed");
        }

        // A node that cannot answer has not said the owner signed.
        let unasked = FakeSui::new()
            .with_object(None, wallet())
            .with_valid_signature(ours.message().as_bytes(), SIGNATURE, OWNER)
            .with_signature_check_unavailable();
        assert_eq!(
            check(&unasked, &ours, Some(&digest))
                .await
                .unwrap_err()
                .code(),
            "step_up_unavailable"
        );
    }
}
