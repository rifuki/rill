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
//! bounded only by the rules the Move contract enforces on the wallet. This signer's own local
//! builds (`rill_spend`, `rill_swap`, `rill_stake`, a hand-supplied `rill_execute`) are refused on
//! a wallet it holds a usable grant for whenever it reads grants from a Rill API, and on one whose
//! grant the node could not check ([`Refusal::NodeUnavailable`]), so they are not a route around a
//! grant; see `stdio::granted_wallet_gate`.

use std::collections::BTreeMap;

use rill_chain::{SignatureCheck, SuiRead};
use rill_core::grant::{Grant, SignedGrant};
use rill_core::runtime_bounds::{self, Limit, Rule};
use serde_json::Value;
use sui_sdk_types::Address;

use crate::grant_floor::GrantFloors;
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
    WrongNetwork {
        grant: String,
        signer: String,
    },
    NotThisAgent {
        grant: String,
        signer: String,
    },
    WrongDeployment {
        grant: String,
        signer: String,
    },
    Expired,
    RunSetDisagrees(String),
    RunSetInvalid(String),
    WalletUnreadable(String),
    AgentRotated {
        wallet_agent: String,
    },
    WalletRevoked,
    WalletExpired,
    BadSignature(String),
    /// The node could not answer a read the verdict depends on: the wallet, or whether the owner
    /// signed. Unlike every other refusal this says nothing about the grant itself, so a caller
    /// deciding whether a wallet is granted must not read it as "not granted".
    NodeUnavailable(String),
    /// The owner signed this grant, but this signer has already verified a newer revision of it.
    Superseded {
        floor: u64,
        given: u64,
    },
}

impl Refusal {
    /// The code an agent reads instead of parsing prose. A superseded grant is told apart because
    /// it is the one refusal that says the server served an approval the owner has since replaced.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Superseded { .. } => "grant_superseded",
            _ => "grant_refused",
        }
    }

    /// Whether this refusal leaves open that the grant is the owner's and usable. Only a node that
    /// could not answer does: every other refusal is a fact about the grant or the wallet.
    pub fn is_indeterminate(&self) -> bool {
        matches!(self, Self::NodeUnavailable(_))
    }
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
                "the grant names agent_wallet package {grant}, and this signer uses {signer}; \
                 if the package was upgraded, the owner has to grant this action again in Studio"
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
            Self::NodeUnavailable(why) => write!(
                f,
                "this signer could not check the grant, because the node could not answer: {why}"
            ),
            Self::Superseded { floor, given } => write!(
                f,
                "revision {given} of this grant is superseded: this signer has already verified \
                 revision {floor}, signed by the wallet's owner; an older approval is never used"
            ),
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
        .map_err(|e| Refusal::NodeUnavailable(format!("the wallet could not be read: {e}")))?;
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
        Err(e) => Err(Refusal::NodeUnavailable(format!(
            "the owner's signature could not be verified: {e}"
        ))),
    }
}

/// Raise each slot's floor to the newest grant verified here, then refuse every verified grant
/// below its floor.
///
/// Only a grant that passed every check raises a floor, so a forged or expired grant with a high
/// revision cannot lock a slot. An equal revision stays usable, which is what lets a workflow pin
/// the current revision exactly. A floor that cannot be read or written is an error, never "no
/// floor".
pub fn apply_floor(
    checked: &mut [(SignedGrant, Result<VerifiedGrant, Refusal>)],
    floors: &GrantFloors,
) -> Result<(), String> {
    let mut newest: BTreeMap<(String, String, String), u64> = BTreeMap::new();
    for (signed, verdict) in checked.iter() {
        if let Ok(verified) = verdict {
            let grant = &signed.grant;
            let slot = (
                verified.owner.clone(),
                grant.wallet_id.clone(),
                grant.action_id.clone(),
            );
            let revision = newest.entry(slot).or_insert(0);
            *revision = (*revision).max(grant.revision);
        }
    }
    for ((owner, wallet, action_id), revision) in &newest {
        floors.raise(owner, wallet, action_id, *revision)?;
    }
    for (signed, verdict) in checked.iter_mut() {
        let Ok(verified) = verdict else { continue };
        let grant = &signed.grant;
        let floor = floors.floor(&verified.owner, &grant.wallet_id, &grant.action_id)?;
        if grant.revision < floor {
            *verdict = Err(Refusal::Superseded {
                floor,
                given: grant.revision,
            });
        }
    }
    Ok(())
}

