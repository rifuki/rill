# Rill

**The transaction layer for AI agents on Sui** — in Rust.

An agent can transact with any Sui protocol without inventing parameters or risking the whole
wallet. The server builds and simulates transactions **without ever holding a key**; a local
binary holds the key and trusts nothing the server sends without re-deriving it independently;
two on-chain Move contracts bound every action.

## Where things stand

The workspace builds and 359 Rust tests pass. Another 32, which need a live fullnode, are
`#[ignore]` and run explicitly (see [Build](#build)). The two Move packages pass their own 36
and 2.

What is proven, on testnet, each with its digest recorded in this repository:

| | |
|---|---|
| owner creates a wallet and names a different key as its agent | `5vShErFjcfE7TWGeygrBvB2Fqru8jxfLfA71njznyypj` |
| owner attaches rules, an owner-only call | `Gupv6mCdiMREfo16sQUT1HLEoGgGSBZVZpXtug5fNJyr` |
| the agent spends within them | `xh6cWff3sjxXuo7fxqKtqfUFsdtcAYCHRseqKdL9pKC` |
| the owner attempts the same spend | refused before any Move code runs: the `AgentCap` is the agent's |
| a spend over the per-transaction cap | refused, and the refusal names `per_tx` |
| a spend driven over MCP by `rill_spend` | `DpTPdMKbDSndfAqekmX8EUFyDdYePAk338Y9fqgmWhmW` |
| `rill_execute` running the whole path: validate, byte-pin, re-simulate, sign, submit | commit `06df501` |
| owner revokes; the agent's next spend, same key, same capability, is refused | `7b6xSFWQuJW3fRpZ77een1KuuwEnmzSfKspzFeUWdr15` |
| a gated spend flowing into a DeepBook order, in one agent-signed transaction | `GiL7unaYVnx7TF9QDtpUgc3nFSdWxVgkLb6sMDQfCm77` |

The addresses and objects behind these are in `docs/OVERNIGHT.md`, and the sequence that produced
each digest is in the commit that records it.

The Bun/TypeScript implementation this replaces is the **specification**. Its behaviour is
re-expressed here, with conformance fixtures in `fixtures/` checked against it by
`ts/verify-reference.ts`.

## Install

The release is produced by a tag on `github.com/rifuki/rill`, and that is the one origin that
publishes this binary. Pushing a `v*` tag there runs `.github/workflows/release.yaml`, which builds
one asset per platform, checks that each one starts, and attaches it with a checksum beside it.
Nothing else publishes `rill-wallet`; an install line that names any other repository is wrong.

```sh
# macOS, Apple silicon
curl -fsSLO https://github.com/rifuki/rill/releases/latest/download/rill-wallet-darwin-arm64
curl -fsSLO https://github.com/rifuki/rill/releases/latest/download/rill-wallet-darwin-arm64.sha256
shasum -a 256 -c rill-wallet-darwin-arm64.sha256
chmod +x rill-wallet-darwin-arm64 && mv rill-wallet-darwin-arm64 rill-wallet

# macOS, Intel
curl -fsSLO https://github.com/rifuki/rill/releases/latest/download/rill-wallet-darwin-x64
curl -fsSLO https://github.com/rifuki/rill/releases/latest/download/rill-wallet-darwin-x64.sha256
shasum -a 256 -c rill-wallet-darwin-x64.sha256
chmod +x rill-wallet-darwin-x64 && mv rill-wallet-darwin-x64 rill-wallet

# Linux, x86_64
curl -fsSLO https://github.com/rifuki/rill/releases/latest/download/rill-wallet-linux-x64
curl -fsSLO https://github.com/rifuki/rill/releases/latest/download/rill-wallet-linux-x64.sha256
shasum -a 256 -c rill-wallet-linux-x64.sha256
chmod +x rill-wallet-linux-x64 && mv rill-wallet-linux-x64 rill-wallet

./rill-wallet --status
```

The `.sha256` file is exactly what `shasum -a 256 <asset>` printed on the build runner, one line of
`<hex>  <asset>`, so `shasum -a 256 -c` checks it against the file of that name in the current
directory: run the two downloads and the check from the same place. On a Linux box without
`shasum`, `sha256sum -c rill-wallet-linux-x64.sha256` reads the same format.

`--status` prints `rill-wallet` on its first line and then whether it can sign. With no key it says
`not ready` and exits 1: that is the expected answer on a fresh machine, not a broken download. The
key comes from `RILL_SUI_PRIVATE_KEY` or the `sui` CLI's own keystore, never from an argument.

### Which tag produces a release, and the series it continues

Tags are `rill-wallet-v<version>`. The series did not start here: `rill-wallet-v0.2.0` was published
on 2026-07-19 from the TypeScript repository, carrying these same three asset names, so the next
release from this repository is `rill-wallet-v0.3.0` rather than a restart. The workflow's tag
filter accepts that spelling and a bare `v*`, and its publish job is gated on the same two, because
a tag that builds the assets and attaches them to nothing is the failure that looks most like
success.

One thing is still inconsistent, and saying so is cheaper than a stranger finding it. The
instruction generators in the TypeScript repository still hand agents a download URL on the old
origin, which will not carry releases built from this source. Moving them is owed by U13 of
`docs/plans/2026-09-10-001-feat-rill-operational-mcp-plan.md`; until that lands, the install lines
above are the ones that resolve.

## Why a rebuild rather than a port

Three problems in the TypeScript version are one problem: invariants the code documents but the
language does not enforce.

- **A float reached the money path**, in a codebase whose own stated rule is that no IEEE-754
  value may touch a token amount. `@mysten/deepbook-v3` converts a price with
  `BigInt(Math.round(value * floatScalar * quoteScalar / baseScalar))`, and the order price and
  quantity arrive as `z.number()`. Divergence is reproducible at `2362.123456` on a 1e12-multiplier
  pool. Every one of its 746 tests passed.
- **The reading side has the same defect.** `midPrice` ends
  `Number(bcs.U64.parse(bytes)) * baseScalar / quoteScalar / FLOAT_SCALAR`, so a price read off the
  order book has been through a double before it is used — and the usual next step feeds it back
  in as an order price, through a second one.
- **The signer and the contract disagree about which deployment they mean.** Not stale prose: two
  packages are deployed, one generation apart, and the repo's documents point at different ones.
  See [Known gaps](#known-gaps).

Here each is structurally impossible: integer-only money types with no `f64` constructor,
validation state carried in the type so an unchecked envelope cannot reach `sign()`, and a single
declaration producer. CI fails the build on an `f32`/`f64` anywhere outside a comment, and on any
I/O dependency reaching `rill-core`.

## Layout

| Path | Role |
|---|---|
| `crates/rill-core` | Pure domain logic — **no I/O, enforced in CI** |
| `crates/rill-chain` | The only crate that talks to Sui, behind a trait |
| `crates/rill-ptb` | Transaction building; direct Move calls, no protocol SDK |
| `crates/rill-policy` | Type-state envelope verification |
| `crates/rill-mcp` | Shared MCP wiring and tool definitions |
| `crates/rill-auth` | OAuth 2.1 authorization server + Sign-In With Sui |
| `crates/rill-store` | Persistence behind a trait |
| `bins/rill-server` | axum — REST, MCP, OAuth |
| `bins/rill` | The local binary that holds the key |
| `move/` | On-chain contracts, carried over unchanged |

## `rill`, the local binary

One binary, every local job. Run it with no arguments and it reports readiness and lists what it
can do — it never falls through to the protocol loop, because one human-readable line on stdout
corrupts the MCP wire with nothing to say where the corruption came from.

```sh
rill              # status, then the command list
rill mcp          # speak MCP over stdio — this is what an agent runs
rill status       # readiness; exits non-zero when it cannot sign
rill address      # just the address, so it composes
rill capabilities # what the loaded run-set permits, in order
rill describe <package>::<module>::<function>
```

## The whole path, on chain

One transaction, signed only by the agent, with no owner key anywhere in it:

```
request_spend -> budget::prove -> per_tx::prove -> confirm_spend
              -> balance_manager::deposit_with_cap
              -> balance_manager::generate_proof_as_trader
              -> pool::place_limit_order
```

`GiL7unaYVnx7TF9QDtpUgc3nFSdWxVgkLb6sMDQfCm77` on testnet — 10 DEEP bid at 0.015 SUI, funded by
coins the agent wallet released under its own on-chain rules.

The DeepBook half runs on delegated capabilities: a `DepositCap` to fund the BalanceManager and a
`TradeCap` to trade on it. Neither is the owner's key, which is what makes the transaction
signable at all — `request_spend` asserts the sender is the *agent*, and a PTB has one sender.

## What an agent sees

`rill mcp` speaks MCP over stdio. An agent reads the wallet's limits from the chain that enforces
them, spends within them, and is refused by the contract when it exceeds them — all in one session:

One tool per question, and reads never share a tool with writes:

```jsonc
// rill_wallet { wallet }
{ "rules": [ { "module": "budget", "enforcement": "on-chain",
               "enforcedBy": "the Move contract, which aborts the transaction" },
             { "module": "per_tx", "enforcement": "on-chain",
               "enforcedBy": "the Move contract, which aborts the transaction" } ],
  "preFlightRules": [ { "module": "recipient_allowlist", "enforcement": "pre-flight",
                        "enforcedBy": "the signer, before it signs" } ],
  "note": "Two layers hold this wallet's limits, and each rule above says which. … Nothing on
           chain checks a destination, a protocol, an asset, or a recipient: those limits are
           pre-flight, enforced by this signer refusing to sign …" }

// rill_spend  0.005 SUI
{ "submitted": true, "digest": "DpTPdMKbDSndfAqekmX8EUFyDdYePAk338Y9fqgmWhmW",
  "callSequence": ["…::request_spend", "…::budget::prove", "…::per_tx::prove",
                   "…::confirm_spend"] }

// rill_spend  0.05 SUI  — over the per-transaction cap
isError: true
"per_tx refused it: this spend is larger than the per-transaction cap.
 The limit is on chain, not in this client — raising it here changes nothing, and neither will
 retrying with the same amount. Spend less, or have the wallet's owner attach different rules."
```

The last one is the point. A custodial agent wallet enforces limits in a server, and a server can be
talked out of an answer. Here the limit is a Move contract: the client that built the transaction
cannot widen it, the agent that asked cannot widen it, and the refusal arrives from the chain.

**Which layer holds each limit.** That is true of four kinds of rule, and the read says which. The
budget, the per-transaction cap, the rate limit and the time window are held by the Move contract
and proved on chain against the real transaction. Nothing on chain checks a destination, a
protocol, an asset, or a recipient: protocol scope, asset scope and recipient allowlist are
pre-flight, enforced by this signer refusing to sign, and they exist only where a run-set gives it
something to refuse against. The slippage floor is enforced by the signer refusing to sign an
envelope whose guard call does not match, and by the chain aborting when the floor is breached.
Every rule the wallet read returns carries its layer, computed per rule by the one producer
(`RuleKind::enforcement` in `rill-core`) and never written as a constant; a test reads the source
and fails if one appears.

The prove list is not a guess either — it is read from the wallet with `policy_rules`, because
`confirm_spend` counts receipts against the wallet's live policy and a mismatch aborts.

**Why a read never shares a tool with a write.** An MCP client decides whether to stop and ask a
human from a tool's `destructiveHint`, and annotations are per-tool. A tool that could both read and
spend would carry that hint always, so every harmless read would raise an approval prompt — and a
prompt that fires on everything is one people learn to click through.

The surface is `rill_status` (can this signer act), `rill_wallet` (what does this wallet permit),
`rill_spend`, `rill_execute`. One question each, rather than one tool with a switch: an answer whose
shape depends on which argument was passed is an answer an agent has to discover by trying it.

## Integrating a protocol without an SDK

Building a call needs one thing: its exact shape. In TypeScript that comes from a per-protocol
SDK, which is why "does it have an SDK?" decides there which protocols are reachable — and why a
stale SDK is a class of bug. `@mysten/deepbook-v3` sends a price through a double; the reference
signer required an entry point its deployed contract no longer had. Both are failures of the copy,
not of the contract.

The chain publishes the real thing, and it cannot drift from the contract because it *is* the
contract:

```console
$ rill describe 0x1eabed72…89b2fb::pool::flash_swap     # Cetus, no Rust SDK anywhere
public fun pool::flash_swap<T0, T1>(&…config::GlobalConfig, &mut …pool::Pool<T0, T1>, bool,
  bool, u64, u128, &0x2::clock::Clock): …balance::Balance<T0>, …balance::Balance<T1>,
  …pool::FlashSwapReceipt<T0, T1>

7 argument(s) a PTB command must carry:
   0  &0x1eabed72…89b2fb::config::GlobalConfig
   1  &mut 0x1eabed72…89b2fb::pool::Pool<T0, T1>
   …
```

So "is protocol X supported?" has the same answer for every X: it is deployed, so its signature is
readable, so the call can be built. What remains is arranging arguments in the declared order —
which `crates/rill-chain/tests/deepbook_signature.rs` checks the builder still does, against the
deployed package rather than against its own fixtures.

The key comes from `RILL_SUI_PRIVATE_KEY`, read from the environment of whatever launches the
process — never from an MCP config file, a command-line argument, or anything the agent can read.

## Build

```sh
cargo build --workspace
cargo test --workspace
cargo tree -p rill-core --edges normal   # must show no tokio / axum / sui-rpc
```

Tests that need a fullnode are `#[ignore]` by default and named for what they prove:

```sh
cargo test -p rill-ptb  --test book_live     -- --ignored --nocapture
cargo test -p rill-chain --test package_probe -- --ignored --nocapture
```

The contracts are tested by their own toolchain:

```sh
cd move/agent_wallet && sui move test   # 36
cd move/rill_guard   && sui move test   # 2
```

## Known gaps

Recorded here rather than discovered during a demo.

**Two `agent_wallet` packages are deployed on testnet, and the reference repo's own documents
disagree about which is current.** Asked directly (`rill-chain/tests/package_probe.rs`,
reproducible):

| package | named by | `request_spend` | `confirm_spend` | `spend` |
|---|---|---|---|---|
| `0xb02f39d6…563740` | `Published.toml`, `.env.example` | present | present | absent |
| `0xd9265581…a636da` | README, `pitch.tsx` | absent | absent | present |

The first is current, and every wallet behind the digests above was created from it. The funded
testnet sender still holds three older `AgentCap` objects typed
`0xd9265581…::agent_wallet::AgentCap`; a capability minted by one package cannot authorise a call
in another, so they are unused rather than a hazard. `rill status` warns when a run-set names the
old package rather than letting it surface as a Move abort at signing time.
