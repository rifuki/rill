//! Owner-signed action grants, held for the signer that will fetch them.
//!
//! The store keeps what it was given and decides nothing about trust: every grant is checked by
//! the server against the chain before it is saved, and again by the signer before it is used.
//! What the store does enforce is ordering, so an older approval can never replace a newer one.

use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use rill_core::grant::SignedGrant;

use crate::file::{io, write_atomic};
use crate::{StoreError, StoreResult};

/// The most grants one deployment holds. Reaching it is refused rather than evicting.
pub const MAX_STORED_GRANTS: usize = 2_000;

/// Addresses compare in one normalized form, as for skills.
fn normalized(address: &str) -> String {
    address.trim().to_lowercase()
}

fn same_slot(a: &SignedGrant, b: &SignedGrant) -> bool {
    normalized(&a.grant.agent) == normalized(&b.grant.agent)
        && normalized(&a.grant.wallet_id) == normalized(&b.grant.wallet_id)
        && a.grant.action_id == b.grant.action_id
}

/// On disk, a top-level JSON array of signed grants.
pub struct FileGrantStore {
    path: PathBuf,
    grants: Mutex<Vec<SignedGrant>>,
}

impl FileGrantStore {
    /// Load, tolerating a missing file. A corrupt one starts empty and says so: grants are
    /// re-signable by their owners, and refusing to boot would take every other route down.
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let grants = match fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str::<Vec<SignedGrant>>(&raw).unwrap_or_else(|e| {
                eprintln!(
                    "[store] {} could not be read ({e}); starting empty. Owners will need to \
                     sign their grants again.",
                    path.display()
                );
                Vec::new()
            }),
            Err(_) => Vec::new(),
        };
        Self {
            path,
            grants: Mutex::new(grants),
        }
    }

    /// Every grant held for `agent`.
    pub fn list_for_agent(&self, agent: &str) -> Vec<SignedGrant> {
        let wanted = normalized(agent);
        if wanted.is_empty() {
            return Vec::new();
        }
        self.grants
            .lock()
            .map(|grants| {
                grants
                    .iter()
                    .filter(|g| normalized(&g.grant.agent) == wanted)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The revision held for this agent, wallet and action, or 0 when there is none.
    pub fn current_revision(&self, agent: &str, wallet_id: &str, action_id: &str) -> u64 {
        self.grants
            .lock()
            .ok()
            .and_then(|grants| {
                grants
                    .iter()
                    .find(|g| {
                        normalized(&g.grant.agent) == normalized(agent)
                            && normalized(&g.grant.wallet_id) == normalized(wallet_id)
                            && g.grant.action_id == action_id
                    })
                    .map(|g| g.grant.revision)
            })
            .unwrap_or(0)
    }

    /// Save, replacing the grant for the same agent, wallet and action only with a newer revision.
    pub fn save(&self, signed: SignedGrant) -> StoreResult<()> {
        let mut grants = self
            .grants
            .lock()
            .map_err(|_| StoreError::Io("store lock poisoned".into()))?;
        if let Some(existing) = grants.iter_mut().find(|g| same_slot(g, &signed)) {
            if signed.grant.revision <= existing.grant.revision {
                return Err(StoreError::StaleRevision {
                    current: existing.grant.revision,
                    given: signed.grant.revision,
                });
            }
            *existing = signed;
        } else {
            if grants.len() >= MAX_STORED_GRANTS {
                return Err(StoreError::GrantsAtCapacity {
                    limit: MAX_STORED_GRANTS,
                });
            }
            grants.push(signed);
        }
        let json = serde_json::to_string_pretty(&*grants).map_err(io)?;
        write_atomic(&self.path, &json, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rill_core::grant::Grant;
    use serde_json::json;

    fn signed(agent: &str, revision: u64) -> SignedGrant {
        SignedGrant {
            grant: Grant {
                network: "mainnet".into(),
                action_id: "skill_x".into(),
                action_name: "swap".into(),
                agent: agent.into(),
                wallet_id: "0xb1".into(),
                wallet_package_id: "0xc1".into(),
                expires_at_ms: "1000".into(),
                revision,
                run_set: json!({}),
                build_arguments: json!({}),
            },
            signature: format!("sig-{revision}"),
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rill-grants-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join("grants.json")
    }

    #[test]
    fn a_newer_revision_replaces_and_an_older_one_is_refused() {
        let path = scratch("revisions");
        let store = FileGrantStore::load(&path);
        store.save(signed("0xA2", 1)).unwrap();
        store.save(signed("0xa2", 2)).unwrap();
        assert_eq!(store.list_for_agent("0xa2").len(), 1);
        assert_eq!(store.current_revision("0xa2", "0xb1", "skill_x"), 2);
        assert_eq!(
            store.save(signed("0xa2", 2)),
            Err(StoreError::StaleRevision {
                current: 2,
                given: 2
            })
        );
        assert_eq!(
            store.save(signed("0xa2", 1)),
            Err(StoreError::StaleRevision {
                current: 2,
                given: 1
            })
        );
        // Reloaded from disk, the latest approval is what remains.
        let reloaded = FileGrantStore::load(&path);
        assert_eq!(reloaded.list_for_agent("0xa2")[0].signature, "sig-2");
    }

    #[test]
    fn one_agents_grants_are_never_listed_for_another() {
        let store = FileGrantStore::load(scratch("isolation"));
        store.save(signed("0xa2", 1)).unwrap();
        assert!(store.list_for_agent("0xa3").is_empty());
        assert!(store.list_for_agent("").is_empty());
    }
}
