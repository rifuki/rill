//! Configuration and shared state.
//!
//! # Boot refuses rather than degrades
//!
//! Two settings are mandatory on mainnet and generated on testnet. A missing OAuth secret on
//! mainnet would mean tokens signed with a value that dies with the process — which surfaces as
//! every connected agent getting unexplainable 401s after each deploy, a symptom far harder to
//! diagnose than a refusal at startup. A missing guard package would mean every slippage floor
//! silently unenforced.
//!
//! Testnet generates a per-boot secret so local development needs no setup, and says so loudly.

use std::sync::Arc;

use rill_chain::grpc::GrpcSui;
use rill_store::file::{FileOAuthStore, FileSkillStore};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    Testnet,
    Mainnet,
}

impl Network {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Testnet => "testnet",
            Self::Mainnet => "mainnet",
        }
    }
}

/// An owner secret shorter than this is refused at boot. It is the only thing standing between a
/// stranger and a 90-day build credential, so it is held to the length of what `openssl rand -hex
/// 16` produces rather than to whatever an operator typed.
const MIN_OWNER_SECRET_CHARS: usize = 32;

pub struct Config {
    pub port: u16,
    pub network: Network,
    pub public_base_url: String,
    /// Read once `rill-chain` is wired into the build endpoints. Resolved at boot rather than at
    /// first use so a misconfigured endpoint is visible in `/health` before anyone calls it.
    pub sui_rpc_url: String,
    /// Empty only on a misconfigured mainnet — see [`Config::boot_check`].
    pub oauth_secret: String,
    /// True when the secret came from the environment, and therefore survives a restart.
    pub oauth_secret_from_env: bool,
    pub guard_package_id: Option<String>,
    /// The operator credential that authorizes minting a long-lived agent credential.
    ///
    /// Not a token and not a key: it is the only owner identity this deployment has until
    /// Sign-In With Sui is wired up here, and it is what keeps the agent-token grant from being
    /// reachable by anything that merely holds a token. Unset means this deployment mints none.
    pub owner_secret: Option<String>,
    /// The one Sui address agent credentials are minted for.
    ///
    /// Taken from configuration, never from the request, so a caller who holds the owner secret
    /// still cannot mint a credential that acts for somebody else's address.
    pub owner_address: Option<String>,
    pub skills_store_path: String,
    pub oauth_store_path: String,
}

/// Read an environment variable, treating blank as unset.
///
/// A variable set to the empty string is how a deployment platform expresses "I have no value for
/// this", and treating it as a configured value would mint credentials authenticated by an empty
/// secret.
fn trimmed_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

