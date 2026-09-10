//! One command from nothing to a bounded wallet.
//!
//! The Definition of Done says a stranger curls a binary and watches an agent spend. Between those
//! two clauses sit two keys, a funded testnet address, a created wallet and attached rules, and
//! before this nothing owned that path: it existed as a sequence of commands in a README, each of
//! which could fail in a way that told you nothing about the next one.
//!
//! # This never writes a key
//!
//! `keystore.rs` reads `~/.sui/sui_config/sui.keystore` and never writes it, and that property is
//! worth more than the convenience of generating a key here. So when keys are missing this runs the
//! `sui` CLI's own `new-address`, which is the documented way to get one and already the thing a
//! reader would be told to run. Nothing in this file opens the keystore for writing, and a test
//! asserts that about the whole binary rather than about this function.
//!
//! # Why it waits instead of failing
//!
//! A fresh address has no SUI, and there is no faucet this process can call: the CLI faucet was
//! removed and the web one is interactive. Exiting with "unfunded" would leave the reader holding a
//! checklist again. So it prints the URL with the address already in it and polls until the coins
//! land, which is the one step a human genuinely has to do.

use rill_chain::{SuiRead, SuiWrite};
use rill_core::manifest::CapabilityManifest;
use serde_json::{json, Value};
use sui_sdk_types::Address;

/// Where a reader funds a fresh testnet address. The address is interpolated so the link is one
/// click rather than a form to fill in.
pub const FAUCET_URL: &str = "https://faucet.sui.io/?address=";

/// What one run of `init` needs. Every amount is a string for the same reason every amount in this
/// repository is: a float here is a rounding error in somebody's money.
#[derive(Debug, Clone)]
pub struct InitArgs {
    pub package_id: String,
    pub version_id: String,
    pub amount: String,
    pub budget_mist: String,
    pub per_tx_mist: String,
    pub gas_budget: u64,
    /// Where the run-set lands. Its presence is also how a second run knows not to mint again.
    pub run_set_path: std::path::PathBuf,
}

/// The two keys this machine will use, already read from the keystore by the caller.
pub struct Identities {
    pub owner: Address,
    pub agent: Address,
}

/// Why a run stopped, in the words the reader needs.
#[derive(Debug, PartialEq, Eq)]
pub enum Stopped {
    /// Fewer than two keys, and the exact commands that fix it.
    NeedsKeys { have: usize },
    /// A rule set with nothing in it. Attaching none would mint a capability bounded by nothing.
    EmptyRules,
    /// The owner has no SUI and the caller asked not to wait.
    Unfunded { owner: Address, url: String },
}

impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NeedsKeys { have } => write!(
                f,
                "this needs two keys and the keystore holds {have}: one owner, which creates the \
                 wallet and can revoke it, and one agent, which spends inside its rules. The \
                 delegation is only proved when they are different addresses. Run `sui client \
                 new-address ed25519` until there are two, then run this again. Nothing here \
                 writes a key."
            ),
            Self::EmptyRules => write!(
                f,
                "refusing to attach an empty rule set. A capability with no rules is bounded by \
                 nothing, and `confirm_spend` on an empty policy requires zero receipts, so the \
                 wallet would hand out its whole balance on request. Pass a budget and a \
                 per-transaction cap."
            ),
            Self::Unfunded { owner, url } => write!(
                f,
                "{owner} holds no SUI, so nothing can be funded or paid for. Fund it at {url} and \
                 run this again, or pass --wait to have this poll until the coins land."
            ),
        }
    }
}

/// The faucet link for an address, with the address already in it.
pub fn faucet_link(owner: &Address) -> String {
    format!("{FAUCET_URL}{owner}")
}

