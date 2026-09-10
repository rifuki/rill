//! The third token kind: the credential a deployed agent can actually use.
//!
//! R8. An agent with no browser needs a bearer that outlives an hour, and the only long-lived thing
//! this server used to issue was a refresh token, which `/mcp` refuses by design. A long-lived
//! *access* token was not an option either: nothing checks a stateless HMAC against a store, so
//! `/oauth/revoke` would have answered `{"revoked": true}` and revoked nothing, and a leaked
//! environment variable would have stayed valid for its whole lifetime.
//!
//! What is provable in this crate is the signed half: the kind is inside the MAC, it is
//! audience-bound exactly as an access token is, and its scope can never be wider than the build
//! surface. The stateful half is not provable here and is not meant to be: `rill-auth` has no store
//! and no I/O. `crates/rill-store/tests/file_store.rs` covers the handle, and
//! `bins/rill-server/tests/http_contract.rs` covers the two together, which is where a revoke has
//! to stop the next call.

use rill_auth::oauth::{is_build_surface_only, narrow_to_build_surface, AGENT_TOKEN_SCOPES};
use rill_auth::tokens::{
    secret_matches, sign_token, verify_bearer, verify_token, Expectation, TokenClaims, TokenError,
    TokenKind, BEARER_KINDS,
};

const SECRET: &str = "a-test-secret";
const AUD: &str = "https://api.rill.test/mcp";
const NOW: u64 = 1_757_000_000;
/// Ninety days, the agent credential's lifetime.
const AGENT_TTL: u64 = 90 * 24 * 60 * 60;

fn claims(kind: TokenKind, aud: &str, scope: &str) -> TokenClaims {
    TokenClaims {
        t: kind,
        sub: "0xb649a075e07c7cf0baebeaa82150416218c63943e2e767fe93a24aa5c7ce64a9".into(),
        cid: "rill-agent-credential".into(),
        scope: scope.into(),
        aud: aud.into(),
        exp: NOW + AGENT_TTL,
        jti: rill_auth::tokens::random_id(),
    }
}

fn bearer(token: &str) -> Result<TokenClaims, TokenError> {
    verify_bearer(token, SECRET, AUD, NOW)
}

#[test]
fn an_agent_credential_is_accepted_as_a_bearer() {
    let token = sign_token(&claims(TokenKind::Agent, AUD, "mcp"), SECRET).unwrap();
    let back = bearer(&token).unwrap();
    assert_eq!(back.t, TokenKind::Agent);
    assert_eq!(back.scope, "mcp");
}

#[test]
fn an_access_token_is_still_accepted_as_a_bearer() {
    let token = sign_token(&claims(TokenKind::Access, AUD, "mcp"), SECRET).unwrap();
    assert_eq!(bearer(&token).unwrap().t, TokenKind::Access);
}

/// The property the signed `t` claim exists for, at the endpoint that matters. Adding a third kind
/// must not have widened the set of things a bearer may be.
#[test]
fn a_refresh_token_is_still_not_a_bearer() {
    let token = sign_token(&claims(TokenKind::Refresh, AUD, "mcp"), SECRET).unwrap();
    assert!(matches!(
        bearer(&token),
        Err(TokenError::NotABearerToken {
            found: TokenKind::Refresh
        })
    ));
}

/// The refusal has to say what to do about it, because the caller holding a refresh token is not
/// doing anything malicious: they are one exchange away from a token that works.
#[test]
fn the_refusal_tells_a_refresh_holder_what_to_do_instead() {
    let message = TokenError::NotABearerToken {
        found: TokenKind::Refresh,
    }
    .to_string();
    assert!(message.contains("token endpoint"), "{message}");
    assert!(message.contains("agent credential"), "{message}");
}

/// Two kinds, named. A new kind reaching the bearer path must be a deliberate edit to this list
/// rather than a side effect of adding a variant.
#[test]
fn exactly_two_kinds_may_be_presented_as_a_bearer() {
    assert_eq!(BEARER_KINDS, &[TokenKind::Access, TokenKind::Agent]);
}

/// RFC 8707 resource binding, which a long-lived credential needs more than a one-hour token does:
/// it is the thing that keeps a 90-day bearer from being replayable against a second deployment
/// that happens to share the secret.
#[test]
fn an_agent_credential_for_another_deployment_is_refused() {
    let token = sign_token(
        &claims(TokenKind::Agent, "https://other.test/mcp", "mcp"),
        SECRET,
    )
    .unwrap();
    assert!(matches!(
        bearer(&token),
        Err(TokenError::WrongAudience { .. })
    ));
}

#[test]
fn an_agent_credential_signed_with_another_secret_is_refused() {
    let token = sign_token(&claims(TokenKind::Agent, AUD, "mcp"), "someone-elses").unwrap();
    assert!(matches!(bearer(&token), Err(TokenError::BadSignature)));
}

#[test]
fn an_expired_agent_credential_is_refused() {
    let mut c = claims(TokenKind::Agent, AUD, "mcp");
    c.exp = NOW - 1;
    let token = sign_token(&c, SECRET).unwrap();
    assert!(matches!(bearer(&token), Err(TokenError::Expired)));
}

