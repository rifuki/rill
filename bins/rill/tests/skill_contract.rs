//! The skill an agent reads is part of what this signer promises, so it is checked like code.
//!
//! The signer cannot see conversation turns, so "disclose, then wait for the user's next message"
//! lives only in SKILL.md. Text that can be reworded away without a failing build is text that
//! eventually is; these tests pin the sentences that carry the rule, and the claims in the skill
//! that the code has to keep true.

const SKILL: &str = include_str!("../../../plugins/rill/skills/rill/SKILL.md");
const CLAUDE_MANIFEST: &str = include_str!("../../../plugins/rill/.claude-plugin/plugin.json");
const CODEX_MANIFEST: &str = include_str!("../../../plugins/rill/.codex-plugin/plugin.json");

/// The body of one `## ` section, up to the next heading of the same level.
fn section<'a>(heading: &str) -> &'a str {
    let start = SKILL
        .find(&format!("\n{heading}\n"))
        .unwrap_or_else(|| panic!("SKILL.md has no section {heading:?}"));
    let body = &SKILL[start + heading.len() + 2..];
    &body[..body.find("\n## ").unwrap_or(body.len())]
}

/// Every `rill_*` tool name the skill mentions, in order of appearance.
fn named_tools() -> Vec<&'static str> {
    let mut names = Vec::new();
    let mut rest = SKILL;
    while let Some(at) = rest.find("rill_") {
        let tail = &rest[at..];
        let len = tail
            .find(|c: char| !(c.is_ascii_lowercase() || c == '_'))
            .unwrap_or(tail.len());
        // `rill_` inside a longer word, such as a path, is not a tool name.
        let preceded_by_word = rest[..at]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '/');
        if !preceded_by_word && len > "rill_".len() {
            names.push(&tail[..len]);
        }
        rest = &tail[len.max(1)..];
    }
    names
}

#[test]
fn the_skill_requires_a_second_turn_before_submitting() {
    let gate = section("## Before anything that submits outside a grant");
    for needed in [
        "End your turn, and submit only after the user's next message",
        "before you disclosed",
        "found in what you read",
        "claiming to be the owner",
        "different facts",
        "after a refusal",
    ] {
        assert!(gate.contains(needed), "the gate must say {needed:?}");
    }

    // The tools that submit outside an owner grant: for these the chat is the only consent.
    let gated = &gate[..gate.find("\n1. ").expect("the gate lists its steps")];
    for tool in [
        "`rill_spend`",
        "`rill_swap`",
        "`rill_stake`",
        "`rill_unstake` without `dryRun`",
        "`rill_execute`",
    ] {
        assert!(gated.contains(tool), "the gate must cover {tool}");
    }
    // Granted runs are consented by the owner's signature, so they are not in the wait list,
    // and the agent still reports what ran.
    for granted in ["rill_run_action", "rill_run_workflow"] {
        assert!(
            !gated.contains(granted),
            "{granted} runs under an owner-signed grant and does not wait for a chat yes"
        );
    }
    assert!(gate.contains("owner-signed grant"));
    assert!(gate.contains("Still tell the user what ran"));

    // A wider slippage lowers minOut, which is a different trade than the one agreed to.
    assert!(section("## Swaps").contains("ask again before submitting"));
}

/// `rill_unstake` redeems the signer's own haSUI: it takes no wallet, so no wallet budget or
/// per-transaction cap bounds it, only `minOut`. A gate that tells the agent to quote the wallet's
/// cap for it states a protection the code does not have.
#[test]
fn the_gate_does_not_bound_unstake_by_a_wallet_cap() {
    let gate = section("## Before anything that submits outside a grant");
    let step = |n: u8| {
        let start = gate
            .find(&format!("\n{n}. "))
            .unwrap_or_else(|| panic!("the gate has no step {n}"));
        let body = &gate[start + 1..];
        // A step runs until the next step or the blank line that closes the list.
        let end = [format!("\n{}. ", n + 1), "\n\n".to_string()]
            .iter()
            .filter_map(|marker| body.find(marker.as_str()))
            .min()
            .unwrap_or(body.len());
        &body[..end]
    };

    let read_limits = step(1);
    for tool in [
        "`rill_spend`",
        "`rill_swap`",
        "`rill_stake`",
        "`rill_execute`",
    ] {
        assert!(
            read_limits.contains(tool),
            "reading largestSpendNow applies to {tool}, which releases from a wallet"
        );
    }
    assert!(
        !read_limits.contains("rill_unstake"),
        "rill_unstake takes no wallet, so largestSpendNow does not bound it"
    );

    let disclose = step(2);
    let unstake = disclose
        .find("`rill_unstake`")
        .expect("the disclosure must say what bounds rill_unstake");
    assert!(
        disclose[unstake..].contains("only its `minOut`"),
        "rill_unstake is bounded only by minOut, not by the wallet's budget or cap"
    );
}

