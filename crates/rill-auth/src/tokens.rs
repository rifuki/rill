//! Compact, stateless, HMAC-signed bearer tokens.
//!
//! Deliberately not a JWT library. An OAuth access token is opaque to the client by spec — the
//! client never parses it — so the only consumer of this format is this server. A short
//! hand-rolled format keeps the auth surface auditable in one file and adds no dependency that
//! could ship an `alg: none` class of bug.
//!
//! Two properties this module exists to guarantee:
//!
//! **Type separation is signed, not inferred.** The token kind is inside the MAC, and verification
//! requires the caller to state which kind it expects. A refresh token replayed at the MCP
//! endpoint as a bearer fails closed, even though both were signed with the same secret.
//!
//! **Audience is signed.** A token minted for one deployment cannot be replayed against another
//! that happens to share a secret. That is RFC 8707 resource binding, and the MCP authorization
//! spec requires it.
//!
//! # One kind here is deliberately not self-sufficient
//!
//! [`TokenKind::Agent`] is the long-lived credential a deployed agent carries in an environment
//! variable. Everything in this file verifies it exactly as it verifies an access token, and that
//! is not enough on its own: a signature and an expiry cannot be taken back, so a leaked
//! 90-day bearer would stay valid for 90 days. The caller must also find a live handle for its
//! `jti` in the store on **every** request. [`verify_bearer`] says so at the call site, because the
//! failure it prevents is silent: an operator revokes a credential, reads `{"revoked": true}`, and
//! the agent keeps building until expiry.

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Wire prefix. Bump only on a breaking payload change — a `v2.` token then fails verification
/// here rather than being misread as a `v1.` one.
const TOKEN_VERSION: &str = "v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenKind {
    Access,
    Refresh,
    /// The long-lived credential a deployed agent carries in an environment variable, because
    /// seven of eleven agent frameworks accept only a static bearer and the Claude Agent SDK will
    /// not open a browser.
    ///
    /// Stateful, unlike the other two: the store holds a handle for its `jti`, checked on every
    /// request, so a revoke stops the next call instead of taking effect at expiry.
    Agent,
}

/// The kinds that may be presented as a bearer at a protected resource.
///
/// A list rather than a pair of `||` comparisons at each call site: adding a kind must be one edit
/// here, not a hunt for every endpoint that forgot to include it, and forgetting one the other way
/// would quietly accept a refresh token as a bearer.
pub const BEARER_KINDS: &[TokenKind] = &[TokenKind::Access, TokenKind::Agent];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenClaims {
    /// Token kind — signed, so it can never be reinterpreted as the other.
    pub t: TokenKind,
    /// Subject: the authenticated Sui address, which is the identity Rill scopes skills by.
    pub sub: String,
    /// The OAuth client this was issued to.
    pub cid: String,
    /// Space-delimited granted scopes.
    pub scope: String,
    /// Audience — the protected resource URL this token is valid for.
    pub aud: String,
    /// Expiry, seconds since epoch.
    pub exp: u64,
    /// Unique id. For a refresh token this is the rotation handle the store revokes.
    pub jti: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenError {
    NoSecret,
    Missing,
    Malformed,
    UnsupportedVersion(String),
    BadSignature,
    /// The token is valid, but it is not the kind the caller asked for.
    WrongKind {
        expected: TokenKind,
        found: TokenKind,
    },
    /// The token is valid and is not a kind that may be presented as a bearer at all.
    NotABearerToken {
        found: TokenKind,
    },
    /// The token was minted for a different resource.
    WrongAudience {
        expected: String,
        found: String,
    },
    Expired,
}

impl std::fmt::Display for TokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSecret => write!(f, "no signing secret is configured; refusing to verify"),
            Self::Missing => write!(f, "no token was presented"),
            Self::Malformed => write!(f, "the token is malformed"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported token version \"{v}\""),
            Self::BadSignature => write!(f, "the token signature is not valid"),
            Self::WrongKind { expected, found } => write!(
                f,
                "expected a {expected:?} token but this is a {found:?} one"
            ),
            Self::NotABearerToken { found } => write!(
                f,
                "a {found:?} token is not a bearer credential; exchange it at the token endpoint \
                 for an access token, or ask the owner for an agent credential"
            ),
            Self::WrongAudience { expected, found } => {
                write!(f, "this token was issued for {found}, not for {expected}")
            }
            Self::Expired => write!(f, "the token has expired"),
        }
    }
}

impl std::error::Error for TokenError {}

fn b64(input: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(input)
}

fn unb64(input: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(input)
        .ok()
}

fn mac_of(payload: &str, secret: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts a key of any length");
    mac.update(format!("{TOKEN_VERSION}.{payload}").as_bytes());
    b64(&mac.finalize().into_bytes())
}

/// A fresh 256-bit random identifier, base64url. Used for `jti` and for opaque codes.
pub fn random_id() -> String {
    use rand::RngCore as _;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    b64(&bytes)
}

