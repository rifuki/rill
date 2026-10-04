//! Ordered, separately granted transactions with durable refusal of duplicate workflow runs.

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

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

impl Workflow {
    /// Validate before constructing a receipt path or touching any signing surface.
    pub fn validate(&self) -> Result<(), String> {
        if self.run_id.is_empty()
            || self.run_id.len() > 96
            || !self
                .run_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        {
            return Err(
                "runId must contain 1 to 96 ASCII letters, digits, hyphens or underscores".into(),
            );
        }
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

/// Return a saved receipt on retry, including after a crash, without invoking `execute` again.
/// An incomplete or unreadable receipt fails closed. Partial successes are never rolled back.
pub fn run(
    directory: &Path,
    workflow: &Workflow,
    mut execute: impl FnMut(&Step) -> Value,
) -> Result<Value, String> {
    workflow.validate()?;
    std::fs::create_dir_all(directory).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
    }
    let path = directory.join(format!("{}.json", workflow.run_id));
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
            let bytes = std::fs::read(&path)
                .map_err(|e| format!("receipt unreadable; do not retry: {e}"))?;
            let mut report: Value = serde_json::from_slice(&bytes).map_err(|e| {
                format!("receipt incomplete; inspect chain before another run: {e}")
            })?;
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
        Err(error) => return Err(error.to_string()),
    };
    let mut report = json!({"workflow":workflow, "status":"running", "steps":[], "replayed":false,
        "note":"Separate transactions and budgets. Outputs are not automatically reinvested. Successful steps cannot be undone; a repeated runId returns this receipt."});
    checkpoint(&mut file, &report)?;
    // Persist the directory entry before any transaction. A failed fsync stops before signing.
    File::open(directory)
        .and_then(|directory| directory.sync_all())
        .map_err(|e| e.to_string())?;
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
