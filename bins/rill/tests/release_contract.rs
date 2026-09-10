//! The release contract: one origin, one set of asset names, one name the binary answers to.
//!
//! The filename a stranger downloads is decided by three literals that nothing ties together: the
//! workflow's matrix `asset:` values, its publish `files:` list, and the README's curl lines. A
//! fourth, the smoke step's grep, has to match what the binary prints. Each can drift alone and
//! still leave the build green, which is how the workflow came to build a bin that did not exist
//! and grep for a name the binary did not print. So the files are read here as text and checked
//! against each other and against the code (plan KTD-5, U5).
//!
//! The origin is `github.com/rifuki/rill`: this repository's only remote, and so the only place a
//! tag pushed here can put an asset. The README may send nobody anywhere else.
//!
//! The smoke step is also run, not only read: extracted from the workflow and handed binaries that
//! start, that cannot start, and that answer to the wrong name. A step that is only grepped for
//! its wording passes a crashing binary as easily as the old one did.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use rill_cli::keystore::PRIVATE_KEY_VAR;
use rill_cli::runset::RUN_SET_VAR;
use rill_cli::BINARY_NAME;

const RELEASE_WORKFLOW: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../.github/workflows/release.yaml"
));
const README: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../README.md"));
const MANIFEST: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"));

/// The one origin. A download line that names any other repository is a broken instruction.
const ORIGIN: &str = "https://github.com/rifuki/rill/releases";

/// The asset names, pinned. These are what the README hands out, so a rename here renames every
/// install instruction already in the wild and must be deliberate.
const ASSETS: [&str; 3] = [
    "rill-wallet-darwin-arm64",
    "rill-wallet-darwin-x64",
    "rill-wallet-linux-x64",
];

/// Runner labels GitHub has retired. A matrix entry naming one never schedules, and the release
/// quietly comes out one asset short.
const RETIRED_RUNNERS: [&str; 4] = ["macos-11", "macos-12", "macos-13", "ubuntu-20.04"];

const SMOKE_STEP: &str = "The binary must start and report itself";

fn leading_spaces(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Every `asset:` value in the workflow's matrix, in order.
fn matrix_assets() -> Vec<&'static str> {
    RELEASE_WORKFLOW
        .lines()
        .filter_map(|line| line.trim().strip_prefix("asset:"))
        .map(str::trim)
        .collect()
}

/// Every `runner:` value in the workflow's matrix.
fn matrix_runners() -> Vec<&'static str> {
    RELEASE_WORKFLOW
        .lines()
        .filter_map(|line| line.trim().strip_prefix("- runner:"))
        .map(str::trim)
        .collect()
}

/// The lines of the block under `key: |`, trimmed, which is how the publish step lists files.
fn block_after(key: &str) -> Vec<&'static str> {
    let mut lines = RELEASE_WORKFLOW.lines();
    let header = lines
        .find(|line| line.trim() == key)
        .unwrap_or_else(|| panic!("release.yaml has no `{key}` block"));
    let indent = leading_spaces(header);
    lines
        .take_while(|line| line.trim().is_empty() || leading_spaces(line) > indent)
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect()
}

/// The shell of the step called `name`, dedented, as the runner would hand it to bash.
fn step_script(name: &str) -> String {
    let mut lines = RELEASE_WORKFLOW.lines();
    lines
        .find(|line| line.trim() == format!("- name: {name}"))
        .unwrap_or_else(|| panic!("release.yaml has no step named {name:?}"));
    let run = lines
        .find(|line| line.trim() == "run: |")
        .unwrap_or_else(|| panic!("the {name:?} step has no `run: |` block"));
    let indent = leading_spaces(run);
    let body: Vec<&str> = lines
        .take_while(|line| line.trim().is_empty() || leading_spaces(line) > indent)
        .collect();
    let strip = body
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| leading_spaces(line))
        .min()
        .unwrap_or(0);
    body.iter()
        .map(|line| line.get(strip..).unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The `## Install` section of the README, up to the next heading.
fn install_section() -> &'static str {
    let start = README
        .find("\n## Install\n")
        .expect("README.md has no `## Install` section");
    let section = &README[start + 1..];
    let end = section[3..]
        .find("\n## ")
        .map(|i| i + 3)
        .unwrap_or(section.len());
    &section[..end]
}

