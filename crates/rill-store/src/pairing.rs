//! Single-replica persistent signer pairing. Pairing confers no authority to spend.
use crate::{StoreError, StoreResult};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Mutex,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingRequest {
    pub request_id: String,
    pub owner: String,
    pub agent: String,
    pub network: String,
    pub domain: String,
    pub nonce: String,
    pub expires_at: u64,
    pub proved: bool,
}
impl PairingRequest {
    pub fn message(&self) -> String {
        format!("Rill signer pairing v1\nPurpose: prove agent signer; no spend authorization\nDomain: {}\nRequest: {}\nNetwork: {}\nOwner: {}\nAgent: {}\nNonce: {}\nExpiresAt: {}",self.domain,self.request_id,self.network,self.owner,self.agent,self.nonce,self.expires_at)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairedAgent {
    pub owner: String,
    pub agent: String,
    pub network: String,
    pub paired_at: u64,
}
#[derive(Default, Clone, Serialize, Deserialize)]
struct Records {
    requests: BTreeMap<String, PairingRequest>,
    agents: Vec<PairedAgent>,
}
pub struct PairingStore {
    path: Option<PathBuf>,
    records: Mutex<Records>,
}
fn refused(message: &str) -> StoreError {
    StoreError::Corrupt(message.into())
}
impl PairingStore {
    pub fn memory() -> Self {
        Self {
            path: None,
            records: Mutex::new(Records::default()),
        }
    }
    pub fn load(path: impl AsRef<Path>) -> StoreResult<Self> {
        let path = path.as_ref().to_path_buf();
        let records = match std::fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|e| StoreError::Corrupt(e.to_string()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Records::default(),
            Err(e) => return Err(StoreError::Io(e.to_string())),
        };
        Ok(Self {
            path: Some(path),
            records: Mutex::new(records),
        })
    }
    fn change<T>(&self, update: impl FnOnce(&mut Records) -> StoreResult<T>) -> StoreResult<T> {
        let mut current = self
            .records
            .lock()
            .map_err(|_| refused("pairing lock poisoned"))?;
        let mut next = current.clone();
        let result = update(&mut next)?;
        if let Some(path) = &self.path {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent).map_err(|e| StoreError::Io(e.to_string()))?;
            }
            let tmp = path.with_extension("pairing.tmp");
            let bytes =
                serde_json::to_vec(&next).map_err(|e| StoreError::Corrupt(e.to_string()))?;
            std::fs::write(&tmp, bytes)
                .and_then(|()| std::fs::rename(&tmp, path))
                .map_err(|e| StoreError::Io(e.to_string()))?;
        }
        *current = next;
        Ok(result)
    }
    pub fn prepare(&self, request: PairingRequest, now: u64) -> StoreResult<()> {
        self.change(|records| {
            records.requests.retain(|_, r| r.expires_at > now);
            if records
                .requests
                .values()
                .filter(|r| r.owner == request.owner)
                .count()
                >= 10
            {
                return Err(StoreError::AtCapacity { limit: 10 });
            }
            if records.requests.len() >= 1000 || records.requests.contains_key(&request.request_id)
            {
                return Err(refused("pairing request capacity or duplicate ID"));
            }
            records.requests.insert(request.request_id.clone(), request);
            Ok(())
        })
    }
    pub fn get(&self, id: &str, now: u64) -> StoreResult<Option<PairingRequest>> {
        Ok(self
            .records
            .lock()
            .map_err(|_| refused("pairing lock poisoned"))?
            .requests
            .get(id)
            .filter(|r| r.expires_at > now)
            .cloned())
    }
    pub fn prove(&self, id: &str, agent: &str, now: u64) -> StoreResult<()> {
        self.change(|records| {
            let request = records
                .requests
                .get_mut(id)
                .filter(|r| r.expires_at > now)
                .ok_or_else(|| refused("pairing request missing or expired"))?;
            if request.agent != agent || request.proved {
                return Err(refused("pairing signer mismatch or proof already used"));
            }
            request.proved = true;
            Ok(())
        })
    }
    pub fn confirm(&self, id: &str, owner: &str, now: u64) -> StoreResult<PairedAgent> {
        self.change(|records| {
            let request = records
                .requests
                .get(id)
                .filter(|r| r.expires_at > now)
                .ok_or_else(|| refused("pairing request missing or expired"))?;
            if request.owner != owner || !request.proved {
                return Err(refused(
                    "pairing requires original owner and verified agent proof",
                ));
            }
            let paired = PairedAgent {
                owner: owner.into(),
                agent: request.agent.clone(),
                network: request.network.clone(),
                paired_at: now,
            };
            if records.agents.len() >= 10000 {
                return Err(refused("paired agent capacity reached"));
            }
            records.agents.retain(|a| {
                !(a.owner == paired.owner && a.agent == paired.agent && a.network == paired.network)
            });
            records.agents.push(paired.clone());
            records.requests.remove(id);
            Ok(paired)
        })
    }
    pub fn list(&self, owner: &str) -> StoreResult<Vec<PairedAgent>> {
        Ok(self
            .records
            .lock()
            .map_err(|_| refused("pairing lock poisoned"))?
            .agents
            .iter()
            .filter(|a| a.owner == owner)
            .cloned()
            .collect())
    }
}
