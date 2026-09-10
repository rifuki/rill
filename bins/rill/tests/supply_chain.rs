//! Every build path is held to the checked-in lockfile, to one pinned toolchain, and to a reviewed
//! set of dependencies that run code at build time (plan U6, R6).
//!
//! Three flags and one list carry this, and all four are the kind of thing that goes missing without
//! anything turning red. The release workflow passed `--locked` and the Dockerfile did not, so the
//! hosted server could resolve dependency versions nobody reviewed while the binaries users checked
//! a checksum against were built from `Cargo.lock`. Two artifacts, two trees, one claim.
//!
//! The list is the part `--locked` cannot do. Pinning versions says nothing about what those
//! versions do at build time, and the live vector is a `build.rs` appearing inside a dependency tree
//! that was already approved: it runs arbitrary code with the privileges of whoever started the
//! build, before any test here executes, and a campaign is currently using exactly that to read Sui
//! keystores off developer and CI machines. A keystore that leaves cannot be called back. So
//! `scripts/build-scripts.baseline` records which packages carry a build script, CI diffs the tree
//! against it, and the audit script is run here rather than only read: a refusal that is grepped for
//! its wording passes a script that refuses nothing.
//!
//! The files are read as text because that is where the facts live. A test asserting `--locked`
//! reaches cargo would have to run three builds; a test asserting the flag is on the line is worth
//! having, as long as it also fails when the line disappears, which is what every assertion below
//! is checked against by deleting the thing it guards.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const DOCKERFILE: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Dockerfile"));
const CI: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../.github/workflows/ci.yaml"
));
const RELEASE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../.github/workflows/release.yaml"
));
const TOOLCHAIN: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../rust-toolchain.toml"
));
const AUDIT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../scripts/audit-build-scripts.sh"
));
const BASELINE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../scripts/build-scripts.baseline"
));

/// The path CI calls, spelled the way CI spells it.
const AUDIT_COMMAND: &str = "scripts/audit-build-scripts.sh check";

/// The baseline's path, named in the refusal so a reader is not left hunting for it.
const BASELINE_PATH: &str = "scripts/build-scripts.baseline";

fn repo(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative)
}

/// Every file that builds something. A build path missing from this list is a build path nothing
/// below checks, which is how the Dockerfile kept its unlocked build while the release had one.
fn build_paths() -> [(&'static str, &'static str); 4] {
    [
        ("Dockerfile", DOCKERFILE),
        (".github/workflows/ci.yaml", CI),
        (".github/workflows/release.yaml", RELEASE),
        ("scripts/audit-build-scripts.sh", AUDIT),
    ]
}

/// Subcommands that read `Cargo.lock`, and will rewrite it when the manifests have moved on. Each
/// one must say `--locked`, so that a lockfile needing a rewrite fails the build instead of being
/// rewritten inside it.
const RESOLVING: [&str; 9] = [
    "build", "check", "clippy", "test", "tree", "metadata", "run", "doc", "rustc",
];

/// Subcommands that resolve nothing. `cargo fmt` is the only one these files use, and it rejects
/// `--locked` outright: it forwards unrecognised arguments to rustfmt.
const NON_RESOLVING: [&str; 1] = ["fmt"];

struct Invocation {
    line_no: usize,
    subcommand: &'static str,
    line: &'static str,
}

/// What can stand immediately before `cargo` and still leave it a command: the start of a line, a
/// YAML or Dockerfile `run` key, a shell keyword, or the end of a pipeline or list.
///
/// The word "cargo" inside prose is not an invocation, and the first shape of this check thought
/// otherwise: it read `echo "rustc and cargo are both $pinned"` as a call to `cargo are` and
/// demanded a `--locked` on it. Skipping comment lines was not enough, because the sentence that
/// tripped it is a line the build really does run. So the position is what decides.
fn opens_a_command(previous: &str) -> bool {
    matches!(
        previous,
        "RUN" | "run:" | "if" | "then" | "else" | "do" | "!" | "&&" | "||" | "|" | ";" | "$("
    ) || previous.ends_with("&&")
        || previous.ends_with("||")
        || previous.ends_with('|')
        || previous.ends_with(';')
        || previous.ends_with("$(")
}

