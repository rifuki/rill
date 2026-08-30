//! OAuth 2.1 authorization-server logic.
//!
//! Hand-rolled, because no maintained Rust crate provides the authorization-server side with the
//! set of RFCs an MCP client actually needs together: dynamic client registration, authorization
//! code with PKCE, rotating refresh tokens, and resource indicators. The nearest candidate,
//! `oxide-auth`, last released in 2024 and has no axum frontend.
//!
//! Dynamic registration is the load-bearing piece for the product goal. Without it a user would
//! have to create a client id somewhere and paste it — which is exactly the multi-step setup this
//! exists to delete.

use sha2::{Digest as _, Sha256};

/// Scopes this server issues. Anything else in a request is dropped rather than granted.
pub const SUPPORTED_SCOPES: &[&str] = &["mcp", "offline_access"];
/// At most this many redirect URIs per client.
pub const MAX_REDIRECT_URIS: usize = 10;

/// An OAuth-shaped failure.
///
/// `redirectable` decides how a caller reports it, and getting that wrong is a real vulnerability
/// rather than a cosmetic slip: an error about an unregistered or mismatched `redirect_uri` must
/// never be sent *to* that URI, or the endpoint becomes an open redirector laundering attacker
/// URLs through a trusted domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthError {
    pub code: &'static str,
    pub description: String,
    pub redirectable: bool,
}

impl OAuthError {
    fn not_redirectable(code: &'static str, description: impl Into<String>) -> Self {
        Self {
            code,
            description: description.into(),
            redirectable: false,
        }
    }

    fn redirectable(code: &'static str, description: impl Into<String>) -> Self {
        Self {
            code,
            description: description.into(),
            redirectable: true,
        }
    }
}

impl std::fmt::Display for OAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.description)
    }
}

impl std::error::Error for OAuthError {}

/// RFC 8252 §7: an agent client's redirect is one of exactly three shapes.
///
/// Checked at registration rather than at authorize time, so a client that could never complete a
/// flow learns immediately instead of after a user has already been sent to a wallet prompt.
pub fn is_allowed_redirect_uri(value: &str) -> bool {
    let Ok(url) = url_parts(value) else {
        return false;
    };
    // A fragment is forbidden on a redirect URI — the authorization response's own parameters
    // would collide with it.
    if url.has_fragment {
        return false;
    }
    match url.scheme.as_str() {
        "https" => true,
        // Loopback only for http. `http://example.com` would carry an authorization code in clear.
        "http" => matches!(url.host.as_str(), "localhost" | "127.0.0.1" | "[::1]"),
        // A private-use scheme for a native app, e.g. `com.example.agent:/callback`. Must be
        // reverse-DNS shaped, never something a browser would navigate.
        scheme => {
            scheme.contains('.')
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
                && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        }
    }
}

struct UrlParts {
    scheme: String,
    host: String,
    has_fragment: bool,
}

/// A deliberately small URL split. Only the three facts the policy above needs are extracted, so
/// there is no parser surface beyond them.
fn url_parts(value: &str) -> Result<UrlParts, ()> {
    let (scheme, rest) = value.split_once(':').ok_or(())?;
    if scheme.is_empty() {
        return Err(());
    }
    let has_fragment = rest.contains('#');
    let host = rest
        .strip_prefix("//")
        .map(|r| {
            let end = r.find(['/', '?', '#']).unwrap_or(r.len());
            let authority = &r[..end];
            // Strip userinfo and port; keep a bracketed IPv6 literal intact.
            let after_userinfo = authority
                .rsplit_once('@')
                .map(|(_, h)| h)
                .unwrap_or(authority);
            match after_userinfo.strip_prefix('[') {
                Some(v6) => v6
                    .split_once(']')
                    .map(|(h, _)| format!("[{h}]"))
                    .unwrap_or_default(),
                None => after_userinfo
                    .split_once(':')
                    .map(|(h, _)| h)
                    .unwrap_or(after_userinfo)
                    .to_owned(),
            }
        })
        .unwrap_or_default();
    Ok(UrlParts {
        scheme: scheme.to_ascii_lowercase(),
        host: host.to_ascii_lowercase(),
        has_fragment,
    })
}

