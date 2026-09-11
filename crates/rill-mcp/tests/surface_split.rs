//! What the two transports share, what they do not, and why the difference is the design.
//!
//! `rill_mcp::tools(Surface)` is the single producer for both. The HTTP builder holds no key; the
//! local signer does. These assertions sit outside the producer's own module on purpose: each
//! transport is assembled in its own crate, so a drift between them is invisible to any reader of
//! either one.
//!
//! The in-module test that existed before this file checked only that the builder lacks the four
//! signing tools, which is one-sided. Dropping a signing tool from the signer entirely passed it,
//! and so did adding an unrelated tool to one surface.

use rill_mcp::{negotiate_protocol_version, tools, Surface, LATEST_PROTOCOL_VERSION};
use std::collections::BTreeMap;

/// The tools that need a key. Asserted in both directions below rather than trusted.
const SIGNING_TOOLS: [&str; 5] = [
    "rill_create_wallet",
    "rill_attach_rules",
    "rill_spend",
    "rill_swap",
    "rill_execute",
];

fn by_name(surface: Surface) -> BTreeMap<String, rill_mcp::Tool> {
    tools(surface)
        .into_iter()
        .map(|t| (t.name.to_string(), t))
        .collect()
}

/// The surfaces share no tool name at all, and that is the design rather than an accident.
///
/// KTD-3 is titled "Two disjoint surfaces" and then says the test should assert the surfaces
/// "differ by exactly the signing tools". Those are two different claims, and the code implements
/// the first: the builder speaks a catalogue vocabulary (list, describe, build) and the signer
/// speaks a wallet one (status, read, create, attach, spend, execute), with nothing in common. The
/// difference is therefore every tool on both sides, not four of them. Asserting the sentence would
/// mean bending a test into passing or bending the code to match a sentence; asserting what the
/// design actually guarantees is the third option, and this comment is here because the next reader
/// will hit the same contradiction.
#[test]
fn the_surfaces_share_no_tool_name() {
    let actions = by_name(Surface::Actions);
    let wallet = by_name(Surface::Wallet);
    let shared: Vec<&String> = actions.keys().filter(|n| wallet.contains_key(*n)).collect();
    assert!(
        shared.is_empty(),
        "the two surfaces are disjoint by design; {shared:?} appears on both, so either the design \
         changed or a tool was added to the wrong list"
    );
    assert!(
        !actions.is_empty() && !wallet.is_empty(),
        "both surfaces must offer something"
    );
}

/// The property keylessness rests on: every tool that needs a key is on the signer, and not one of
/// them is reachable on the transport that holds no key.
#[test]
fn every_signing_tool_is_on_the_signer_and_none_on_the_builder() {
    let actions = by_name(Surface::Actions);
    let wallet = by_name(Surface::Wallet);
    for signing in SIGNING_TOOLS {
        assert!(
            wallet.contains_key(signing),
            "{signing} is missing from the signer, so the flow lost a step"
        );
        assert!(
            !actions.contains_key(signing),
            "{signing} needs a key and the builder has none"
        );
    }
    // The other direction, by name: a fifth signing tool added to the signer and forgotten here
    // fails this rather than quietly widening what the signer offers.
    let other: Vec<String> = wallet
        .keys()
        .filter(|n| !SIGNING_TOOLS.contains(&n.as_str()))
        .cloned()
        .collect();
    assert_eq!(
        other,
        vec!["rill_status".to_string(), "rill_wallet".to_string()],
        "the signer's non-signing tools are status and the wallet read; a new name here is either a \
         signing tool missing from SIGNING_TOOLS or a capability the builder should also expose"
    );
}

/// If a tool ever does appear on both transports, it must be the same tool.
///
/// Vacuous today, by the test above. Kept because the day someone shares one is exactly the day a
/// client can read a description on one transport and act on it through the other, and nobody will
/// remember to write this then.
#[test]
fn a_tool_on_both_transports_would_have_to_be_identical_on_both() {
    let actions = by_name(Surface::Actions);
    let wallet = by_name(Surface::Wallet);
    for (name, a) in &actions {
        let Some(w) = wallet.get(name) else { continue };
        assert_eq!(
            a.description, w.description,
            "{name} describes itself differently on the two transports"
        );
        assert_eq!(
            a.input_schema, w.input_schema,
            "{name} takes different arguments on the two transports"
        );
        assert_eq!(
            a.annotations.as_ref().map(|x| x.read_only_hint),
            w.annotations.as_ref().map(|x| x.read_only_hint),
            "{name} is read-only on one transport and not the other"
        );
        assert_eq!(
            a.annotations.as_ref().map(|x| x.destructive_hint),
            w.annotations.as_ref().map(|x| x.destructive_hint),
            "{name} is destructive on one transport and not the other, which is the one annotation a \
             client uses to decide whether to ask a human first"
        );
    }
}

/// Nothing the builder offers may be marked destructive, because none of it does anything.
#[test]
fn nothing_the_builder_offers_is_destructive() {
    for tool in tools(Surface::Actions) {
        let destructive = tool
            .annotations
            .as_ref()
            .and_then(|a| a.destructive_hint)
            .unwrap_or(false);
        assert!(
            !destructive,
            "{} is marked destructive on a surface that cannot sign, so it cannot destroy anything",
            tool.name
        );
    }
}

/// Negotiation is one function for both transports now, so this covers both at once.
#[test]
fn a_client_naming_a_revision_we_speak_gets_that_revision_back() {
    for asked in ["2025-03-26", "2024-11-05", "2025-06-18"] {
        assert_eq!(
            negotiate_protocol_version(Some(asked)),
            asked,
            "an older client that names a revision this implementation speaks must keep working"
        );
    }
}

#[test]
fn a_client_naming_nothing_or_something_we_do_not_speak_is_told_the_newest() {
    assert_eq!(negotiate_protocol_version(None), LATEST_PROTOCOL_VERSION);
    for asked in ["2099-01-01", "", "2025-06-17", "garbage"] {
        assert_eq!(
            negotiate_protocol_version(Some(asked)),
            LATEST_PROTOCOL_VERSION,
            "answering {asked:?} with itself would claim an implementation that does not exist"
        );
    }
}

/// The advertised revision is the newest supported one, not whichever happens to be listed first.
#[test]
fn the_advertised_revision_is_the_newest_one_supported() {
    let mut sorted = rill_mcp::SUPPORTED_PROTOCOL_VERSIONS.to_vec();
    sorted.sort_unstable();
    sorted.reverse();
    assert_eq!(
        LATEST_PROTOCOL_VERSION, sorted[0],
        "the list is newest-first and the advertised version is its head; one of those is wrong"
    );
}
