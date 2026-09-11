//! Shared MCP tool definitions for both binaries.
//!
//! # Annotations
//!
//! The reference ships no tool annotations at all, which leaves a client unable to tell the
//! keyless builders from the one tool that submits a real transaction — and that separation is the
//! whole of Rill's security model. Every tool here declares whether it modifies anything, and
//! `execute_rill_action` is the only one marked destructive.
//!
//! # Names
//!
//! Namespaced with a `rill_` prefix. `list_actions` and `describe_action` are generic enough that
//! a second connected server could plausibly offer the same names, and an agent choosing between
//! two identically-named tools chooses arbitrarily.

/// Re-exported so a consumer can name what [`tools`] returns without taking a dependency on the
/// MCP crate itself. The server renders the agent-facing document from this list, and a document
/// that names a tool nobody offers is an instruction an agent cannot follow.
pub use rmcp::model::Tool;
use rmcp::model::ToolAnnotations;
use serde_json::{json, Map, Value};
use std::borrow::Cow;
use std::sync::Arc;

/// Which tools a connection exposes. The signer and the server share definitions but not
/// capabilities: only the signer holds a key, and only it offers a tool that can spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// The keyless builder. Reads and compiles; cannot sign.
    Actions,
    /// The local signer. Holds the key.
    Wallet,
}

fn object_schema(value: Value) -> Arc<Map<String, Value>> {
    Arc::new(
        value
            .as_object()
            .cloned()
            .expect("a tool schema must be a JSON object"),
    )
}

fn no_arguments() -> Arc<Map<String, Value>> {
    object_schema(json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    }))
}

/// A tool that only reads.
fn read_only(
    name: &'static str,
    description: &'static str,
    schema: Arc<Map<String, Value>>,
) -> Tool {
    Tool::new(Cow::Borrowed(name), Cow::Borrowed(description), schema).annotate(
        ToolAnnotations::new()
            .read_only(true)
            .destructive(false)
            .idempotent(true)
            .open_world(false),
    )
}

/// The one tool that submits a transaction.
fn destructive(
    name: &'static str,
    description: &'static str,
    schema: Arc<Map<String, Value>>,
) -> Tool {
    Tool::new(Cow::Borrowed(name), Cow::Borrowed(description), schema).annotate(
        ToolAnnotations::new()
            .read_only(false)
            // Irreversible: once submitted, a transaction is on chain.
            .destructive(true)
            // Every envelope is single-use and expires; replaying one is not a no-op.
            .idempotent(false)
            .open_world(true),
    )
}

/// Every tool a surface exposes.
/// The protocol revisions this implementation speaks, newest first.
///
/// One list, because there are two transports and a client cannot tell them apart by anything it
/// should have to care about. Both once carried their own copy of this and of the negotiation
/// beside it, so the stdio signer and the HTTP builder could have come to advertise different
/// revisions of the same protocol without a single test noticing. KTD-3 asks for one producer per
/// claim, and a protocol version is a claim.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// The newest revision either transport will offer when a client names none, or names one this
/// implementation does not speak.
pub const LATEST_PROTOCOL_VERSION: &str = SUPPORTED_PROTOCOL_VERSIONS[0];

/// The revision to answer an `initialize` with, given whatever the client asked for.
///
/// A client that names a revision this implementation speaks gets that one back, which is what lets
/// an older client keep working. Anything else, including no request at all and a revision from the
/// future, gets the newest: the handshake then states plainly what the client is actually talking
/// to, rather than echoing a version nothing here implements.
pub fn negotiate_protocol_version(requested: Option<&str>) -> &'static str {
    requested
        .and_then(|asked| {
            SUPPORTED_PROTOCOL_VERSIONS
                .iter()
                .find(|supported| **supported == asked)
        })
        .copied()
        .unwrap_or(LATEST_PROTOCOL_VERSION)
}

