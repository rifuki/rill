//! The durable receipt store behind every tool that submits on its own build: one caller-chosen
//! key, one execution, and the first outcome handed back on every retry, including after a restart.
//!
//! These drive `workflow::once` directly, with closures standing in for the tools. A closure that
//! panics on a repeat is the assertion that a repeat never executes.

use std::sync::atomic::{AtomicUsize, Ordering};

use rill_cli::workflow::{once, ReceiptError};
use serde_json::{json, Value};

fn directory() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rill-idempotency-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn spend(amount: &str) -> Value {
    json!({"tool":"rill_spend","arguments":{"wallet":"0x1","cap":"0x2","amount":amount}})
}

fn landed(digest: &str) -> Value {
    json!({"structuredContent":{"submitted":true,"digest":digest},"isError":false})
}

#[test]
fn once_executes_exactly_once_and_replays_after_restart() {
    let dir = directory();
    let first = once(&dir, "pay-1", &spend("0.01"), || landed("D")).unwrap();
    assert_eq!(first["status"], "completed");
    assert_eq!(first["replayed"], false);
    assert_eq!(first["result"]["structuredContent"]["digest"], "D");

    // Nothing is held in memory between the two calls, so this is what a restarted signer sees.
    let again = once(&dir, "pay-1", &spend("0.01"), || {
        panic!("a retry with the same key must never execute")
    })
    .unwrap();
    assert_eq!(again["replayed"], true);
    assert_eq!(again["status"], "completed");
    assert_eq!(again["result"]["structuredContent"]["digest"], "D");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn same_key_different_request_is_a_conflict_and_never_executes() {
    let dir = directory();
    once(&dir, "pay-2", &spend("0.01"), || landed("D")).unwrap();
    let conflict = once(&dir, "pay-2", &spend("0.02"), || {
        panic!("a key reused for other arguments must never execute")
    });
    match conflict {
        Err(ReceiptError::Conflict { stored }) => {
            assert_eq!(
                stored,
                spend("0.01"),
                "the conflict names what the key belongs to"
            )
        }
        other => panic!("expected a conflict, got {other:?}"),
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn key_order_in_the_arguments_is_not_a_different_request() {
    let dir = directory();
    once(&dir, "pay-order", &spend("0.01"), || landed("D")).unwrap();
    let reordered: Value = serde_json::from_str(
        r#"{"arguments":{"amount":"0.01","cap":"0x2","wallet":"0x1"},"tool":"rill_spend"}"#,
    )
    .unwrap();
    let again = once(&dir, "pay-order", &reordered, || panic!("same request")).unwrap();
    assert_eq!(again["replayed"], true);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_running_receipt_reports_interrupted_and_never_executes() {
    let dir = directory();
    // A crash after the claim and before the outcome was recorded.
    let _ = std::panic::catch_unwind(|| {
        once(&dir, "pay-3", &spend("0.01"), || {
            panic!("process interruption")
        })
    });
    let report = once(&dir, "pay-3", &spend("0.01"), || {
        panic!("an uncertain call must never execute again")
    })
    .unwrap();
    assert_eq!(report["status"], "interrupted");
    assert_eq!(report["replayed"], true);
    let note = report["note"].as_str().unwrap();
    assert!(note.contains("Inspect the chain"), "{note}");
    assert!(report["claimedAtMs"].as_u64().is_some(), "{report}");
    std::fs::remove_dir_all(dir).unwrap();
}

/// The window between `create_new` and the first write: another process holds the claim and has
/// not written it yet. That is a call in flight, not a corrupt store, and it must not execute.
#[test]
fn an_empty_or_half_written_receipt_is_running_and_never_executes() {
    let dir = directory();
    for (key, contents) in [("empty", ""), ("half", "{\"version\":1,\"idempot")] {
        std::fs::write(dir.join(format!("{key}.json")), contents).unwrap();
        let report = once(&dir, key, &spend("0.01"), || {
            panic!("a claim another call holds must never execute")
        })
        .unwrap();
        assert_eq!(report["status"], "interrupted", "{key}: {report}");
        assert_eq!(report["replayed"], true);
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_refusal_is_stored_and_replayed_not_reexecuted() {
    let dir = directory();
    let first = once(
        &dir,
        "pay-4",
        &spend("0.01"),
        || json!({"isError":true,"structuredContent":{"code":"submit_failed"}}),
    )
    .unwrap();
    assert_eq!(first["status"], "stopped");
    let again = once(&dir, "pay-4", &spend("0.01"), || {
        panic!("a lost submission must never be retried under its key")
    })
    .unwrap();
    assert_eq!(again["status"], "stopped");
    assert_eq!(again["replayed"], true);
    assert_eq!(
        again["result"]["structuredContent"]["code"],
        "submit_failed"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn traversal_and_overlong_keys_are_refused_before_any_file_is_created() {
    let dir = directory();
    let long = "a".repeat(97);
    for key in ["../x", "a/b", "", long.as_str(), ".hidden", "a b"] {
        match once(&dir, key, &spend("0.01"), || {
            panic!("{key:?} must not execute")
        }) {
            Err(ReceiptError::InvalidKey(message)) => {
                assert!(message.contains("idempotencyKey"), "{message}")
            }
            other => panic!("{key:?} was not refused as invalid: {other:?}"),
        }
    }
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    assert!(!dir.parent().unwrap().join("x.json").exists());
    std::fs::remove_dir_all(dir).unwrap();
}

/// Two signer processes (Claude Code and Codex can each launch one) racing on one key. The loser
/// must answer interrupted while the winner is still executing, and must never execute itself.
#[test]
fn simultaneous_calls_with_one_key_execute_once() {
    // The signer's layout, with every level below the state directory still missing, so the race
    // is also over who creates the receipt directories.
    let root = directory();
    let dir = root.join("requests").join("testnet").join("0xff");
    let count = AtomicUsize::new(0);
    let barrier = std::sync::Barrier::new(2);
    let results: Vec<Value> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let (dir, count, barrier) = (&dir, &count, &barrier);
                scope.spawn(move || {
                    barrier.wait();
                    once(dir, "race", &spend("0.01"), || {
                        count.fetch_add(1, Ordering::SeqCst);
                        // Long enough that the loser reads the claim while this is in flight.
                        std::thread::sleep(std::time::Duration::from_millis(300));
                        landed("D")
                    })
                    .unwrap()
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let statuses: std::collections::BTreeSet<&str> = results
        .iter()
        .map(|r| r["status"].as_str().unwrap())
        .collect();
    assert_eq!(
        statuses,
        ["completed", "interrupted"].into_iter().collect(),
        "{results:?}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let file = std::fs::metadata(dir.join("race.json")).unwrap();
        assert_eq!(file.permissions().mode() & 0o777, 0o600);
        for folder in [
            root.join("requests"),
            root.join("requests").join("testnet"),
            dir,
        ] {
            let mode = std::fs::metadata(&folder).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{}", folder.display());
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}
