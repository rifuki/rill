//! The file stores, including compatibility with the reference deployment's real data files.

use rill_store::file::{FileOAuthStore, FileSkillStore, MAX_STORED_SKILLS};
use rill_store::{
    AgentHandle, AuthorizationCode, AuthorizationRequest, OAuthClient, OAuthStore, PublishedSkill,
    RefreshHandle, RequestKind, SkillStore, StoreError,
};

const NOW: u64 = 1_756_600_000_000;
/// The reference deployment's data directory. Machine-specific, so it is read from the
/// environment and the tests that use it skip when it is absent — a teammate without that checkout
/// still gets a green suite, and CI does not need the other repo present.
fn reference_data() -> Option<std::path::PathBuf> {
    let dir = std::path::PathBuf::from(std::env::var("RILL_REFERENCE_DATA").unwrap_or_else(|_| {
        "../../../mgodonf/web3/sui/deepsurge/rill/rill-backend/data".to_string()
    }));
    dir.exists().then_some(dir)
}

fn tmp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("rill-store-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn skill(id: &str, owner: Option<&str>, created_at: &str) -> PublishedSkill {
    PublishedSkill {
        id: id.into(),
        name: "an action".into(),
        description: "does a thing".into(),
        flow: serde_json::json!({ "nodes": [], "edges": [] }),
        tool_defs: None,
        policy_id: None,
        owner: owner.map(str::to_owned),
        created_at: created_at.into(),
    }
}

/// R6: the existing deployment's files must load with no migration step.
#[test]
fn the_reference_deployments_skills_file_loads_unchanged() {
    let Some(path) = reference_data()
        .map(|d| d.join("skills.json"))
        .filter(|p| p.exists())
    else {
        eprintln!("reference data not present; skipping");
        return;
    };
    let store = FileSkillStore::load(&path);
    assert!(
        store.count() > 0,
        "the reference file holds skills and they must all parse"
    );
    // Everything in that file predates ownership, so all of it must be visible anonymously.
    assert_eq!(store.list_unowned().len(), store.count());
}

#[test]
fn the_reference_deployments_oauth_file_loads_unchanged() {
    let Some(path) = reference_data()
        .map(|d| d.join("oauth.json"))
        .filter(|p| p.exists())
    else {
        eprintln!("reference data not present; skipping");
        return;
    };
    let store = FileOAuthStore::load(&path, NOW);
    assert!(store.get_client("nobody").is_none());
}

#[test]
fn a_corrupt_file_starts_empty_rather_than_stopping_the_boot() {
    let path = tmp("corrupt.json");
    std::fs::write(&path, "{ this is not json").unwrap();
    let store = FileSkillStore::load(&path);
    assert_eq!(
        store.count(),
        0,
        "re-publishing is recoverable; refusing to boot is not"
    );
}

#[test]
fn a_saved_skill_survives_a_reload() {
    let path = tmp("roundtrip.json");
    let _ = std::fs::remove_file(&path);
    let store = FileSkillStore::load(&path);
    store
        .save(skill("skill_a", Some("0xowner"), "2026-08-30"))
        .unwrap();

    let reloaded = FileSkillStore::load(&path);
    assert_eq!(reloaded.count(), 1);
    assert_eq!(
        reloaded.get("skill_a").unwrap().owner.as_deref(),
        Some("0xowner")
    );
}

/// The authorization boundary. Exact match, nothing else.
#[test]
fn one_owners_catalogue_never_contains_anothers() {
    let path = tmp("owners.json");
    let _ = std::fs::remove_file(&path);
    let store = FileSkillStore::load(&path);
    store
        .save(skill("a", Some("0xalice"), "2026-08-01"))
        .unwrap();
    store.save(skill("b", Some("0xbob"), "2026-08-02")).unwrap();
    store.save(skill("c", None, "2026-08-03")).unwrap();

    let alice: Vec<String> = store
        .list_by_owner("0xalice")
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(alice, vec!["a"]);
    assert!(store.list_by_owner("0xcarol").is_empty());
    assert_eq!(
        store.list_unowned().len(),
        1,
        "an unowned skill belongs to nobody's catalogue"
    );
}