pub fn tools(surface: Surface) -> Vec<Tool> {
    match surface {
        Surface::Actions => vec![
            read_only(
                "rill_list_actions",
                "List the actions available from this Rill endpoint. Builds only; Rill never signs.",
                no_arguments(),
            ),
            read_only(
                "rill_describe_action",
                "Describe an action's parameters, wallet binding, targets, and simulation rule.",
                object_schema(json!({
                    "type": "object",
                    "properties": { "actionId": { "type": "string" } },
                    "required": ["actionId"],
                    "additionalProperties": false
                })),
            ),
            read_only(
                "rill_build_action",
                "Compile and strictly simulate an action, returning an unsigned ExecutionEnvelope. \
                 No key is involved and nothing is submitted; signing happens locally in rill.",
                object_schema(json!({
                    "type": "object",
                    "properties": {
                        "actionId": { "type": "string" },
                        "sender": {
                            "type": "string",
                            "description": "The agent's Sui address. Public, never a key."
                        },
                        // Public object ids only. The keyless guard refuses anything key-shaped in
                        // here, however it is spelled.
                        "agentWallet": {
                            "type": "object",
                            "description": "Public ids identifying the funding wallet and its rules.",
                            "properties": {
                                "packageId": { "type": "string" },
                                "walletId": { "type": "string" },
                                "capId": { "type": "string" },
                                "capVersion": { "type": "integer" },
                                "capDigest": { "type": "string" },
                                "versionId": { "type": "string" },
                                "capabilityManifest": { "type": "object" }
                            },
                            "required": [
                                "packageId", "walletId", "capId", "capVersion", "capDigest",
                                "versionId", "capabilityManifest"
                            ],
                            "additionalProperties": false
                        },
                        "params": {
                            "type": "object",
                            "description": "Runtime values. Amounts are decimal STRINGS, never numbers: a JSON number would put a float on the money path."
                        }
                    },
                    "required": ["actionId", "sender", "agentWallet", "params"],
                    "additionalProperties": false
                })),
            ),
        ],
        Surface::Wallet => vec![
            // One tool per question, not one tool with a switch.
            //
            // These two answer different things and need different arguments: whether this signer
            // can act, and what a particular wallet permits. Folding them into one tool with an
            // optional argument makes the answer's shape depend on the call, which an agent has to
            // discover by trying it.
            //
            // What is never folded is a read into a write. Annotations are per-tool, and an MCP
            // client decides from `destructiveHint` whether to stop and ask a human. A tool that
            // could both read and spend would carry that hint always, so every harmless read would
            // raise an approval prompt — and a prompt that fires on everything is one people learn
            // to click through.
            read_only(
                "rill_status",
                "Whether this signer can act: its address, network, whether mainnet signing is \
                 allowed, what run-set is pinned, and the last refusal with its reason if there \
                 was one. Says nothing about any particular wallet — use rill_wallet for that.",
                no_arguments(),
            ),
            // The description carries the distinction the code computes. An earlier version
            // called the read "Authoritative" and stopped there, and an agent reading it could
            // conclude that every limit lived on chain; half of them do not.
            read_only(
                "rill_wallet",
                "What one agent wallet permits, and which layer holds each limit. The budget, \
                 per-transaction cap, rate limit and time window are on-chain rules: read live \
                 from the Move contract that proves them, so this is the answer rather than a \
                 local copy, and nothing in this process can widen them. Protocol scope, asset \
                 scope and recipient allowlist are pre-flight rules: this signer enforces them by \
                 refusing to sign, and they are listed from the loaded run-set, if one is loaded. \
                 Nothing on the chain checks a destination, a protocol, an asset or a recipient. \
                 The slippage floor is pre-flight too, and enforced twice: this signer refuses to \
                 sign an envelope whose guard call does not match, and the chain aborts if the \
                 floor is breached once it runs.",
                object_schema(json!({
                    "type": "object",
                    "properties": {
                        "wallet": { "type": "string", "description": "The AgentWallet object id." }
                    },
                    "required": ["wallet"],
                    "additionalProperties": false
                })),
            ),
            // The two owner-side steps, which used to be commands a person typed.
            //
            // Without them the flow an agent could drive started halfway through: it could spend
            // from a wallet and could not get one, so every demonstration began with a human at a
            // terminal. Offering them here does not widen what an agent may do, because the
            // contract decides that and not this list: `add_rule` asserts the owner, so a signer
            // launched with the agent's key is refused by name the moment it tries.
            destructive(
                "rill_create_wallet",
                "Mint an agent wallet and the AgentCap that drives it, funded from the key this \
                 signer holds. The key here becomes the wallet's owner and the cap goes to the \
                 agent address you name, which is what makes the two identities different keys. \
                 THIS SUBMITS A REAL TRANSACTION and cannot be undone. The wallet is shared with \
                 NO rules attached, and a wallet with no rules has no limits, so call \
                 rill_attach_rules with the id this returns before the cap is worth anything. Do \
                 not retry a success: a second call mints a second wallet and funds it again.",
                object_schema(json!({
                    "type": "object",
                    "properties": {
                        "agent": {
                            "type": "string",
                            "description": "The agent's Sui address, which receives the AgentCap. Public, never a key."
                        },
                        "amount": {
                            "type": "string",
                            "description": "Decimal SUI to fund the wallet with, as text, never a number. \"0.05\", not 0.05."
                        },
                        "budget": {
                            "type": "string",
                            "description": "Total mist the agent may ever spend from this wallet, as text. Proved on chain by the budget rule."
                        },
                        "perTx": {
                            "type": "string",
                            "description": "Most mist one transaction may release, as text. Proved on chain by the per_tx rule."
                        },
                        "days": {
                            "type": "integer",
                            "description": "How long the wallet stays usable. Defaults to 30."
                        }
                    },
                    "required": ["agent", "amount", "budget", "perTx"],
                    "additionalProperties": false
                })),
            ),
            destructive(
                "rill_attach_rules",
                "Attach the rules that bound a wallet: a total budget and a per-transaction cap, \
                 both held by the Move contract and proved against every spend. Owner-only, and \
                 the contract is what enforces that: signed with the agent's key this comes back \
                 refused, naming agent_wallet. THIS SUBMITS A REAL TRANSACTION and cannot be \
                 undone. It reads the wallet's live rules first and reconciles against them, so a \
                 retry with the same values is not a second attach; a value that differs is \
                 removed and re-added, because add_rule is not idempotent.",
                object_schema(json!({
                    "type": "object",
                    "properties": {
                        "wallet": {
                            "type": "string",
                            "description": "The AgentWallet object id, from rill_create_wallet."
                        },
                        "budget": {
                            "type": "string",
                            "description": "Total mist the agent may ever spend from this wallet, as text, never a number."
                        },
                        "perTx": {
                            "type": "string",
                            "description": "Most mist one transaction may release, as text, never a number."
                        }
                    },
                    "required": ["wallet", "budget", "perTx"],
                    "additionalProperties": false
                })),
            ),
            destructive(
                "rill_spend",
                "Release funds from an agent wallet and send them, gated by the rules the wallet \
                 carries on chain. THIS SUBMITS A REAL TRANSACTION and cannot be undone. If the \
                 amount exceeds a rule, the contract refuses it and nothing moves — that is the \
                 wallet working, not an error to retry. Do not retry a refusal with the same or a \
                 larger amount, and do not retry a success at all: a second call sends a second \
                 payment.",
                object_schema(json!({
                    "type": "object",
                    "properties": {
                        "wallet": { "type": "string", "description": "The AgentWallet object id." },
                        "cap": { "type": "string", "description": "The AgentCap this signer holds." },
                        "amount": {
                            "type": "string",
                            "description": "Decimal SUI, as text, never a number. \"0.01\", not 0.01."
                        },
                        "to": {
                            "type": "string",
                            "description": "Recipient address. Defaults to the signer."
                        }
                    },
                    "required": ["wallet", "cap", "amount"],
                    "additionalProperties": false
                })),
            ),
            destructive(
                "rill_swap",
                "Release funds from an agent wallet and swap them on Cetus, in one transaction, \
                 gated by the rules the wallet carries on chain. THIS SUBMITS A REAL TRANSACTION \
                 and cannot be undone. The SUI that enters the swap is released by the contract \
                 against its own rules, so a swap larger than a rule allows is refused by the chain \
                 and nothing moves; that is the wallet working, not an error to retry. The swap \
                 returns two coins, the one it bought and whatever it did not spend, and both are \
                 sent to this signer. The price bound is derived from the direction rather than \
                 taken from you: a bound on the wrong side aborts inside Cetus with a code that \
                 names nothing. minOut is the bound that matters and you must supply it: the \
                 wallet's rules cap what goes into the swap and nothing in them caps what comes \
                 back, so without a floor a thin pool or a sandwich returns dust and the \
                 transaction still succeeds. Do not retry a success: a second call swaps again.",
                object_schema(json!({
                    "type": "object",
                    "properties": {
                        "wallet": { "type": "string", "description": "The AgentWallet object id." },
                        "cap": { "type": "string", "description": "The AgentCap this signer holds." },
                        "amount": {
                            "type": "string",
                            "description": "Decimal SUI to release and swap, as text, never a number. \"0.001\", not 0.001."
                        },
                        "pool": { "type": "string", "description": "The Cetus pool object id." },
                        "integratePackage": {
                            "type": "string",
                            "description": "Cetus's integrate package, where router::swap lives."
                        },
                        "globalConfig": { "type": "string", "description": "Cetus's GlobalConfig object id." },
                        "coinTypeA": { "type": "string", "description": "The pool's coin A type, in the pool's own order." },
                        "coinTypeB": { "type": "string", "description": "The pool's coin B type." },
                        "a2b": {
                            "type": "boolean",
                            "description": "True to spend coin A and buy B. The wallet releases SUI, so set this to whichever side SUI is not."
                        },
                        "minOut": {
                            "type": "string",
                            "description": "The least the bought coin may hold for this swap to land, in that coin's BASE UNITS, as text. Base units and not a decimal, because the bought token's decimals are not read here: \"1000000\" is 1.0 of a six-decimal token and 0.001 of a nine-decimal one. An on-chain assertion checks the coin against this after the swap, so a worse fill aborts the whole transaction and the wallet keeps its SUI. Compute it from the quote you priced the swap against, minus the slippage you are willing to accept."
                        },
                        "acceptAnyOutput": {
                            "type": "boolean",
                            "description": "Send the swap with no floor at all, accepting whatever the pool returns. Only with minOut \"0\", and only when you mean it: the report records the swap as unprotected. Omit it and a zero minOut is refused instead."
                        }
                    },
                    "required": ["wallet", "cap", "amount", "pool", "integratePackage", "globalConfig", "coinTypeA", "coinTypeB", "a2b", "minOut"],
                    "additionalProperties": false
                })),
            ),
            destructive(
                "rill_execute",
                "Validate, byte-pin, re-simulate, sign, and submit one ExecutionEnvelope built \
                 elsewhere. Distinct from rill_spend, which builds locally: this is the path where \
                 a keyless server proposes and this signer independently re-derives everything \
                 before agreeing. THIS SUBMITS A REAL TRANSACTION and cannot be undone. An \
                 envelope this signer has already handed to the chain is refused on a second call, \
                 naming the digest of the first, so a retry cannot become a second transaction. \
                 Build a new action instead. If it refused before submitting, read why in \
                 rill_status and change what it objected to: retrying an unchanged envelope \
                 produces the same refusal.",
                object_schema(json!({
                    "type": "object",
                    "properties": { "envelope": { "type": "object" } },
                    "required": ["envelope"],
                    "additionalProperties": false
                })),
            ),
        ],
    }
}

