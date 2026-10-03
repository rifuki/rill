//! Owner-signed action grants, checked before the signer uses one.
//!
//! A grant carries the run set an action may execute under. It arrives from the Rill server, which
//! is exactly the party a run set exists to constrain, so nothing in it is used until the chain
//! says the wallet's owner signed it, the wallet still names this signer as its agent, and neither
//! the grant nor the wallet has ended. See `docs/plans/2026-10-04-owner-signed-action-grants.md`.
//!
//! What a verified grant does and does not do, stated so it is never overstated: it stops this
//! signer from signing a transaction the owner did not approve, whatever the server sends. It does
//! not constrain someone who holds the agent's key and builds a transaction themselves; that is
//! bounded only by the rules the Move contract enforces on the wallet.

use rill_chain::{SignatureCheck, SuiRead};
use rill_core::grant::{Grant, SignedGrant};
use serde_json::Value;
use sui_sdk_types::Address;

use crate::runset::RunSet;

/// A grant whose every check passed, ready to stand in for a run-set file.
#[derive(Debug, Clone)]
pub struct VerifiedGrant {
    pub grant: Grant,
    pub run_set: RunSet,
    /// The wallet's owner as the chain reports it, who signed the grant.
    pub owner: String,
}

/// Why a grant was not used. Each names the check that failed, because a refusal that only says
/// "invalid grant" leaves the owner nothing to fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    WrongNetwork { grant: String, signer: String },
    NotThisAgent { grant: String, signer: String },
    WrongDeployment { grant: String, signer: String },
    Expired,
    RunSetDisagrees(String),
    RunSetInvalid(String),
    WalletUnreadable(String),
    AgentRotated { wallet_agent: String },
    WalletRevoked,
    WalletExpired,
    BadSignature(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongNetwork { grant, signer } => write!(
                f,
                "the grant is for {grant} and this signer runs on {signer}"
            ),
            Self::NotThisAgent { grant, signer } => write!(
                f,
                "the grant is for agent {grant}, and this signer holds {signer}"
            ),
            Self::WrongDeployment { grant, signer } => write!(
                f,
                "the grant names agent_wallet package {grant}, and this signer uses {signer}"
            ),
            Self::Expired => write!(f, "the grant has expired; the owner can sign a new one"),
            Self::RunSetDisagrees(what) => {
                write!(f, "the grant's run set does not match the grant: {what}")
            }
            Self::RunSetInvalid(why) => write!(f, "the grant's run set is not valid: {why}"),
            Self::WalletUnreadable(why) => write!(f, "the wallet could not be read: {why}"),
            Self::AgentRotated { wallet_agent } => write!(
                f,
                "the wallet's agent is now {wallet_agent}, so this signer is no longer its agent"
            ),
            Self::WalletRevoked => write!(f, "the wallet has been revoked by its owner"),
            Self::WalletExpired => write!(f, "the wallet has expired; its owner can extend it"),
            Self::BadSignature(why) => {
                write!(f, "the grant is not signed by the wallet's owner: {why}")
            }
        }
    }
}

