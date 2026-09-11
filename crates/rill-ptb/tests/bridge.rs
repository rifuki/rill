//! What a bridge refuses, and why each refusal is not something the Move signature could express.
//!
//! `send_token<T>(bridge, target_chain: u8, target_address: vector<u8>, coin, ctx)`. Every one of
//! those parameters accepts a value that destroys the transfer: any `u8` is a chain, any byte vector
//! is an address, and any coin type compiles. The bridge checks the chain id and the token registry
//! on chain and aborts with a code; it cannot check that a 32-byte value is a Sui address somebody
//! pasted into an Ethereum field, because both are byte vectors of a length it was not told.

use rill_ptb::bridge::{
    carries, chain, expected_bridge_targets, send_token, Bridge, BridgeError, BRIDGE_OBJECT,
    BRIDGE_PACKAGE, ETHEREUM_ADDRESS_BYTES,
};
use rill_ptb::shared::SharedObjects;
use sui_transaction_builder::TransactionBuilder;

const SUI: &str = "0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI";

/// The five the deployed testnet bridge carries, as its treasury stores them: no `0x` prefix.
fn carried() -> Vec<String> {
    [
        "7fd9268baa20a130e52f85935a928d9fc715365a85251eaec0a223524a258b92::btc::BTC",
        "d4e8b2874af2ccd2f067dc208ffc25a420b0c7a91d8f71c249f730d2e158afeb::eth::ETH",
        "a09fd1f4c7cfafcafdec341cd971c28621b451c8a60b950a92685d64cf1f1e0a::usdc::USDC",
        "85ae32e1c848dd9759917abdb0f2e19114f9b1ee47a2d6d2429af9b9ab0458fc::usdt::USDT",
        "5b3e288552de1d0c645227273d5342a3385c4781d1aa4dfaf55f9cc7dbf31ed5::pepe::PEPE",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn resolved() -> SharedObjects {
    let mut shared = SharedObjects::new();
    shared.insert(BRIDGE_OBJECT.parse().expect("an address"), 39_292_637);
    shared
}

fn bridge(chain_id: u8, address: Vec<u8>, coin_type: &str) -> Bridge {
    Bridge {
        target_chain: chain_id,
        target_address: address,
        coin_type: coin_type.to_string(),
    }
}

fn eth_address() -> Vec<u8> {
    vec![0xab; ETHEREUM_ADDRESS_BYTES]
}

/// A coin to hand the bridge. Split from gas, which is how every other adapter's tests make one.
fn coin(tx: &mut TransactionBuilder) -> sui_transaction_builder::Argument {
    let amount = tx.pure(&1_000_000u64);
    let gas = tx.gas();
    tx.split_coins(gas, vec![amount])
        .into_iter()
        .next()
        .expect("one coin")
}

fn build(b: &Bridge) -> Result<(), BridgeError> {
    let mut tx = TransactionBuilder::new();
    let c = coin(&mut tx);
    send_token(&mut tx, b, &carried(), c, &resolved())
}

/// A token the bridge carries, to a real Ethereum address, builds.
#[test]
fn a_carried_token_to_an_ethereum_address_builds() {
    let usdc = "0xa09fd1f4c7cfafcafdec341cd971c28621b451c8a60b950a92685d64cf1f1e0a::usdc::USDC";
    assert_eq!(
        build(&bridge(chain::ETH_SEPOLIA, eth_address(), usdc)),
        Ok(())
    );
}

/// SUI is refused, and the refusal explains why an agent wallet cannot bridge its own funds.
///
/// This is the case that matters in practice: every `AgentWallet` here is `AgentWallet<SUI>`. Without
/// this the transfer aborts inside the bridge with a code that names neither the token nor the reason.
#[test]
fn sui_is_refused_and_the_refusal_names_what_is_carried_instead() {
    let err = build(&bridge(chain::ETH_SEPOLIA, eth_address(), SUI))
        .expect_err("the bridge carries no SUI");
    let BridgeError::TokenNotCarried { coin_type, carried } = &err else {
        panic!("wrong refusal: {err:?}");
    };
    assert_eq!(coin_type, SUI);
    assert_eq!(carried.len(), 5);

    let said = err.to_string();
    assert!(
        said.contains("does not carry"),
        "the refusal must say the bridge does not carry it: {said}"
    );
    for name in ["USDC", "USDT", "ETH", "BTC"] {
        assert!(
            said.contains(name),
            "and must list {name}, so a caller learns what it could hold instead: {said}"
        );
    }
    assert!(
        said.contains("cannot bridge its own funds"),
        "and must say plainly what that means for a SUI-funded wallet: {said}"
    );
}

/// A recipient of the wrong length is refused, because the contract would accept it.
///
/// Thirty-two bytes is the length of a Sui address, which is exactly the mistake: a byte vector of
/// the wrong length type-checks, reaches the far chain, and corresponds to nothing anybody holds.
#[test]
fn a_recipient_of_the_wrong_length_is_refused_before_the_funds_leave() {
    let usdc = "0xa09fd1f4c7cfafcafdec341cd971c28621b451c8a60b950a92685d64cf1f1e0a::usdc::USDC";
    for (bytes, label) in [
        (32usize, "a Sui address"),
        (19, "one byte short"),
        (21, "one too many"),
    ] {
        let err = build(&bridge(chain::ETH_SEPOLIA, vec![1; bytes], usdc)).unwrap_err();
        assert_eq!(
            err,
            BridgeError::AddressWrongLength {
                target_chain: chain::ETH_SEPOLIA,
                expected: ETHEREUM_ADDRESS_BYTES,
                got: bytes
            },
            "{label} must be refused"
        );
    }
    assert!(
        build(&bridge(chain::ETH_SEPOLIA, vec![], usdc))
            .unwrap_err()
            .to_string()
            .contains("nowhere recoverable"),
        "and an empty recipient is its own refusal"
    );
}

/// A Sui chain as the destination is refused: that is not a bridge route.
#[test]
fn a_sui_destination_is_refused_as_not_a_route() {
    let usdc = "0xa09fd1f4c7cfafcafdec341cd971c28621b451c8a60b950a92685d64cf1f1e0a::usdc::USDC";
    for id in [
        chain::SUI_MAINNET,
        chain::SUI_TESTNET,
        chain::SUI_DEVNET,
        chain::SUI_LOCAL_TEST,
    ] {
        assert_eq!(
            build(&bridge(id, eth_address(), usdc)),
            Err(BridgeError::DestinationIsSui { target_chain: id }),
            "chain {id} is a Sui chain"
        );
    }
}

/// A chain id the bridge does not use is refused, naming the ones it does.
#[test]
fn an_unknown_chain_id_is_refused_with_the_ones_that_work() {
    let usdc = "0xa09fd1f4c7cfafcafdec341cd971c28621b451c8a60b950a92685d64cf1f1e0a::usdc::USDC";
    let err = build(&bridge(99, eth_address(), usdc)).unwrap_err();
    assert_eq!(err, BridgeError::UnknownDestination { target_chain: 99 });
    let said = err.to_string();
    assert!(said.contains("10") && said.contains("11"), "{said}");
}

/// The registry is matched on the type, whether or not the caller wrote the `0x` prefix.
///
/// The bridge stores types without it. A comparison that did not account for that would refuse every
/// token the bridge actually carries, which is the most expensive possible direction for this bug:
/// it looks like the bridge supports nothing.
#[test]
fn the_prefix_does_not_decide_whether_a_token_is_carried() {
    let without = "a09fd1f4c7cfafcafdec341cd971c28621b451c8a60b950a92685d64cf1f1e0a::usdc::USDC";
    let with = format!("0x{without}");
    assert!(carries(&carried(), without));
    assert!(carries(&carried(), &with));
    assert!(!carries(&carried(), SUI));
    // And a type that merely contains a carried one is not a match.
    assert!(!carries(&carried(), &format!("0x2::wrap::W<{with}>")));
}

/// An empty registry refuses everything and says the read saw nothing.
///
/// A caller whose registry read failed must not end up bridging against an empty list that silently
/// matches nothing in a way that reads as "unsupported token".
#[test]
fn an_empty_registry_refuses_and_says_the_read_was_empty() {
    let mut tx = TransactionBuilder::new();
    let c = coin(&mut tx);
    let err = send_token(
        &mut tx,
        &bridge(chain::ETH_SEPOLIA, eth_address(), SUI),
        &[],
        c,
        &resolved(),
    )
    .unwrap_err();
    assert!(
        err.to_string().contains("nothing this read could see"),
        "{err}"
    );
}

/// The pinned sequence is one call, against the framework package.
#[test]
fn the_pinned_sequence_is_the_one_bridge_call() {
    assert_eq!(
        expected_bridge_targets(),
        vec![format!("{BRIDGE_PACKAGE}::bridge::send_token")]
    );
}

/// A bridge with no known shared version for the Bridge object is refused, not built at zero.
#[test]
fn an_unresolved_bridge_object_is_refused() {
    let usdc = "0xa09fd1f4c7cfafcafdec341cd971c28621b451c8a60b950a92685d64cf1f1e0a::usdc::USDC";
    let mut tx = TransactionBuilder::new();
    let c = coin(&mut tx);
    let err = send_token(
        &mut tx,
        &bridge(chain::ETH_SEPOLIA, eth_address(), usdc),
        &carried(),
        c,
        &SharedObjects::new(),
    )
    .unwrap_err();
    assert!(
        matches!(err, BridgeError::UnknownShared(_)),
        "a shared object referenced at an unknown version must be refused: {err:?}"
    );
}

/// A type taken straight from the registry builds, prefix and all.
///
/// The registry stores `a09fd1f4…::usdc::USDC` with no `0x`. An earlier version accepted that in
/// `carries` and then refused it in the `TypeTag` parse, so a caller doing the obvious thing, reading
/// the registry and passing a type back, got `BadIdentifier` for a token the bridge demonstrably
/// carries. The offline tests missed it because they all passed prefixed types while the registry
/// fixture was prefix-less; the live test against the real registry is what found it.
#[test]
fn a_type_in_the_registrys_own_form_builds_rather_than_being_called_a_bad_identifier() {
    for coin_type in &carried() {
        assert!(
            !coin_type.starts_with("0x"),
            "the fixture must keep the registry's own form, or this test proves nothing"
        );
        assert_eq!(
            build(&bridge(chain::ETH_SEPOLIA, eth_address(), coin_type)),
            Ok(()),
            "{coin_type} is carried, so it must build"
        );
    }
}

/// And a prefixed type still builds, so the normalisation works in both directions.
#[test]
fn a_prefixed_type_builds_too() {
    let prefixed = format!("0x{}", carried()[1]);
    assert_eq!(
        build(&bridge(chain::ETH_SEPOLIA, eth_address(), &prefixed)),
        Ok(())
    );
}
