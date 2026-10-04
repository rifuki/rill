use rill_store::pairing::{PairingRequest, PairingStore};
fn request() -> PairingRequest {
    PairingRequest {
        request_id: "one".into(),
        owner: "owner".into(),
        agent: "agent".into(),
        network: "mainnet".into(),
        domain: "https://rill.example".into(),
        nonce: "nonce".into(),
        expires_at: 100,
        proved: false,
    }
}

#[test]
fn one_owner_cannot_fill_the_shared_pending_request_quota() {
    let store = PairingStore::memory();
    for index in 0..10 {
        let mut r = request();
        r.request_id = format!("owner-a-{index}");
        store.prepare(r, 1).unwrap();
    }
    let mut extra = request();
    extra.request_id = "owner-a-extra".into();
    assert!(store.prepare(extra, 1).is_err());
    let mut other = request();
    other.owner = "other-owner".into();
    other.request_id = "owner-b-first".into();
    assert!(store.prepare(other, 1).is_ok());
}
#[test]
fn owner_confirmation_is_single_use_and_requires_proof() {
    let store = PairingStore::memory();
    store.prepare(request(), 1).unwrap();
    assert!(store.confirm("one", "owner", 2).is_err());
    assert!(store.prove("one", "other", 2).is_err());
    store.prove("one", "agent", 2).unwrap();
    assert!(store.confirm("one", "other", 2).is_err());
    store.confirm("one", "owner", 2).unwrap();
    assert!(store.confirm("one", "owner", 2).is_err());
    assert_eq!(store.list("owner").unwrap().len(), 1);
    assert!(store.list("other").unwrap().is_empty());
}
#[test]
fn expired_and_replayed_proofs_fail() {
    let store = PairingStore::memory();
    store.prepare(request(), 1).unwrap();
    assert!(store.prove("one", "agent", 100).is_err());
    store.prove("one", "agent", 2).unwrap();
    assert!(store.prove("one", "agent", 3).is_err());
}
#[test]
fn message_binds_every_authority_field() {
    let request = request();
    let message = request.message();
    for field in [
        "owner",
        "agent",
        "mainnet",
        "https://rill.example",
        "nonce",
        "one",
        "100",
    ] {
        assert!(message.contains(field));
    }
}
#[test]
fn confirmed_pairing_survives_restart_and_request_stays_consumed() {
    let path = std::env::temp_dir().join(format!("rill-pairing-test-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let store = PairingStore::load(&path).unwrap();
    store.prepare(request(), 1).unwrap();
    store.prove("one", "agent", 2).unwrap();
    store.confirm("one", "owner", 3).unwrap();
    drop(store);
    let store = PairingStore::load(&path).unwrap();
    assert_eq!(store.list("owner").unwrap()[0].paired_at, 3);
    assert!(store.confirm("one", "owner", 4).is_err());
    std::fs::remove_file(path).unwrap();
}
#[test]
fn simultaneous_confirmation_has_exactly_one_winner() {
    let store = std::sync::Arc::new(PairingStore::memory());
    store.prepare(request(), 1).unwrap();
    store.prove("one", "agent", 2).unwrap();
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let store = store.clone();
            std::thread::spawn(move || store.confirm("one", "owner", 3).is_ok())
        })
        .collect();
    assert_eq!(
        workers
            .into_iter()
            .map(|w| usize::from(w.join().unwrap()))
            .sum::<usize>(),
        1
    );
}
