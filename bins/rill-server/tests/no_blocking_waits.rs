//! The server never blocks a thread waiting for the chain.
//!
//! `rill_chain::settle` polls a node with `std::thread::sleep` between attempts, which is correct
//! where it is used: every caller is a `bins/rill` command or stdio tool that owns a current-thread
//! runtime built for one operation with nothing else scheduled on it, so blocking that thread
//! blocks nothing. Its module documentation says so, and says it must not be called from a shared
//! runtime.
//!
//! Here that would be a different thing entirely. This server's runtime serves every concurrent
//! request, so one `settle` call would stall up to thirty seconds of unrelated traffic: an agent
//! waiting on an unrelated build, an OAuth exchange, a health check the orchestrator uses to decide
//! whether to replace the container. Thirty seconds is long enough for that healthcheck to fail
//! three times, which is the configured threshold.
//!
//! The rule was documented and enforced by nothing, so it held only as long as whoever edited this
//! crate had read that module. This is the check.

use std::fs;
use std::path::{Path, PathBuf};

fn server_sources() -> Vec<PathBuf> {
    fn walk(dir: &Path, found: &mut Vec<PathBuf>) {
        let entries =
            fs::read_dir(dir).unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()));
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, found);
            } else if path.extension().is_some_and(|e| e == "rs") {
                found.push(path);
            }
        }
    }
    let mut found = Vec::new();
    walk(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut found,
    );
    assert!(
        found.len() > 3,
        "the walk found {} files, so it is not looking where the server lives",
        found.len()
    );
    found
}

#[test]
fn no_server_source_waits_on_the_chain_by_blocking_its_thread() {
    for path in server_sources() {
        let source = fs::read_to_string(&path).expect("a source file");
        let name = path
            .strip_prefix(env!("CARGO_MANIFEST_DIR"))
            .unwrap_or(&path)
            .display()
            .to_string();
        for (index, line) in source.lines().enumerate() {
            let code = line.split("//").next().unwrap_or(line);
            assert!(
                !code.contains("settle::"),
                "{name}:{}: this server's runtime serves every request, and settle blocks its \
                 thread for up to thirty seconds. See the module note in rill-chain.",
                index + 1
            );
            assert!(
                !code.contains("thread::sleep"),
                "{name}:{}: a blocking sleep here stalls every other request on this runtime",
                index + 1
            );
        }
    }
}

/// The check is worthless if it is looking at the wrong tree, so it must be able to see a breach.
#[test]
fn the_check_would_see_a_blocking_wait_if_one_were_added() {
    let planted = "    let ok = rill_chain::settle::wait_until_readable(&chain, id).await;";
    let code = planted.split("//").next().unwrap_or(planted);
    assert!(
        code.contains("settle::"),
        "the matcher no longer recognises the call it exists to refuse"
    );
    let commented = "    // rill_chain::settle::wait_until_readable is deliberately not used here";
    assert!(
        !commented
            .split("//")
            .next()
            .unwrap_or(commented)
            .contains("settle::"),
        "a sentence explaining the rule must not fail the rule"
    );
}
