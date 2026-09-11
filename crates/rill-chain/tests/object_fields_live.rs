//! An object read carries the object's own fields, not only its reference.
//!
//! `ObjectSummary::fields` existed for a long time and was hardcoded to `None` by the gRPC client,
//! so every caller that wanted a wallet's remaining budget had nowhere to get it and
//! `rill_wallet` reported which rules were attached without a single number beside them. The mask
//! now asks for `json` on a single-object read. Only a live node can show that it does: a fake
//! returns whatever a test put in it, which is exactly the shape this test exists to doubt.

use rill_chain::{grpc::GrpcSui, SuiRead};

const TESTNET: &str = "https://fullnode.testnet.sui.io:443";

/// An `AgentWallet` on testnet, shared, carrying two rules and a balance.
const WALLET: &str = "0x74d0e7b3d0956b08d40834ef19ae0fc9c48f35b09a928c57a517d6a20d8859cf";

#[tokio::test]
#[ignore = "requires a Sui testnet fullnode"]
async fn a_wallet_read_carries_the_numbers_an_agent_needs() {
    let chain = GrpcSui::new(TESTNET).expect("a client");
    let summary = chain.get_object(WALLET).await.expect("the wallet reads");

    let fields = summary
        .fields
        .as_ref()
        .expect("a single-object read must carry the object's fields");

    // The three an agent cannot act sensibly without: what is left, what is gone, and until when.
    for key in ["budget", "spent", "expires_at_ms"] {
        assert!(
            fields.get(key).is_some(),
            "the node returned fields without `{key}`: {fields}"
        );
    }

    // And they survive as numbers. protobuf carries every number as a double, so a balance above
    // 2^53 read back through an f64 would be rounded; the conversion writes integral values back
    // out as integers precisely so a `u64` field is not quietly approximated.
    let budget = &fields["budget"];
    assert!(
        budget.is_u64() || budget.is_i64() || budget.is_string(),
        "a balance must come back exact, not as a float: {budget}"
    );
}

/// Listing an address's objects does not pay for contents it was not asked for.
///
/// The two masks exist for this reason. A gas-coin sweep walks every object an address holds, and
/// asking for each one's Move fields there would buy nothing and cost on every coin.
#[tokio::test]
#[ignore = "requires a Sui testnet fullnode"]
async fn a_listing_stays_reference_only() {
    let chain = GrpcSui::new(TESTNET).expect("a client");
    let owned = chain
        .list_owned_objects("0xb93cbb8f841a3442e5112c50880f20db9735cb1bb5f1459e745c5f602a2fe29a")
        .await
        .expect("the listing reads");
    assert!(!owned.is_empty(), "the signer holds at least a gas coin");
    assert!(
        owned.iter().all(|o| o.fields.is_none()),
        "a listing asked for references and must not be billed for contents"
    );
}

/// A wallet's rule ceilings are reachable: they are dynamic fields on its policy.
///
/// This is the read that turns "a budget rule is attached" into "the budget is 0.2 SUI and 0.051 of
/// it is spent". The Move modules have getters for every one of these values, but only `rate_limit`
/// exposes a `view` that can produce the `&Config` they take, so three of the four are public
/// functions whose argument no caller can obtain. Reading the field directly needs no package
/// upgrade and works against wallets that already exist.
#[tokio::test]
#[ignore = "requires a Sui testnet fullnode"]
async fn a_rules_configured_ceiling_is_readable_without_a_move_change() {
    let chain = GrpcSui::new(TESTNET).expect("a client");
    let wallet = chain.get_object(WALLET).await.expect("the wallet reads");
    let fields = wallet.fields.as_ref().expect("the wallet's fields");
    let policy = fields["policy"]["id"]
        .as_str()
        .expect("the wallet names its policy object");

    let dyn_fields = chain
        .list_dynamic_fields(policy)
        .await
        .expect("the policy's dynamic fields read");
    assert!(
        !dyn_fields.is_empty(),
        "this wallet carries two rules, so its policy carries two fields"
    );
    assert!(
        dyn_fields.iter().all(|f| f.fields.is_some()),
        "a field read without its contents says which rule is attached and not what it allows"
    );

    // The two things the extraction depends on, pinned here because only a node can confirm them:
    // `value_type` names the Config type, so the module is the segment before `::Config`; and the
    // numbers live under `value`, as strings, because protobuf has no u64.
    let budget = dyn_fields
        .iter()
        .find(|f| {
            f.value_type
                .as_deref()
                .is_some_and(|t| t.ends_with("::budget::Config"))
        })
        .expect("the budget rule's config is one of the fields");
    let value = &budget.fields.as_ref().expect("its contents")["value"];
    assert!(
        value["total_mist"].is_string() && value["spent"].is_string(),
        "a u64 ceiling must arrive exact, as text, not as a double: {value}"
    );
}
