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
    /// The address to listen on. `0.0.0.0` by default, because a container that bound loopback
    /// would be unreachable from outside itself and the failure would look like a crash.
    pub bind_address: String,
    /// The operator's acknowledgement that this deployment issues authorization codes to anyone who
    /// can reach it. See [`Config::boot_check`].
    pub open_authorization_acknowledged: bool,
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

        // Unset on testnet, the secret is generated once and kept beside the store rather than
        // regenerated per boot.
        //
        // A per-boot secret invalidates every token the moment the process restarts, so a local
        // agent has to complete the OAuth flow again after every rebuild. The old code warned about
        // that, which is not the same as not doing it: the warning is printed at boot and read, if
        // ever, long before the agent fails. Keeping it turns "connected" into something that
        // survives a restart.
        //
        // On disk beside `oauth.json`, which already holds authorization codes and agent credential
        // records, so this is not a new class of secret in that directory. Mainnet still refuses to
        // start without an explicit one: a key an operator cannot rotate from their own secret
        // manager is not one they control.
        let oauth_store_path =
            std::env::var("OAUTH_STORE_PATH").unwrap_or_else(|_| "./data/oauth.json".into());
        let generated_path = std::path::Path::new(&oauth_store_path).with_file_name("oauth-secret");
        let oauth_secret = match from_env {
            Some(secret) => secret,
            None => match network {
                Network::Mainnet => String::new(),
                Network::Testnet => read_or_create_secret(&generated_path),
            },
        };

        if !oauth_secret_from_env && network == Network::Testnet {
            eprintln!(
                "[oauth] RILL_OAUTH_SECRET is unset, so a generated one is in use, kept at {}. \
                 Tokens survive a restart because of that file: delete it and every connected agent \
                 must re-authorize. Set RILL_OAUTH_SECRET for anything you want to rotate from a \
                 secret manager rather than a filesystem.",
                generated_path.display()
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
            bind_address: std::env::var("BIND_ADDRESS").unwrap_or_else(|_| "0.0.0.0".into()),
            open_authorization_acknowledged: std::env::var("RILL_ALLOW_OPEN_AUTHORIZATION")
                .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true")),
            guard_package_id: std::env::var("RILL_GUARD_PACKAGE_ID")
                .ok()
                .filter(|s| !s.is_empty()),
            owner_secret: trimmed_env("RILL_OWNER_SECRET"),
            owner_address: trimmed_env("RILL_OWNER_ADDRESS"),
            skills_store_path: std::env::var("SKILLS_STORE_PATH")
                .unwrap_or_else(|_| "./data/skills.json".into()),
            oauth_store_path,
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
        // `/oauth/authorize` has no consent step: it returns an authorization code to whoever asks,
        // registration is open, and a public client presents no credential. On loopback that is
        // defensible, because reaching the port already means being on the machine. Bound wider it
        // means anyone who can route to this port can mint a token for the build surface and read
        // the owner's catalogue. What they cannot do is sign: the key is in a separate process and
        // this one has none, which is what bounds the exposure rather than removing it.
        //
        // So the combination is a decision rather than a default. A container legitimately needs a
        // wide bind, and setting the flag in a compose file is one line; discovering this from the
        // outside is not.
        if !is_loopback(&self.bind_address) && !self.open_authorization_acknowledged {
            return Err(format!(
                "Refusing to start: BIND_ADDRESS is {} and this deployment has no consent step, so \
                 anyone who can reach port {} could register a client and mint an access token for \
                 the build surface. The signing key is not here, so they could not sign anything, \
                 but they could read this owner's published actions and have envelopes built. Set \
                 BIND_ADDRESS=127.0.0.1 to keep it on this machine, or \
                 RILL_ALLOW_OPEN_AUTHORIZATION=1 to say that a wide bind is intended.",
                self.bind_address, self.port
            ));
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

/// The generated signing secret, created on first boot and reused after that.
///
/// A read that fails for any reason falls back to a fresh secret rather than refusing to start: a
/// local server that will not run because of a permissions problem on a convenience file is worse
/// than one whose tokens do not survive this particular restart. The warning above names the path so
/// the cause is visible either way.
pub(crate) fn read_or_create_secret(path: &std::path::Path) -> String {
    if let Ok(existing) = std::fs::read_to_string(path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }
    let fresh = rill_auth::tokens::random_id();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::write(path, &fresh).is_ok() {
        // Readable by this user only. A secret at 644 in a shared directory is one anybody on the
        // machine can sign tokens with.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
    }
    fresh
}

/// Whether an address keeps traffic on this machine.
///
/// Both families, and the unspecified forms are deliberately **not** loopback: `0.0.0.0` and `::`
/// mean every interface, which is the case this check exists for.
fn is_loopback(address: &str) -> bool {
    let trimmed = address.trim().trim_start_matches('[').trim_end_matches(']');
    match trimmed.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        // A hostname rather than an address. `localhost` is the one that resolves to loopback on
        // every machine this runs on; anything else is treated as wide, because guessing that a
        // name is local is how this check would be bypassed by accident.
        Err(_) => trimmed.eq_ignore_ascii_case("localhost"),
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

#[cfg(test)]
mod secret_tests {
    //! The generated signing secret, tested without touching the process environment.
    //!
    //! An earlier version of these drove `Config::from_env` with `set_var`, which passed serially and
    //! raced every other test in the same binary. The behaviour worth pinning is this function's, and
    //! it needs a path and nothing else.

    use super::read_or_create_secret;

    fn dir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "rill-secret-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A second call returns the first secret, which is what makes a token survive a restart.
    ///
    /// The old behaviour minted one per boot and printed a warning. A warning at boot is read, if
    /// ever, long before the agent fails, and what an operator actually meets is their agent asking
    /// to re-authorize after a rebuild. This is the assertion that separates "warned about" from
    /// "fixed".
    #[test]
    fn the_same_path_yields_the_same_secret() {
        let path = dir().join("oauth-secret");
        let first = read_or_create_secret(&path);
        assert!(!first.is_empty());
        assert_eq!(read_or_create_secret(&path), first);
    }

    /// Deleting the file is the documented way to invalidate every token, so it must do that.
    #[test]
    fn removing_the_file_produces_a_different_secret() {
        let path = dir().join("oauth-secret");
        let first = read_or_create_secret(&path);
        std::fs::remove_file(&path).unwrap();
        assert_ne!(read_or_create_secret(&path), first);
    }

    /// Owner-only on disk. A secret at 644 in a shared directory is one anybody on the machine can
    /// sign tokens with.
    #[cfg(unix)]
    #[test]
    fn the_file_is_not_readable_by_others() {
        use std::os::unix::fs::PermissionsExt;
        let path = dir().join("oauth-secret");
        read_or_create_secret(&path);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "mode was {mode:o}");
    }

    /// A blank or whitespace-only file is treated as absent rather than used as a secret.
    ///
    /// An empty HMAC key signs tokens anyone can forge, and a file truncated by a disk problem is
    /// exactly how one would arrive.
    #[test]
    fn a_blank_file_is_replaced_rather_than_used() {
        let path = dir().join("oauth-secret");
        std::fs::write(&path, "   \n").unwrap();
        let secret = read_or_create_secret(&path);
        assert!(!secret.trim().is_empty());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().trim(),
            secret,
            "and the replacement is written back, so the next boot agrees with this one"
        );
    }

    /// A path whose parent does not exist yet still works: the directory is created.
    #[test]
    fn a_missing_directory_is_created() {
        let path = dir().join("nested").join("deeper").join("oauth-secret");
        let secret = read_or_create_secret(&path);
        assert!(
            path.exists(),
            "the file was not written: {}",
            path.display()
        );
        assert_eq!(read_or_create_secret(&path), secret);
    }
}