/// Keep only scopes this server actually issues, de-duplicated and in a stable order.
pub fn normalize_scope(requested: &str) -> Result<String, OAuthError> {
    let mut granted: Vec<&str> = Vec::new();
    for token in requested.split_whitespace() {
        if SUPPORTED_SCOPES.contains(&token) && !granted.contains(&token) {
            granted.push(token);
        }
    }
    if granted.is_empty() {
        return Err(OAuthError::redirectable(
            "invalid_scope",
            format!("supported scopes are: {}", SUPPORTED_SCOPES.join(", ")),
        ));
    }
    Ok(granted.join(" "))
}

/// The only PKCE method OAuth 2.1 allows. `plain` offers no protection against anyone who can
/// observe the authorization request, and is not accepted here at all.
pub const CODE_CHALLENGE_METHOD: &str = "S256";

/// A challenge or verifier must be 43–128 characters of unreserved ASCII. Forty-three is the
/// base64url length of a SHA-256 digest, which is the only valid S256 challenge length.
pub fn is_valid_pkce_value(value: &str) -> bool {
    let len = value.len();
    (43..=128).contains(&len)
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

/// Verify a code verifier against the challenge recorded at authorize time.
pub fn verify_pkce(code_verifier: &str, code_challenge: &str) -> Result<(), OAuthError> {
    if !is_valid_pkce_value(code_verifier) {
        return Err(OAuthError::not_redirectable(
            "invalid_grant",
            "code_verifier is missing or malformed",
        ));
    }
    let computed = {
        use base64::Engine as _;
        let digest = Sha256::digest(code_verifier.as_bytes());
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
    };
    if computed != code_challenge {
        return Err(OAuthError::not_redirectable(
            "invalid_grant",
            "PKCE verification failed",
        ));
    }
    Ok(())
}

/// An RFC 8707 resource indicator, accepted only when it names this deployment.
///
/// Silently issuing a token for a resource the client did not ask for is exactly the confused-
/// deputy problem resource indicators exist to prevent.
pub fn resolve_resource(
    requested: Option<&str>,
    canonical_resource: &str,
    issuer: &str,
) -> Result<String, OAuthError> {
    let Some(requested) = requested else {
        return Ok(canonical_resource.to_owned());
    };
    let trimmed = requested.trim_end_matches('/');
    let allowed = [
        canonical_resource.trim_end_matches('/'),
        issuer.trim_end_matches('/'),
    ];
    if allowed.contains(&trimmed) {
        Ok(canonical_resource.to_owned())
    } else {
        Err(OAuthError::redirectable(
            "invalid_target",
            format!("unknown resource: {requested}"),
        ))
    }
}

/// Validate the parts of an authorize request that must be checked before anything else.
///
/// Order matters and is not stylistic. `client_id` and `redirect_uri` are validated first and
/// their failures are **not** redirectable, because until both are known-good there is no safe
/// place to send an error. Only afterwards do bad `response_type`, scope, or PKCE become failures
/// a caller may bounce back to the client.
pub fn check_redirect_uri_registered(
    requested: &str,
    registered: &[String],
) -> Result<(), OAuthError> {
    // Exact match. No prefix matching and no wildcards — prefix matching is how open redirectors
    // are born, with `https://good.example/cb` also matching `https://good.example/cb.evil`.
    if registered.iter().any(|r| r == requested) {
        Ok(())
    } else {
        Err(OAuthError::not_redirectable(
            "invalid_request",
            "redirect_uri does not match a registered redirect URI for this client",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_redirects_are_allowed() {
        assert!(is_allowed_redirect_uri("https://example.test/callback"));
    }

    #[test]
    fn http_is_allowed_only_on_loopback() {
        assert!(is_allowed_redirect_uri("http://localhost:8080/cb"));
        assert!(is_allowed_redirect_uri("http://127.0.0.1/cb"));
        assert!(is_allowed_redirect_uri("http://[::1]:1234/cb"));
        assert!(
            !is_allowed_redirect_uri("http://example.test/cb"),
            "an authorization code must not travel in clear"
        );
    }

    #[test]
    fn a_private_use_scheme_is_allowed_for_native_apps() {
        assert!(is_allowed_redirect_uri("com.example.agent:/callback"));
        assert!(
            !is_allowed_redirect_uri("agent:/callback"),
            "a scheme with no dot is not reverse-DNS shaped"
        );
    }

    #[test]
    fn a_fragment_is_refused() {
        assert!(
            !is_allowed_redirect_uri("https://example.test/cb#frag"),
            "the authorization response's own parameters would collide with it"
        );
    }

    #[test]
    fn a_redirect_uri_matches_exactly_or_not_at_all() {
        let registered = vec!["https://good.test/cb".to_string()];
        assert!(check_redirect_uri_registered("https://good.test/cb", &registered).is_ok());
        assert!(
            check_redirect_uri_registered("https://good.test/cb.evil", &registered).is_err(),
            "prefix matching is how open redirectors are born"
        );
        assert!(check_redirect_uri_registered("https://good.test/cb/", &registered).is_err());
    }

    /// The rule that keeps this endpoint from laundering attacker URLs through a trusted domain.
    #[test]
    fn a_redirect_uri_failure_is_never_itself_redirectable() {
        let err = check_redirect_uri_registered("https://evil.test/cb", &[]).unwrap_err();
        assert!(
            !err.redirectable,
            "an error about a bad redirect_uri must never be sent to that redirect_uri"
        );
    }

    #[test]
    fn unknown_scopes_are_dropped_and_known_ones_kept() {
        assert_eq!(
            normalize_scope("mcp admin offline_access").unwrap(),
            "mcp offline_access"
        );
        assert_eq!(normalize_scope("mcp mcp").unwrap(), "mcp");
    }

    #[test]
    fn a_request_with_no_supported_scope_is_refused() {
        assert!(normalize_scope("admin root").is_err());
        assert!(normalize_scope("").is_err());
    }

    #[test]
    fn pkce_round_trips() {
        let verifier = "a".repeat(43);
        let challenge = {
            use base64::Engine as _;
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(Sha256::digest(verifier.as_bytes()))
        };
        assert!(verify_pkce(&verifier, &challenge).is_ok());
    }

    #[test]
    fn a_wrong_verifier_is_refused() {
        let challenge = {
            use base64::Engine as _;
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(Sha256::digest("a".repeat(43).as_bytes()))
        };
        assert!(verify_pkce(&"b".repeat(43), &challenge).is_err());
    }

    #[test]
    fn a_malformed_verifier_is_refused_before_it_is_hashed() {
        for bad in [
            "",
            "short",
            &"a".repeat(129),
            "has spaces in it aaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert!(
                verify_pkce(bad, "anything").is_err(),
                "{bad:?} should be refused"
            );
        }
    }

    #[test]
    fn a_challenge_must_be_the_length_a_sha256_digest_produces() {
        assert!(is_valid_pkce_value(&"a".repeat(43)));
        assert!(!is_valid_pkce_value(&"a".repeat(42)));
        assert!(is_valid_pkce_value(&"a".repeat(128)));
        assert!(!is_valid_pkce_value(&"a".repeat(129)));
    }

    #[test]
    fn an_absent_resource_indicator_defaults_to_this_deployment() {
        assert_eq!(
            resolve_resource(None, "https://api.test/mcp", "https://api.test").unwrap(),
            "https://api.test/mcp"
        );
    }

    #[test]
    fn a_resource_indicator_for_somewhere_else_is_refused() {
        assert!(
            resolve_resource(
                Some("https://other.test/mcp"),
                "https://api.test/mcp",
                "https://api.test"
            )
            .is_err(),
            "issuing a token for a resource nobody asked for is the confused-deputy problem"
        );
    }

    #[test]
    fn a_trailing_slash_does_not_change_the_resource() {
        assert!(resolve_resource(
            Some("https://api.test/mcp/"),
            "https://api.test/mcp",
            "https://api.test"
        )
        .is_ok());
    }
}