fn same_address(a: &str, b: &str) -> bool {
    match (a.parse::<Address>(), b.parse::<Address>()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn text_field(fields: &Value, name: &str) -> Option<String> {
    let value = fields.get(name)?;
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_u64().map(|n| n.to_string()))
}

/// Check one grant for the signer holding `signer` on `network`, against `wallet_package`.
///
/// Cheap checks first, then the wallet read, then the signature: the node is asked to verify only a
/// grant that would otherwise be usable.
pub async fn verify(
    chain: &impl SuiRead,
    signed: &SignedGrant,
    signer: &str,
    network: &str,
    wallet_package: &str,
    now_ms: u64,
) -> Result<VerifiedGrant, Refusal> {
    let grant = &signed.grant;
    if grant.network != network {
        return Err(Refusal::WrongNetwork {
            grant: grant.network.clone(),
            signer: network.to_owned(),
        });
    }
    if !same_address(&grant.agent, signer) {
        return Err(Refusal::NotThisAgent {
            grant: grant.agent.clone(),
            signer: signer.to_owned(),
        });
    }
    if !same_address(&grant.wallet_package_id, wallet_package) {
        return Err(Refusal::WrongDeployment {
            grant: grant.wallet_package_id.clone(),
            signer: wallet_package.to_owned(),
        });
    }
    if grant.is_expired(now_ms) {
        return Err(Refusal::Expired);
    }

    let run_set =
        RunSet::from_value(&grant.run_set).map_err(|e| Refusal::RunSetInvalid(e.to_string()))?;
    let run_set_network = serde_json::to_value(run_set.network)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    for (what, agrees) in [
        ("network", run_set_network == grant.network),
        ("sender", same_address(&run_set.sender, &grant.agent)),
        ("action", run_set.action_id == grant.action_id),
        ("wallet", same_address(&run_set.wallet_id, &grant.wallet_id)),
        (
            "wallet package",
            same_address(&run_set.wallet_package_id, &grant.wallet_package_id),
        ),
    ] {
        if !agrees {
            return Err(Refusal::RunSetDisagrees(what.to_owned()));
        }
    }

    let wallet = chain
        .get_object(&grant.wallet_id)
        .await
        .map_err(|e| Refusal::WalletUnreadable(e.to_string()))?;
    let fields = wallet
        .fields
        .ok_or_else(|| Refusal::WalletUnreadable("the node returned no fields".into()))?;
    let owner = text_field(&fields, "owner")
        .ok_or_else(|| Refusal::WalletUnreadable("no owner field".into()))?;
    let wallet_agent = text_field(&fields, "agent")
        .ok_or_else(|| Refusal::WalletUnreadable("no agent field".into()))?;
    if !same_address(&wallet_agent, signer) {
        return Err(Refusal::AgentRotated { wallet_agent });
    }
    if fields.get("revoked") == Some(&Value::Bool(true)) {
        return Err(Refusal::WalletRevoked);
    }
    let expires = text_field(&fields, "expires_at_ms")
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| Refusal::WalletUnreadable("no expiry field".into()))?;
    if now_ms >= expires {
        return Err(Refusal::WalletExpired);
    }

    match chain
        .verify_personal_message(grant.message().as_bytes(), &signed.signature, &owner)
        .await
    {
        Ok(SignatureCheck::Valid) => Ok(VerifiedGrant {
            grant: grant.clone(),
            run_set,
            owner,
        }),
        Ok(SignatureCheck::Invalid(why)) => Err(Refusal::BadSignature(why)),
        Err(e) => Err(Refusal::BadSignature(format!(
            "the node could not be asked: {e}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rill_chain::fake::FakeSui;
    use rill_chain::{ObjectRef, ObjectSummary};
    use serde_json::json;

    const OWNER: &str = "0x00000000000000000000000000000000000000000000000000000000000000a1";
    const AGENT: &str = "0x00000000000000000000000000000000000000000000000000000000000000a2";
    const OTHER: &str = "0x00000000000000000000000000000000000000000000000000000000000000a3";
    const WALLET: &str = "0x00000000000000000000000000000000000000000000000000000000000000b1";
    const CAP: &str = "0x00000000000000000000000000000000000000000000000000000000000000b2";
    const VERSION: &str = "0x00000000000000000000000000000000000000000000000000000000000000b3";
    const PACKAGE: &str = "0x00000000000000000000000000000000000000000000000000000000000000c1";
    const NOW: u64 = 1_000_000;
    const SIGNATURE: &str = "owner-signature";

    fn run_set() -> Value {
        json!({
            "label": "swap",
            "network": "mainnet",
            "sender": AGENT,
            "actionId": "skill_x",
            "walletPackageId": PACKAGE,
            "walletId": WALLET,
            "agentCapId": CAP,
            "versionId": VERSION,
            "capabilityManifest": {
                "walletCoinType": "0x2::sui::SUI",
                "rules": [{"kind": "budget", "totalMist": "100000000"}, {"kind": "per_tx", "maxMist": "50000000"}]
            },
            "allowedTargets": [format!("{PACKAGE}::agent_wallet::request_spend")],
            "allowedObjectIds": [WALLET],
            "maxAmountBaseUnits": "50000000",
            "declaredSpendBaseUnits": "20000000",
            "minimumRemainingBaseUnits": "0",
            "gasCeilingBaseUnits": "100000000"
        })
    }

    fn signed() -> SignedGrant {
        SignedGrant {
            grant: Grant {
                network: "mainnet".into(),
                action_id: "skill_x".into(),
                action_name: "cetus swap".into(),
                agent: AGENT.into(),
                wallet_id: WALLET.into(),
                wallet_package_id: PACKAGE.into(),
                expires_at_ms: (NOW + 60_000).to_string(),
                revision: 1,
                run_set: run_set(),
                build_arguments: json!({"sender": AGENT}),
            },
            signature: SIGNATURE.into(),
        }
    }

    fn wallet(agent: &str, revoked: bool, expires: u64) -> ObjectSummary {
        ObjectSummary {
            reference: ObjectRef {
                id: WALLET.into(),
                version: 5,
                digest: sui_sdk_types::Digest::ZERO.to_string(),
            },
            object_type: Some(format!(
                "{PACKAGE}::agent_wallet::AgentWallet<0x2::sui::SUI>"
            )),
            fields: Some(json!({
                "owner": OWNER, "agent": agent, "revoked": revoked,
                "expires_at_ms": expires.to_string(), "budget": "100000000", "spent": "0"
            })),
            shared_initial_version: Some(4),
        }
    }

    /// A chain where the wallet is live, names AGENT, and OWNER signed exactly this grant.
    fn chain_for(signed: &SignedGrant, wallet: ObjectSummary) -> FakeSui {
        FakeSui::new()
            .with_object(None, wallet)
            .with_valid_signature(signed.grant.message().as_bytes(), &signed.signature, OWNER)
    }

    async fn check(chain: &FakeSui, signed: &SignedGrant) -> Result<VerifiedGrant, Refusal> {
        verify(chain, signed, AGENT, "mainnet", PACKAGE, NOW).await
    }

    #[tokio::test]
    async fn a_grant_the_owner_signed_for_a_live_wallet_is_used() {
        let grant = signed();
        let chain = chain_for(&grant, wallet(AGENT, false, NOW + 3_600_000));
        let verified = check(&chain, &grant).await.unwrap();
        assert_eq!(verified.owner, OWNER);
        assert_eq!(verified.run_set.action_id, "skill_x");
    }

    #[tokio::test]
    async fn a_modified_workflow_no_longer_matches_the_owners_signature() {
        let original = signed();
        let chain = chain_for(&original, wallet(AGENT, false, NOW + 3_600_000));
        let mut altered = original.clone();
        altered.grant.run_set["maxAmountBaseUnits"] = json!("90000000");
        assert!(matches!(
            check(&chain, &altered).await,
            Err(Refusal::BadSignature(_))
        ));
        let mut retargeted = original;
        retargeted.grant.run_set["allowedTargets"] = json!(["0x2::transfer::public_transfer"]);
        assert!(matches!(
            check(&chain, &retargeted).await,
            Err(Refusal::BadSignature(_))
        ));
    }

    #[tokio::test]
    async fn a_signature_from_anyone_but_the_wallets_owner_is_refused() {
        let grant = signed();
        let chain = FakeSui::new()
            .with_object(None, wallet(AGENT, false, NOW + 3_600_000))
            .with_valid_signature(grant.grant.message().as_bytes(), SIGNATURE, OTHER);
        assert!(matches!(
            check(&chain, &grant).await,
            Err(Refusal::BadSignature(_))
        ));
    }

    #[tokio::test]
    async fn the_wrong_network_or_deployment_is_refused_before_any_chain_read() {
        let grant = signed();
        let chain = FakeSui::new();
        assert!(matches!(
            verify(&chain, &grant, AGENT, "testnet", PACKAGE, NOW).await,
            Err(Refusal::WrongNetwork { .. })
        ));
        assert!(matches!(
            verify(&chain, &grant, AGENT, "mainnet", OTHER, NOW).await,
            Err(Refusal::WrongDeployment { .. })
        ));
        assert!(matches!(
            verify(&chain, &grant, OTHER, "mainnet", PACKAGE, NOW).await,
            Err(Refusal::NotThisAgent { .. })
        ));
    }

    #[tokio::test]
    async fn a_rotated_agent_a_revoked_wallet_and_an_expired_wallet_end_the_grant() {
        let grant = signed();
        let rotated = chain_for(&grant, wallet(OTHER, false, NOW + 3_600_000));
        assert!(matches!(
            check(&rotated, &grant).await,
            Err(Refusal::AgentRotated { .. })
        ));
        let revoked = chain_for(&grant, wallet(AGENT, true, NOW + 3_600_000));
        assert_eq!(
            check(&revoked, &grant).await.unwrap_err(),
            Refusal::WalletRevoked
        );
        let expired = chain_for(&grant, wallet(AGENT, false, NOW));
        assert_eq!(
            check(&expired, &grant).await.unwrap_err(),
            Refusal::WalletExpired
        );
    }

    #[tokio::test]
    async fn an_expired_grant_is_refused_even_on_a_live_wallet() {
        let mut grant = signed();
        grant.grant.expires_at_ms = NOW.to_string();
        let chain = chain_for(&grant, wallet(AGENT, false, NOW + 3_600_000));
        assert_eq!(check(&chain, &grant).await.unwrap_err(), Refusal::Expired);
    }

    #[tokio::test]
    async fn a_run_set_that_contradicts_its_grant_is_refused() {
        let mut grant = signed();
        grant.grant.run_set["walletId"] = json!(OTHER);
        let chain = chain_for(&grant, wallet(AGENT, false, NOW + 3_600_000));
        assert_eq!(
            check(&chain, &grant).await.unwrap_err(),
            Refusal::RunSetDisagrees("wallet".into())
        );
    }
}