/// Refuse an agent's runtime override unless the owner signed a bound that it keeps within.
///
/// The builder checks the same overrides against the published node, but the builder is the party
/// a grant exists to constrain, so the signer checks them first, from the grant alone, and sends
/// nothing it would refuse. What this covers is the params object an agent passes. The values the
/// builder then encodes into the transaction are not re-read from its bytes here; that check is a
/// later release.
///
/// A grant signed before bounds were signed with it has no `runtimeBounds`. For those the signer
/// falls back to the strictest table it can build from the signed `params`: see [`legacy_bound`].
pub fn check_overrides(build_arguments: &Value, overrides: &Value) -> Result<(), String> {
    let overrides = overrides
        .as_object()
        .ok_or("runtime params must be an object keyed by node id")?;
    let signed = build_arguments.get("runtimeBounds");
    for (node, values) in overrides {
        // A flat value would reach a single-action builder unscoped, outside any node's bounds.
        let values = values.as_object().ok_or_else(|| {
            format!("{node} must be an object of runtime values keyed by a node id")
        })?;
        for (key, value) in values {
            let bound = match signed {
                Some(bounds) => signed_bound(bounds, node, key)?,
                None => legacy_bound(build_arguments, node, key)?,
            };
            bound
                .check(key, value)
                .map_err(|why| format!("{node}.{key}: {why}"))?;
        }
    }
    Ok(())
}

fn signed_bound(bounds: &Value, node: &str, key: &str) -> Result<Limit, String> {
    let bounds = bounds
        .as_object()
        .ok_or("the grant's runtimeBounds is not an object")?;
    let bound = bounds
        .get(node)
        .and_then(|n| n.get(key))
        .ok_or_else(|| format!("{node}.{key} is not an owner-bounded runtime parameter"))?;
    serde_json::from_value(bound.clone())
        .map_err(|e| format!("{node}.{key} has a bound this signer cannot read: {e}"))
}

