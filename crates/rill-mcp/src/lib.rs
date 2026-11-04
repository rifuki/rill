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
    /// The local signer as an agent runs it. Holds the agent's key: reads, quotes, spends and
    /// executes inside the wallet's rules, and offers none of the owner's steps.
    Wallet,
    /// The local signer started with `--owner`: everything on [`Surface::Wallet`] plus the two
    /// steps only a wallet's owner can take, minting a wallet and attaching its rules.
    ///
    /// Separate because an agent offered `rill_create_wallet` will reach for it. Signed with the
    /// agent's key it mints a wallet the agent owns and funds from its own gas, so the agent ends up
    /// holding both halves of a delegation whose point is that it holds one.
    Owner,
}

/// The tools only a wallet's owner can use: on [`Surface::Owner`], never on [`Surface::Wallet`].
pub const OWNER_TOOLS: &[&str] = &["rill_create_wallet", "rill_attach_rules"];

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

/// The tools that build their transaction afresh on every call, and so take an `idempotencyKey`.
///
/// A retry of one of these is a second transaction: new gas, new object versions, new bytes, so
/// nothing on chain tells it from a second intent. Only the caller knows which it meant, and the
/// key is how it says so. `rill_execute` re-signs the exact bytes it is given, which the chain
/// refuses to land twice, `rill_attach_rules` reconciles against the live rules, and
/// `rill_run_workflow` has its own `runId`, so those three take none.
pub const KEYED_TOOLS: &[&str] = &[
    "rill_create_wallet",
    "rill_spend",
    "rill_swap",
    "rill_stake",
    "rill_unstake",
    "rill_run_action",
];

/// What every keyed tool's description says about retrying, so the rule reads the same on each.
macro_rules! keyed_retry {
    () => {
        "A retry with the same idempotencyKey returns the first outcome, including after a \
         restart, and never submits again. An outcome reported as interrupted means check the \
         chain before using a new key."
    };
}

/// The schema property every keyed tool requires.
fn idempotency_key_property() -> Value {
    json!({
        "type": "string",
        "pattern": "^[A-Za-z0-9_-]{1,96}$",
        "description": "Required. One value per intended transaction, chosen before the first call. Send the same value on every retry of that transaction: a repeat returns the first outcome and never submits again. A new value is a new transaction."
    })
}

/// A tool that submits a transaction it builds itself, guarded by a caller-chosen key.
///
/// The schema gains an `idempotencyKey`, required unless the tool has a dry run, keeping any
/// wording the tool already declares for it.
fn destructive_keyed(name: &'static str, description: &'static str, mut schema: Value) -> Tool {
    if let Some(properties) = schema["properties"].as_object_mut() {
        properties
            .entry("idempotencyKey")
            .or_insert_with(idempotency_key_property);
    }
    // A tool with a dry run cannot list the key as required: a dry run submits nothing and takes
    // none, and a schema-validating client would refuse it before the signer saw it. The signer
    // refuses a submitting call without the key with missing_idempotency_key instead.
    let has_dry_run = schema["properties"].get("dryRun").is_some();
    if let Some(required) = schema["required"].as_array_mut().filter(|_| !has_dry_run) {
        required.push(json!("idempotencyKey"));
    }
    Tool::new(
        Cow::Borrowed(name),
        Cow::Borrowed(description),
        object_schema(schema),
    )
    .annotate(
        ToolAnnotations::new()
            .read_only(false)
            .destructive(true)
            // The same arguments, idempotencyKey included, return the first outcome and never
            // submit again.
            .idempotent(true)
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
        Surface::Actions => surface_tools(Surface::Actions),
        Surface::Owner => surface_tools(Surface::Owner),
        Surface::Wallet => surface_tools(Surface::Owner)
            .into_iter()
            .filter(|t| !OWNER_TOOLS.contains(&t.name.as_ref()))
            .collect(),
    }
}

