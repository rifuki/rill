//! The lowest grant revision this signer will still use, per owner, wallet and action.
//!
//! # What it guards against
//!
//! Grants arrive from the Rill server, which is exactly the party a grant exists to constrain. The
//! owner's signature binds a grant's revision, so a server cannot relabel an old grant, but it can
//! drop the newest one and serve an older approval the owner signed earlier: a looser per-transaction
//! cap, other targets, a later expiry. Once this signer has verified revision N, it refuses every
//! revision below N for the same slot, across restarts, whatever the server serves.
//!
//! # What it does not guard against
//!
//! A fresh machine that has never seen the newer grant has no floor to compare with. Someone with
//! write access to these files can lower a floor, but that person holds the agent's key anyway and
//! is bounded only by the rules the Move contract enforces (the same scope as `grants.rs`).
//!
//! # Layout
//!
//! `<state dir>/grant-floors/<network>/<signer>/<owner>/<wallet>/<actionId>/<revision>`, one empty
//! file per revision verified. Raising adds a file name and reading takes the largest, so two signer
//! processes (Claude Code and Codex can each launch one) raise the same slot without a lock, and no
//! read-modify-write race can lower a floor.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use sui_sdk_types::Address;

/// Where floors live under the signer's state directory.
pub const FLOORS_DIR: &str = "grant-floors";

pub struct GrantFloors {
    root: PathBuf,
}

/// A name used as one path component. The same alphabet as a workflow's runId, so nothing that
/// reaches the filesystem can climb out of the floor directory or name a hidden file.
fn component<'a>(what: &str, value: &'a str) -> Result<&'a str, String> {
    if value.is_empty()
        || value.len() > 96
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err(format!(
            "{what} {value:?} is not 1 to 96 ASCII letters, digits, hyphens or underscores, so no \
             grant revision floor can be kept for it"
        ));
    }
    Ok(value)
}

/// One spelling per address, so `0xa1` and its full form are the same slot.
fn address(what: &str, value: &str) -> Result<String, String> {
    value
        .parse::<Address>()
        .map(|a| a.to_string())
        .map_err(|_| format!("{what} {value:?} is not a Sui address"))
}

fn create_private_dir(path: &Path) -> Result<(), String> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .map_err(|e| format!("creating {}: {e}", path.display()))
}

/// The signer's local state root for a config file: its directory, made absolute once. A relative
/// RILL_CONFIG would otherwise put the floors under whatever directory the MCP client launched the
/// signer from, and each project would start with none.
pub fn state_dir_for(config_path: &Path) -> Option<PathBuf> {
    std::path::absolute(config_path)
        .ok()?
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(Path::to_path_buf)
}

impl GrantFloors {
    /// The floors this signer keeps on `network`, under `state_dir`.
    pub fn open(state_dir: &Path, network: &str, signer: &str) -> Result<Self, String> {
        let base = state_dir.join(FLOORS_DIR);
        let root = base
            .join(component("network", network)?)
            .join(address("signer", signer)?);
        create_private_dir(&root)?;
        // The directory may predate this process with a wider mode; the floor is this user's alone.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| format!("restricting {}: {e}", base.display()))?;
        }
        Ok(Self { root })
    }

    fn slot(&self, owner: &str, wallet: &str, action_id: &str) -> Result<PathBuf, String> {
        Ok(self
            .root
            .join(address("owner", owner)?)
            .join(address("wallet", wallet)?)
            .join(component("actionId", action_id)?))
    }

    /// The highest revision verified for this slot, or 0 when none has been.
    ///
    /// An entry that is not a revision is an error rather than ignored: reading it as "no floor"
    /// would reopen exactly the rollback this exists to close.
    pub fn floor(&self, owner: &str, wallet: &str, action_id: &str) -> Result<u64, String> {
        let slot = self.slot(owner, wallet, action_id)?;
        let entries = match std::fs::read_dir(&slot) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(format!("reading {}: {e}", slot.display())),
        };
        let mut floor = 0;
        for entry in entries {
            let entry = entry.map_err(|e| format!("reading {}: {e}", slot.display()))?;
            let name = entry.file_name();
            let revision = name
                .to_str()
                .filter(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
                .and_then(|n| n.parse::<u64>().ok())
                .ok_or_else(|| {
                    format!(
                        "{} holds {name:?}, which is not a grant revision; refusing to guess this \
                         grant's floor",
                        slot.display()
                    )
                })?;
            floor = floor.max(revision);
        }
        Ok(floor)
    }

    /// Record that `revision` was verified for this slot. Never lowers the floor.
    pub fn raise(
        &self,
        owner: &str,
        wallet: &str,
        action_id: &str,
        revision: u64,
    ) -> Result<(), String> {
        let slot = self.slot(owner, wallet, action_id)?;
        create_private_dir(&slot)?;
        let path = slot.join(revision.to_string());
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            // Another process may have created this entry and not yet made it durable; this one
            // is about to rely on it, so it syncs the directories either way.
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("recording {}: {e}", path.display())),
        }
        // The name is the record, so it is the directory entries that must reach the disk: the
        // slot's and, when they were just created, every parent's up to the signer's root.
        let mut directory = slot.as_path();
        loop {
            File::open(directory)
                .and_then(|d| d.sync_all())
                .map_err(|e| format!("persisting {}: {e}", directory.display()))?;
            if directory == self.root {
                return Ok(());
            }
            directory = match directory.parent() {
                Some(parent) => parent,
                None => return Ok(()),
            };
        }
    }
}