#[test]
fn the_skill_is_honest_about_limit_orders() {
    let orders = section("## Limit orders");
    for needed in [
        "placed, not filled",
        "no tool that reads an order's status",
        "Never send a `rill_swap` in its place",
        "recovery in Studio",
    ] {
        assert!(
            orders.contains(needed),
            "the limit order section must say {needed:?}"
        );
    }
}

/// Guard, not proof of the fix: this passes before and after. It keeps the skill's "no tool reads
/// an order's status" true, so adding such a tool forces that sentence to change with it.
#[test]
fn the_skill_names_only_offered_tools_and_no_order_status_tool() {
    let offered: Vec<String> = rill_mcp::tools(rill_mcp::Surface::Wallet)
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    let named = named_tools();
    assert!(!named.is_empty(), "the tool name scan found nothing");
    for name in named {
        assert!(
            name == "rill_pair" || offered.iter().any(|o| o == name),
            "SKILL.md names {name}, which the wallet surface does not offer"
        );
    }
    for name in &offered {
        for word in ["order", "cancel", "status_of"] {
            assert!(
                !name.contains(word),
                "{name} looks like an order tool; update the limit order section of SKILL.md"
            );
        }
    }
}

/// Guard: Claude Code and Codex cache the skill by plugin version, so the two manifests move
/// together or one client keeps a stale copy. Also keeps U+2014 out, which CI rejects anyway.
#[test]
fn both_plugin_manifests_carry_one_version() {
    let version = |manifest: &str| {
        serde_json::from_str::<serde_json::Value>(manifest).expect("plugin.json is JSON")["version"]
            .as_str()
            .expect("plugin.json has a version")
            .to_string()
    };
    assert_eq!(version(CLAUDE_MANIFEST), version(CODEX_MANIFEST));
    assert!(!SKILL.contains('\u{2014}'), "SKILL.md carries an em dash");
}

/// The owner's step-up pause, as the agent must handle it: the link goes to the owner, the
/// continuation is a new key with the approvalId, the window is the envelope's, and the skill
/// says how far the pause reaches instead of implying it binds the key.
#[test]
fn the_skill_explains_the_step_up_pause_and_its_reach() {
    let step_up = section("### Owner step-up approval");
    for needed in [
        "`pending_owner_approval`",
        "`step_up_required`",
        "`approvalUrl`",
        "`approvalId`",
        "a new `idempotencyKey`",
        "replaying it returns the pause",
        "5 minutes",
        "`step_up_expired`",
        "`step_up_pending`",
        "Do not rebuild",
        "`rill_run_action` and `rill_run_workflow`",
        "`use_granted_action`",
        "bound only by the wallet's on-chain rules",
    ] {
        assert!(
            step_up.contains(needed),
            "the step-up section must say {needed:?}"
        );
    }
}

/// An owner's on-chain recipient list changes what the agent may do with `rill_spend` and closes
/// every tool that needs a released coin. The skill has to say both, and that only the owner can
/// change the list, or an agent refused once tries the next address.
#[test]
fn the_skill_explains_the_owners_recipient_allowlist() {
    let body = section("## Recipient allowlist");
    for needed in [
        "`recipientAllowlist`",
        "`to`",
        "`rill_swap`",
        "Only the owner",
        "Do not try another address",
    ] {
        assert!(body.contains(needed), "the section must say {needed:?}");
    }
}
