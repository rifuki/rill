//! Persistence behind a trait, with a file-backed implementation.
//!
//! The file implementation reads the reference deployment's existing `skills.json` and
//! `oauth.json` unchanged, so going live needs no migration step.
//!
//! # Single-use is a store concern, not a caller's
//!
//! Authorization codes and refresh handles are read **and removed** in one operation. Making that
//! the store's job rather than the caller's is deliberate: a caller that reads, validates, then
//! deletes has a window where two concurrent redemptions both see a valid code, and every call
//! site would have to close it identically. Here there is one place to get it right.
//!
//! # One replica
//!
//! The file implementation holds state in memory and rewrites the whole file on change. Two
//! replicas would each hold half the authorization codes and reject the other's. The traits exist
//! so a Postgres implementation can drop in without touching a caller — see the note on
//! [`OAuthStore`].

pub mod file;

use serde::{Deserialize, Serialize};

/// A published action. `owner` is optional and permanently so: everything published before the
/// authorization server existed has none, and those links must keep working.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishedSkill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub flow: serde_json::Value,
    /// Present on disk but **not authoritative** — it is recomputed from `flow` on load, so a
    /// stale or hand-edited value cannot change what a tool advertises.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_defs: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_id: Option<String>,
    /// The Sui address that published this. An unowned skill matches no address, so it can never
    /// appear in someone else's catalogue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    pub created_at: String,
}

/// A dynamically registered OAuth client. No secret is ever issued — a secret shipped to a desktop
/// agent is not a secret, and OAuth 2.1 public clients authenticate with PKCE instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthClient {
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    pub redirect_uris: Vec<String>,
    pub scope: String,
    pub created_at: String,
}

/// Which flow parked an authorization request.
///
/// Two different things end in a wallet signature — an agent connecting over OAuth, and the studio
/// signing in to publish as itself. They must not be interchangeable: a signature collected for a
/// studio login must never be redeemable for an agent's authorization code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RequestKind {
    Agent,
    Studio,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorizationRequest {
    pub request_id: String,
    pub kind: RequestKind,
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    pub redirect_uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    pub scope: String,
    pub code_challenge: String,
    pub resource: String,
    /// The exact bytes the wallet must sign. Generated server-side and handed to the browser
    /// verbatim, so the two sides can never derive different bytes from one intent.
    pub message: String,
    /// Epoch milliseconds.
    pub expires_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorizationCode {
    pub code: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    /// The authenticated Sui address.
    pub sub: String,
    pub scope: String,
    pub resource: String,
    pub expires_at: u64,
}

/// A live refresh handle. The token itself is signed and stateless; this record is what makes it
/// revocable, because rotation deletes the handle and a replayed token then finds nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshHandle {
    pub jti: String,
    pub sub: String,
    pub client_id: String,
    pub scope: String,
    pub resource: String,
    pub expires_at: u64,
}

/// A live long-lived agent credential.
///
/// The token is signed and stateless like every other one here, so this record is the only thing
/// that can stop it: the protected resource looks the `jti` up on **every** request, and a revoke
/// deletes the record. Without it a leaked 90-day bearer would keep working for 90 days while
/// `/oauth/revoke` answered `{"revoked": true}`, which RFC 7009 requires it to answer whether or
/// not anything was revoked.
///
/// Read, never taken. A refresh handle is consumed on use because rotation is the point; consuming
/// this one would revoke the credential on its first call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentHandle {
    pub jti: String,
    /// The owner address this credential acts for. It decides which catalogue the agent sees.
    pub sub: String,
    pub client_id: String,
    /// Narrowed at issuance to the build surface. Kept here so an operator reading the file can see
    /// what a credential can reach without decoding a token.
    pub scope: String,
    pub resource: String,
    /// Epoch milliseconds, so a file an operator opens says when each credential was minted.
    pub issued_at: u64,
    /// Epoch milliseconds.
    pub expires_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    Io(String),
    Corrupt(String),
    /// The store is full. Refusing is the honest answer — evicting someone else's published action
    /// to make room for a new one is not.
    AtCapacity {
        limit: usize,
    },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(m) => write!(f, "store I/O failed: {m}"),
            Self::Corrupt(m) => write!(f, "store contents could not be read: {m}"),
            Self::AtCapacity { limit } => write!(
                f,
                "the store holds its maximum of {limit} skills; refusing rather than evicting \
                 somebody else's"
            ),
        }
    }
}

impl std::error::Error for StoreError {}

pub type StoreResult<T> = Result<T, StoreError>;

/// Published actions.
pub trait SkillStore {
    fn get(&self, id: &str) -> Option<PublishedSkill>;
    /// Skills owned by one address, newest first.
    ///
    /// Exact match on a normalized address — never a prefix, never case-insensitive contains.
    /// This is an authorization boundary, and it is the only thing between one user's catalogue
    /// and another's.
    fn list_by_owner(&self, address: &str) -> Vec<PublishedSkill>;
    /// Skills with no owner — everything published before ownership existed.
    fn list_unowned(&self) -> Vec<PublishedSkill>;
    fn save(&self, skill: PublishedSkill) -> StoreResult<()>;
    fn count(&self) -> usize;
}

/// OAuth state.
///
/// Every `take_*` is read-and-remove. See the module note on why that lives here.
pub trait OAuthStore {
    fn save_client(&self, client: OAuthClient) -> StoreResult<()>;
    fn get_client(&self, client_id: &str) -> Option<OAuthClient>;

    fn save_request(&self, request: AuthorizationRequest) -> StoreResult<()>;
    /// Read without consuming, so a failed signature leaves the request usable and the user can
    /// retry in the same tab rather than restarting from their agent.
    fn get_request(&self, request_id: &str, now_ms: u64) -> Option<AuthorizationRequest>;
    fn take_request(&self, request_id: &str, now_ms: u64) -> Option<AuthorizationRequest>;

    fn save_code(&self, code: AuthorizationCode) -> StoreResult<()>;
    fn take_code(&self, code: &str, now_ms: u64) -> Option<AuthorizationCode>;

    fn save_refresh(&self, handle: RefreshHandle) -> StoreResult<()>;
    fn take_refresh(&self, jti: &str, now_ms: u64) -> Option<RefreshHandle>;

    fn save_agent(&self, handle: AgentHandle) -> StoreResult<()>;
    /// Read **without** consuming, and the one exception to this trait's take-everything rule.
    ///
    /// This runs on every request an agent credential makes. Taking the handle here would revoke
    /// the credential on its first call, which is the same bug as not having a handle at all, only
    /// harder to see.
    fn get_agent(&self, jti: &str, now_ms: u64) -> Option<AgentHandle>;
    /// Kill one agent credential. Returns whether a live handle actually died, so a caller can tell
    /// a real revocation from a no-op even though RFC 7009 makes it answer 200 either way.
    fn revoke_agent(&self, jti: &str) -> StoreResult<bool>;

    /// Revoke every live handle for one address, refresh and agent alike: "sign me out
    /// everywhere", and the thing to reach for when an address reports a compromised agent.
    /// Returns how many died.
    ///
    /// Agent credentials are included deliberately. Leaving them would make this the one call an
    /// operator reaches for in an incident and the one that leaves the longest-lived credential
    /// alive.
    fn revoke_subject(&self, sub: &str) -> StoreResult<usize>;
}
