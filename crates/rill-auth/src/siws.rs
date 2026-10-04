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
        "spending authority: every spend is separately bounded by your on-chain agent wallet, and"
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

/// A stable rejection for malformed, unsupported, or invalid wallet signatures.
#[derive(Debug, thiserror::Error)]
pub enum SignInError {
    #[error("Signature verification failed.")]
    InvalidSignature,
    #[error(
        "This signature scheme is not supported. Use Ed25519, Secp256k1, Secp256r1, or multisig."
    )]
    UnsupportedScheme,
}

/// Verify the exact stored message using Sui's personal-message intent and derive its signer.
/// No address provided by the caller participates in authentication.
pub fn verify_sign_in_signature(message: &str, encoded: &str) -> Result<String, SignInError> {
    use sui_sdk_types::{PersonalMessage, UserSignature};
    let signature =
        UserSignature::from_base64(encoded.trim()).map_err(|_| SignInError::InvalidSignature)?;
    let digest = PersonalMessage(message.as_bytes().into()).signing_digest();
    match &signature {
        UserSignature::Simple(simple) => verify_simple(&digest, simple)?,
        UserSignature::Multisig(multisig) => verify_multisig(&digest, multisig)?,
        _ => return Err(SignInError::UnsupportedScheme),
    }
    Ok(signature.derive_address().to_string())
}

/// What checking a sign-in signature needs: nothing more, or the node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignInCheck {
    /// Verified here, by the address the signature derives to.
    Verified(String),
    /// A zkLogin or passkey signature. Its validity depends on chain state (zkLogin's JWKs and
    /// epoch), so the node must verify it, against one of these addresses derived from the
    /// signature itself. A zkLogin seed with a leading zero byte derives two; either may be the
    /// account's.
    AskNode { candidates: Vec<String> },
}

/// Like [`verify_sign_in_signature`], but routes the schemes only a node can verify to it instead
/// of refusing them. Browser wallets signed in with Google sign with zkLogin, so refusing it
/// refused most of the people who would ever open Studio.
pub fn check_sign_in_signature(message: &str, encoded: &str) -> Result<SignInCheck, SignInError> {
    use sui_sdk_types::UserSignature;
    let signature =
        UserSignature::from_base64(encoded.trim()).map_err(|_| SignInError::InvalidSignature)?;
    match &signature {
        UserSignature::ZkLogin(zklogin) => Ok(SignInCheck::AskNode {
            candidates: zklogin.derive_address().map(|a| a.to_string()).collect(),
        }),
        UserSignature::Passkey(passkey) => Ok(SignInCheck::AskNode {
            candidates: vec![passkey.derive_address().to_string()],
        }),
        _ => verify_sign_in_signature(message, encoded).map(SignInCheck::Verified),
    }
}

fn verify_simple(
    digest: &[u8],
    signature: &sui_sdk_types::SimpleSignature,
) -> Result<(), SignInError> {
    use k256::ecdsa::signature::Verifier as _;
    use sui_sdk_types::SimpleSignature;
    let invalid = |_| SignInError::InvalidSignature;
    match signature {
        SimpleSignature::Ed25519 {
            signature,
            public_key,
        } => {
            let key =
                ed25519_dalek::VerifyingKey::from_bytes(public_key.inner()).map_err(invalid)?;
            key.verify_strict(
                digest,
                &ed25519_dalek::Signature::from_bytes(signature.inner()),
            )
            .map_err(invalid)
        }
        SimpleSignature::Secp256k1 {
            signature,
            public_key,
        } => {
            let key =
                k256::ecdsa::VerifyingKey::from_sec1_bytes(public_key.inner()).map_err(invalid)?;
            let signature =
                k256::ecdsa::Signature::from_slice(signature.inner()).map_err(invalid)?;
            key.verify(digest, &signature).map_err(invalid)
        }
        SimpleSignature::Secp256r1 {
            signature,
            public_key,
        } => {
            let key =
                p256::ecdsa::VerifyingKey::from_sec1_bytes(public_key.inner()).map_err(invalid)?;
            let signature =
                p256::ecdsa::Signature::from_slice(signature.inner()).map_err(invalid)?;
            key.verify(digest, &signature).map_err(invalid)
        }
        _ => Err(SignInError::UnsupportedScheme),
    }
}

fn verify_multisig(
    digest: &[u8],
    signature: &sui_sdk_types::MultisigAggregatedSignature,
) -> Result<(), SignInError> {
    use sui_sdk_types::{
        MultisigMemberPublicKey as Key, MultisigMemberSignature as Sig, SimpleSignature,
    };
    let committee = signature.committee();
    if !committee.is_valid()
        || signature.signatures().len() != signature.bitmap().count_ones() as usize
    {
        return Err(SignInError::InvalidSignature);
    }
    let indices = (0..16).filter(|i| signature.bitmap() & (1 << i) != 0);
    let mut weight = 0u16;
    for (index, member_signature) in indices.zip(signature.signatures()) {
        let member = committee
            .members()
            .get(index)
            .ok_or(SignInError::InvalidSignature)?;
        let simple = match (member.public_key(), member_signature) {
            (Key::Ed25519(public_key), Sig::Ed25519(signature)) => SimpleSignature::Ed25519 {
                public_key: *public_key,
                signature: *signature,
            },
            (Key::Secp256k1(public_key), Sig::Secp256k1(signature)) => SimpleSignature::Secp256k1 {
                public_key: *public_key,
                signature: *signature,
            },
            (Key::Secp256r1(public_key), Sig::Secp256r1(signature)) => SimpleSignature::Secp256r1 {
                public_key: *public_key,
                signature: *signature,
            },
            _ => return Err(SignInError::UnsupportedScheme),
        };
        verify_simple(digest, &simple)?;
        weight += u16::from(member.weight());
    }
    if weight < committee.threshold() {
        return Err(SignInError::InvalidSignature);
    }
    Ok(())
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
