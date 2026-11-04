//! Ordered, separately granted transactions with durable refusal of duplicate workflow runs, and
//! the same durable claim for one keyed call (`once`).

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Workflow {
    pub run_id: String,
    pub network: String,
    pub signer: String,
    pub owner: String,
    pub steps: Vec<Step>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Step {
    pub action_id: String,
    pub wallet_id: String,
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Map<String, Value>>,
}

/// A caller-chosen name that becomes a receipt's file name. One alphabet for every such name, so
/// nothing a caller sends can climb out of the receipt directory or name a hidden file.
pub fn validate_id(field: &str, id: &str) -> Result<(), String> {
    if id.is_empty()
        || id.len() > 96
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err(format!(
            "{field} must contain 1 to 96 ASCII letters, digits, hyphens or underscores"
        ));
    }
    Ok(())
}

impl Workflow {
    /// Validate before constructing a receipt path or touching any signing surface.
    pub fn validate(&self) -> Result<(), String> {
        validate_id("runId", &self.run_id)?;
        if !matches!(self.network.as_str(), "mainnet" | "testnet") {
            return Err("network must be mainnet or testnet".into());
        }
        for address in [&self.signer, &self.owner] {
            address
                .parse::<sui_sdk_types::Address>()
                .map_err(|_| "invalid workflow owner or signer")?;
        }
        if self.steps.is_empty() || self.steps.len() > 10 {
            return Err("a workflow needs 1 to 10 separately approved steps".into());
        }
        let mut wallets = std::collections::BTreeSet::new();
        for step in &self.steps {
            let wallet = step
                .wallet_id
                .parse::<sui_sdk_types::Address>()
                .map_err(|_| "invalid step walletId")?;
            if !wallets.insert(wallet) {
                return Err(
                    "select each vault once; repeated spending requires a separate workflow".into(),
                );
            }
            if step.action_id.is_empty() || step.revision == 0 {
                return Err("every step needs an actionId and positive grant revision".into());
            }
        }
        Ok(())
    }
}

fn checkpoint(file: &mut File, report: &Value) -> Result<(), String> {
    let bytes = serde_json::to_vec(report).map_err(|e| e.to_string())?;
    file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    file.set_len(0).map_err(|e| e.to_string())?;
    file.write_all(&bytes).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| format!("workflow receipt could not be persisted: {e}; do not create a new run to retry, inspect chain first"))
}

/// Why a keyed call was not answered with an outcome. In every case `execute` did not run.
#[derive(Debug)]
pub enum ReceiptError {
    /// The key is not a safe file name, so nothing was created for it.
    InvalidKey(String),
    /// The key already belongs to a different call; `stored` is that call's request.
    Conflict { stored: Value },
    /// A receipt exists and could not be read, so whether its call ran is unknown.
    Unreadable(String),
    /// No claim could be made durable, so nothing was attempted.
    Storage(String),
}

impl std::fmt::Display for ReceiptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidKey(message) | Self::Unreadable(message) | Self::Storage(message) => {
                f.write_str(message)
            }
            Self::Conflict { .. } => f.write_str(
                "This idempotencyKey already belongs to a different call; no transaction submitted.",
            ),
        }
    }
}

/// What a receipt file held when a claim found it already there.
enum Existing {
    Receipt(Value),
    /// Empty or not valid JSON: the window between `create_new` and the first write, in this or
    /// another signer process, or a write cut off by a crash. Either way a call may be in flight,
    /// so this reads as running, never as free to claim. `modified_ms` is when the claim was made.
    Unwritten {
        modified_ms: Option<u64>,
    },
}

enum Claim {
    New(File),
    Existing(Existing),
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Create `directory` and every missing parent as this user's alone, and return the directories
/// whose entries must reach the disk before a claim inside `directory` is durable: `directory`
/// itself, each one that was missing, and the parent that holds the topmost of those. The first
/// keyed call per network and signer creates the whole chain, and a receipt in a directory whose
/// own entry was lost in a crash is a receipt that was never written.
fn create_private_dirs(directory: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut missing = Vec::new();
    let mut cursor = directory;
    while std::fs::symlink_metadata(cursor).is_err() {
        missing.push(cursor.to_path_buf());
        match cursor.parent() {
            Some(parent) => cursor = parent,
            None => break,
        }
    }
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    for dir in missing.iter().rev() {
        match builder.create(dir) {
            // Another signer process may have just made it and not yet synced it; this one is
            // about to rely on it, so it is synced below either way.
            Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => return Err(error),
            _ => {}
        }
    }
    let holder = missing.last().and_then(|top| top.parent()).map(|parent| {
        if parent.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            parent.to_path_buf()
        }
    });
    if missing.is_empty() {
        missing.push(directory.to_path_buf());
    }
    missing.extend(holder);
    Ok(missing)
}

