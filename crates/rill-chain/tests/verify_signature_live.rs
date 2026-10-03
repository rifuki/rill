//! The node's personal-message verification, against a real fullnode.
//!
//! An owner-signed grant is only as trustworthy as this check, and nothing offline can stand in for
//! it: the request shape (the `PersonalMessage` name, the BCS length prefix, the address) is what
//! the node either accepts or quietly refuses. A throwaway key signs here, so nothing is funded and
//! no key leaves the test.

use rill_chain::grpc::GrpcSui;
use rill_chain::{SignatureCheck, SuiRead};
use sui_crypto::ed25519::Ed25519PrivateKey;
use sui_crypto::SuiSigner;
use sui_sdk_types::PersonalMessage;

fn endpoint() -> String {
    std::env::var("SUI_RPC_URL").unwrap_or_else(|_| "https://fullnode.mainnet.sui.io:443".into())
}

#[tokio::test]
#[ignore = "asks a live fullnode to verify signatures"]
async fn the_node_accepts_a_real_personal_message_signature_and_nothing_else() {
    let key = Ed25519PrivateKey::new([42; 32]);
    let address = key.public_key().derive_address().to_string();
    let message = b"Rill action grant v1\nlive verification check";
    let signature = key
        .sign_personal_message(&PersonalMessage(message[..].into()))
        .expect("signed")
        .to_base64();
    let chain = GrpcSui::new(&endpoint()).expect("a client");

    assert_eq!(
        chain
            .verify_personal_message(message, &signature, &address)
            .await
            .expect("the node answered"),
        SignatureCheck::Valid
    );
    assert!(matches!(
        chain
            .verify_personal_message(b"a different message", &signature, &address)
            .await
            .expect("the node answered"),
        SignatureCheck::Invalid(_)
    ));
    let other = Ed25519PrivateKey::new([7; 32])
        .public_key()
        .derive_address()
        .to_string();
    assert!(matches!(
        chain
            .verify_personal_message(message, &signature, &other)
            .await
            .expect("the node answered"),
        SignatureCheck::Invalid(_)
    ));
}