#[test]
fn an_owner_lookup_is_case_and_whitespace_insensitive_but_never_partial() {
    let path = tmp("normalize.json");
    let _ = std::fs::remove_file(&path);
    let store = FileSkillStore::load(&path);
    store
        .save(skill("a", Some("0xAliCe"), "2026-08-01"))
        .unwrap();

    assert_eq!(store.list_by_owner("  0xalice  ").len(), 1);
    assert!(
        store.list_by_owner("0xalic").is_empty(),
        "a prefix is not a match; prefix matching here would be a data leak"
    );
}

#[test]
fn an_empty_address_matches_nothing() {
    let path = tmp("empty-addr.json");
    let _ = std::fs::remove_file(&path);
    let store = FileSkillStore::load(&path);
    store.save(skill("a", None, "2026-08-01")).unwrap();
    assert!(
        store.list_by_owner("").is_empty(),
        "an empty address must not sweep up every unowned skill"
    );
}

#[test]
fn owned_skills_come_back_newest_first() {
    let path = tmp("order.json");
    let _ = std::fs::remove_file(&path);
    let store = FileSkillStore::load(&path);
    store.save(skill("old", Some("0xo"), "2026-08-01")).unwrap();
    store.save(skill("new", Some("0xo"), "2026-08-30")).unwrap();
    let ids: Vec<String> = store
        .list_by_owner("0xo")
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(ids, vec!["new", "old"]);
}

#[test]
fn the_store_refuses_rather_than_evicting_when_full() {
    let path = tmp("capacity.json");
    let _ = std::fs::remove_file(&path);
    let store = FileSkillStore::load(&path);
    for i in 0..MAX_STORED_SKILLS {
        store
            .save(skill(&format!("s{i}"), None, "2026-08-01"))
            .unwrap();
    }
    assert!(matches!(
        store.save(skill("one-too-many", None, "2026-08-01")),
        Err(StoreError::AtCapacity { .. })
    ));
    assert!(
        store.get("s0").is_some(),
        "nobody else's published action is discarded to make room"
    );
}

// ── oauth ──

fn request(id: &str, kind: RequestKind, expires_at: u64) -> AuthorizationRequest {
    AuthorizationRequest {
        request_id: id.into(),
        kind,
        client_id: "c".into(),
        client_name: None,
        redirect_uri: "https://example.test/cb".into(),
        state: None,
        scope: "mcp".into(),
        code_challenge: "x".repeat(43),
        resource: "https://api.test/mcp".into(),
        message: "sign this".into(),
        expires_at,
    }
}

#[test]
fn an_authorization_code_is_single_use() {
    let path = tmp("codes.json");
    let _ = std::fs::remove_file(&path);
    let store = FileOAuthStore::load(&path, NOW);
    store
        .save_code(AuthorizationCode {
            code: "abc".into(),
            client_id: "c".into(),
            redirect_uri: "https://example.test/cb".into(),
            code_challenge: "x".repeat(43),
            sub: "0xuser".into(),
            scope: "mcp".into(),
            resource: "r".into(),
            expires_at: NOW + 60_000,
        })
        .unwrap();

    assert!(store.take_code("abc", NOW).is_some());
    assert!(
        store.take_code("abc", NOW).is_none(),
        "a code that survives its first redemption is replayable by anyone who saw the redirect"
    );
}

#[test]
fn a_refresh_handle_dies_when_it_is_redeemed() {
    let path = tmp("refresh.json");
    let _ = std::fs::remove_file(&path);
    let store = FileOAuthStore::load(&path, NOW);
    let handle = RefreshHandle {
        jti: "j1".into(),
        sub: "0xuser".into(),
        client_id: "c".into(),
        scope: "mcp".into(),
        resource: "r".into(),
        expires_at: NOW + 60_000,
    };
    store.save_refresh(handle).unwrap();
    assert!(store.take_refresh("j1", NOW).is_some());
    assert!(
        store.take_refresh("j1", NOW).is_none(),
        "rotation is what makes a stolen refresh token observable"
    );
}