/// Every `cargo <subcommand>` a build path would actually run.
///
/// Comment lines are skipped: the prose in these files explains what a build script does during
/// `cargo build`, and a check that read its own explanation as an invocation could only be silenced
/// by deleting the explanation. Everything else is read, including the help text the audit script
/// prints, because a command offered to a reader should carry the same flag the build does.
///
/// The flag has to be on the same line as the subcommand. That is stricter than bash requires, and
/// deliberately so: a continuation would let `--locked` sit where a reader of the build path does
/// not see it either.
fn cargo_invocations(text: &'static str) -> Vec<Invocation> {
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim_start().starts_with('#'))
        .flat_map(|(index, line)| {
            let tokens: Vec<&'static str> = line.split_whitespace().collect();
            tokens
                .windows(2)
                .enumerate()
                .filter(|(position, pair)| {
                    pair[0] == "cargo" && (*position == 0 || opens_a_command(tokens[position - 1]))
                })
                .map(|(_, pair)| Invocation {
                    line_no: index + 1,
                    subcommand: pair[1],
                    line,
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The exact version `rust-toolchain.toml` pins.
fn pinned_toolchain() -> &'static str {
    TOOLCHAIN
        .lines()
        .find_map(|line| line.trim().strip_prefix("channel = "))
        .expect("rust-toolchain.toml must set a channel")
        .trim()
        .trim_matches('"')
}

/// Every toolchain the workflows install, read out of the `uses:` ref.
fn installed_toolchains(workflow: &'static str) -> Vec<&'static str> {
    workflow
        .lines()
        .filter_map(|line| line.trim().strip_prefix("- uses: dtolnay/rust-toolchain@"))
        .map(str::trim)
        .collect()
}

/// The baseline's data lines: one `name version` per package, comments and blanks dropped, which is
/// the same reduction the audit script performs.
fn reviewed_packages(baseline: &str) -> Vec<&str> {
    baseline
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect()
}

// The lockfile.

/// The unit's requirement, and the defect it was written for: the image build had no `--locked`, so
/// the hosted server and the released binaries could come from different dependency trees.
#[test]
fn every_cargo_command_in_every_build_path_refuses_a_lockfile_change() {
    let mut checked = 0;
    for (name, text) in build_paths() {
        let invocations = cargo_invocations(text);
        assert!(
            !invocations.is_empty(),
            "{name} is listed as a build path but runs no cargo command; either it stopped being \
             one, or this check is reading the wrong file"
        );
        for Invocation {
            line_no,
            subcommand,
            line,
        } in invocations
        {
            if NON_RESOLVING.contains(&subcommand) {
                continue;
            }
            assert!(
                RESOLVING.contains(&subcommand),
                "{name}:{line_no} runs `cargo {subcommand}`, and this check does not know whether \
                 that resolves dependencies. Add it to RESOLVING or NON_RESOLVING, having decided \
                 which: {line}"
            );
            assert!(
                line.contains("--locked"),
                "{name}:{line_no} resolves dependencies without --locked, so a run that rewrote \
                 Cargo.lock would build dependency versions nobody reviewed: {line}"
            );
            checked += 1;
        }
    }
    // A scanner that silently matches nothing asserts nothing. Six is the count at the time of
    // writing, minus a margin: the point is that the loop above did work, not the exact number.
    assert!(
        checked >= 6,
        "only {checked} resolving cargo commands were found across every build path; the scanner \
         has stopped seeing them"
    );
}

/// The one invocation in `text` that builds `bin`, as a command rather than as a mention. The
/// comment above each of these build lines explains `--locked` and therefore contains the word, so a
/// substring search over the whole file would pass a file whose build line had lost the flag.
fn build_line(text: &'static str, bin: &str) -> &'static str {
    let mut found = cargo_invocations(text)
        .into_iter()
        .filter(|invocation| invocation.subcommand == "build" && invocation.line.contains(bin));
    let line = found
        .next()
        .unwrap_or_else(|| panic!("no `cargo build` command for {bin}"))
        .line;
    assert!(
        found.next().is_none(),
        "more than one `cargo build` command names {bin}; this check would only hold one of them"
    );
    line
}

/// Named on its own because it is the line this unit exists to change, and a reader of the diff
/// should find the claim next to the file rather than inside a loop.
#[test]
fn the_image_builds_the_server_from_the_checked_in_lockfile() {
    assert!(
        build_line(DOCKERFILE, "--bin rill-server").contains("--locked"),
        "the Dockerfile must build rill-server with --locked: the hosted image and the released \
         binaries have to come from one dependency tree, or the checksum a user verifies says \
         nothing about what is serving them"
    );
}

#[test]
fn the_release_builds_the_wallet_from_the_checked_in_lockfile() {
    assert!(
        build_line(RELEASE, "--bin rill-wallet").contains("--locked"),
        "a release built from a lockfile cargo had to rewrite is a release built from dependencies \
         nobody reviewed"
    );
}

// The toolchain.

/// `rust-toolchain.toml` naming `stable` would pin nothing: the compiler would change under the
/// repository every six weeks, and the first symptom would be a build that fails on a commit that
/// did not change.
#[test]
fn the_toolchain_file_pins_an_exact_version() {
    let pinned = pinned_toolchain();
    let parts: Vec<&str> = pinned.split('.').collect();
    assert_eq!(
        parts.len(),
        3,
        "rust-toolchain.toml pins {pinned:?}; it must name an exact version, three numbers, not a \
         channel"
    );
    for part in parts {
        assert!(
            !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()),
            "rust-toolchain.toml pins {pinned:?}, which is not three numbers"
        );
    }
    assert!(
        TOOLCHAIN.contains("rustfmt") && TOOLCHAIN.contains("clippy"),
        "the pinned toolchain must carry rustfmt and clippy, or the jobs that run them install a \
         second toolchain to get them"
    );
}

/// The pin binds only if every job installs it. `dtolnay/rust-toolchain@stable` installed stable and
/// made it the default; the toolchain file still won inside the directory, so the jobs were right by
/// accident, through a precedence rule one `RUSTUP_TOOLCHAIN` in the environment reverses. Naming
/// the version in `uses:` removes the accident, and CI reads the version back out of the tools that
/// run (the step asserted below) so that a disagreement is a failure rather than a surprise.
#[test]
fn every_workflow_installs_exactly_the_pinned_toolchain() {
    let pinned = pinned_toolchain();
    for (name, workflow) in [
        (".github/workflows/ci.yaml", CI),
        (".github/workflows/release.yaml", RELEASE),
    ] {
        let installed = installed_toolchains(workflow);
        assert!(
            !installed.is_empty(),
            "{name} installs no Rust toolchain; this check is reading the wrong file"
        );
        for toolchain in installed {
            assert_eq!(
                toolchain, pinned,
                "{name} installs {toolchain}, but rust-toolchain.toml pins {pinned}. A job on a \
                 different compiler reports on something nobody chose."
            );
        }
    }
}

#[test]
fn ci_reads_the_running_toolchain_back_out_of_the_tools_themselves() {
    let step = step_script(CI, "The pinned toolchain is the one that runs");
    assert!(
        step.contains("rust-toolchain.toml"),
        "the check must read the pin from rust-toolchain.toml rather than repeat it:\n{step}"
    );
    for tool in ["rustc", "cargo"] {
        assert!(
            step.contains(tool),
            "the check must ask {tool} what version it is running at; a toolchain that binds for \
             one of the two and not the other is the interesting case:\n{step}"
        );
    }
    assert!(
        step.contains("--version"),
        "the check must read the version out of the tool rather than infer it:\n{step}"
    );
    assert!(
        step.contains("::error::"),
        "a mismatch must be reported as an error annotation, not just a non-zero exit:\n{step}"
    );
}

// The build scripts.

#[test]
fn both_workflows_diff_the_build_script_baseline() {
    for (name, workflow) in [
        (".github/workflows/ci.yaml", CI),
        (".github/workflows/release.yaml", RELEASE),
    ] {
        // The whole line, and a line that runs: a commented-out step is a substring match and no
        // check at all.
        let runs = workflow.lines().any(|line| {
            line.trim().trim_start_matches("- ").trim() == format!("run: {AUDIT_COMMAND}")
        });
        assert!(
            runs,
            "{name} must run `{AUDIT_COMMAND}`: --locked pins which versions resolve and says \
             nothing about a build.rs appearing inside them"
        );
    }
}

#[test]
fn the_audit_script_is_executable_as_committed() {
    let mode = fs::metadata(repo("scripts/audit-build-scripts.sh"))
        .expect("the audit script must exist")
        .permissions()
        .mode();
    assert!(
        mode & 0o111 != 0,
        "scripts/audit-build-scripts.sh is mode {mode:o}; CI runs it as a command, and a lost \
         execute bit fails the job with `Permission denied` and no hint of why"
    );
}

/// The baseline is a diff surface, so its order and its shape are what make a change readable.
#[test]
fn the_baseline_is_one_sorted_name_and_version_per_line() {
    let packages = reviewed_packages(BASELINE);
    assert!(
        packages.len() > 20,
        "the baseline holds {} entries, which is too few to be this workspace's tree",
        packages.len()
    );
    let mut sorted = packages.clone();
    sorted.sort_unstable();
    assert_eq!(
        packages, sorted,
        "the baseline must be sorted, or a one-package change shows up as a rewritten file"
    );
    let mut unique = sorted.clone();
    unique.dedup();
    assert_eq!(
        sorted, unique,
        "the baseline must hold each name and version once"
    );
    for entry in packages {
        let mut fields = entry.split(' ');
        let name = fields.next().unwrap_or_default();
        let version = fields.next().unwrap_or_default();
        assert!(
            !name.is_empty() && fields.next().is_none(),
            "baseline entry {entry:?} is not `name version`"
        );
        assert!(
            version.starts_with(|c: char| c.is_ascii_digit()),
            "baseline entry {entry:?} has no version; a name alone would pass every version of \
             that package, including the one that added the build script"
        );
    }
    assert!(
        BASELINE.starts_with('#'),
        "the baseline must open with what it is and how to regenerate it; it is read by whoever \
         the refusal sends here"
    );
}

// The audit, run.

/// A private directory per test, holding a copy of the audit script and a baseline to act on. The
/// tests here run in parallel and two of them write a baseline, so nothing is shared.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str, baseline: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("rill-supply-chain-{}-{label}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let scripts = dir.join("scripts");
        fs::create_dir_all(&scripts).expect("create the scratch scripts directory");
        fs::write(scripts.join("audit-build-scripts.sh"), AUDIT).expect("copy the audit script");
        fs::write(scripts.join("build-scripts.baseline"), baseline).expect("write the baseline");
        Scratch(dir)
    }

    fn baseline(&self) -> String {
        fs::read_to_string(self.0.join("scripts/build-scripts.baseline"))
            .expect("read the scratch baseline")
    }

    /// Runs the audit as CI runs it, over a captured `cargo metadata` document.
    ///
    /// Captured rather than live: a nested `cargo metadata` can block on the package-cache lock the
    /// outer cargo holds, and a test that hangs is worse than no test. What is exercised here is
    /// the comparison and the refusal, which is the part that can be wrong. Whether the real tree
    /// matches the real baseline is a question only the real `cargo metadata` answers, and CI asks
    /// it on every run.
    fn audit(&self, mode: &str, packages: &[(&str, &str)]) -> Output {
        let metadata = self.0.join(format!("metadata-{mode}.json"));
        fs::write(&metadata, metadata_document(packages)).expect("write the metadata document");
        Command::new("bash")
            .args([
                "--noprofile",
                "--norc",
                self.0
                    .join("scripts/audit-build-scripts.sh")
                    .to_str()
                    .expect("a utf-8 scratch path"),
                mode,
            ])
            .env("RILL_CARGO_METADATA", &metadata)
            .output()
            .expect("bash must be present to run the audit")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A `cargo metadata` document in which `packages` carry a build script and one package does not.
/// The package without one is the control: a filter that listed every dependency instead of the
/// build-script-bearing ones would pass every other assertion here.
fn metadata_document(packages: &[(&str, &str)]) -> String {
    let mut entries: Vec<serde_json::Value> = packages
        .iter()
        .map(|(name, version)| {
            serde_json::json!({
                "name": name,
                "version": version,
                "targets": [
                    { "kind": ["lib"], "name": name },
                    { "kind": ["custom-build"], "name": "build-script-build" },
                ],
            })
        })
        .collect();
    entries.push(serde_json::json!({
        "name": "no-build-script-here",
        "version": "9.9.9",
        "targets": [{ "kind": ["lib"], "name": "no-build-script-here" }],
    }));
    serde_json::json!({ "packages": entries }).to_string()
}

fn committed_packages() -> Vec<(&'static str, &'static str)> {
    reviewed_packages(BASELINE)
        .into_iter()
        .map(|entry| {
            let (name, version) = entry.split_once(' ').expect("`name version`");
            (name, version)
        })
        .collect()
}

fn text(output: &Output) -> String {
    format!(
        "exit {:?}\n--- stdout ---\n{}--- stderr ---\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn the_audit_passes_when_the_tree_carries_exactly_the_reviewed_build_scripts() {
    let scratch = Scratch::new("match", BASELINE);
    let out = scratch.audit("check", &committed_packages());
    assert!(
        out.status.success(),
        "the reviewed list must pass against a tree that matches it:\n{}",
        text(&out)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("on the reviewed list"),
        "a pass must say what it checked:\n{}",
        text(&out)
    );
}

/// The vector itself: a dependency that did not run code at build time now does, with no version of
/// ours changed and `--locked` satisfied.
#[test]
fn a_dependency_that_gains_a_build_script_fails_the_audit() {
    let scratch = Scratch::new("gained", BASELINE);
    let mut packages = committed_packages();
    packages.push(("totally-innocent-helper", "0.1.0"));
    let out = scratch.audit("check", &packages);
    let said = text(&out);

    assert!(
        !out.status.success(),
        "a new build script must fail the build, not land silently:\n{said}"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("+totally-innocent-helper 0.1.0"),
        "the refusal must name the package that gained the build script:\n{said}"
    );
    // What to do, not just what is wrong. A refusal nobody can act on gets the check deleted.
    assert!(
        stdout.contains("Review each new build script"),
        "the refusal must say to review the build script:\n{said}"
    );
    assert!(
        stdout.contains(BASELINE_PATH) && stdout.contains("in the same commit"),
        "the refusal must name the baseline and say the update belongs with the change:\n{said}"
    );
    assert!(
        stdout.contains("audit-build-scripts.sh update"),
        "the refusal must hand over the command that regenerates the baseline:\n{said}"
    );
}

/// The other direction is also a review. A build script leaving the tree is good news and still
/// means the dependency graph moved, which is worth one look before the baseline forgets.
#[test]
fn a_build_script_that_leaves_the_tree_also_fails_the_audit() {
    let scratch = Scratch::new("left", BASELINE);
    let mut packages = committed_packages();
    let (gone_name, gone_version) = packages.remove(0);
    let out = scratch.audit("check", &packages);
    let said = text(&out);

    assert!(
        !out.status.success(),
        "a build script that disappeared must still be reconciled:\n{said}"
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(&format!("-{gone_name} {gone_version}")),
        "the refusal must name what left:\n{said}"
    );
}

#[test]
fn update_rewrites_the_baseline_so_the_next_check_passes() {
    let scratch = Scratch::new("update", BASELINE);
    let mut packages = committed_packages();
    packages.push(("totally-innocent-helper", "0.1.0"));

    let updated = scratch.audit("update", &packages);
    assert!(
        updated.status.success(),
        "update must rewrite the baseline:\n{}",
        text(&updated)
    );

    let written = scratch.baseline();
    assert!(
        written.starts_with('#'),
        "update must keep the header that tells the next reader what the file is:\n{written}"
    );
    assert!(
        written.contains("\ntotally-innocent-helper 0.1.0\n"),
        "update must record the package that was reviewed:\n{written}"
    );

    let out = scratch.audit("check", &packages);
    assert!(
        out.status.success(),
        "an updated baseline must satisfy the check it just failed:\n{}",
        text(&out)
    );
}

/// Shared with `release_contract.rs` in shape, not in code: the two tests read different workflows
/// for different reasons, and a helper crate between them would be a third place to keep in step.
fn step_script(workflow: &'static str, name: &str) -> String {
    let mut lines = workflow.lines();
    lines
        .find(|line| line.trim() == format!("- name: {name}"))
        .unwrap_or_else(|| panic!("no step named {name:?}"));
    let run = lines
        .find(|line| line.trim() == "run: |")
        .unwrap_or_else(|| panic!("the {name:?} step has no `run: |` block"));
    let indent = run.len() - run.trim_start().len();
    lines
        .take_while(|line| line.trim().is_empty() || line.len() - line.trim_start().len() > indent)
        .collect::<Vec<_>>()
        .join("\n")
}