/// Claim `<directory>/<id>.json` with `first` as its content, durable before this returns: the
/// file and every directory entry it hangs from that this call created are fsynced, so a crash
/// after this cannot forget the claim. `id` must already be validated; it is used as a file name.
fn claim(directory: &Path, id: &str, first: &Value) -> Result<Claim, ReceiptError> {
    let storage = |e: std::io::Error| ReceiptError::Storage(e.to_string());
    let entries = create_private_dirs(directory).map_err(storage)?;
    // The leaf may predate this process with a wider mode; the receipts in it are this user's.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
            .map_err(storage)?;
    }
    let path = directory.join(format!("{id}.json"));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = match options.open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let bytes = std::fs::read(&path).map_err(|e| {
                ReceiptError::Unreadable(format!("receipt unreadable; do not retry: {e}"))
            })?;
            return Ok(Claim::Existing(
                match serde_json::from_slice::<Value>(&bytes) {
                    Ok(receipt) if receipt.is_object() => Existing::Receipt(receipt),
                    _ => Existing::Unwritten {
                        modified_ms: std::fs::metadata(&path)
                            .and_then(|m| m.modified())
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_millis() as u64),
                    },
                },
            ));
        }
        Err(error) => return Err(storage(error)),
    };
    // Nothing has run yet, so a claim that cannot be made durable is withdrawn rather than left
    // behind to read as an interrupted call that never happened.
    let durable = checkpoint(&mut file, first).and_then(|()| {
        // Persist the directory entries before any transaction. A failed fsync stops before signing.
        entries.iter().try_for_each(|entry| {
            File::open(entry)
                .and_then(|entry| entry.sync_all())
                .map_err(|e| format!("persisting {}: {e}", entry.display()))
        })
    });
    if let Err(error) = durable {
        let _ = std::fs::remove_file(&path);
        return Err(ReceiptError::Storage(error));
    }
    Ok(Claim::New(file))
}

/// Return a saved receipt on retry, including after a crash, without invoking `execute` again.
/// An unreadable receipt fails closed, and one not yet written reads as interrupted. Partial
/// successes are never rolled back.
pub fn run(
    directory: &Path,
    workflow: &Workflow,
    mut execute: impl FnMut(&Step) -> Value,
) -> Result<Value, String> {
    workflow.validate()?;
    let mut report = json!({"workflow":workflow, "status":"running", "steps":[], "replayed":false,
        "note":"Separate transactions and budgets. Outputs are not automatically reinvested. Successful steps cannot be undone; a repeated runId returns this receipt."});
    let mut file = match claim(directory, &workflow.run_id, &report).map_err(|e| e.to_string())? {
        Claim::New(file) => file,
        Claim::Existing(Existing::Receipt(mut report)) => {
            if report["workflow"] != json!(workflow) {
                return Err(
                    "runId already belongs to a different workflow; no transaction submitted"
                        .into(),
                );
            }
            report["replayed"] = json!(true);
            if report["status"] == "running" {
                report["status"] = json!("interrupted");
                report["note"] = json!("Execution may still be running or its response was lost. Inspect the recorded step and chain; this run will never be submitted again.");
            }
            return Ok(report);
        }
        // The workflow it was claimed for is not on disk yet, so it cannot be compared; it is
        // never run under this runId either way.
        Claim::Existing(Existing::Unwritten { .. }) => {
            return Ok(
                json!({"workflow":Value::Null, "status":"interrupted", "steps":[], "replayed":true,
                "note":"This runId was claimed and its receipt is not written yet: execution may still be running in another signer process, or was cut off. Inspect the chain; this run will never be submitted again."}),
            );
        }
    };
    for (index, step) in workflow.steps.iter().enumerate() {
        report["activeStep"] = json!(index);
        checkpoint(&mut file, &report)?;
        let result = execute(step);
        let succeeded = result["submitted"] == true
            && result["digest"]
                .as_str()
                .is_some_and(|digest| !digest.is_empty());
        let Some(steps) = report["steps"].as_array_mut() else {
            return Err("invalid local workflow receipt".into());
        };
        steps.push(json!({"index":index,"actionId":step.action_id,"walletId":step.wallet_id,"result":result}));
        if !succeeded {
            report["status"] = json!("stopped");
            checkpoint(&mut file, &report)?;
            return Ok(report);
        }
        checkpoint(&mut file, &report)?;
    }
    report["status"] = json!("completed");
    report
        .as_object_mut()
        .map(|object| object.remove("activeStep"));
    checkpoint(&mut file, &report)?;
    Ok(report)
}