fn surface_tools(surface: Surface) -> Vec<Tool> {
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
                                "coinType": { "type": "string" },
                                "versionId": { "type": "string" },
                                "capabilityManifest": { "type": "object" },
                                "protectedSwap": {
                                    "type": "object",
                                    "properties": {
                                        "adapterPackageId": {"type":"string"},
                                        "revision": {"type":"integer","minimum":1},
                                        "owner": {"type":"string"}
                                    },
                                    "required": ["adapterPackageId", "revision", "owner"],
                                    "additionalProperties": false
                                }
                            },
                            "required": [
                                "packageId", "walletId", "capId",
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
        Surface::Wallet | Surface::Owner => vec![
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
                 scope, recipient allowlist and slippage floor are pre-flight rules, listed only \
                 when a run set is loaded from a file for this wallet: this signer refuses to sign \
                 a breach of a listed rule only on the tools it lists under signerRefusesOn \
                 (protocol_scope on rill_execute; slippage_floor on rill_swap; asset_scope on \
                 rill_spend, rill_swap, rill_stake; recipient_allowlist on rill_spend), and the \
                 Rill builder checks them when it compiles an action. rill_run_action and \
                 rill_run_workflow check the granted action's own run set instead, which this \
                 read does not list, so a null list says no file is loaded, not that no grant \
                 binds the wallet. Nothing \
                 on the chain checks a protocol, an asset, a recipient or a manifest's slippage \
                 floor. rill_swap's own minOut is a Move call on the coin the swap bought, so the \
                 chain aborts a fill below it. Read \
                 largestSpendNow first: it is the biggest single spend this wallet allows right \
                 now and names the limit holding it, so a spend that would be refused need not be \
                 attempted.",
                object_schema(json!({
                    "type": "object",
                    "properties": {
                        "wallet": { "type": "string", "description": "The AgentWallet object id." }
                    },
                    "required": ["wallet"],
                    "additionalProperties": false
                })),
            ),
            // The read that makes rill_swap's minOut answerable.
            //
            // Requiring a floor and leaving a caller no way to compute one is how a required field
            // becomes a guess. An agent has no price source of its own, so without this the honest
            // choices were to guess the floor or escape it with acceptAnyOutput, and the second is
            // the outcome the floor exists to prevent.
            read_only(
                "rill_quote",
                "What a swap would return at the pool's price right now, and the minOut to pass to \
                 rill_swap. Call this immediately before rill_swap: it reads the pool, so it also \
                 answers the things you would otherwise have to be told, and `swapArguments` in the \
                 result is the rest of that call ready to pass through. The coin types and the \
                 direction come from the pool itself, so they cannot be given the wrong way round. \
                 The estimate is the price at a single tick: a swap large enough to cross a tick \
                 boundary fills worse, and a pool that moves between this read and the swap fills \
                 differently again, which is what the floor is for. If a swap is refused with \
                 E_SLIPPAGE, widen slippageBps and quote again; do not remove the floor. This reads \
                 only and submits nothing.",
                object_schema(json!({
                    "type": "object",
                    "properties": {
                        "wallet": { "type": "string", "description": "The AgentWallet that would fund the swap. A quote simulates the gated spend, so it needs this." },
                        "cap": { "type": "string", "description": "The AgentCap this signer holds for that wallet." },
                        "pool": { "type": "string", "description": "The Cetus pool object id. One side of it must be SUI, since that is what an agent wallet releases. The coin types and the direction are read from the pool, so you do not supply them." },
                        "amount": {
                            "type": "string",
                            "description": "Decimal SUI to be released and swapped, as text, never a number. \"0.001\", not 0.001."
                        },
                        "slippageBps": {
                            "type": "string",
                            "description": "How far below the quote the floor should sit, in basis points, as text. \"100\" is one percent. Defaults to \"100\". Widen it for a thin pool or a large trade rather than dropping the floor."
                        }
                    },
                    "required": ["wallet", "cap", "pool", "amount"],
                    "additionalProperties": false
                })),
            ),
            // The two owner-side steps, which used to be commands a person typed. Offered only on
            // `Surface::Owner`: see its doc for why an agent's surface leaves them out.
            destructive_keyed(
                "rill_create_wallet",
                concat!("Mint an agent wallet and the AgentCap that drives it, funded from the key this \
                 signer holds. The key here becomes the wallet's owner and the cap goes to the \
                 agent address you name, which is what makes the two identities different keys. \
                 THIS SUBMITS TWO REAL TRANSACTIONS and cannot be undone: one mints and funds the \
                 wallet, the second attaches the budget and perTx rules you pass, because a wallet \
                 cannot have rules added in the transaction that creates it. A success means both \
                 landed and the wallet is bounded. If the second fails the answer is an error \
                 named created_but_unbounded, carrying the wallet id: the wallet then exists with \
                 no limit, so do not hand the cap to anything and call rill_attach_rules with that \
                 id. ", keyed_retry!()),
                json!({
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
                }),
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
                        },
                        "amount": {
                            "type": "string",
                            "description": "Optional. Decimal SUI as text to fund the wallet with in the same transaction. Only for a wallet that is still empty, with nothing spent and no funding yet: the way to finish a wallet from rill_create_wallet whose own attach did not run."
                        }
                    },
                    "required": ["wallet", "budget", "perTx"],
                    "additionalProperties": false
                })),
            ),
            destructive_keyed(
                "rill_spend",
                concat!("Release funds from an agent wallet and send them, gated by the rules the wallet \
                 carries on chain. THIS SUBMITS A REAL TRANSACTION and cannot be undone. If the \
                 amount exceeds a rule, the contract refuses it and nothing moves — that is the \
                 wallet working, not an error to retry. On a wallet this signer holds a usable owner grant for, it refuses with \
                 use_granted_action: run the granted action with rill_run_action instead. grants_unavailable means this signer could not tell whether a grant applies \
                 because the API or the node did not answer; nothing moved, so report it. \
                 pre_flight_refused names a rule from the run set this signer holds for the \
                 wallet; report it rather than retrying. Do not retry a refusal \
                 with the same or a larger amount. ", keyed_retry!()),
                json!({
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
                }),
            ),
            destructive_keyed(
                "rill_swap",
                concat!("Release funds from an agent wallet and swap them on Cetus, in one transaction, \
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
                 transaction still succeeds. On a wallet this signer holds a usable owner grant for, it refuses with \
                 use_granted_action: run the granted action with rill_run_action instead. grants_unavailable means this signer could not tell whether a grant applies \
                 because the API or the node did not answer; nothing moved, so report it. \
                 pre_flight_refused names a rule from the run set this signer holds for the \
                 wallet; report it rather than retrying. ", keyed_retry!()),
                json!({
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
                            "description": "Optional. Cetus's integrate package, where router::swap lives. Defaults to the deployment for this network, so omit it unless you are pointing at another one."
                        },
                        "globalConfig": { "type": "string", "description": "Optional. Cetus's GlobalConfig object id. Defaults like integratePackage." },
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
                    "required": ["wallet", "cap", "amount", "pool", "coinTypeA", "coinTypeB", "a2b", "minOut"],
                    "additionalProperties": false
                }),
            ),
            destructive_keyed(
                "rill_stake",
                concat!("Release SUI from an agent wallet and liquid-stake it with Haedal, in one \
                 transaction, gated by the rules the wallet carries on chain. THIS SUBMITS A REAL \
                 TRANSACTION and cannot be undone. The SUI is released by the contract against its \
                 own rules, so a stake larger than a rule allows is refused by the chain and nothing \
                 moves; read rill_wallet's largestSpendNow first. Haedal's minimum is 1 SUI and a \
                 smaller amount is refused before anything is built. The haSUI Haedal mints goes to \
                 this signer, not back to the wallet. There is no minOut: Haedal mints at its own \
                 exchange rate, which a sandwich cannot move inside the transaction, and the report \
                 shows what arrived. On a wallet this signer holds a usable owner grant for, it refuses with \
                 use_granted_action: run the granted action with rill_run_action instead. grants_unavailable means this signer could not tell whether a grant applies \
                 because the API or the node did not answer; nothing moved, so report it. \
                 pre_flight_refused names a rule from the run set this signer holds for the \
                 wallet; report it rather than retrying. ", keyed_retry!()),
                json!({
                    "type": "object",
                    "properties": {
                        "wallet": { "type": "string", "description": "The AgentWallet object id." },
                        "cap": { "type": "string", "description": "The AgentCap this signer holds." },
                        "amount": {
                            "type": "string",
                            "description": "Decimal SUI to release and stake, as text, never a number. At least \"1\"."
                        },
                        "validator": {
                            "type": "string",
                            "description": "Optional. The validator to delegate to. Omit it, or pass 0x0, to let Haedal choose, which is what its testnet deployment expects since it delegates to none of its own yet."
                        }
                    },
                    "required": ["wallet", "cap", "amount"],
                    "additionalProperties": false
                }),
            ),
            read_only(
                "rill_portfolio",
                "Read directly owned token balances and objects for an address, defaulting to this signer. Exact base units; excludes shared vault funds and external lending positions. No transaction or signature.",
                object_schema(json!({"type":"object","properties":{"owner":{"type":"string"}},"additionalProperties":false})),
            ),
            destructive_keyed(
                "rill_unstake",
                concat!("Immediately redeem this signer's own Haedal haSUI for SUI. THIS SUBMITS A REAL TRANSACTION and cannot be undone. Requires a positive minOut in decimal SUI and enforces it on chain. The protocol charges its live instant-redemption fee and may lack liquidity. Delayed unstaking creates a ticket and needs a later epoch claim; this tool does not do that. Simulates before signing. Receiver defaults to signer. Mainnet only. ", keyed_retry!()),
                json!({"type":"object","properties":{
                    "amount":{"type":"string","description":"Positive decimal haSUI amount."},
                    "minOut":{"type":"string","description":"Positive minimum decimal SUI after protocol fee."},
                    "receiver":{"type":"string","description":"Public Sui address receiving redeemed SUI; defaults to signer."},
                    "dryRun":{"type":"boolean","description":"Simulate without submitting; defaults false."},
                    "idempotencyKey":{"type":"string","pattern":"^[A-Za-z0-9_-]{1,96}$","description":"Required to submit. One value per intended redemption, chosen before the first call. Send the same value on every retry of that redemption: a repeat returns the first outcome and never submits again. A new value is a new transaction. A dry run (dryRun true) submits nothing, so it ignores the key."}
                },"required":["amount","minOut"],"additionalProperties":false}),
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
                 produces the same refusal. On a wallet this signer holds a usable owner grant \
                 for, an envelope passed here is refused with use_granted_action: run the granted \
                 action with rill_run_action instead. grants_unavailable means this signer could not tell whether a grant applies \
                 because the API or the node did not answer; nothing moved, so report it.",
                object_schema(json!({
                    "type": "object",
                    "properties": { "envelope": { "type": "object" } },
                    "required": ["envelope"],
                    "additionalProperties": false
                })),
            ),
            read_only(
                "rill_actions",
                "List the actions this signer may run: every grant the owner of a wallet signed for \
                 this agent, each checked against the chain (the owner's signature, the wallet still \
                 naming this agent, not revoked, not expired). Unusable grants are listed with the \
                 reason. Call this first to learn an actionId for rill_run_action. Reads only.",
                no_arguments(),
            ),
            destructive_keyed(
                "rill_run_action",
                concat!("Run one action the wallet's owner granted this agent: the Rill builder prepares the \
                 transaction, and this signer checks it against the owner-signed run set, \
                 re-simulates it, signs and submits. THIS SUBMITS A REAL TRANSACTION and cannot be \
                 undone. A refusal naming a rule or the grant is the wallet working; report it \
                 rather than retrying. Every call builds a fresh transaction, so only the key \
                 tells a retry from a second run. ", keyed_retry!(), " Above the owner's step-up \
                 threshold (stepUpAboveBaseUnits in rill_actions) this answers \
                 pending_owner_approval with code step_up_required and submits nothing: give the \
                 owner approvalUrl, then call again with the same actionId, the approvalId and a \
                 NEW idempotencyKey, since the first key's outcome is that pause. The owner has \
                 the 5 minutes the transaction is valid for; after that step_up_expired means run \
                 the action again. The signer submits only after it verifies the current owner's \
                 signature over that exact transaction. This pause binds this tool and \
                 rill_run_workflow; whoever can read the agent's key is bound only by the \
                 wallet's on-chain rules."),
                json!({
                    "type": "object",
                    "properties": {
                        "actionId": { "type": "string", "description": "The actionId from rill_actions." },
                        "walletId": { "type": "string", "description": "Optional. Which wallet's grant to use, when more than one wallet granted this action." },
                        "revision": { "type": "integer", "minimum": 1, "description": "Optional exact owner-signed grant revision." },
                        "params": { "type": "object", "description": "Optional. Runtime values keyed by node id, as the action's build_action schema allows, for example a tighter minimum output. Amounts are decimal text." },
                        "approvalId": { "type": "string", "pattern": "^[A-Za-z0-9_-]{1,96}$", "description": "Optional. The approvalId a step_up_required answer gave, once the owner has approved it in Studio. Runs exactly the transaction the owner approved; pass it with actionId and a new idempotencyKey only." }
                    },
                    "required": ["actionId"],
                    "additionalProperties": false
                }),
            ),
            destructive(
                "rill_run_workflow",
                "Run an ordered workflow of separately approved actions. THIS SUBMITS REAL TRANSACTIONS and cannot be undone. Every step pins its grant revision and vault, using one owner, network and signer. Stops on failure, preserves earlier successes, and does not reinvest outputs. A durable runId receipt prevents resubmission across restarts. Retry the SAME runId to inspect results; never create a fresh runId to retry uncertain or successful steps.",
                object_schema(json!({
                    "type":"object", "additionalProperties":false,
                    "required":["runId","network","signer","owner","steps"],
                    "properties":{
                        "runId":{"type":"string","pattern":"^[A-Za-z0-9_-]{1,96}$"},
                        "network":{"type":"string","enum":["mainnet","testnet"]},
                        "signer":{"type":"string"}, "owner":{"type":"string"},
                        "steps":{"type":"array","minItems":1,"maxItems":10,"items":{
                            "type":"object","additionalProperties":false,"required":["actionId","walletId","revision"],
                            "properties":{"actionId":{"type":"string"},"walletId":{"type":"string"},"revision":{"type":"integer","minimum":1},"params":{"type":"object"}}
                        }}
                    }
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
            "rill_stake",
            "rill_unstake",
            "rill_execute",
            "rill_run_action",
            "rill_run_workflow",
        ];

        let destructive: Vec<String> = [Surface::Actions, Surface::Owner]
            .into_iter()
            .flat_map(tools)
            .filter(|t| t.annotations.as_ref().unwrap().destructive_hint == Some(true))
            .map(|t| t.name.to_string())
            .collect();
        assert_eq!(destructive, SUBMITS);

        // And the inverse: nothing that submits is left unmarked.
        for tool in [Surface::Actions, Surface::Owner]
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
    fn the_owner_surface_offers_every_step_from_no_wallet_to_a_bounded_spend() {
        let names: Vec<String> = tools(Surface::Owner)
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

    /// An agent's surface offers no owner step, and loses nothing else.
    ///
    /// Run by an agent, `rill_create_wallet` mints a wallet the agent owns: it holds both halves of
    /// the delegation. The owner's steps are on `Surface::Owner`, which is a flag somebody passes.
    #[test]
    fn the_agent_surface_is_the_owner_surface_without_the_owner_steps() {
        let wallet: Vec<String> = tools(Surface::Wallet)
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        let owner: Vec<String> = tools(Surface::Owner)
            .into_iter()
            .map(|t| t.name.to_string())
            .filter(|name| !OWNER_TOOLS.contains(&name.as_str()))
            .collect();
        assert_eq!(wallet, owner);
        for step in OWNER_TOOLS {
            assert!(
                !wallet.contains(&step.to_string()),
                "{step} is on the agent's surface"
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

    /// The tools that build afresh on every call cannot tell a retry from a second intent, so each
    /// one takes a caller-chosen key. The three that need none keep saying they are not idempotent.
    #[test]
    fn every_rebuilding_submitter_requires_an_idempotency_key() {
        let tools: Vec<Tool> = tools(Surface::Owner);
        let find = |name: &str| {
            tools
                .iter()
                .find(|t| t.name == name)
                .unwrap_or_else(|| panic!("{name} is not offered"))
        };
        for name in KEYED_TOOLS {
            let tool = find(name);
            let schema = &tool.input_schema;
            let required = schema["required"]
                .as_array()
                .is_some_and(|r| r.contains(&json!("idempotencyKey")));
            if schema["properties"].get("dryRun").is_some() {
                // A dry run submits nothing and needs no key, so the schema cannot demand one on
                // every call; the signer refuses a submitting call without it instead.
                assert!(!required, "{name} must not demand a key for a dry run");
                assert!(
                    schema["properties"]["idempotencyKey"]["description"]
                        .as_str()
                        .is_some_and(|d| d.starts_with("Required to submit")),
                    "{name} must say the key is required to submit"
                );
            } else {
                assert!(required, "{name} must require idempotencyKey");
            }
            assert_eq!(
                schema["properties"]["idempotencyKey"]["pattern"],
                json!("^[A-Za-z0-9_-]{1,96}$"),
                "{name}"
            );
            assert_eq!(
                tool.annotations.as_ref().unwrap().idempotent_hint,
                Some(true),
                "{name}"
            );
            let description = tool.description.as_deref().unwrap_or_default();
            assert!(
                description.contains("same idempotencyKey"),
                "{name} must say how to retry: {description}"
            );
        }
        for name in ["rill_execute", "rill_attach_rules", "rill_run_workflow"] {
            let tool = find(name);
            assert_eq!(
                tool.annotations.as_ref().unwrap().idempotent_hint,
                Some(false),
                "{name}"
            );
            assert!(
                tool.input_schema["properties"]
                    .get("idempotencyKey")
                    .is_none(),
                "{name} takes no idempotencyKey"
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

    /// The description names, for each pre-flight rule, exactly the tools the producer says refuse
    /// it. Written out in the description because an agent reads it before any call; checked here
    /// against the table so the two cannot drift.
    #[test]
    fn the_wallet_read_description_names_the_tools_that_refuse_each_pre_flight_rule() {
        use rill_core::manifest::{Enforcement, RuleKind};
        let description = tools(Surface::Wallet)
            .into_iter()
            .find(|t| t.name == "rill_wallet")
            .and_then(|t| t.description.map(|d| d.to_string()))
            .expect("rill_wallet has a description");
        for kind in RuleKind::all().filter(|k| k.enforcement() == Enforcement::PreFlight) {
            let label = format!("{} on ", kind.module());
            let at = description
                .find(&label)
                .unwrap_or_else(|| panic!("{} is not listed: {description}", kind.module()));
            let rest = &description[at + label.len()..];
            let listed = &rest[..rest.find([';', ')']).expect("the list ends")];
            // The rules this read lists come from the loaded run-set file. rill_run_action and
            // rill_run_workflow swap in the grant's run set, so they never check these.
            let expected = kind
                .signer_tool_names()
                .into_iter()
                .filter(|tool| !["rill_run_action", "rill_run_workflow"].contains(tool))
                .collect::<Vec<_>>()
                .join(", ");
            assert_eq!(listed, expected, "{}", kind.module());
        }
        assert!(
            description.contains(
                "rill_run_action and rill_run_workflow check the granted action's own run set"
            ),
            "{description}"
        );
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
