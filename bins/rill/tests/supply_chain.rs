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
/// The install instructions a stranger pastes. Read here because the chain they form is the thing
/// that stops an unverified file being made executable.
const README: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../README.md"));
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

/// Every toolchain the workflows install, read out of the `uses:` line.
///
/// Two shapes, and both are deliberate. A ref (`@1.96.0`) names the version directly. A commit
/// (`@ebb3d16... # 1.96.0`) pins the action's own code as well, and then the version lives in the
/// trailing comment, which is the only place a reviewer can read it: a bare forty-character hash
/// says nothing about what it is supposed to be. Release does the second, because every action in
/// that job runs with write access to the asset a stranger downloads.
fn installed_toolchains(workflow: &'static str) -> Vec<&'static str> {
    workflow
        .lines()
        .filter_map(|line| line.trim().strip_prefix("- uses: dtolnay/rust-toolchain@"))
        .map(|rest| {
            let rest = rest.trim();
            match rest.split_once('#') {
                Some((_, comment)) => comment.trim(),
                None => rest,
            }
        })
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

// ── The toolchain check, run rather than read ────────────────────────────────────────────────────

/// The pinned-toolchain step must actually fail when the toolchain is wrong.
///
/// Its only test asserted that the step's script contains the strings `rust-toolchain.toml`,
/// `rustc`, `cargo`, `--version` and `::error::`. Replacing the comparison with a bare
/// `echo "::error:: $tool is $got, rust-toolchain.toml, --version"` keeps every one of those words,
/// prints an annotation, never exits non-zero, and left the suite green: CI's only runtime pin was
/// disarmed and the test that guards it could not tell. Keyword presence is not behaviour.
///
/// So the step is extracted and run, with `rustc` and `cargo` stubs on PATH reporting whatever this
/// test wants them to. The same shape as the release smoke step's harness and the audit script's.
fn run_toolchain_step(reported_version: &str) -> std::process::Output {
    let scratch = std::env::temp_dir().join(format!(
        "rill-toolchain-step-{}-{}",
        std::process::id(),
        reported_version.replace('.', "-")
    ));
    let bin = scratch.join("bin");
    fs::create_dir_all(&bin).expect("a scratch bin directory");

    // `rustc --version` prints "rustc 1.2.3 (hash date)", and the step takes field two.
    for tool in ["rustc", "cargo"] {
        let path = bin.join(tool);
        fs::write(
            &path,
            format!("#!/bin/sh\necho \"{tool} {reported_version} (stub)\"\n"),
        )
        .expect("write a stub");
        let mut perms = fs::metadata(&path).expect("stat the stub").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).expect("make the stub executable");
    }

    // The step reads rust-toolchain.toml from the working directory, so it runs in a scratch copy
    // rather than the repository: a test that wrote there would be editing the thing under test.
    fs::write(scratch.join("rust-toolchain.toml"), TOOLCHAIN).expect("copy the toolchain file");
    let script = scratch.join("step.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\n{}\n",
            step_script(CI, "The pinned toolchain is the one that runs")
        ),
    )
    .expect("write the step");

    let out = Command::new("bash")
        .args([
            "--noprofile",
            "--norc",
            script.to_str().expect("a utf-8 path"),
        ])
        .current_dir(&scratch)
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .output()
        .expect("bash must be present");
    let _ = fs::remove_dir_all(&scratch);
    out
}

#[test]
fn the_toolchain_step_passes_when_the_running_compiler_is_the_pinned_one() {
    let out = run_toolchain_step(pinned_toolchain());
    assert!(
        out.status.success(),
        "the step must pass when rustc and cargo report the pinned version:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(pinned_toolchain()),
        "and it must say which version it found"
    );
}

/// The case that matters: a compiler that is not the pinned one has to fail the job, not annotate it.
#[test]
fn the_toolchain_step_fails_when_the_running_compiler_is_not_the_pinned_one() {
    let wrong = "1.70.0";
    assert_ne!(
        wrong,
        pinned_toolchain(),
        "the fixture must actually differ"
    );
    let out = run_toolchain_step(wrong);
    assert!(
        !out.status.success(),
        "a compiler that is not the pinned one must fail the job. An annotation without a non-zero \
         exit is a green run that built with the wrong compiler:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(
        said.contains("::error::") && said.contains(wrong) && said.contains(pinned_toolchain()),
        "the failure must name both versions, or nobody can tell what outranked the file: {said}"
    );
}

