use rill_cli::workflow::{run, Workflow};
use serde_json::json;

fn definition() -> Workflow {
    serde_json::from_value(json!({
        "runId":"workflow-test", "network":"mainnet", "signer":"0x1", "owner":"0x2",
        "steps":[
            {"actionId":"swap", "walletId":"0x3", "revision":1},
            {"actionId":"stake", "walletId":"0x4", "revision":2},
            {"actionId":"order", "walletId":"0x5", "revision":1}
        ]
    }))
    .unwrap()
}
fn directory() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rill-workflow-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
#[test]
fn success_is_ordered_and_restart_replay_never_executes() {
    let dir = directory();
    let workflow = definition();
    let mut called = vec![];
    let first = run(&dir, &workflow, |step| {
        called.push(step.action_id.clone());
        json!({"submitted":true,"digest":step.action_id})
    })
    .unwrap();
    assert_eq!(called, ["swap", "stake", "order"]);
    assert_eq!(first["status"], "completed");
    let replay = run(&dir, &workflow, |_| panic!("must not submit again")).unwrap();
    assert_eq!(replay["steps"], first["steps"]);
    assert_eq!(replay["replayed"], true);
    std::fs::remove_dir_all(dir).unwrap();
}
#[test]
fn refusal_preserves_success_and_stops_before_order() {
    let dir = directory();
    let mut called = vec![];
    let report = run(&dir, &definition(), |step| {
        called.push(step.action_id.clone());
        if step.action_id == "stake" {
            json!({"error":"per_tx"})
        } else {
            json!({"submitted":true,"digest":"confirmed-swap"})
        }
    })
    .unwrap();
    assert_eq!(called, ["swap", "stake"]);
    assert_eq!(report["status"], "stopped");
    assert_eq!(report["steps"][0]["result"]["digest"], "confirmed-swap");
    run(&dir, &definition(), |_| {
        panic!("stopped workflows cannot restart")
    })
    .unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}
#[test]
fn interrupted_claim_fails_closed_and_definition_changes_are_refused() {
    let dir = directory();
    let workflow = definition();
    let _ = std::panic::catch_unwind(|| run(&dir, &workflow, |_| panic!("process interruption")));
    let report = run(&dir, &workflow, |_| panic!("uncertain step cannot rerun")).unwrap();
    assert_eq!(report["status"], "interrupted");
    let mut changed = definition();
    changed.steps[0].revision = 7;
    assert!(run(&dir, &changed, |_| json!({})).is_err());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_lost_submit_response_stops_the_workflow_and_is_never_retried() {
    let dir = directory();
    let report = run(
        &dir,
        &definition(),
        |_| json!({"error":"submit_failed","reason":"response lost"}),
    )
    .unwrap();
    assert_eq!(report["steps"].as_array().unwrap().len(), 1);
    assert_eq!(report["status"], "stopped");
    run(&dir, &definition(), |_| {
        panic!("unknown submission must not retry")
    })
    .unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn simultaneous_calls_claim_a_run_only_once() {
    let dir = directory();
    let workflow = definition();
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    std::thread::scope(|scope| {
        for _ in 0..2 {
            let count = &count;
            let dir = &dir;
            let workflow = &workflow;
            scope.spawn(move || {
                // A racing reader may see an incomplete claim and fail closed.
                let _ = run(dir, workflow, |_| {
                    count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    json!({"submitted":true,"digest":"confirmed"})
                });
            });
        }
    });
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 3);
    std::fs::remove_dir_all(dir).unwrap();
}
#[test]
fn malformed_identifiers_and_duplicate_vaults_are_refused_before_execution() {
    let dir = directory();
    let mut workflow = definition();
    workflow.run_id = "../escape".into();
    assert!(run(&dir, &workflow, |_| panic!("invalid input cannot execute")).is_err());
    workflow.run_id = "valid".into();
    workflow.steps[1].wallet_id = workflow.steps[0].wallet_id.clone();
    assert!(run(&dir, &workflow, |_| panic!(
        "duplicate vault cannot execute"
    ))
    .is_err());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn gas_fence_rejects_equal_and_older_versions_but_accepts_a_fresh_reference() {
    use sui_sdk_types::{Address, Digest, ObjectReference};
    let address: Address = "0x1".parse().unwrap();
    let consumed = std::collections::HashMap::from([(address, 17)]);
    for version in [16, 17] {
        let object = ObjectReference::new(address, version, Digest::ZERO);
        assert!(rill_cli::workflow::gas_is_stale(&[object], &consumed));
    }
    let fresh = ObjectReference::new(address, 18, Digest::ZERO);
    assert!(!rill_cli::workflow::gas_is_stale(&[fresh], &consumed));
}
