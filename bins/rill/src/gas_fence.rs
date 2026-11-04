//! The gas references this signer has already spent, kept on disk so a restart remembers them.
//!
//! # What it guards against
//!
//! The Rill builder picks the agent's gas coin from an index that lags the chain. Right after a
//! submission it can hand back a transaction paying with the version that submission consumed;
//! signed, that transaction fails on chain as a stale object. `rill_run_action` waits that out by
//! refusing any build whose gas reference is at or below one it has consumed. That fence used to
//! live only in memory, so a signer restarted right after its own submission (a client reload, a
//! crash, a second client starting its own signer) built the very next action with no fence at
//! all.
//!
//! # Layout
//!
//! `<state dir>/consumed-gas/<network>/<signer>/<gas object>/<version>`, one empty file per
//! consumed reference. Recording adds a name and reading takes the largest per object, the same
//! lock-free shape as `grant_floor`, so two signer processes never lower each other's fence.
//!
//! A lost write only loses the fence for that coin, which is the behaviour before this existed: the
//! chain still refuses a stale reference, so nothing is ever double spent because of it.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use sui_sdk_types::Address;

/// Where fences live under the signer's state directory.
pub const FENCE_DIR: &str = "consumed-gas";

/// One spelling per network name, from the same alphabet as every other path component here.
fn network_component(network: &str) -> Option<&str> {
    (!network.is_empty()
        && network.len() <= 32
        && network
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'))
    .then_some(network)
}

fn root(state_dir: &Path, network: &str, signer: &Address) -> Option<PathBuf> {
    Some(
        state_dir
            .join(FENCE_DIR)
            .join(network_component(network)?)
            .join(signer.to_string()),
    )
}

fn create_private_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(path)
    }
}

/// Every consumed reference recorded for this signer on this network, highest version per coin.
/// Unreadable entries are skipped: a missing fence is the old behaviour, never a wrong one.
pub fn load(state_dir: &Path, network: &str, signer: &Address) -> HashMap<Address, u64> {
    let mut fence = HashMap::new();
    let Some(root) = root(state_dir, network, signer) else {
        return fence;
    };
    let Ok(objects) = std::fs::read_dir(&root) else {
        return fence;
    };
    for object in objects.flatten() {
        let Some(id) = object
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<Address>().ok())
        else {
            continue;
        };
        let Ok(versions) = std::fs::read_dir(object.path()) else {
            continue;
        };
        let highest = versions
            .flatten()
            .filter_map(|v| v.file_name().to_str()?.parse::<u64>().ok())
            .max();
        if let Some(version) = highest {
            fence.insert(id, version);
        }
    }
    fence
}

/// Record that this signer consumed `object` at `version`. Called after a submission landed, so
/// a failure here is reported to the caller as a warning and never undoes the submission.
pub fn record(
    state_dir: &Path,
    network: &str,
    signer: &Address,
    object: &Address,
    version: u64,
) -> Result<(), String> {
    let root = root(state_dir, network, signer)
        .ok_or_else(|| format!("network {network:?} cannot name a gas fence directory"))?;
    let dir = root.join(object.to_string());
    create_private_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(version.to_string());
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    file.sync_all()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    File::open(&dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| format!("{}: {e}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rill-gas-fence-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn addr(byte: u8) -> Address {
        let mut bytes = [0u8; 32];
        bytes[31] = byte;
        Address::new(bytes)
    }

    #[test]
    fn a_recorded_reference_is_read_back_by_a_fresh_load_at_its_highest_version() {
        let dir = scratch("roundtrip");
        let (signer, coin) = (addr(1), addr(2));
        record(&dir, "mainnet", &signer, &coin, 7).unwrap();
        record(&dir, "mainnet", &signer, &coin, 9).unwrap();
        record(&dir, "mainnet", &signer, &coin, 8).unwrap();
        let fence = load(&dir, "mainnet", &signer);
        assert_eq!(fence.get(&coin), Some(&9));
        assert!(
            load(&dir, "testnet", &signer).is_empty(),
            "networks are separate"
        );
        assert!(
            load(&dir, "mainnet", &addr(3)).is_empty(),
            "signers are separate"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_network_name_that_could_climb_out_is_refused() {
        let dir = scratch("traversal");
        assert!(record(&dir, "../x", &addr(1), &addr(2), 1).is_err());
        assert!(load(&dir, "../x", &addr(1)).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn fence_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("modes");
        let (signer, coin) = (addr(1), addr(2));
        record(&dir, "mainnet", &signer, &coin, 3).unwrap();
        let coin_dir = root(&dir, "mainnet", &signer)
            .unwrap()
            .join(coin.to_string());
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&coin_dir), 0o700);
        assert_eq!(mode(&coin_dir.join("3")), 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
