//! File-backed stores, compatible with the reference deployment's data files.
//!
//! Load on boot, hold in memory, rewrite the whole file on change. Write volume is a registration,
//! an authorize, a token exchange — small enough that this is sufficient and simple enough to be
//! obviously correct.
//!
//! Two behaviours are load-bearing:
//!
//! **A corrupt file does not stop the boot.** Starting empty means clients must re-register, which
//! is one click. Refusing to boot is not recoverable by anyone but an operator at a terminal.
//!
//! **Writes are atomic.** Temp file then rename, so a process killed mid-write leaves the previous
//! good file rather than a truncated one. A half-written `oauth.json` would take every connected
//! agent offline.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::{
    AuthorizationCode, AuthorizationRequest, OAuthClient, OAuthStore, PublishedSkill,
    RefreshHandle, SkillStore, StoreError, StoreResult,
};

/// The most published skills one deployment holds. Reaching it is refused rather than evicting.
pub const MAX_STORED_SKILLS: usize = 500;

fn io<E: std::fmt::Display>(e: E) -> StoreError {
    StoreError::Io(e.to_string())
}

/// Write a file so a reader never sees a partial one.
fn write_atomic(path: &Path, contents: &str, mode: Option<u32>) -> StoreResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(io)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, contents).map_err(io)?;
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode)).map_err(io)?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    fs::rename(&tmp, path).map_err(io)
}

// ── skills ────────────────────────────────────────────────────────────────────────────────────

/// On disk this is a top-level JSON **array**, matching the reference exactly.
pub struct FileSkillStore {
    path: PathBuf,
    skills: Mutex<Vec<PublishedSkill>>,
}

impl FileSkillStore {
    /// Load, tolerating a missing or corrupt file.
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let skills = match fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str::<Vec<PublishedSkill>>(&raw).unwrap_or_else(|e| {
                eprintln!(
                    "[store] {} could not be read ({e}); starting empty. Published skills will \
                     need re-publishing.",
                    path.display()
                );
                Vec::new()
            }),
            Err(_) => Vec::new(),
        };
        Self {
            path,
            skills: Mutex::new(skills),
        }
    }

    fn persist(&self, skills: &[PublishedSkill]) -> StoreResult<()> {
        let json = serde_json::to_string_pretty(skills).map_err(io)?;
        write_atomic(&self.path, &json, None)
    }
}

/// Addresses compare after trimming and lowercasing, and nowhere else. Doing it in one function
/// keeps the authorization boundary from depending on every call site remembering to.
fn normalized(address: &str) -> String {
    address.trim().to_lowercase()
}

impl SkillStore for FileSkillStore {
    fn get(&self, id: &str) -> Option<PublishedSkill> {
        self.skills
            .lock()
            .ok()?
            .iter()
            .find(|s| s.id == id)
            .cloned()
    }

    fn list_by_owner(&self, address: &str) -> Vec<PublishedSkill> {
        let wanted = normalized(address);
        if wanted.is_empty() {
            // An empty address must match nothing. Without this it would match every unowned
            // skill, which is the opposite of the intent.
            return Vec::new();
        }
        let Ok(skills) = self.skills.lock() else {
            return Vec::new();
        };
        let mut found: Vec<PublishedSkill> = skills
            .iter()
            .filter(|s| s.owner.as_deref().map(normalized) == Some(wanted.clone()))
            .cloned()
            .collect();
        found.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        found
    }

    fn list_unowned(&self) -> Vec<PublishedSkill> {
        self.skills
            .lock()
            .map(|s| s.iter().filter(|s| s.owner.is_none()).cloned().collect())
            .unwrap_or_default()
    }

    fn save(&self, skill: PublishedSkill) -> StoreResult<()> {
        let mut skills = self
            .skills
            .lock()
            .map_err(|_| StoreError::Io("store lock poisoned".into()))?;
        if let Some(existing) = skills.iter_mut().find(|s| s.id == skill.id) {
            *existing = skill;
        } else {
            if skills.len() >= MAX_STORED_SKILLS {
                return Err(StoreError::AtCapacity {
                    limit: MAX_STORED_SKILLS,
                });
            }
            skills.push(skill);
        }
        self.persist(&skills)
    }

    fn count(&self) -> usize {
        self.skills.lock().map(|s| s.len()).unwrap_or(0)
    }
}

// ── oauth ─────────────────────────────────────────────────────────────────────────────────────