/// The kind is inside the MAC, so the payload cannot be edited from one kind into another.
#[test]
fn an_access_token_cannot_be_edited_into_an_agent_credential() {
    use base64::Engine as _;
    let token = sign_token(&claims(TokenKind::Access, AUD, "mcp"), SECRET).unwrap();
    let parts: Vec<&str> = token.split('.').collect();
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .unwrap();
    let mut forged: TokenClaims = serde_json::from_slice(&raw).unwrap();
    forged.t = TokenKind::Agent;
    forged.exp = NOW + AGENT_TTL * 4;
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_string(&forged).unwrap());
    let tampered = format!("{}.{}.{}", parts[0], payload, parts[2]);
    assert!(matches!(bearer(&tampered), Err(TokenError::BadSignature)));
}

/// The wire spelling of the kind is part of the format. Renaming it would invalidate every
/// credential already sitting in somebody's environment variable, and the failure would read as
/// "the server stopped accepting my token" with nothing to point at.
#[test]
fn the_agent_kind_is_spelled_agent_on_the_wire() {
    use base64::Engine as _;
    let token = sign_token(&claims(TokenKind::Agent, AUD, "mcp"), SECRET).unwrap();
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(token.split('.').nth(1).unwrap())
        .unwrap();
    let json = String::from_utf8(payload).unwrap();
    assert!(json.contains("\"t\":\"agent\""), "{json}");
}

/// Named so the division of labour is not mistaken for a hole: this crate cannot know a credential
/// was revoked, because revocation is a store fact and there is no store here. The refusal happens
/// at the protected resource, which checks the handle on every request.
#[test]
fn verifying_the_signature_is_only_half_the_check_for_an_agent_credential() {
    let token = sign_token(&claims(TokenKind::Agent, AUD, "mcp"), SECRET).unwrap();
    let back = bearer(&token).unwrap();
    assert_eq!(
        back.t,
        TokenKind::Agent,
        "a revoked credential still verifies here; the store is what refuses it"
    );
    assert!(
        !back.jti.is_empty(),
        "the jti is the handle the store is looked up by, so an empty one would make the \
         credential unrevocable"
    );
}

/// An agent credential must never reach further than an access token with the same scope, and the
/// one scope it may carry is the build surface.
#[test]
fn the_build_surface_is_the_only_scope_an_agent_credential_may_carry() {
    assert_eq!(AGENT_TOKEN_SCOPES, &["mcp"]);
    assert_eq!(narrow_to_build_surface("mcp").unwrap(), "mcp");
    assert_eq!(
        narrow_to_build_surface("mcp mcp").unwrap(),
        "mcp",
        "duplicates collapse rather than producing a scope string nothing matches"
    );
}

/// Dropping the scope silently would hand back a working credential to an operator who asked for
/// more than the build surface, and they would believe their deployed agent could change its own
/// limits.
#[test]
fn asking_for_anything_beyond_the_build_surface_is_refused_by_name() {
    let e = narrow_to_build_surface("mcp offline_access").unwrap_err();
    assert_eq!(e.code, "invalid_scope");
    assert!(
        e.description.contains("offline_access"),
        "the refusal must name the scope that was refused: {}",
        e.description
    );
    assert!(
        e.description.contains("build surface"),
        "and what it may reach instead: {}",
        e.description
    );

    let owner_side = narrow_to_build_surface("owner").unwrap_err();
    assert!(
        owner_side.description.contains("interactive"),
        "an owner-side request must be told where owner-side access comes from: {}",
        owner_side.description
    );
}

#[test]
fn an_empty_scope_is_refused_rather_than_treated_as_unlimited() {
    assert!(narrow_to_build_surface("   ").is_err());
    assert!(!is_build_surface_only(""));
    assert!(!is_build_surface_only("   "));
}

/// The check the protected resource runs on every request, so a credential minted by some future
/// path with a wider scope is still refused where it is used.
#[test]
fn a_scope_outside_the_build_surface_fails_the_check_at_the_resource() {
    assert!(is_build_surface_only("mcp"));
    assert!(!is_build_surface_only("mcp offline_access"));
    assert!(!is_build_surface_only("owner"));
    assert!(!is_build_surface_only("mcp owner"));
}

/// The owner secret is compared in constant time, and an unconfigured deployment cannot be
/// authenticated against by sending nothing.
#[test]
fn an_unset_owner_secret_matches_nothing_at_all() {
    assert!(!secret_matches("", ""));
    assert!(!secret_matches("anything", ""));
    assert!(secret_matches("s3cret", "s3cret"));
    assert!(!secret_matches("s3cret", "s3cre"));
    assert!(!secret_matches("s3cre", "s3cret"));
}

/// An agent credential is not a refresh token and cannot be rotated into a new access token: the
/// refresh grant states the kind it expects, and the MAC settles the rest.
#[test]
fn an_agent_credential_cannot_be_spent_as_a_refresh_token() {
    let token = sign_token(&claims(TokenKind::Agent, AUD, "mcp"), SECRET).unwrap();
    let as_refresh = verify_token(
        &token,
        SECRET,
        Expectation {
            kind: TokenKind::Refresh,
            audience: AUD,
            now_secs: NOW,
        },
    );
    assert!(matches!(as_refresh, Err(TokenError::WrongKind { .. })));
}