impl Config {
    pub fn from_env() -> Self {
        let port = std::env::var("PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(3939);
        let network = match std::env::var("SUI_NETWORK").as_deref() {
            Ok("mainnet") => Network::Mainnet,
            // Testnet is the default. An operator opts *into* mainnet explicitly rather than
            // landing on it by omission.
            _ => Network::Testnet,
        };
        let public_base_url =
            std::env::var("PUBLIC_BASE_URL").unwrap_or_else(|_| format!("http://localhost:{port}"));

        let from_env = std::env::var("RILL_OAUTH_SECRET")
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
        let oauth_secret_from_env = from_env.is_some();
        let oauth_secret = from_env.unwrap_or_else(|| match network {
            Network::Mainnet => String::new(),
            Network::Testnet => rill_auth::tokens::random_id(),
        });

        if !oauth_secret_from_env && network == Network::Testnet {
            eprintln!(
                "[oauth] RILL_OAUTH_SECRET is unset, so a random per-boot secret is in use. Every issued \
                 token becomes invalid when this process restarts, and connected agents must \
                 re-authorize. Set it for anything longer-lived than local development."
            );
        }

        Self {
            port,
            network,
            sui_rpc_url: std::env::var("SUI_RPC_URL").unwrap_or_else(|_| match network {
                Network::Mainnet => "https://fullnode.mainnet.sui.io:443".into(),
                Network::Testnet => "https://fullnode.testnet.sui.io:443".into(),
            }),
            public_base_url,
            oauth_secret,
            oauth_secret_from_env,
            guard_package_id: std::env::var("RILL_GUARD_PACKAGE_ID")
                .ok()
                .filter(|s| !s.is_empty()),
            owner_secret: trimmed_env("RILL_OWNER_SECRET"),
            owner_address: trimmed_env("RILL_OWNER_ADDRESS"),
            skills_store_path: std::env::var("SKILLS_STORE_PATH")
                .unwrap_or_else(|_| "./data/skills.json".into()),
            oauth_store_path: std::env::var("OAUTH_STORE_PATH")
                .unwrap_or_else(|_| "./data/oauth.json".into()),
        }
    }

    /// Whether this deployment can mint a long-lived agent credential at all.
    ///
    /// Both halves or neither: without the secret there is nobody to authenticate, and without the
    /// address there is no subject to mint for. [`Config::boot_check`] refuses a half-configured
    /// deployment rather than letting the grant answer 400 forever, because an operator reads that
    /// as a broken server.
    pub fn issues_agent_credentials(&self) -> bool {
        self.owner_secret.is_some() && self.owner_address.is_some()
    }

    /// This deployment's base URL, with no trailing slash, and the only way to read it.
    ///
    /// `PUBLIC_BASE_URL` comes from an operator, and an operator who types a trailing slash is
    /// doing something entirely reasonable. Seven places read this value and four of them trimmed
    /// while three did not, so `https://api.rill.example/` produced `…/mcp` in some answers and
    /// `…//mcp` in others. That is not cosmetic: tokens are audience-bound to the resource string
    /// per RFC 8707 and the audience is compared as a string, so a deployment configured that way
    /// issues tokens bound to one spelling and checks them against another, and every request fails
    /// with an invalid-audience error that names nothing an operator can act on.
    ///
    /// One accessor rather than normalising at load, because tests construct `Config` literally and
    /// a normalisation that only happens in `from_env` is one a test cannot see.
    pub fn base(&self) -> &str {
        self.public_base_url.trim_end_matches('/')
    }

    /// The MCP endpoint tokens are audience-bound to: the one URL a user pastes into an agent.
    pub fn resource(&self) -> String {
        format!("{}/mcp", self.base())
    }

    /// Refuse to start rather than run in a state whose failures are hard to attribute.
    pub fn boot_check(&self) -> Result<(), String> {
        // The agent-credential settings are checked on every network, not only mainnet: a
        // half-configured pair or a typo in the address produces a grant that refuses every
        // request, and the operator has no way to tell that from a bug in the server.
        match (&self.owner_secret, &self.owner_address) {
            (Some(secret), Some(address)) => {
                if secret.chars().count() < MIN_OWNER_SECRET_CHARS {
                    return Err(format!(
                        "Refusing to start: RILL_OWNER_SECRET is shorter than \
                         {MIN_OWNER_SECRET_CHARS} characters. It authenticates the only caller \
                         allowed to mint a 90-day build credential, so a guessable value hands \
                         that out. Generate one with `openssl rand -hex 32`."
                    ));
                }
                if address.parse::<sui_sdk_types::Address>().is_err() {
                    return Err(format!(
                        "Refusing to start: RILL_OWNER_ADDRESS is \"{address}\", which is not a \
                         Sui address. Agent credentials are minted for that address and see only \
                         the actions it published, so a typo produces a credential with an empty \
                         catalogue and no error to explain it."
                    ));
                }
            }
            (Some(_), None) => {
                return Err("Refusing to start: RILL_OWNER_SECRET is set but \
                            RILL_OWNER_ADDRESS is not. Agent credentials need an owner address to \
                            act for, and minting would refuse every request. Set both, or \
                            neither to issue none."
                    .into())
            }
            (None, Some(_)) => {
                return Err("Refusing to start: RILL_OWNER_ADDRESS is set but \
                            RILL_OWNER_SECRET is not. Without the secret there is nobody \
                            authorized to mint an agent credential, so the address would have no \
                            effect. Set both, or neither to issue none."
                    .into())
            }
            (None, None) => {}
        }
        if self.network != Network::Mainnet {
            return Ok(());
        }
        if self.oauth_secret.is_empty() {
            return Err(
                "Refusing to start: SUI_NETWORK=mainnet requires RILL_OAUTH_SECRET, the HMAC \
                 secret every issued token is signed with. On testnet a random per-boot secret is \
                 generated so local development needs no setup, but doing that on mainnet would \
                 sign every connected agent out on each restart and deploy. Generate one with \
                 `openssl rand -hex 32` and put it in your secret manager."
                    .into(),
            );
        }
        if self.guard_package_id.is_none() {
            return Err(
                "Refusing to start: SUI_NETWORK=mainnet requires RILL_GUARD_PACKAGE_ID, the \
                 deployed rill_guard package. Without it every slippage floor would be silently \
                 unenforced, which is worse than refusing to build the transaction at all."
                    .into(),
            );
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub skills: Arc<FileSkillStore>,
    /// Loaded at boot — a corrupt file must surface at startup, not on the first sign-in. Read
    /// once the OAuth endpoints are wired.
    pub oauth: Arc<FileOAuthStore>,
    /// The only thing here that talks to Sui. Reads and simulates; it cannot sign, because nothing
    /// in this process holds a key.
    pub chain: Arc<GrpcSui>,
    /// DeepBook's published package on this network, from the environment. There is no default:
    /// building against the wrong DeepBook would produce a transaction that compiles, simulates
    /// against nothing real, and fails on chain.
    pub deepbook_package_id: Option<String>,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let chain = GrpcSui::new(&config.sui_rpc_url).expect(
            "the Sui endpoint must be a usable URL; it is checked at boot, not per request",
        );
        Self {
            skills: Arc::new(FileSkillStore::load(&config.skills_store_path)),
            oauth: Arc::new(FileOAuthStore::load(&config.oauth_store_path, now_ms)),
            chain: Arc::new(chain),
            deepbook_package_id: std::env::var("DEEPBOOK_PACKAGE_ID")
                .ok()
                .filter(|s| !s.is_empty()),
            config: Arc::new(config),
        }
    }
}

/// The server's own network enum maps onto the envelope's. Two enums rather than one because the
/// envelope's is part of a wire contract and this one is configuration — coupling them would make
/// a config change a protocol change.
impl From<Network> for rill_core::envelope::Network {
    fn from(value: Network) -> Self {
        match value {
            Network::Testnet => Self::Testnet,
            Network::Mainnet => Self::Mainnet,
        }
    }
}