/// Said for a key whose call was claimed and whose outcome was never recorded.
pub const INTERRUPTED_NOTE: &str =
    "This key's call started and its outcome was never recorded: it \
    may still be running in another signer process, or it was cut off. It will never be submitted \
    again under this key. Inspect the chain for a transaction from this signer after claimedAtMs \
    before choosing a new key.";

/// Run `execute` at most once per `key`, ever, and hand back its first outcome on every repeat.
///
/// # Why the claim comes first
///
/// The tools this guards build a fresh transaction on every call, so a retry after a timeout or a
/// lost answer is a second transaction with nothing on chain to stop it. Only the caller knows a
/// retry from a new intent, and it says so with the key. The claim is durable on disk before
/// `execute` runs, so a crash between the two is remembered as interrupted, never forgotten and
/// never run again.
///
/// `request` is the call without its key. A key sent with a different request is a conflict, not
/// a replay: replaying would answer a call nobody made, and running it would spend under a key
/// already spent. `execute` returns an MCP `CallToolResult`; a refusal is stored like a success.
pub fn once(
    directory: &Path,
    key: &str,
    request: &Value,
    execute: impl FnOnce() -> Value,
) -> Result<Value, ReceiptError> {
    validate_id("idempotencyKey", key).map_err(ReceiptError::InvalidKey)?;
    let mut receipt = json!({"version":1, "idempotencyKey":key, "request":request,
        "status":"running", "claimedAtMs":now_ms(), "replayed":false});
    let mut file = match claim(directory, key, &receipt)? {
        Claim::New(file) => file,
        Claim::Existing(Existing::Receipt(mut stored)) => {
            if stored["request"] != *request {
                return Err(ReceiptError::Conflict {
                    stored: stored["request"].clone(),
                });
            }
            stored["replayed"] = json!(true);
            if stored["status"] == "running" {
                stored["status"] = json!("interrupted");
                stored["note"] = json!(INTERRUPTED_NOTE);
            }
            return Ok(stored);
        }
        Claim::Existing(Existing::Unwritten { modified_ms }) => {
            return Ok(
                json!({"version":1, "idempotencyKey":key, "request":Value::Null,
                "status":"interrupted", "claimedAtMs":modified_ms, "replayed":true,
                "note":INTERRUPTED_NOTE}),
            );
        }
    };
    let result = execute();
    let landed = result["structuredContent"]["submitted"] == true
        && result["structuredContent"]["digest"]
            .as_str()
            .is_some_and(|digest| !digest.is_empty());
    receipt["status"] = json!(if landed { "completed" } else { "stopped" });
    receipt["result"] = result;
    // The outcome goes back whatever happens here: an error in its place would hide a digest that
    // is already on chain. The claim on disk still stops a second execution.
    if checkpoint(&mut file, &receipt).is_err() {
        receipt["receiptWarning"] =
            json!("receipt not persisted; a retry with this key will report interrupted");
    }
    Ok(receipt)
}

/// A builder response must not reuse a gas reference consumed by an earlier confirmed step.
pub fn gas_is_stale(
    objects: &[sui_sdk_types::ObjectReference],
    consumed: &std::collections::HashMap<sui_sdk_types::Address, u64>,
) -> bool {
    objects.iter().any(|object| {
        consumed
            .get(object.object_id())
            .is_some_and(|version| object.version() <= *version)
    })
}
