//! Pending step-up approvals: a transaction an agent's signer paused on, waiting for its owner.
//!
//! In memory only. An entry lives exactly as long as the envelope it describes, at most five
//! minutes, and a lost entry costs the agent one rebuild and the owner one more look, so there is
//! nothing worth a file. The store decides nothing about trust: the server checks a request before
//! it is prepared and an owner's signature before it is stored, and the signer checks both again
//! before it signs anything.

use std::collections::BTreeMap;
use std::sync::Mutex;

use rill_core::step_up::StepUpApproval;

use crate::{StoreError, StoreResult};

/// The most requests held at once. Reaching it is refused rather than evicting one an owner may be
/// reading right now.
pub const MAX_PENDING_STEP_UPS: usize = 500;

/// The most requests one agent holds at once. Opening a request costs nothing on chain, so the
/// global cap alone would let one agent with one cheap wallet hold every slot; with this, an agent
/// that floods the store blocks only itself.
pub const MAX_PENDING_STEP_UPS_PER_AGENT: usize = 4;

/// One request, as the owner's approval page reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepUpEntry {
    pub approval: StepUpApproval,
    /// The base64 transaction the approval names, so the page can recompute its digest itself.
    pub unsigned_ptb: String,
    /// The owner's signature over [`StepUpApproval::message`], once given.
    pub owner_signature: Option<String>,
}

#[derive(Default)]
pub struct StepUpStore {
    entries: Mutex<BTreeMap<String, StepUpEntry>>,
}

/// One spelling per address, so case or leading zeros cannot make one agent look like many.
fn agent_key(address: &str) -> String {
    let hex = address.strip_prefix("0x").unwrap_or(address);
    let digits = hex.trim_start_matches('0');
    digits.to_ascii_lowercase()
}

fn poisoned() -> StoreError {
    StoreError::Io("step-up lock poisoned".into())
}

