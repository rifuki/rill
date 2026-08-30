//! The run-set: what this signer is permitted to do, pinned before anything arrives.
//!
//! Everything a policy check compares against comes from here, and nothing comes from the envelope
//! being checked. That is the whole point of the file existing separately: an envelope that
//! supplied its own limits would be asking to be trusted about how much of someone's money it may
//! move.
//!
//! # It is written by a human or by onboarding, never by an agent
//!
//! No MCP tool writes this file. An agent that could widen its own limits has no limits, and the
//! Move contract makes the same choice — `add_rule` and `rotate_agent` are owner-only precisely so
//! the agent cannot reach them.
//!
//! # Public values only
//!
//! Object ids, addresses, amounts, a manifest. The key lives in the process environment and is
//! never persisted; a run-set is safe to read, diff, and commit alongside a deployment.

use std::path::{Path, PathBuf};

use rill_core::envelope::Network;
use rill_core::manifest::CapabilityManifest;
use rill_policy::LocalPolicy;
use serde::{Deserialize, Serialize};

/// Where a run-set lives unless told otherwise.
pub const RUN_SET_VAR: &str = "RILL_RUN_SET_PATH";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunSet {
    /// A label for humans. Never compared against anything.
    pub label: String,
    pub network: Network,
    /// The address this signer holds the key for. Checked against the key on load, so a run-set
    /// paired with the wrong key fails at startup rather than at signing time.
    pub sender: String,
    /// The one action this run may build. An envelope for anything else is refused.
    pub action_id: String,

    pub wallet_package_id: String,
    pub wallet_id: String,
    pub agent_cap_id: String,
    pub version_id: String,
    pub capability_manifest: CapabilityManifest,

    /// Exact Move call sequence, in order.
    pub allowed_targets: Vec<String>,
    pub allowed_object_ids: Vec<String>,

    /// Ceiling one, from this file.
    pub max_amount_base_units: String,
    /// Ceiling two. Recorded separately and compared separately — two limits from one source is
    /// one limit written twice.
    pub declared_spend_base_units: String,
    /// What must remain in the wallet afterwards. No on-chain rule expresses this.
    pub minimum_remaining_base_units: String,
    pub gas_ceiling_base_units: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunSetError {
    NotConfigured,
    Unreadable(String),
    Malformed(String),
    /// The run-set names an address this signer does not hold the key for.
    WrongKey {
        run_set: String,
        signer: String,
    },
    BadAmount {
        field: &'static str,
        reason: String,
    },
    EmptyTargets,
}

impl std::fmt::Display for RunSetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotConfigured => write!(
                f,
                "no run-set is configured. Set {RUN_SET_VAR} to a run-set file — without one there \
                 are no pinned limits, and signing against limits nobody set is worse than not \
                 signing."
            ),
            Self::Unreadable(m) => write!(f, "the run-set could not be read: {m}"),
            Self::Malformed(m) => write!(f, "the run-set is not valid: {m}"),
            Self::WrongKey { run_set, signer } => write!(
                f,
                "this run-set is for {run_set} but this signer holds the key for {signer}; refusing \
                 to run a policy written for somebody else's wallet"
            ),
            Self::BadAmount { field, reason } => write!(f, "{field}: {reason}"),
            Self::EmptyTargets => write!(
                f,
                "allowedTargets is empty, which would permit no transaction at all — an empty \
                 allowlist is a mistake, not a lockdown"
            ),
        }
    }
}

impl std::error::Error for RunSetError {}

fn amount(value: &str, field: &'static str) -> Result<u64, RunSetError> {
    rill_core::amounts::parse_u64_string(value).map_err(|e| RunSetError::BadAmount {
        field,
        reason: e.to_string(),
    })
}