pub fn sign_token(claims: &TokenClaims, secret: &str) -> Result<String, TokenError> {
    if secret.is_empty() {
        return Err(TokenError::NoSecret);
    }
    let payload = b64(serde_json::to_string(claims)
        .map_err(|_| TokenError::Malformed)?
        .as_bytes());
    let signature = mac_of(&payload, secret);
    Ok(format!("{TOKEN_VERSION}.{payload}.{signature}"))
}

/// What the caller must state up front.
///
/// Both fields are required rather than optional-with-a-default. An optional audience check is one
/// forgotten argument away from accepting any token this server ever signed.
pub struct Expectation<'a> {
    pub kind: TokenKind,
    pub audience: &'a str,
    pub now_secs: u64,
}

pub fn verify_token(
    token: &str,
    secret: &str,
    expected: Expectation<'_>,
) -> Result<TokenClaims, TokenError> {
    verify_inner(
        token,
        secret,
        expected.audience,
        expected.now_secs,
        |found| {
            if found == expected.kind {
                Ok(())
            } else {
                Err(TokenError::WrongKind {
                    expected: expected.kind,
                    found,
                })
            }
        },
    )
}

/// Verify a token presented as a bearer at a protected resource, accepting any of [`BEARER_KINDS`].
///
/// # This is half of the check for an agent credential
///
/// A [`TokenKind::Agent`] token that passes here is signed, audience-bound and unexpired, and may
/// still have been revoked. The caller must look its `jti` up in the store and refuse when no live
/// handle is found. Nothing in this module can do that: it has no store and, deliberately, no I/O.
pub fn verify_bearer(
    token: &str,
    secret: &str,
    audience: &str,
    now_secs: u64,
) -> Result<TokenClaims, TokenError> {
    verify_inner(token, secret, audience, now_secs, |found| {
        if BEARER_KINDS.contains(&found) {
            Ok(())
        } else {
            Err(TokenError::NotABearerToken { found })
        }
    })
}

/// Everything both public verifiers share, with the kind decision left to the caller.
///
/// One body rather than two, so the signature, audience and expiry checks cannot drift apart, and
/// `gate` runs in exactly the position the kind check always occupied.
fn verify_inner(
    token: &str,
    secret: &str,
    audience: &str,
    now_secs: u64,
    gate: impl FnOnce(TokenKind) -> Result<(), TokenError>,
) -> Result<TokenClaims, TokenError> {
    if secret.is_empty() {
        return Err(TokenError::NoSecret);
    }
    if token.is_empty() {
        return Err(TokenError::Missing);
    }
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(TokenError::Malformed);
    }
    let (version, payload, signature) = (parts[0], parts[1], parts[2]);
    if version != TOKEN_VERSION {
        return Err(TokenError::UnsupportedVersion(version.to_owned()));
    }

    // Constant-time comparison. A length-varying or early-exit compare leaks the expected
    // signature one byte at a time to anyone who can time responses.
    let expected_mac = mac_of(payload, secret);
    if !constant_time_eq(expected_mac.as_bytes(), signature.as_bytes()) {
        return Err(TokenError::BadSignature);
    }

    let claims: TokenClaims = unb64(payload)
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .ok_or(TokenError::Malformed)?;

    if claims.sub.is_empty()
        || claims.cid.is_empty()
        || claims.aud.is_empty()
        || claims.jti.is_empty()
    {
        return Err(TokenError::Malformed);
    }
    gate(claims.t)?;
    if claims.aud != audience {
        return Err(TokenError::WrongAudience {
            expected: audience.to_owned(),
            found: claims.aud.clone(),
        });
    }
    if claims.exp <= now_secs {
        return Err(TokenError::Expired);
    }
    Ok(claims)
}

/// Compare a presented shared secret against a configured one, without an early exit.
///
/// Exposed because the agent-credential grant authenticates the deployment owner with a configured
/// secret rather than with a token, and a plain `==` there hands the secret to anyone who can time
/// the endpoint. An empty configured secret matches nothing, so an unconfigured deployment cannot
/// be authenticated against by sending an empty header.
pub fn secret_matches(presented: &str, configured: &str) -> bool {
    !configured.is_empty() && constant_time_eq(presented.as_bytes(), configured.as_bytes())
}

/// Compare without an early exit. Length is compared first and folded into the result rather than
/// short-circuiting, so a wrong-length token takes the same path as a wrong-value one.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= x ^ y;
    }
    diff == 0
}