impl StepUpStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Hold a checked request under `id` until its envelope expires.
    pub fn prepare(
        &self,
        id: String,
        approval: StepUpApproval,
        unsigned_ptb: String,
        now_ms: u64,
    ) -> StoreResult<()> {
        let mut entries = self.entries.lock().map_err(|_| poisoned())?;
        entries.retain(|_, entry| !entry.approval.is_expired(now_ms));
        if entries.contains_key(&id) {
            return Err(StoreError::Conflict(format!(
                "step-up request {id} already exists"
            )));
        }
        let agent = agent_key(&approval.agent);
        let held = entries
            .values()
            .filter(|entry| agent_key(&entry.approval.agent) == agent)
            .count();
        if held >= MAX_PENDING_STEP_UPS_PER_AGENT {
            return Err(StoreError::AtCapacity {
                limit: MAX_PENDING_STEP_UPS_PER_AGENT,
            });
        }
        if entries.len() >= MAX_PENDING_STEP_UPS {
            return Err(StoreError::AtCapacity {
                limit: MAX_PENDING_STEP_UPS,
            });
        }
        entries.insert(
            id,
            StepUpEntry {
                approval,
                unsigned_ptb,
                owner_signature: None,
            },
        );
        Ok(())
    }

    /// The live request under `id`, or `None` when there is none or it has expired.
    pub fn get(&self, id: &str, now_ms: u64) -> StoreResult<Option<StepUpEntry>> {
        Ok(self
            .entries
            .lock()
            .map_err(|_| poisoned())?
            .get(id)
            .filter(|entry| !entry.approval.is_expired(now_ms))
            .cloned())
    }

    /// Record the owner's signature, once. A second approval is a conflict, so whatever the owner
    /// signed first is what the signer reads.
    pub fn approve(
        &self,
        id: &str,
        owner_signature: &str,
        now_ms: u64,
    ) -> StoreResult<StepUpEntry> {
        let mut entries = self.entries.lock().map_err(|_| poisoned())?;
        let entry = entries
            .get_mut(id)
            .filter(|entry| !entry.approval.is_expired(now_ms))
            .ok_or_else(|| StoreError::NotFound(format!("step-up request {id}")))?;
        if entry.owner_signature.is_some() {
            return Err(StoreError::Conflict(format!(
                "step-up request {id} is already approved"
            )));
        }
        entry.owner_signature = Some(owner_signature.to_owned());
        Ok(entry.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approval(expires_at_ms: u64) -> StepUpApproval {
        approval_from("0xa2", expires_at_ms)
    }

    fn approval_from(agent: &str, expires_at_ms: u64) -> StepUpApproval {
        StepUpApproval {
            network: "testnet".into(),
            agent: agent.into(),
            wallet_id: "0xb1".into(),
            wallet_package_id: "0xc1".into(),
            action_id: "skill_x".into(),
            action_name: "swap".into(),
            grant_revision: 1,
            grant_digest: "aa".repeat(32),
            pinned_digest: "bb".repeat(32),
            transaction_digest: "tx".into(),
            spend_base_units: "20".into(),
            threshold_base_units: "10".into(),
            coin_type: "0x2::sui::SUI".into(),
            expires_at_ms: expires_at_ms.to_string(),
        }
    }

    #[test]
    fn an_approval_is_stored_once_and_a_second_one_is_a_conflict() {
        let store = StepUpStore::new();
        store
            .prepare("r1".into(), approval(1_000), "ptb".into(), 0)
            .unwrap();
        assert!(store
            .get("r1", 0)
            .unwrap()
            .unwrap()
            .owner_signature
            .is_none());

        let approved = store.approve("r1", "sig", 10).unwrap();
        assert_eq!(approved.owner_signature.as_deref(), Some("sig"));
        assert_eq!(
            store
                .get("r1", 10)
                .unwrap()
                .unwrap()
                .owner_signature
                .as_deref(),
            Some("sig")
        );
        assert!(matches!(
            store.approve("r1", "other", 20),
            Err(StoreError::Conflict(_))
        ));
        assert_eq!(
            store
                .get("r1", 20)
                .unwrap()
                .unwrap()
                .owner_signature
                .as_deref(),
            Some("sig"),
            "the first approval stands"
        );
    }

    #[test]
    fn an_entry_ends_with_its_envelope_and_a_duplicate_id_is_refused() {
        let store = StepUpStore::new();
        store
            .prepare("r1".into(), approval(1_000), "ptb".into(), 0)
            .unwrap();
        assert!(matches!(
            store.prepare("r1".into(), approval(1_000), "ptb".into(), 0),
            Err(StoreError::Conflict(_))
        ));
        assert!(store.get("r1", 1_000).unwrap().is_none());
        assert!(matches!(
            store.approve("r1", "sig", 1_000),
            Err(StoreError::NotFound(_))
        ));
    }

    /// Full is refused rather than evicting a request an owner may be reading right now.
    #[test]
    fn a_full_store_refuses_instead_of_evicting() {
        let store = StepUpStore::new();
        for n in 0..MAX_PENDING_STEP_UPS {
            store
                .prepare(
                    format!("r{n}"),
                    approval_from(&format!("0x{n:x}"), 1_000),
                    "ptb".into(),
                    0,
                )
                .unwrap();
        }
        assert!(matches!(
            store.prepare("one-more".into(), approval(1_000), "ptb".into(), 0),
            Err(StoreError::AtCapacity { .. })
        ));
        assert!(store.get("r0", 0).unwrap().is_some());
        // Expired entries make room.
        store
            .prepare("later".into(), approval(5_000), "ptb".into(), 1_000)
            .unwrap();
    }

    /// Opening a request costs an agent nothing on chain, so without a limit per agent one agent
    /// with one cheap wallet could hold every slot and stop every other owner's approvals.
    #[test]
    fn one_agent_at_its_limit_does_not_stop_another_agent() {
        let store = StepUpStore::new();
        for n in 0..MAX_PENDING_STEP_UPS_PER_AGENT {
            store
                .prepare(
                    format!("a{n}"),
                    approval_from("0xa2", 1_000),
                    "ptb".into(),
                    0,
                )
                .unwrap();
        }
        // The same address written another way is the same agent.
        for spelling in ["0xa2", "0xA2", "0x00a2", &format!("0x{:0>64}", "a2")] {
            assert!(
                matches!(
                    store.prepare(
                        format!("more-{spelling}"),
                        approval_from(spelling, 1_000),
                        "ptb".into(),
                        0
                    ),
                    Err(StoreError::AtCapacity {
                        limit: MAX_PENDING_STEP_UPS_PER_AGENT
                    })
                ),
                "{spelling} must count against the same agent"
            );
        }
        store
            .prepare("b0".into(), approval_from("0xb7", 1_000), "ptb".into(), 0)
            .unwrap();
        // An agent's own requests ending makes room for it again.
        store
            .prepare(
                "a-later".into(),
                approval_from("0xa2", 5_000),
                "ptb".into(),
                1_000,
            )
            .unwrap();
    }
}