/// Argument keys a keyless surface must never accept.
///
/// The builder holds no key and must never be talked into behaving as though it does. Comparison
/// is on a normalized key — lowercase, with separators stripped — so `private_key`, `privateKey`
/// and `PRIVATE-KEY` are one thing rather than three chances to miss.
pub const FORBIDDEN_KEYLESS_ARGUMENTS: &[&str] = &[
    "privatekey",
    "secretkey",
    "mnemonic",
    "seedphrase",
    "keypair",
    "execute",
    "force",
];

fn normalize(key: &str) -> String {
    key.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Reject any argument that asks a keyless surface to sign or to accept key material.
///
/// Recursive, because a forbidden key nested one object down is the same request wearing a hat.
pub fn assert_keyless_arguments(args: &Value) -> Result<(), String> {
    match args {
        Value::Object(map) => {
            for (key, value) in map {
                let normalized = normalize(key);
                if FORBIDDEN_KEYLESS_ARGUMENTS.contains(&normalized.as_str()) {
                    return Err(format!(
                        "\"{key}\" is not accepted here. Rill Cloud holds no key and never signs \
                         or submits; pass public identifiers only, and sign locally with rill."
                    ));
                }
                assert_keyless_arguments(value)?;
            }
            Ok(())
        }
        Value::Array(items) => items.iter().try_for_each(assert_keyless_arguments),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_declares_whether_it_modifies_anything() {
        for surface in [Surface::Actions, Surface::Wallet] {
            for tool in tools(surface) {
                let a = tool
                    .annotations
                    .as_ref()
                    .unwrap_or_else(|| panic!("{} has no annotations", tool.name));
                assert!(
                    a.read_only_hint.is_some() && a.destructive_hint.is_some(),
                    "{} must say whether it modifies anything",
                    tool.name
                );
            }
        }
    }

    /// Every tool that submits is marked destructive, and nothing else is.
    ///
    /// The list is written out rather than counted. An earlier version asserted there was exactly
    /// one, which failed the moment a second submitting tool was added correctly — a test that
    /// makes the right change look like a break trains people to edit the test without reading it.
    /// Naming them means adding a submitting tool without its annotation still fails, which is the
    /// thing worth catching: an agent decides whether to ask a human from this flag.
    #[test]
    fn every_submitting_tool_is_marked_destructive_and_no_other_is() {
        const SUBMITS: &[&str] = &[
            "rill_create_wallet",
            "rill_attach_rules",
            "rill_spend",
            "rill_swap",
            "rill_execute",
        ];

        let destructive: Vec<String> = [Surface::Actions, Surface::Wallet]
            .into_iter()
            .flat_map(tools)
            .filter(|t| t.annotations.as_ref().unwrap().destructive_hint == Some(true))
            .map(|t| t.name.to_string())
            .collect();
        assert_eq!(destructive, SUBMITS);

        // And the inverse: nothing that submits is left unmarked.
        for tool in [Surface::Actions, Surface::Wallet]
            .into_iter()
            .flat_map(tools)
        {
            let submits = SUBMITS.contains(&tool.name.as_ref());
            let marked = tool.annotations.as_ref().unwrap().destructive_hint == Some(true);
            assert_eq!(
                submits, marked,
                "{} submits={submits} but is marked destructive={marked}",
                tool.name
            );
        }
    }

    /// Every step from no wallet to a bounded spend is a tool, not a command somebody types.
    ///
    /// R3 is not "a spend tool exists". Creating a wallet and attaching its rules used to be
    /// commands run by hand, so the flow an agent could drive started halfway through: it could
    /// spend from a wallet and could not get one. A surface missing either step sends whoever is
    /// checking the claim back to a terminal.
    #[test]
    fn the_wallet_surface_offers_every_step_from_no_wallet_to_a_bounded_spend() {
        let names: Vec<String> = tools(Surface::Wallet)
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        for step in ["rill_create_wallet", "rill_attach_rules", "rill_spend"] {
            assert!(
                names.contains(&step.to_string()),
                "{step} is missing, so the flow still needs a human at a terminal: {names:?}"
            );
        }
    }

    /// The keyless surface gains nothing from the flow above. It holds no key, so it could not
    /// complete any of it, and offering a tool it cannot finish is worse than not offering one.
    #[test]
    fn the_two_surfaces_differ_by_exactly_the_tools_that_need_a_key() {
        let actions: Vec<String> = tools(Surface::Actions)
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        for signing in [
            "rill_create_wallet",
            "rill_attach_rules",
            "rill_spend",
            "rill_execute",
        ] {
            assert!(
                !actions.contains(&signing.to_string()),
                "{signing} needs a key and the builder has none"
            );
        }
    }

    /// A tool that moves money must say so in words, not only in a flag an agent may not read.
    #[test]
    fn every_destructive_tool_says_it_cannot_be_undone() {
        for tool in [Surface::Actions, Surface::Wallet]
            .into_iter()
            .flat_map(tools)
        {
            if tool.annotations.as_ref().unwrap().destructive_hint != Some(true) {
                continue;
            }
            let description = tool.description.as_deref().unwrap_or_default();
            assert!(
                description.contains("cannot be undone"),
                "{} submits a real transaction and its description does not say it is irreversible",
                tool.name
            );
            assert!(
                description.to_lowercase().contains("retry"),
                "{} must tell an agent what not to retry; a money tool is not idempotent",
                tool.name
            );
        }
    }

    /// The words that name a scoping rule. None of these is enforced on chain, and no description
    /// may say otherwise.
    const SCOPING_WORDS: &[&str] = &["destination", "protocol", "recipient", "asset"];

    /// The sentences of a description that name a scoping rule, lowercased.
    fn scoping_sentences(description: &str) -> Vec<String> {
        description
            .to_lowercase()
            .split(". ")
            .filter(|sentence| SCOPING_WORDS.iter().any(|word| sentence.contains(word)))
            .map(str::to_owned)
            .collect()
    }

    /// A snapshot of the wallet read's description, checked for the claim it used to make.
    ///
    /// The check is not vacuous: the description must name both layers, and must name the
    /// scoping rules somewhere, so that the sentence-level assertion has sentences to examine. A
    /// description that avoided the words entirely would fail here rather than pass by silence.
    #[test]
    fn the_wallet_read_description_never_puts_a_scoping_rule_on_chain() {
        let description = tools(Surface::Wallet)
            .into_iter()
            .find(|t| t.name == "rill_wallet")
            .and_then(|t| t.description.map(|d| d.to_string()))
            .expect("rill_wallet has a description");

        assert!(
            description.contains("on-chain") && description.contains("pre-flight"),
            "both layers must be named: {description}"
        );

        let sentences = scoping_sentences(&description);
        assert!(
            !sentences.is_empty(),
            "the description must name the scoping rules, or the check below checks nothing"
        );
        for sentence in sentences {
            assert!(
                !claims_the_chain(&sentence),
                "a scoping rule is placed on chain: {sentence:?}"
            );
            assert!(
                attributes_the_rule(&sentence),
                "a scoping rule is named without saying who holds it: {sentence:?}"
            );
        }
    }

    /// Words that put a rule on the chain. Checked case-insensitively and with "the" optional,
    /// because "on chain", "on-chain" and "on the chain" are the same claim.
    const CHAIN_WORDS: [&str; 4] = ["on chain", "on-chain", "on the chain", "move contract"];

    /// Whether a sentence hands a rule to the chain.
    ///
    /// A sentence that names the chain in order to deny it holds the rule ("nothing on the chain
    /// checks a recipient") is the opposite claim and the strongest form of the correction this
    /// whole unit is about, so it is not a violation. Anything else naming the chain is.
    fn claims_the_chain(sentence: &str) -> bool {
        let lower = sentence.to_lowercase();
        if lower.contains("nothing on") {
            return false;
        }
        CHAIN_WORDS.iter().any(|word| lower.contains(word))
    }

    /// Whether a sentence says who does hold the rule, or says plainly that the chain does not.
    fn attributes_the_rule(sentence: &str) -> bool {
        let lower = sentence.to_lowercase();
        lower.contains("pre-flight") || lower.contains("signer") || lower.contains("nothing on")
    }

    /// The same rule for every tool on both surfaces, so the claim cannot move to another
    /// description and survive.
    #[test]
    fn no_tool_description_puts_a_scoping_rule_on_chain() {
        for tool in [Surface::Actions, Surface::Wallet]
            .into_iter()
            .flat_map(tools)
        {
            let description = tool.description.as_deref().unwrap_or_default();
            for sentence in scoping_sentences(description) {
                // Every way a sentence can put a rule on the chain, not only the two phrasings
                // that were in the text when this was written. "the Move contract enforces the
                // recipient allowlist" names neither "on chain" nor "on-chain" and would have
                // passed the narrower check while making exactly the claim it exists to stop.
                assert!(
                    !claims_the_chain(&sentence),
                    "{} puts a scoping rule on the chain: {sentence:?}",
                    tool.name
                );
            }
        }
    }

    /// The keyless surface must offer nothing that can spend.
    #[test]
    fn the_builder_surface_is_entirely_read_only() {
        for tool in tools(Surface::Actions) {
            assert_eq!(
                tool.annotations.as_ref().unwrap().read_only_hint,
                Some(true),
                "{} is on the keyless surface and must not modify anything",
                tool.name
            );
        }
    }

    #[test]
    fn every_tool_name_is_namespaced() {
        for surface in [Surface::Actions, Surface::Wallet] {
            for tool in tools(surface) {
                assert!(
                    tool.name.starts_with("rill_"),
                    "{} could collide with another connected server",
                    tool.name
                );
            }
        }
    }

    #[test]
    fn every_schema_refuses_unknown_arguments() {
        for surface in [Surface::Actions, Surface::Wallet] {
            for tool in tools(surface) {
                assert_eq!(
                    tool.input_schema.get("additionalProperties"),
                    Some(&Value::Bool(false)),
                    "{} accepts arguments nobody declared",
                    tool.name
                );
            }
        }
    }

    #[test]
    fn key_material_is_refused_however_it_is_spelled() {
        for spelling in [
            "privateKey",
            "private_key",
            "PRIVATE-KEY",
            "SecretKey",
            "mnemonic",
        ] {
            let args = json!({ spelling: "suiprivkey1..." });
            assert!(
                assert_keyless_arguments(&args).is_err(),
                "{spelling} must be refused"
            );
        }
    }

    #[test]
    fn a_forbidden_key_nested_deeper_is_still_refused() {
        let args = json!({ "params": { "wallet": { "keypair": "..." } } });
        assert!(
            assert_keyless_arguments(&args).is_err(),
            "nesting is not a disguise"
        );
    }

    #[test]
    fn asking_the_builder_to_execute_is_refused() {
        assert!(assert_keyless_arguments(&json!({ "execute": true })).is_err());
        assert!(assert_keyless_arguments(&json!({ "force": true })).is_err());
    }

    #[test]
    fn ordinary_arguments_pass() {
        let args = json!({
            "actionId": "skill_abc",
            "params": { "price": "2.5", "quantity": "1", "poolKey": "SUI_DBUSDC" }
        });
        assert!(assert_keyless_arguments(&args).is_ok());
    }
}