// ── The lists above are hand-written, so this is what notices a new one ──────────────────────────

/// Every workflow in the directory is one of the workflows these checks read.
///
/// `build_paths()` and the toolchain check both name their files by hand, which is the same shape of
/// omission that let the Dockerfile keep an unlocked build while the release had one. Adding
/// `.github/workflows/nightly.yaml` with an unlocked `cargo build` and a floating toolchain ref left
/// the whole suite green, because nothing reads the directory.
///
/// This does not check the new file's contents: it fails, and names it, so whoever added it adds it
/// to the lists. A check that tried to guess what a new workflow should contain would be a check
/// nobody could add a workflow past.
#[test]
fn no_workflow_exists_that_these_checks_do_not_read() {
    let dir = repo(".github/workflows");
    let mut present: Vec<String> = fs::read_dir(&dir)
        .expect("the workflows directory")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".yaml") || name.ends_with(".yml"))
        .collect();
    present.sort();

    let mut read: Vec<String> = build_paths()
        .iter()
        .map(|(path, _)| path.to_string())
        .filter(|path| path.starts_with(".github/workflows/"))
        .map(|path| path.trim_start_matches(".github/workflows/").to_string())
        .collect();
    read.sort();

    assert!(
        !present.is_empty(),
        "no workflows found, so this check is looking in the wrong place: {}",
        dir.display()
    );
    assert_eq!(
        present, read,
        "a workflow exists that the --locked and toolchain checks do not read. Add it to \
         build_paths() and to every_workflow_installs_exactly_the_pinned_toolchain, or those checks \
         silently stop covering the build."
    );
}

