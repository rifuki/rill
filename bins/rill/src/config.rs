//! `~/.rill/config.json`: the settings an MCP launch would otherwise carry in its environment.
//!
//! An agent client starts the signer from a config it owns (`.mcp.json`, a plugin, `config.toml`),
//! and every variable that config had to name was a step a person could get wrong: six of them for
//! a mainnet signer, one of which (`RILL_ALLOW_MAINNET`) is the deliberate opt-in. A plugin cannot
//! know any of them for its user. So they live in one file this machine owns, written once by
//! `rill setup`, and the plugin launches the bare command.
//!
//! The environment still wins. A variable set for a run is the more specific statement, and every
//! existing launch that sets them keeps behaving exactly as it did.
//!
//! The file names an address, never a key. The key stays in the Sui keystore; this only says which
//! of its keys signs, the same thing `--as` says.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Points at a config file other than `~/.rill/config.json`.
pub const CONFIG_VAR: &str = "RILL_CONFIG";

/// Where the file lives under `$HOME`.
pub const DEFAULT_PATH: &str = ".rill/config.json";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    /// `testnet` or `mainnet`. Becomes `SUI_NETWORK`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
    /// The address whose key signs. Becomes `RILL_SIGN_AS`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sign_as: Option<String>,
    /// The mainnet opt-in, recorded by a person running `rill setup --allow-mainnet`. Becomes
    /// `RILL_ALLOW_MAINNET=true`; absent or false leaves mainnet signing off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_mainnet: Option<bool>,
    /// A pinned run-set. Becomes `RILL_RUN_SET_PATH`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_set: Option<String>,
    /// A fullnode other than the public one. Becomes `SUI_RPC_URL`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpc_url: Option<String>,
}

impl Config {
    /// The environment this file stands for, in the variables the rest of the binary reads.
    pub fn variables(&self) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        if let Some(network) = &self.network {
            out.push(("SUI_NETWORK", network.clone()));
        }
        if let Some(address) = &self.sign_as {
            out.push((crate::keystore::SIGN_AS_VAR, address.clone()));
        }
        if self.allow_mainnet == Some(true) {
            out.push(("RILL_ALLOW_MAINNET", "true".into()));
        }
        if let Some(path) = &self.run_set {
            out.push((crate::runset::RUN_SET_VAR, path.clone()));
        }
        if let Some(url) = &self.rpc_url {
            out.push(("SUI_RPC_URL", url.clone()));
        }
        out
    }
}

/// The config file this process reads: `$RILL_CONFIG`, else `~/.rill/config.json`.
pub fn path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var(CONFIG_VAR) {
        return Some(PathBuf::from(path));
    }
    std::env::var("HOME")
        .ok()
        .map(|home| Path::new(&home).join(DEFAULT_PATH))
}

/// Read a config file. A missing file is no config; an unreadable or malformed one is an error,
/// because a signer that silently ignored its config would sign as whichever key it found.
pub fn read(path: &Path) -> Result<Option<Config>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| format!("{} is not a valid Rill config: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("reading {}: {e}", path.display())),
    }
}

/// Set each variable the config names and the environment does not, and say which were set.
///
/// Called once at the top of `main`, before any thread exists, which is what makes setting the
/// process environment sound.
pub fn apply(config: &Config) -> Vec<&'static str> {
    let mut applied = Vec::new();
    for (name, value) in config.variables() {
        if std::env::var_os(name).is_none() {
            std::env::set_var(name, value);
            applied.push(name);
        }
    }
    applied
}

/// Write the config, readable by this user only.
pub fn write(path: &Path, config: &Config) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let text = serde_json::to_string_pretty(config).map_err(|e| e.to_string())? + "\n";
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("restricting {}: {e}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| format!("replacing {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rill-config-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("config.json")
    }

    #[test]
    fn a_written_config_reads_back_and_names_the_variables_it_stands_for() {
        let path = scratch("roundtrip");
        let config = Config {
            network: Some("mainnet".into()),
            sign_as: Some("0x3e".into()),
            allow_mainnet: Some(true),
            run_set: None,
            rpc_url: None,
        };
        write(&path, &config).unwrap();
        assert_eq!(read(&path).unwrap(), Some(config.clone()));
        assert_eq!(
            config.variables(),
            vec![
                ("SUI_NETWORK", "mainnet".to_owned()),
                ("RILL_SIGN_AS", "0x3e".to_owned()),
                ("RILL_ALLOW_MAINNET", "true".to_owned()),
            ]
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn mainnet_signing_stays_off_unless_the_file_opts_in() {
        let config = Config {
            network: Some("mainnet".into()),
            allow_mainnet: Some(false),
            ..Config::default()
        };
        assert!(!config
            .variables()
            .iter()
            .any(|(name, _)| *name == "RILL_ALLOW_MAINNET"));
    }

    #[test]
    fn a_missing_file_is_no_config_and_a_malformed_one_is_refused() {
        let path = scratch("missing");
        assert_eq!(read(&path).unwrap(), None);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"network":"mainnet","signAs":"0x1","extra":1}"#).unwrap();
        assert!(
            read(&path).is_err(),
            "an unknown field is refused, not ignored"
        );
    }
}
