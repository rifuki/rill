//! The in-memory chain, and the contract every implementation of the traits must honour.
//!
//! These run with no network. That is the whole point of the boundary: the rest of the workspace
//! is testable without a fullnode, and a test says what the chain does rather than what one call
//! returns.

use rill_chain::fake::{FakeSui, SimulationBehavior};
use rill_chain::{ChainError, ObjectRef, ObjectSummary, SuiRead, SuiWrite, Verification};

fn object(id: &str, ty: &str) -> ObjectSummary {
    ObjectSummary {
        reference: ObjectRef {
            id: id.into(),
            version: 1,
            digest: "digest".into(),
        },
        object_type: Some(ty.into()),
        fields: None,
    }
}

#[tokio::test]
async fn reads_come_back_consistently() {
    let chain = FakeSui::new()
        .with_object(
            Some("0xowner"),
            object("0xcoin", "0x2::coin::Coin<0x2::sui::SUI>"),
        )
        .with_balance("0xowner", "0x2::sui::SUI", 5_000_000_000);

    assert_eq!(
        chain.get_object("0xcoin").await.unwrap().reference.id,
        "0xcoin"
    );
    assert_eq!(chain.list_owned_objects("0xowner").await.unwrap().len(), 1);
    assert_eq!(
        chain.get_balance("0xowner", "0x2::sui::SUI").await.unwrap(),
        5_000_000_000
    );
}

#[tokio::test]
async fn a_missing_object_is_not_found_rather_than_a_transport_error() {
    let chain = FakeSui::new();
    assert!(matches!(
        chain.get_object("0xnope").await,
        Err(ChainError::NotFound(_))
    ));
}

#[tokio::test]
async fn an_unknown_balance_is_zero_not_an_error() {
    let chain = FakeSui::new();
    assert_eq!(
        chain
            .get_balance("0xstranger", "0x2::sui::SUI")
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn a_successful_simulation_is_verified() {
    let chain = FakeSui::new();
    let outcome = chain.simulate("AAA=").await.unwrap();
    assert!(outcome.ok);
    assert_eq!(outcome.verification, Verification::Verified);
}

/// A real abort is a real answer: the simulation worked, and it said no.
#[tokio::test]
async fn a_genuine_abort_is_a_verified_failure() {
    let chain = FakeSui::new().with_simulation(SimulationBehavior::Fails {
        error: "MoveAbort(.., 5)".into(),
    });
    let outcome = chain.simulate("AAA=").await.unwrap();
    assert!(!outcome.ok);
    assert_eq!(outcome.verification, Verification::Verified);
}

#[tokio::test]
async fn the_cetus_version_abort_comes_back_unverified() {
    let error = format!(
        "MoveAbort in {}::config: checked_package_version",
        rill_chain::CETUS_PACKAGE_IDS[0]
    );
    let chain = FakeSui::new().with_simulation(SimulationBehavior::Fails { error });
    let outcome = chain.simulate("AAA=").await.unwrap();
    assert!(!outcome.ok);
    assert_eq!(outcome.verification, Verification::Unverified);
}

/// The distinction that matters most: an unreachable node is an error, never a failed simulation.
/// Collapsing the two would let a dropped connection read as a verdict about the transaction.
#[tokio::test]
async fn an_unreachable_node_is_an_error_not_a_verdict() {
    let chain = FakeSui::new().with_simulation(SimulationBehavior::Unreachable);
    assert!(matches!(
        chain.simulate("AAA=").await,
        Err(ChainError::Transport(_))
    ));
}

#[tokio::test]
async fn submitting_without_a_signature_is_refused() {
    let chain = FakeSui::new();
    assert!(matches!(
        chain.execute("AAA=", &[]).await,
        Err(ChainError::Rejected(_))
    ));
    assert!(
        chain.submitted().is_empty(),
        "a refused submission must not reach the chain"
    );
}

#[tokio::test]
async fn a_signed_submission_is_recorded() {
    let chain = FakeSui::new();
    let outcome = chain.execute("AAA=", &["sig".into()]).await.unwrap();
    assert!(outcome.success);
    assert_eq!(chain.submitted(), vec!["AAA=".to_string()]);
}
