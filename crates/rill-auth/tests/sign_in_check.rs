//! Which sign-in signatures are verified locally and which are handed to the node.

use rill_auth::siws::{check_sign_in_signature, SignInCheck};
use sui_crypto::SuiSigner as _;

/// A real zkLogin signature from a Slush wallet signed in with Google, over a mainnet grant
/// message. Public by nature: the server serves it to the agent the grant names.
const ZKLOGIN_SIGNATURE: &str = include_str!("fixtures/zklogin-signature.b64");
const ZKLOGIN_ADDRESS: &str = "0x6cc1b08112673f4146557c8464d57311a0f105f8787d5ef5d6aaa981634ef1bf";

#[test]
fn a_simple_signature_is_still_verified_here() {
    let key = sui_crypto::ed25519::Ed25519PrivateKey::new([7; 32]);
    let message = "api.rill.test wants you to sign in";
    let signature = key
        .sign_personal_message(&sui_sdk_types::PersonalMessage(message.as_bytes().into()))
        .unwrap()
        .to_base64();
    assert_eq!(
        check_sign_in_signature(message, &signature).unwrap(),
        SignInCheck::Verified(key.public_key().derive_address().to_string())
    );
    assert!(check_sign_in_signature("another message", &signature).is_err());
}

/// Refusing zkLogin refused everyone signed in to their wallet with Google.
#[test]
fn a_zklogin_signature_is_handed_to_the_node_with_the_address_it_derives() {
    let SignInCheck::AskNode { candidates } =
        check_sign_in_signature("any message", ZKLOGIN_SIGNATURE).unwrap()
    else {
        panic!("a zkLogin signature cannot be verified without the node");
    };
    assert!(
        candidates.iter().any(|c| c == ZKLOGIN_ADDRESS),
        "{candidates:?}"
    );
}

#[test]
fn garbage_is_refused_before_anything_is_asked() {
    assert!(check_sign_in_signature("m", "forged").is_err());
}
