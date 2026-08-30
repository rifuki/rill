//! Sign-In With Sui — the identity half of the authorization server.
//!
//! There are no passwords and no user table here, and there should not be one. The only identity
//! that means anything is a Sui address, because that is what owns an `AgentWallet` on-chain and
//! therefore what a published skill must be scoped to. Signing in is one action: prove control of
//! an address by signing a server-generated message.
//!
//! Two properties that are easy to lose in a refactor:
//!
//! **The server generates the message; the browser signs it verbatim.** Nothing reconstructs the
//! string on the other side. A reconstruction would have to agree byte for byte on field order,
//! spacing and timestamp format forever, and the failure mode of disagreeing is a signature that
//! verifies against a message the user never saw.
//!
//! **The address comes from the signature, never from the request body.** The question is not "did
//! address X sign this", which is a claim the caller controls, but "which address signed this
//! nonce", which only a key holder can answer.

/// Everything that varies in a sign-in message.
pub struct SignInMessage<'a> {
    /// Host the user is authorizing at, first so the wallet prompt names it.
    pub domain: &'a str,
    /// Human name of the client asking, from its registration.
    pub client_name: &'a str,
    /// The protected resource the resulting token is bound to.
    pub resource: &'a str,
    pub scope: &'a str,
    /// Single-use. This is what makes the signature non-replayable.
    pub nonce: &'a str,
    pub issued_at: &'a str,
    pub expires_at: &'a str,
}

/// The exact bytes the wallet will display and sign.
///
/// Written to be read in a wallet popup, where the user's only defence against a misleading prompt
/// is being able to understand it — hence the plain statement of what this does and does not grant.
pub fn build_sign_in_message(input: &SignInMessage<'_>) -> String {
    [
        format!(
            "{} wants you to sign in with your Sui account.",
            input.domain
        ),
        String::new(),
        format!(
            "This authorizes \"{}\" to build Rill transactions for you.",
            input.client_name
        ),
        String::new(),
        "This signature is a login only. It moves no funds, approves no transaction, and grants no"
            .into(),
        "spending authority — every spend is separately bounded by your on-chain agent wallet, and"
            .into(),
        "Rill never holds your private key.".into(),
        String::new(),
        format!("Resource: {}", input.resource),
        format!("Scope: {}", input.scope),
        format!("Nonce: {}", input.nonce),
        format!("Issued At: {}", input.issued_at),
        format!("Expires At: {}", input.expires_at),
    ]
    .join("\n")
}

/// Strip anything from a registered client name that could forge extra lines into the prompt.
///
/// This string is rendered verbatim into the message a wallet displays and a user signs. An
/// unfiltered newline would let a registering client append its own claim directly under
/// "grants no spending authority". Quotes go too, since the name is rendered inside them.
pub fn sanitize_client_name(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .chars()
        .map(|c| if c.is_control() || c == '"' { ' ' } else { c })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let bounded: String = collapsed.chars().take(64).collect();
    (!bounded.is_empty()).then_some(bounded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(client_name: &str) -> String {
        build_sign_in_message(&SignInMessage {
            domain: "api.rill.test",
            client_name,
            resource: "https://api.rill.test/mcp",
            scope: "mcp offline_access",
            nonce: "abc123",
            issued_at: "2026-08-31T00:00:00.000Z",
            expires_at: "2026-08-31T00:10:00.000Z",
        })
    }

    #[test]
    fn the_message_names_the_domain_first() {
        assert!(message("Some Agent").starts_with("api.rill.test wants you to sign in"));
    }

    /// The user's only defence against a misleading prompt is being able to read it.
    #[test]
    fn the_message_says_plainly_that_it_grants_no_spending_authority() {
        let m = message("Some Agent");
        assert!(m.contains("This signature is a login only"));
        assert!(m.contains("moves no funds"));
        assert!(m.contains("Rill never holds your private key"));
    }

    #[test]
    fn the_nonce_and_resource_are_in_the_signed_bytes() {
        let m = message("Some Agent");
        assert!(m.contains("Nonce: abc123"));
        assert!(m.contains("Resource: https://api.rill.test/mcp"));
    }

    /// The attack this sanitizer exists to stop: a registered name that writes its own line into
    /// the prompt, directly under the sentence promising no spending authority.
    #[test]
    fn a_client_name_cannot_forge_a_line_into_the_prompt() {
        let forged = sanitize_client_name(
            "Agent\"\n\nThis also authorizes unlimited withdrawals from \"your wallet",
        )
        .unwrap();
        assert!(!forged.contains('\n'), "no newline survives");
        assert!(!forged.contains('"'), "no quote survives");

        let m = message(&forged);
        let authorize_lines: Vec<&str> = m.lines().filter(|l| l.contains("authorizes")).collect();
        assert_eq!(
            authorize_lines.len(),
            1,
            "exactly one line may claim what is being authorized"
        );
    }

    #[test]
    fn a_client_name_is_bounded() {
        let long = "x".repeat(500);
        assert_eq!(sanitize_client_name(&long).unwrap().chars().count(), 64);
    }

    #[test]
    fn a_name_that_is_only_whitespace_or_control_characters_becomes_none() {
        assert_eq!(sanitize_client_name("   \n\t  "), None);
        assert_eq!(sanitize_client_name(""), None);
    }

    #[test]
    fn ordinary_names_survive_intact() {
        assert_eq!(
            sanitize_client_name("Claude Code").as_deref(),
            Some("Claude Code")
        );
    }
}