/// A private directory per test. The tests here run in parallel, so each gets its own.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "rill-release-contract-{}-{label}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create the scratch directory");
        Scratch(dir)
    }

    /// An empty home, so the `sui` CLI's keystore is not found even on a machine that has one.
    fn home(&self) -> PathBuf {
        let home = self.0.join("home");
        fs::create_dir_all(&home).expect("create the empty home");
        home
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// No key in reach: the variable unset, the run-set unset, and HOME pointed at an empty directory.
/// The point of the release smoke step is what the binary does with nothing, and a developer
/// machine has plenty.
fn keyless(command: &mut Command, home: &Path) {
    command
        .env_remove(PRIVATE_KEY_VAR)
        .env_remove(RUN_SET_VAR)
        .env_remove("SUI_NETWORK")
        .env_remove("RILL_ALLOW_MAINNET")
        .env("HOME", home);
}

/// Runs the smoke step as the runner would: bash with `-e` and `pipefail`, from a checkout that
/// holds `target/<target>/release/rill-wallet`, with no key anywhere.
fn run_smoke_step(scratch: &Scratch, binary: &[u8], executable: bool) -> Output {
    let target = "smoke-test";
    let release = scratch.0.join("target").join(target).join("release");
    fs::create_dir_all(&release).expect("create the release directory");
    let bin = release.join("rill-wallet");
    fs::write(&bin, binary).expect("write the binary under test");
    let mode = if executable { 0o755 } else { 0o644 };
    fs::set_permissions(&bin, fs::Permissions::from_mode(mode)).expect("set the mode");

    let script = step_script(SMOKE_STEP).replace("${{ matrix.target }}", target);
    let mut command = Command::new("bash");
    command
        .args(["--noprofile", "--norc", "-eo", "pipefail", "-c", &script])
        .current_dir(&scratch.0);
    keyless(&mut command, &scratch.home());
    command
        .output()
        .expect("bash must be present to run the smoke step")
}

fn released_binary() -> Vec<u8> {
    fs::read(env!("CARGO_BIN_EXE_rill-wallet")).expect("read the rill-wallet binary")
}

fn text(output: &Output) -> String {
    format!(
        "exit {:?}\n--- stdout ---\n{}--- stderr ---\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

// The binary.

#[test]
fn rill_wallet_status_names_itself_and_exits_not_ready_without_a_key() {
    let scratch = Scratch::new("status");
    let mut command = Command::new(env!("CARGO_BIN_EXE_rill-wallet"));
    command.arg("--status");
    keyless(&mut command, &scratch.home());
    let out = command.output().expect("the rill-wallet binary must run");
    let stdout = String::from_utf8_lossy(&out.stdout);

    // The literal, not the constant. This is the name on the asset a stranger downloaded.
    assert_eq!(
        stdout.lines().next(),
        Some("rill-wallet"),
        "the first line must be the released name:\n{}",
        text(&out)
    );
    assert!(
        stdout.contains("not ready"),
        "with no key the binary must say so:\n{}",
        text(&out)
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "not ready is exit 1, no more and no less:\n{}",
        text(&out)
    );
}

// The workflow, read.

#[test]
fn every_matrix_asset_carries_the_released_name() {
    let assets = matrix_assets();
    assert_eq!(
        assets, ASSETS,
        "the matrix `asset:` values are the filenames the README hands out"
    );
    for asset in &assets {
        assert!(
            asset.starts_with("rill-wallet-"),
            "{asset} does not carry the released name"
        );
    }
}

#[test]
fn the_publish_list_names_every_asset_and_its_checksum() {
    let files = block_after("files: |");
    let expected: Vec<String> = ASSETS
        .iter()
        .flat_map(|asset| [asset.to_string(), format!("{asset}.sha256")])
        .collect();
    assert_eq!(
        files, expected,
        "the publish `files:` list must be exactly each asset with its .sha256 beside it"
    );
    assert!(
        RELEASE_WORKFLOW.contains("fail_on_unmatched_files: true"),
        "a missing asset must fail the release rather than publish a partial one"
    );
}

#[test]
fn the_workflow_builds_the_bin_the_manifest_declares() {
    assert!(
        MANIFEST.contains("name = \"rill-wallet\""),
        "bins/rill/Cargo.toml must declare a [[bin]] named rill-wallet"
    );
    assert!(
        RELEASE_WORKFLOW.contains("--bin rill-wallet"),
        "the workflow must build the rill-wallet bin"
    );
    assert!(
        RELEASE_WORKFLOW.contains("cargo build --release --locked"),
        "a release build must refuse a lockfile change"
    );
}

#[test]
fn the_smoke_step_greps_for_the_name_the_binary_prints() {
    let script = step_script(SMOKE_STEP);
    assert!(
        script.contains(&format!("grep -qx \"{BINARY_NAME}\"")),
        "the smoke step must look for exactly what status() prints ({BINARY_NAME}):\n{script}"
    );
    assert!(
        script.contains("[ \"$code\" -le 1 ]"),
        "the smoke step must fail on any exit code above 1, not swallow it:\n{script}"
    );
    assert!(
        !script.contains("|| true"),
        "the smoke step must not swallow the exit code:\n{script}"
    );
}

#[test]
fn the_publish_job_runs_only_for_a_tag() {
    assert!(
        RELEASE_WORKFLOW.contains("if: startsWith(github.ref, 'refs/tags/v')"),
        "publish must be gated on a tag, which is what makes workflow_dispatch a dry run"
    );
    assert!(
        RELEASE_WORKFLOW.contains("  workflow_dispatch:"),
        "a manual dry run must be possible without a tag"
    );
}

#[test]
fn no_matrix_runner_is_a_retired_image() {
    let runners = matrix_runners();
    assert_eq!(runners.len(), 3, "one runner per asset:\n{runners:?}");
    for runner in runners {
        assert!(
            !RETIRED_RUNNERS.contains(&runner),
            "{runner} has been retired by GitHub; a job on it never schedules"
        );
    }
}

// The README.

#[test]
fn the_readme_install_block_curls_every_asset_from_the_one_origin() {
    let install = install_section();
    let lines: Vec<&str> = install.lines().map(str::trim).collect();
    for asset in ASSETS {
        for name in [asset.to_string(), format!("{asset}.sha256")] {
            // The whole line, not a substring: the binary's URL is a prefix of its checksum's,
            // so a substring match would let the binary line name any origin at all.
            let curl = format!("curl -fsSLO {ORIGIN}/latest/download/{name}");
            assert!(
                lines.contains(&curl.as_str()),
                "the Install section must carry the line `{curl}`:\n{install}"
            );
        }
        assert!(
            install.contains(&format!("shasum -a 256 -c {asset}.sha256")),
            "the Install section must verify {asset} against its checksum:\n{install}"
        );
    }
    assert!(
        install.contains("./rill-wallet --status"),
        "the Install section must end with the first command a stranger runs:\n{install}"
    );
    assert!(
        install.contains("tag on `github.com/rifuki/rill`"),
        "the Install section must say what produces the release and where:\n{install}"
    );
}

#[test]
fn the_readme_names_no_other_origin() {
    for line in README
        .lines()
        .filter(|line| line.contains("releases/download"))
    {
        assert!(
            line.contains(ORIGIN),
            "a download line names an origin other than rifuki/rill: {line}"
        );
    }
    for other in ["naisu-one/rill", "eseslabs/rill"] {
        assert!(
            !README.contains(other),
            "{other} is the TypeScript specification, not a place this binary is published"
        );
    }
}

// The smoke step, run.

#[test]
fn the_smoke_step_passes_the_release_binary() {
    let scratch = Scratch::new("smoke-pass");
    let out = run_smoke_step(&scratch, &released_binary(), true);
    assert!(
        out.status.success(),
        "the smoke step must pass the binary the release ships:\n{}",
        text(&out)
    );
}

#[test]
fn the_smoke_step_fails_a_binary_that_cannot_execute() {
    let scratch = Scratch::new("smoke-noexec");
    let out = run_smoke_step(&scratch, &released_binary(), false);
    assert!(
        !out.status.success(),
        "a file the OS refuses to execute must fail the job:\n{}",
        text(&out)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("cannot start"),
        "the failure must say the binary cannot start:\n{}",
        text(&out)
    );
}

/// The case the earlier step passed: `|| true` swallowed the exit code, and the grep on a second
/// run saw the name. A binary that prints its banner and then aborts is not one to ship.
#[test]
fn the_smoke_step_fails_a_binary_that_identifies_itself_and_then_dies() {
    let scratch = Scratch::new("smoke-dies");
    let dies = "#!/bin/sh\nprintf 'rill-wallet\\n  status : not ready\\n'\nexit 134\n";
    let out = run_smoke_step(&scratch, dies.as_bytes(), true);
    assert!(
        !out.status.success(),
        "exit 134 after the banner must fail the job:\n{}",
        text(&out)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("exited 134"),
        "the failure must name the exit code:\n{}",
        text(&out)
    );
}

#[test]
fn the_smoke_step_fails_a_binary_that_answers_to_the_wrong_name() {
    let scratch = Scratch::new("smoke-wrong-name");
    let wrong = "#!/bin/sh\nprintf 'rill\\n  status : not ready\\n'\nexit 1\n";
    let out = run_smoke_step(&scratch, wrong.as_bytes(), true);
    assert!(
        !out.status.success(),
        "a binary that prints the old name must fail the job:\n{}",
        text(&out)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("did not identify itself"),
        "the failure must say what was missing:\n{}",
        text(&out)
    );
}