/// On disk: one object with exactly these four maps, matching the reference.
#[derive(Debug, Default, Serialize, Deserialize)]
struct OAuthState {
    #[serde(default)]
    clients: HashMap<String, OAuthClient>,
    #[serde(default)]
    requests: HashMap<String, AuthorizationRequest>,
    #[serde(default)]
    codes: HashMap<String, AuthorizationCode>,
    #[serde(default)]
    refresh: HashMap<String, RefreshHandle>,
}

impl OAuthState {
    /// Drop everything past its expiry. Run on load and before every write, so an abandoned
    /// authorize flow cannot accumulate into an unbounded file.
    fn prune(&mut self, now_ms: u64) {
        self.requests.retain(|_, r| r.expires_at > now_ms);
        self.codes.retain(|_, c| c.expires_at > now_ms);
        self.refresh.retain(|_, h| h.expires_at > now_ms);
    }
}

pub struct FileOAuthStore {
    path: PathBuf,
    state: Mutex<OAuthState>,
}

impl FileOAuthStore {
    pub fn load(path: impl Into<PathBuf>, now_ms: u64) -> Self {
        let path = path.into();
        let mut state = match fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str::<OAuthState>(&raw).unwrap_or_else(|e| {
                eprintln!(
                    "[store] {} could not be read ({e}); starting empty. Connected agents will \
                     need to re-authorize.",
                    path.display()
                );
                OAuthState::default()
            }),
            Err(_) => OAuthState::default(),
        };
        state.prune(now_ms);
        Self {
            path,
            state: Mutex::new(state),
        }
    }

    /// Written 0600: this file holds live refresh handles.
    fn persist(&self, state: &OAuthState) -> StoreResult<()> {
        let json = serde_json::to_string_pretty(state).map_err(io)?;
        write_atomic(&self.path, &json, Some(0o600))
    }

    fn with_state<T>(&self, f: impl FnOnce(&mut OAuthState) -> T) -> StoreResult<T> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| StoreError::Io("oauth store lock poisoned".into()))?;
        let out = f(&mut state);
        self.persist(&state)?;
        Ok(out)
    }
}

impl OAuthStore for FileOAuthStore {
    fn save_client(&self, client: OAuthClient) -> StoreResult<()> {
        self.with_state(|s| {
            s.clients.insert(client.client_id.clone(), client);
        })
    }

    fn get_client(&self, client_id: &str) -> Option<OAuthClient> {
        self.state.lock().ok()?.clients.get(client_id).cloned()
    }

    fn save_request(&self, request: AuthorizationRequest) -> StoreResult<()> {
        self.with_state(|s| {
            s.requests.insert(request.request_id.clone(), request);
        })
    }

    fn get_request(&self, request_id: &str, now_ms: u64) -> Option<AuthorizationRequest> {
        let state = self.state.lock().ok()?;
        state
            .requests
            .get(request_id)
            .filter(|r| r.expires_at > now_ms)
            .cloned()
    }

    fn take_request(&self, request_id: &str, now_ms: u64) -> Option<AuthorizationRequest> {
        self.with_state(|s| {
            s.requests
                .remove(request_id)
                .filter(|r| r.expires_at > now_ms)
        })
        .ok()
        .flatten()
    }

    fn save_code(&self, code: AuthorizationCode) -> StoreResult<()> {
        self.with_state(|s| {
            s.codes.insert(code.code.clone(), code);
        })
    }

    fn take_code(&self, code: &str, now_ms: u64) -> Option<AuthorizationCode> {
        // Removed whether or not it had expired. An expired code is spent by this attempt either
        // way, so a caller cannot probe it repeatedly.
        self.with_state(|s| s.codes.remove(code).filter(|c| c.expires_at > now_ms))
            .ok()
            .flatten()
    }

    fn save_refresh(&self, handle: RefreshHandle) -> StoreResult<()> {
        self.with_state(|s| {
            s.refresh.insert(handle.jti.clone(), handle);
        })
    }

    fn take_refresh(&self, jti: &str, now_ms: u64) -> Option<RefreshHandle> {
        self.with_state(|s| s.refresh.remove(jti).filter(|h| h.expires_at > now_ms))
            .ok()
            .flatten()
    }

    fn revoke_subject(&self, sub: &str) -> StoreResult<usize> {
        self.with_state(|s| {
            let before = s.refresh.len();
            s.refresh.retain(|_, h| h.sub != sub);
            before - s.refresh.len()
        })
    }
}