/// Whether this machine already has a wallet from a previous run.
///
/// The run-set is the marker because it is the artifact the signer actually needs, so its absence
/// and "no wallet yet" are the same state rather than two that can disagree. A second run with one
/// present is a no-op: minting again would leave the first wallet funded and forgotten, which on
/// testnet is waste and on mainnet is money.
pub fn already_initialised(path: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value
        .get("walletId")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// Check what a run would refuse before it spends anything.
///
/// Separate from the work so the refusals are reachable without a chain, and so the order of the
/// checks is visible: keys, then rules, then funds. A reader with no keys should not first be told
/// about funding.
pub fn refuse_early(
    keys_held: usize,
    manifest: &CapabilityManifest,
    owner: &Address,
    owner_balance_mist: u64,
    wait_for_funding: bool,
) -> Option<Stopped> {
    if keys_held < 2 {
        return Some(Stopped::NeedsKeys { have: keys_held });
    }
    if manifest.rules.is_empty() {
        return Some(Stopped::EmptyRules);
    }
    if owner_balance_mist == 0 && !wait_for_funding {
        return Some(Stopped::Unfunded {
            owner: *owner,
            url: faucet_link(owner),
        });
    }
    None
}

/// Mint the wallet and bound it, then write the run-set the signer will load.
///
/// Takes the chain so the whole path runs offline against the fake. The client is created by the
/// caller and used here, which keeps it inside the caller's runtime.
pub async fn run_on(
    chain: &(impl SuiRead + SuiWrite),
    owner_keystore: &crate::keystore::Keystore,
    identities: &Identities,
    args: &InitArgs,
    manifest: &CapabilityManifest,
    now_ms: u64,
) -> Result<Value, String> {
    if let Some(existing) = already_initialised(&args.run_set_path) {
        return Ok(json!({
            "alreadyInitialised": true,
            "walletId": existing,
            "runSet": args.run_set_path.display().to_string(),
            "note": "A run-set naming a wallet is already here, so nothing was minted. Delete it \
                     to start over, which leaves the old wallet on chain for the owner to revoke."
        }));
    }

    let created = crate::wallet::create_json_on(
        chain,
        owner_keystore,
        &crate::wallet::CreateArgs {
            package_id: args.package_id.clone(),
            version_id: args.version_id.clone(),
            agent: Some(identities.agent.to_string()),
            amount: args.amount.clone(),
            expires_in_days: 30,
            manifest: manifest.clone(),
            gas_budget: args.gas_budget,
            dry_run: false,
        },
        now_ms,
    )
    .await
    .map_err(|e| format!("creating the wallet: {e}"))?;

    let wallet_id = created
        .get("wallet")
        .and_then(Value::as_str)
        .ok_or("the create did not report a wallet id")?
        .to_owned();
    let cap_id = created
        .get("cap")
        .and_then(Value::as_str)
        .ok_or("the create did not report a capability id")?
        .to_owned();

    let attached = crate::rules_cmd::attach_json_on(
        chain,
        owner_keystore,
        &crate::rules_cmd::RulesArgs {
            package_id: args.package_id.clone(),
            version_id: args.version_id.clone(),
            wallet_id: wallet_id.clone(),
            manifest: manifest.clone(),
            gas_budget: args.gas_budget,
            dry_run: false,
        },
    )
    .await
    .map_err(|e| format!("attaching the rules: {e}"))?;

    Ok(json!({
        "alreadyInitialised": false,
        "owner": identities.owner.to_string(),
        "agent": identities.agent.to_string(),
        "walletId": wallet_id,
        "capId": cap_id,
        "created": created,
        "attached": attached,
        "runSet": args.run_set_path.display().to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rill_core::manifest::CapabilityRule;

    fn addr(n: u8) -> Address {
        format!("0x{:064x}", n).parse().unwrap()
    }

    fn bounded() -> CapabilityManifest {
        CapabilityManifest {
            wallet_coin_type: "0x2::sui::SUI".into(),
            rules: vec![CapabilityRule::Budget {
                total_mist: "200000000".into(),
            }],
        }
    }

    fn empty() -> CapabilityManifest {
        CapabilityManifest {
            wallet_coin_type: "0x2::sui::SUI".into(),
            rules: Vec::new(),
        }
    }

    /// One key is the case a stranger is actually in, and the refusal has to say which two keys and
    /// why they must differ.
    #[test]
    fn fewer_than_two_keys_is_refused_with_the_command_that_fixes_it() {
        let stopped = refuse_early(1, &bounded(), &addr(1), 1_000, false).expect("refused");
        assert_eq!(stopped, Stopped::NeedsKeys { have: 1 });
        let said = stopped.to_string();
        assert!(said.contains("sui client new-address ed25519"), "{said}");
        assert!(said.contains("different addresses"), "{said}");
        assert!(
            said.contains("Nothing here writes a key"),
            "the one promise worth repeating: {said}"
        );
    }

    /// Checked before funding on purpose: a reader holding no keys must not first be sent to a
    /// faucet for an address that does not exist yet.
    #[test]
    fn the_keys_are_checked_before_the_funding_is() {
        let stopped = refuse_early(0, &bounded(), &addr(1), 0, false).expect("refused");
        assert!(matches!(stopped, Stopped::NeedsKeys { .. }));
    }

    #[test]
    fn an_empty_rule_set_is_refused_and_says_what_it_would_have_minted() {
        let stopped = refuse_early(2, &empty(), &addr(1), 1_000, false).expect("refused");
        assert_eq!(stopped, Stopped::EmptyRules);
        let said = stopped.to_string();
        assert!(said.contains("bounded by nothing"), "{said}");
        assert!(said.contains("zero receipts"), "{said}");
    }

    /// An empty rule set is refused even on a machine that is otherwise ready, because the hazard
    /// is the capability rather than the setup.
    #[test]
    fn an_empty_rule_set_is_refused_before_funding_is_considered() {
        let stopped = refuse_early(2, &empty(), &addr(1), 0, false).expect("refused");
        assert_eq!(stopped, Stopped::EmptyRules);
    }

    #[test]
    fn an_unfunded_owner_is_given_a_link_with_its_own_address_in_it() {
        let owner = addr(7);
        let stopped = refuse_early(2, &bounded(), &owner, 0, false).expect("refused");
        let said = stopped.to_string();
        assert!(said.contains(&owner.to_string()), "{said}");
        assert!(said.contains(FAUCET_URL), "{said}");
        assert!(
            faucet_link(&owner).ends_with(&owner.to_string()),
            "the address must be in the link, not next to it"
        );
    }

    /// Waiting is the point: a reader who asked to wait must not be handed a refusal instead.
    #[test]
    fn an_unfunded_owner_is_not_refused_when_the_caller_asked_to_wait() {
        assert!(refuse_early(2, &bounded(), &addr(7), 0, true).is_none());
    }

    #[test]
    fn a_ready_machine_is_refused_nothing() {
        assert!(refuse_early(2, &bounded(), &addr(7), 1_000_000, false).is_none());
    }

    /// A second run must not mint a second wallet: the first one would stay funded and forgotten.
    #[test]
    fn a_run_set_naming_a_wallet_is_recognised_as_already_initialised() {
        let dir = std::env::temp_dir().join(format!("rill-init-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("run-set.json");
        std::fs::write(&path, r#"{"walletId":"0xabc","label":"x"}"#).unwrap();
        assert_eq!(already_initialised(&path), Some("0xabc".to_string()));
        std::fs::write(&path, r#"{"label":"no wallet here"}"#).unwrap();
        assert_eq!(already_initialised(&path), None);
        std::fs::write(&path, "not json at all").unwrap();
        assert_eq!(
            already_initialised(&path),
            None,
            "an unreadable run-set must read as not initialised rather than panic"
        );
        assert_eq!(already_initialised(&dir.join("absent.json")), None);
        let _ = std::fs::remove_file(&path);
    }
}