/// Read-without-consuming, so a mistyped signature does not cost the user the whole flow.
#[test]
fn reading_a_request_leaves_it_usable_but_taking_it_does_not() {
    let path = tmp("requests.json");
    let _ = std::fs::remove_file(&path);
    let store = FileOAuthStore::load(&path, NOW);
    store
        .save_request(request("r1", RequestKind::Agent, NOW + 60_000))
        .unwrap();

    assert!(store.get_request("r1", NOW).is_some());
    assert!(
        store.get_request("r1", NOW).is_some(),
        "reading is not consuming"
    );
    assert!(store.take_request("r1", NOW).is_some());
    assert!(store.take_request("r1", NOW).is_none());
}

#[test]
fn an_expired_record_is_gone_even_though_it_is_still_on_disk() {
    let path = tmp("expired.json");
    let _ = std::fs::remove_file(&path);
    let store = FileOAuthStore::load(&path, NOW);
    store
        .save_request(request("old", RequestKind::Agent, NOW - 1))
        .unwrap();
    assert!(store.get_request("old", NOW).is_none());
}

#[test]
fn expired_records_are_pruned_on_load_so_the_file_cannot_grow_without_bound() {
    let path = tmp("prune.json");
    let _ = std::fs::remove_file(&path);
    {
        let store = FileOAuthStore::load(&path, NOW);
        store
            .save_request(request("stale", RequestKind::Agent, NOW + 1_000))
            .unwrap();
    }
    // Reload well after that request expired.
    let later = FileOAuthStore::load(&path, NOW + 10_000);
    assert!(later.get_request("stale", NOW + 10_000).is_none());
}

#[test]
fn a_studio_request_and_an_agent_request_are_distinguishable() {
    let path = tmp("kinds.json");
    let _ = std::fs::remove_file(&path);
    let store = FileOAuthStore::load(&path, NOW);
    store
        .save_request(request("s", RequestKind::Studio, NOW + 60_000))
        .unwrap();
    assert_eq!(
        store.get_request("s", NOW).unwrap().kind,
        RequestKind::Studio,
        "a signature collected for a studio login must never be redeemable as an agent's code"
    );
}

// ── agent credentials (R8) ──
//
// A long-lived credential is only revocable because of these records. The token itself is a signed
// blob nobody can take back, so a handle that vanishes, or one that a read consumes, is the whole
// difference between `/oauth/revoke` working and `/oauth/revoke` answering 200 and doing nothing.

fn agent(jti: &str, sub: &str, expires_at: u64) -> AgentHandle {
    AgentHandle {
        jti: jti.into(),
        sub: sub.into(),
        client_id: "rill-agent-credential".into(),
        scope: "mcp".into(),
        resource: "https://api.test/mcp".into(),
        issued_at: NOW,
        expires_at,
    }
}

/// The property the whole unit turns on: reading the handle must not spend it, because this read
/// happens on every request the credential makes.
#[test]
fn reading_an_agent_handle_does_not_consume_it() {
    let path = tmp("agents-read.json");
    let _ = std::fs::remove_file(&path);
    let store = FileOAuthStore::load(&path, NOW);
    store
        .save_agent(agent("a", "0xalice", NOW + 60_000))
        .unwrap();

    for call in 1..=3 {
        assert!(
            store.get_agent("a", NOW).is_some(),
            "call {call}: a credential that dies on first use is the same bug as no credential"
        );
    }
}

/// The verification R8 demands: a credential issued once keeps working after a restart. The process
/// holds the handles in memory, so this is the only thing that carries them across one.
#[test]
fn an_agent_credential_survives_a_restart() {
    let path = tmp("agents-restart.json");
    let _ = std::fs::remove_file(&path);
    {
        let store = FileOAuthStore::load(&path, NOW);
        store
            .save_agent(agent("live", "0xalice", NOW + 90 * 24 * 60 * 60 * 1000))
            .unwrap();
    }
    let reloaded = FileOAuthStore::load(&path, NOW);
    assert!(
        reloaded.get_agent("live", NOW).is_some(),
        "otherwise every deployed agent stops working on the next deploy, with a 401 and \
         nothing to explain it"
    );
}

#[test]
fn revoking_an_agent_credential_says_whether_one_actually_died() {
    let path = tmp("agents-revoke.json");
    let _ = std::fs::remove_file(&path);
    let store = FileOAuthStore::load(&path, NOW);
    store
        .save_agent(agent("a", "0xalice", NOW + 60_000))
        .unwrap();

    assert!(store.revoke_agent("a").unwrap(), "a live handle died");
    assert!(store.get_agent("a", NOW).is_none(), "and stays dead");
    assert!(
        !store.revoke_agent("a").unwrap(),
        "a second revoke reports that nothing was there, even though RFC 7009 makes the \
         endpoint answer 200 either way"
    );
}