impl RunSet {
    pub fn from_path(path: &Path) -> Result<Self, RunSetError> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| RunSetError::Unreadable(format!("{}: {e}", path.display())))?;
        let run_set: Self =
            serde_json::from_str(&raw).map_err(|e| RunSetError::Malformed(e.to_string()))?;
        run_set.validate()?;
        Ok(run_set)
    }

    pub fn path_from_env() -> Option<PathBuf> {
        std::env::var(RUN_SET_VAR).ok().map(PathBuf::from)
    }

    fn validate(&self) -> Result<(), RunSetError> {
        if self.allowed_targets.is_empty() {
            return Err(RunSetError::EmptyTargets);
        }
        self.capability_manifest
            .validate()
            .map_err(|e| RunSetError::Malformed(format!("capabilityManifest: {e}")))?;
        amount(&self.max_amount_base_units, "maxAmountBaseUnits")?;
        amount(&self.declared_spend_base_units, "declaredSpendBaseUnits")?;
        amount(
            &self.minimum_remaining_base_units,
            "minimumRemainingBaseUnits",
        )?;
        amount(&self.gas_ceiling_base_units, "gasCeilingBaseUnits")?;
        Ok(())
    }

    /// Confirm the run-set and the loaded key describe the same wallet.
    ///
    /// Checked at load rather than at signing: a mismatch discovered mid-flow costs a user an
    /// unexplained refusal, and one discovered at startup costs them a corrected path.
    pub fn check_key(&self, signer_address: &str) -> Result<(), RunSetError> {
        if self.sender.eq_ignore_ascii_case(signer_address) {
            Ok(())
        } else {
            Err(RunSetError::WrongKey {
                run_set: self.sender.clone(),
                signer: signer_address.to_owned(),
            })
        }
    }

    /// Project to the policy the validation chain compares against.
    pub fn to_policy(&self) -> Result<LocalPolicy, RunSetError> {
        Ok(LocalPolicy {
            network: self.network,
            sender: self.sender.clone(),
            action_id: self.action_id.clone(),
            wallet_package_id: self.wallet_package_id.clone(),
            wallet_id: self.wallet_id.clone(),
            agent_cap_id: self.agent_cap_id.clone(),
            allowed_targets: self.allowed_targets.clone(),
            required_object_ids: self.allowed_object_ids.clone(),
            max_amount_base_units: amount(&self.max_amount_base_units, "maxAmountBaseUnits")?,
            declared_spend_base_units: amount(
                &self.declared_spend_base_units,
                "declaredSpendBaseUnits",
            )?,
            minimum_remaining_base_units: amount(
                &self.minimum_remaining_base_units,
                "minimumRemainingBaseUnits",
            )?,
            gas_ceiling_base_units: amount(&self.gas_ceiling_base_units, "gasCeilingBaseUnits")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn valid() -> serde_json::Value {
        json!({
            "label": "hero-testnet",
            "network": "testnet",
            "sender": "0xagent",
            "actionId": "skill_hero",
            "walletPackageId": "0xpkg",
            "walletId": "0xwallet",
            "agentCapId": "0xcap",
            "versionId": "0xversion",
            "capabilityManifest": {
                "walletCoinType": "0x2::sui::SUI",
                "rules": [{ "kind": "budget", "totalMist": "5000000000" }]
            },
            "allowedTargets": ["0xpkg::agent_wallet::request_spend"],
            "allowedObjectIds": ["0xwallet"],
            "maxAmountBaseUnits": "2000000000",
            "declaredSpendBaseUnits": "2000000000",
            "minimumRemainingBaseUnits": "1000000",
            "gasCeilingBaseUnits": "50000000"
        })
    }

    fn write(value: &serde_json::Value) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rill-runset-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{}.json", rill_auth_random()));
        std::fs::write(&path, serde_json::to_string_pretty(value).unwrap()).unwrap();
        path
    }

    fn rill_auth_random() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos() as u64
    }

    #[test]
    fn a_valid_run_set_loads_and_projects_to_a_policy() {
        let run_set = RunSet::from_path(&write(&valid())).expect("should load");
        let policy = run_set.to_policy().expect("should project");
        assert_eq!(policy.action_id, "skill_hero");
        assert_eq!(policy.max_amount_base_units, 2_000_000_000);
        assert_eq!(policy.minimum_remaining_base_units, 1_000_000);
    }

    /// A run-set paired with the wrong key must fail at load, not at signing time — a mismatch
    /// found mid-flow costs a user an unexplained refusal.
    #[test]
    fn a_run_set_for_another_wallet_is_refused() {
        let run_set = RunSet::from_path(&write(&valid())).unwrap();
        assert!(matches!(
            run_set.check_key("0xsomeone_else"),
            Err(RunSetError::WrongKey { .. })
        ));
        assert!(run_set.check_key("0xAGENT").is_ok(), "case-insensitive");
    }

    #[test]
    fn an_empty_target_list_is_a_mistake_not_a_lockdown() {
        let mut v = valid();
        v["allowedTargets"] = json!([]);
        assert!(matches!(
            RunSet::from_path(&write(&v)),
            Err(RunSetError::EmptyTargets)
        ));
    }

    #[test]
    fn a_manifest_with_no_rules_is_refused_at_load() {
        let mut v = valid();
        v["capabilityManifest"]["rules"] = json!([]);
        assert!(matches!(
            RunSet::from_path(&write(&v)),
            Err(RunSetError::Malformed(_))
        ));
    }

    #[test]
    fn an_amount_that_is_not_a_u64_string_is_refused() {
        for field in [
            "maxAmountBaseUnits",
            "declaredSpendBaseUnits",
            "minimumRemainingBaseUnits",
            "gasCeilingBaseUnits",
        ] {
            let mut v = valid();
            v[field] = json!("1.5");
            assert!(
                matches!(
                    RunSet::from_path(&write(&v)),
                    Err(RunSetError::BadAmount { .. })
                ),
                "{field} must be a base-unit integer string"
            );
        }
    }

    /// Amounts are strings here for the same reason they are everywhere else in this workspace.
    #[test]
    fn a_numeric_amount_is_refused_rather_than_read_as_a_float() {
        let mut v = valid();
        v["maxAmountBaseUnits"] = json!(2000000000u64);
        assert!(matches!(
            RunSet::from_path(&write(&v)),
            Err(RunSetError::Malformed(_))
        ));
    }

    /// A field nobody declared is refused rather than ignored — the same fail-closed posture the
    /// envelope schema takes, and for the same reason.
    #[test]
    fn an_unknown_field_is_refused() {
        let mut v = valid();
        v["allowUnverifiedSimulation"] = json!(true);
        assert!(matches!(
            RunSet::from_path(&write(&v)),
            Err(RunSetError::Malformed(_))
        ));
    }

    #[test]
    fn a_missing_file_is_reported_as_unreadable() {
        assert!(matches!(
            RunSet::from_path(Path::new("/nonexistent/run-set.json")),
            Err(RunSetError::Unreadable(_))
        ));
    }
}