/// Pull a bearer token out of an `Authorization` header.
///
/// Returns `None` rather than an error so the caller owns the 401 shape — an MCP client needs the
/// `WWW-Authenticate` discovery header on that response, and only the route layer can build it.
pub fn bearer_from_header(header: Option<&str>) -> Option<&str> {
    let value = header?.trim();
    let rest = value.strip_prefix("Bearer ").or_else(|| {
        // Case-insensitive scheme, per RFC 7235.
        value
            .get(..7)
            .filter(|p| p.eq_ignore_ascii_case("bearer "))
            .map(|_| &value[7..])
    })?;
    let token = rest.trim();
    (!token.is_empty()).then_some(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "a-test-secret";
    const AUD: &str = "https://api.rill.test/mcp";
    const NOW: u64 = 1_756_600_000;

    fn claims(kind: TokenKind, aud: &str) -> TokenClaims {
        TokenClaims {
            t: kind,
            sub: "0xuser".into(),
            cid: "rill_client_x".into(),
            scope: "mcp".into(),
            aud: aud.into(),
            exp: NOW + 3600,
            jti: random_id(),
        }
    }

    fn expect(kind: TokenKind) -> Expectation<'static> {
        Expectation {
            kind,
            audience: AUD,
            now_secs: NOW,
        }
    }

    #[test]
    fn a_freshly_signed_token_verifies() {
        let token = sign_token(&claims(TokenKind::Access, AUD), SECRET).unwrap();
        let back = verify_token(&token, SECRET, expect(TokenKind::Access)).unwrap();
        assert_eq!(back.sub, "0xuser");
    }

    /// The property the signed `t` claim exists for.
    #[test]
    fn a_refresh_token_presented_as_a_bearer_is_refused() {
        let token = sign_token(&claims(TokenKind::Refresh, AUD), SECRET).unwrap();
        assert!(matches!(
            verify_token(&token, SECRET, expect(TokenKind::Access)),
            Err(TokenError::WrongKind { .. })
        ));
    }

    /// RFC 8707 resource binding. Two deployments sharing a secret must not share tokens.
    #[test]
    fn a_token_for_another_resource_is_refused() {
        let token =
            sign_token(&claims(TokenKind::Access, "https://other.test/mcp"), SECRET).unwrap();
        assert!(matches!(
            verify_token(&token, SECRET, expect(TokenKind::Access)),
            Err(TokenError::WrongAudience { .. })
        ));
    }

    #[test]
    fn a_token_signed_with_another_secret_is_refused() {
        let token = sign_token(&claims(TokenKind::Access, AUD), "someone-elses-secret").unwrap();
        assert!(matches!(
            verify_token(&token, SECRET, expect(TokenKind::Access)),
            Err(TokenError::BadSignature)
        ));
    }

    /// The payload is readable but not writable — changing it invalidates the MAC.
    #[test]
    fn editing_the_payload_invalidates_the_token() {
        let token = sign_token(&claims(TokenKind::Access, AUD), SECRET).unwrap();
        let parts: Vec<&str> = token.split('.').collect();
        let mut forged: TokenClaims = serde_json::from_slice(&unb64(parts[1]).unwrap()).unwrap();
        forged.sub = "0xattacker".into();
        let payload = b64(serde_json::to_string(&forged).unwrap().as_bytes());
        let tampered = format!("{}.{}.{}", parts[0], payload, parts[2]);
        assert!(matches!(
            verify_token(&tampered, SECRET, expect(TokenKind::Access)),
            Err(TokenError::BadSignature)
        ));
    }

    #[test]
    fn an_expired_token_is_refused() {
        let mut c = claims(TokenKind::Access, AUD);
        c.exp = NOW - 1;
        let token = sign_token(&c, SECRET).unwrap();
        assert!(matches!(
            verify_token(&token, SECRET, expect(TokenKind::Access)),
            Err(TokenError::Expired)
        ));
    }

    #[test]
    fn a_token_from_a_future_version_is_refused_rather_than_misread() {
        let token = sign_token(&claims(TokenKind::Access, AUD), SECRET).unwrap();
        let bumped = format!("v2{}", &token[2..]);
        assert!(matches!(
            verify_token(&bumped, SECRET, expect(TokenKind::Access)),
            Err(TokenError::UnsupportedVersion(_))
        ));
    }

    #[test]
    fn a_malformed_token_is_refused() {
        for bad in ["", "one-part", "two.parts", "a.b.c.d"] {
            assert!(verify_token(bad, SECRET, expect(TokenKind::Access)).is_err());
        }
    }

    #[test]
    fn signing_without_a_secret_is_refused() {
        assert_eq!(
            sign_token(&claims(TokenKind::Access, AUD), ""),
            Err(TokenError::NoSecret)
        );
    }

    #[test]
    fn every_generated_id_is_distinct() {
        let a = random_id();
        let b = random_id();
        assert_ne!(a, b);
        assert!(a.len() >= 43, "256 bits of entropy, base64url");
    }

    #[test]
    fn a_bearer_header_is_parsed_case_insensitively() {
        assert_eq!(bearer_from_header(Some("Bearer abc")), Some("abc"));
        assert_eq!(bearer_from_header(Some("bearer abc")), Some("abc"));
        assert_eq!(bearer_from_header(Some("  Bearer  abc  ")), Some("abc"));
        assert_eq!(bearer_from_header(Some("Basic abc")), None);
        assert_eq!(bearer_from_header(Some("Bearer ")), None);
        assert_eq!(bearer_from_header(None), None);
    }

    #[test]
    fn the_comparison_does_not_short_circuit_on_length() {
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"abc", b"abc"));
    }
}