/// A revocation has to outlive the process too, or a restart resurrects a credential somebody
/// already killed.
#[test]
fn a_revoked_agent_credential_does_not_come_back_after_a_restart() {
    let path = tmp("agents-revoke-restart.json");
    let _ = std::fs::remove_file(&path);
    {
        let store = FileOAuthStore::load(&path, NOW);
        store
            .save_agent(agent("gone", "0xalice", NOW + 60_000))
            .unwrap();
        assert!(store.revoke_agent("gone").unwrap());
    }
    let reloaded = FileOAuthStore::load(&path, NOW);
    assert!(reloaded.get_agent("gone", NOW).is_none());
}

#[test]
fn an_expired_agent_handle_is_neither_returned_nor_kept() {
    let path = tmp("agents-expired.json");
    let _ = std::fs::remove_file(&path);
    {
        let store = FileOAuthStore::load(&path, NOW);
        store.save_agent(agent("old", "0xalice", NOW - 1)).unwrap();
        assert!(store.get_agent("old", NOW).is_none(), "past its expiry");
    }
    // Pruned on load, so an abandoned credential cannot accumulate in the file forever.
    let reloaded = FileOAuthStore::load(&path, NOW);
    assert!(reloaded.get_agent("old", NOW - 10).is_none());
}

/// "Sign me out everywhere" must include the longest-lived credential, or it is the one call an
/// operator reaches for in an incident and the one that leaves the leak open.
#[test]
fn revoking_a_subject_kills_its_agent_credentials_too() {
    let path = tmp("agents-subject.json");
    let _ = std::fs::remove_file(&path);
    let store = FileOAuthStore::load(&path, NOW);
    store
        .save_agent(agent("a1", "0xalice", NOW + 60_000))
        .unwrap();
    store
        .save_agent(agent("b1", "0xbob", NOW + 60_000))
        .unwrap();
    store
        .save_refresh(RefreshHandle {
            jti: "a2".into(),
            sub: "0xalice".into(),
            client_id: "c".into(),
            scope: "mcp".into(),
            resource: "r".into(),
            expires_at: NOW + 60_000,
        })
        .unwrap();

    assert_eq!(
        store.revoke_subject("0xalice").unwrap(),
        2,
        "one agent credential and one refresh handle"
    );
    assert!(store.get_agent("a1", NOW).is_none());
    assert!(
        store.get_agent("b1", NOW).is_some(),
        "bob's credential is not collateral damage"
    );
}

#[test]
fn revoking_a_subject_kills_only_that_subjects_handles() {
    let path = tmp("revoke.json");
    let _ = std::fs::remove_file(&path);
    let store = FileOAuthStore::load(&path, NOW);
    for (jti, sub) in [("a", "0xalice"), ("b", "0xalice"), ("c", "0xbob")] {
        store
            .save_refresh(RefreshHandle {
                jti: jti.into(),
                sub: sub.into(),
                client_id: "c".into(),
                scope: "mcp".into(),
                resource: "r".into(),
                expires_at: NOW + 60_000,
            })
            .unwrap();
    }
    assert_eq!(store.revoke_subject("0xalice").unwrap(), 2);
    assert!(store.take_refresh("c", NOW).is_some(), "bob is unaffected");
}

#[test]
fn a_client_registration_survives_a_restart() {
    let path = tmp("clients.json");
    let _ = std::fs::remove_file(&path);
    {
        let store = FileOAuthStore::load(&path, NOW);
        store
            .save_client(OAuthClient {
                client_id: "rill_client_x".into(),
                client_name: Some("Some Agent".into()),
                redirect_uris: vec!["https://example.test/cb".into()],
                scope: "mcp offline_access".into(),
                created_at: "2026-08-30".into(),
            })
            .unwrap();
    }
    let reloaded = FileOAuthStore::load(&path, NOW);
    assert!(
        reloaded.get_client("rill_client_x").is_some(),
        "otherwise every connected agent silently breaks on the next deploy"
    );
}