/// The release build is gated on the supply-chain audit, and the gate is an edge nothing asserted.
///
/// The audit step's presence in both workflows was checked; that the job carrying it gates the build
/// was not. Deleting `needs: supply-chain` from release.yaml left the suite green, so the published
/// binary could be built without the audit having run, which is the whole point of having one.
#[test]
fn the_release_build_waits_for_the_supply_chain_audit() {
    assert!(
        RELEASE.contains("needs: supply-chain"),
        "release.yaml's build job must declare `needs: supply-chain`, or the asset a stranger \
         downloads is built without the build-script audit having run"
    );
    // And the audit job must not be skippable, or the edge leads to a job that did nothing.
    let audit_job = RELEASE
        .split("\n  supply-chain:")
        .nth(1)
        .or_else(|| CI.split("\n  supply-chain:").nth(1))
        .expect("a supply-chain job in one of the workflows");
    let body: String = audit_job
        .lines()
        .take_while(|line| {
            line.trim().is_empty() || line.starts_with("    ") || line.starts_with("      ")
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !body.contains("if:"),
        "the supply-chain job carries an `if:`, so the gate can be skipped and the build proceeds \
         anyway:\n{body}"
    );
}

/// The image's compiler is the pinned one, by its tag rather than by an accident of precedence.
///
/// `FROM rust:1.96-slim` is a floating tag: any 1.96.x, re-pushed at will. The toolchain check read
/// only `uses: dtolnay/rust-toolchain@` lines, so nothing tied the image's compiler to
/// rust-toolchain.toml, and because the Dockerfile copies that file in, the version was resolved by
/// rustup's directory override. That is the accident CI was changed to stop relying on, left alive in
/// the one build path that produces the hosted server.
#[test]
fn the_image_builds_with_exactly_the_pinned_compiler() {
    let from = DOCKERFILE
        .lines()
        .find_map(|line| line.trim().strip_prefix("FROM rust:"))
        .expect("the Dockerfile must build from a rust image");
    let tag = from.split_whitespace().next().unwrap_or(from);
    let version = tag.split('-').next().unwrap_or(tag);
    assert_eq!(
        version,
        pinned_toolchain(),
        "the image builds on rust:{tag} while rust-toolchain.toml pins {}. A floating tag means the \
         hosted server's compiler is whatever the registry had that day.",
        pinned_toolchain()
    );
    assert_eq!(
        version.matches('.').count(),
        2,
        "the tag must name all three numbers: {tag}"
    );
}

/// The install block verifies before it makes anything executable, and the chain is what enforces it.
///
/// U6's README change was exactly this: `shasum -c && chmod +x && mv` on one chain, so a file that
/// does not match its checksum is never made executable and never run. Nothing asserted it. Reverting
/// the three commands to separate lines, which is how they were before, left the suite green: the
/// defect the unit fixed could come back and only a reader would notice.
#[test]
fn every_install_block_verifies_before_it_makes_anything_executable() {
    let blocks: Vec<&str> = README
        .split("```sh")
        .skip(1)
        .filter_map(|rest| rest.split("```").next())
        .filter(|block| block.contains("shasum -a 256 -c") || block.contains("sha256sum -c"))
        .filter(|block| block.contains("chmod +x"))
        .collect();
    assert!(
        blocks.len() >= 3,
        "expected one install block per platform carrying both a checksum and a chmod, found {}",
        blocks.len()
    );

    for block in blocks {
        // The checksum, the chmod and the mv must be one chain. A newline between them is a file
        // made executable whatever the checksum said.
        let chain: Vec<&str> = block
            .lines()
            .map(str::trim)
            .skip_while(|line| {
                !(line.contains("shasum -a 256 -c") || line.contains("sha256sum -c"))
            })
            .take_while(|line| !line.is_empty())
            .collect();
        assert!(
            !chain.is_empty(),
            "no checksum line found in a block that contains one:\n{block}"
        );
        let joined = chain.join(" ");
        assert!(
            joined.contains("&&"),
            "the checksum and what follows it are not chained, so a file that fails the check is \
             still made executable:\n{}",
            chain.join("\n")
        );
        let checksum_at = joined
            .find("-c ")
            .expect("the checksum command is in this chain");
        let chmod_at = joined
            .find("chmod +x")
            .unwrap_or_else(|| panic!("no chmod in:\n{}", chain.join("\n")));
        assert!(
            checksum_at < chmod_at,
            "the chmod runs before the checksum, which verifies nothing:\n{}",
            chain.join("\n")
        );
        if let Some(mv_at) = joined.find("mv ") {
            assert!(
                chmod_at < mv_at,
                "the move runs before the chmod:\n{}",
                chain.join("\n")
            );
        }
    }
}

/// The release workflow pins third-party actions to commits, not to refs.
///
/// `dtolnay/rust-toolchain@1.96.0` looks exact and is not: `git ls-remote` shows 1.96, 1.96.0 and
/// 1.96.1 are all live maintained branches, so that ref pins which Rust gets installed and says
/// nothing about the action's code. Every action in this workflow runs with write access to the job
/// that produces the binary a stranger downloads and verifies a checksum against, which is the one
/// place a mutable reference is least defensible. The report for this unit claimed these refs were
/// exact; they were not.
#[test]
fn the_release_workflow_pins_every_third_party_action_to_a_commit() {
    let uses: Vec<&str> = RELEASE
        .lines()
        .map(str::trim)
        .filter_map(|line| {
            line.strip_prefix("- uses: ")
                .or_else(|| line.strip_prefix("uses: "))
        })
        .collect();
    assert!(
        uses.len() >= 6,
        "expected the release workflow to use several actions, found {}",
        uses.len()
    );

    for entry in uses {
        let (reference, comment) = entry.split_once('#').unwrap_or((entry, ""));
        let reference = reference.trim();
        let at = reference
            .rsplit_once('@')
            .unwrap_or_else(|| panic!("{reference} names no ref at all"))
            .1;
        assert_eq!(
            at.len(),
            40,
            "{reference} is pinned to a ref rather than a commit. A ref is mutable, and this \
             workflow produces the asset a stranger downloads."
        );
        assert!(
            at.chars().all(|c| c.is_ascii_hexdigit()),
            "{reference} does not look like a commit hash"
        );
        assert!(
            !comment.trim().is_empty(),
            "{reference} has no trailing comment saying which version it is, and a bare hash tells \
             a reviewer nothing"
        );
    }
}