/// The bound for a grant signed before `runtimeBounds` existed. Its signed `params` carry only the
/// object ids, the price and a swap's `amount_in`, so:
///
/// - `amount_in`, a price and an object id are held to the signed value: at most it for the
///   amount, exactly it for the rest, since the order side was never signed and a price can only
///   be "not worse" relative to a side.
/// - An amount or floor the grant never signed (`quantity`, `depositSui`, `amount`,
///   `min_amount_out`) is checked here only for form. Its bound is enforced by the builder alone,
///   against the published node, until the owner signs a new grant that carries bounds.
/// - `clientOrderId` must be a `u64`.
/// - A fixed key (`poolKey`, `isBid`, `payWithDeep`) is refused: the grant never signed it, and
///   repeating the published value changes nothing.
fn legacy_bound(build_arguments: &Value, node: &str, key: &str) -> Result<Limit, String> {
    let rule = runtime_bounds::rule_for_key(key)
        .ok_or_else(|| format!("{node}.{key} is not an owner-bounded runtime parameter"))?;
    let signed = build_arguments
        .get("params")
        .and_then(|p| p.get(node))
        .and_then(|n| n.get(key))
        .filter(|v| !v.is_null());
    let unsigned =
        || format!("{node}.{key} was not signed in this grant; the owner can grant it again");
    Ok(match rule {
        Rule::AtMost => Limit {
            max: match signed {
                Some(value) => Some(value.as_str().ok_or_else(unsigned)?.to_owned()),
                None => None,
            },
            min: signed.is_none().then(|| "0".to_owned()),
            u64: runtime_bounds::base_units(key),
            ..Limit::default()
        },
        Rule::AtLeast => Limit {
            min: Some("0".to_owned()),
            u64: true,
            ..Limit::default()
        },
        Rule::Price | Rule::Object => Limit {
            eq: Some(signed.ok_or_else(unsigned)?.clone()),
            ..Limit::default()
        },
        Rule::Fixed => return Err(unsigned()),
        Rule::AnyU64 => Limit {
            u64: true,
            ..Limit::default()
        },
    })
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

    /// After an agent_wallet upgrade every saved grant still names the old package, and the owner's
    /// signature covers it, so nothing short of a new grant fixes it. The agent relays this text,
    /// so it has to say what the owner does next, not only which ids differ.
    #[tokio::test]
    async fn a_grant_on_an_upgraded_package_tells_the_owner_to_grant_again() {
        let grant = signed();
        let refusal = verify(&FakeSui::new(), &grant, AGENT, "mainnet", OTHER, NOW)
            .await
            .unwrap_err();
        let text = refusal.to_string();
        assert!(text.contains(PACKAGE) && text.contains(OTHER), "{text}");
        assert!(text.contains("grant this action again"), "{text}");
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

    /// A node that cannot be asked has not said the grant is forged or the wallet gone. The
    /// granted-wallet gate refuses a local build on that answer, which it can do only if the
    /// verdict says it is unknown rather than wearing a definite refusal's name.
    #[tokio::test]
    async fn a_node_that_cannot_answer_leaves_the_grant_undetermined() {
        let grant = signed();
        let unasked = chain_for(&grant, wallet(AGENT, false, NOW + 3_600_000))
            .with_signature_check_unavailable();
        let refusal = check(&unasked, &grant).await.unwrap_err();
        assert!(
            matches!(refusal, Refusal::NodeUnavailable(_)) && refusal.is_indeterminate(),
            "{refusal:?}"
        );

        let no_wallet = FakeSui::new().with_valid_signature(
            grant.grant.message().as_bytes(),
            &grant.signature,
            OWNER,
        );
        let refusal = check(&no_wallet, &grant).await.unwrap_err();
        assert!(refusal.is_indeterminate(), "{refusal:?}");

        // A node that answered "not this owner's signature" has decided it.
        let forged = FakeSui::new().with_object(None, wallet(AGENT, false, NOW + 3_600_000));
        assert!(!check(&forged, &grant).await.unwrap_err().is_indeterminate());
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

    fn bounded() -> Value {
        json!({
            "sender": AGENT,
            "params": {"order": {"price": "1.5", "tradeCapId": CAP}},
            "runtimeBounds": {
                "order": {
                    "quantity": {"max": "1"},
                    "isBid": {"eq": false},
                    "price": {"min": "1", "max": "2"},
                    "tradeCapId": {"eq": CAP},
                    "clientOrderId": {"u64": true}
                }
            }
        })
    }

    #[test]
    fn check_overrides_enforces_signed_bounds() {
        let args = bounded();
        let ok = |o: Value| check_overrides(&args, &o);
        assert!(ok(json!({"order": {"quantity": "0.5"}})).is_ok());
        assert!(ok(json!({"order": {"quantity": "1.00"}})).is_ok());
        assert!(ok(json!({"order": {"quantity": "2"}})).is_err());
        assert!(ok(json!({"order": {"isBid": true}})).is_err());
        assert!(ok(json!({"order": {"isBid": "false"}})).is_ok());
        assert!(ok(json!({"order": {"price": "2.5"}})).is_err());
        assert!(ok(json!({"order": {"price": "0.9"}})).is_err());
        assert!(ok(json!({"order": {"price": "1.99"}})).is_ok());
        assert!(ok(json!({"order": {"tradeCapId": "0xb2"}})).is_ok());
        assert!(ok(json!({"order": {"tradeCapId": OTHER}})).is_err());
        assert!(ok(json!({"order": {"clientOrderId": "9"}})).is_ok());
        assert!(ok(json!({"order": {"clientOrderId": "x"}})).is_err());
        let absent = ok(json!({"order": {"depositSui": "0.1"}})).unwrap_err();
        assert!(absent.contains("order.depositSui"), "{absent}");
        assert!(ok(json!({"other": {"quantity": "0.5"}})).is_err());
        // A flat value is how a single-action override reaches the builder unscoped.
        assert!(ok(json!({"quantity": "0.5"})).is_err());
        assert!(ok(json!("0.5")).is_err());
    }

    #[test]
    fn check_overrides_refuses_bounds_it_cannot_read() {
        let mut args = bounded();
        args["runtimeBounds"]["order"]["quantity"] = json!({"atMost": "1"});
        assert!(check_overrides(&args, &json!({"order": {"quantity": "0.5"}})).is_err());
        args["runtimeBounds"] = json!("everything");
        assert!(check_overrides(&args, &json!({"order": {"quantity": "0.5"}})).is_err());
    }

    #[test]
    fn check_overrides_legacy_grant_without_bounds() {
        let args = json!({
            "sender": AGENT,
            "params": {
                "swap": {"amount_in": "100"},
                "order": {"price": "1.5", "tradeCapId": CAP, "balanceManagerId": WALLET}
            }
        });
        let ok = |o: Value| check_overrides(&args, &o);
        assert!(ok(json!({"swap": {"amount_in": "50"}})).is_ok());
        assert!(ok(json!({"swap": {"amount_in": "101"}})).is_err());
        // Floors and amounts the legacy grant never signed are bounded by the builder alone.
        assert!(ok(json!({"swap": {"min_amount_out": "7"}})).is_ok());
        assert!(ok(json!({"swap": {"min_amount_out": "7.5"}})).is_err());
        assert!(ok(json!({"order": {"quantity": "0.5"}})).is_ok());
        assert!(ok(json!({"order": {"clientOrderId": "3"}})).is_ok());
        assert!(ok(json!({"order": {"clientOrderId": "-3"}})).is_err());
        // Without the side in the signed params, the signed price is the only safe one.
        assert!(ok(json!({"order": {"price": "1.5"}})).is_ok());
        assert!(ok(json!({"order": {"price": "1.6"}})).is_err());
        assert!(ok(json!({"order": {"tradeCapId": OTHER}})).is_err());
        assert!(ok(json!({"order": {"depositCapId": CAP}})).is_err());
        assert!(ok(json!({"order": {"poolKey": "SUI_USDC"}})).is_err());
        assert!(ok(json!({"order": {"isBid": false}})).is_err());
        assert!(ok(json!({"order": {"integratePackageId": "0x2"}})).is_err());
    }
}
