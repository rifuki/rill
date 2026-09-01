# Rill

**The transaction layer for AI agents on Sui** — in Rust.

An agent can transact with any Sui protocol without inventing parameters or risking the whole
wallet. The server builds and simulates transactions **without ever holding a key**; a local
binary holds the key and trusts nothing the server sends without re-deriving it independently;
two on-chain Move contracts bound every action.

## Where things stand

The workspace builds, 278 Rust tests pass — including reads against live testnet and mainnet
fullnodes — and the two Move packages pass their own 36 and 2. What it does not yet have is a demonstrated end-to-end submission — see
[Known gaps](#known-gaps), which names exactly what is missing and why.

The Bun/TypeScript implementation this replaces is the **specification**. Its behaviour is
re-expressed here, with conformance fixtures in `fixtures/` checked against it by
`ts/verify-reference.ts`.

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

**The demo wallet's capabilities belong to the superseded contract.** Two `agent_wallet` packages
are deployed on testnet, and the reference repo's own documents disagree about which is current.
Asked directly (`rill-chain/tests/package_probe.rs`, reproducible):

| package | named by | `request_spend` | `confirm_spend` | `spend` |
|---|---|---|---|---|
| `0xb02f39d6…563740` | `Published.toml`, `.env.example` | present | present | absent |
| `0xd9265581…a636da` | README, `pitch.tsx` | absent | absent | present |

The first is current. The funded testnet sender's three `AgentCap` objects are all typed
`0xd9265581…::agent_wallet::AgentCap` — the superseded one. A capability minted by one package
cannot authorise a call in another, so **an end-to-end submission needs a fresh `AgentCap` minted
from `0xb02f39d6…` first.** `rill status` warns when a run-set names the old package rather than
letting it surface as a Move abort at signing time.

**Submission is unproven, and a signature is the only thing missing.** `create_wallet` against the
current package passes the **strict** simulation on testnet — checks on, real gas objects, the same
gate the build path runs before anything may be signed:

```text
ok           : true
verification : Verified
gas          : 4477760
balance      : -1004477760 …::sui::SUI      (1 SUI funded + gas)
```

Keyless, and nothing was submitted. `cargo test -p rill-ptb --test create_wallet_live -- --ignored`
reproduces it. What it proves is that the transaction *would* execute; signing it needs a key this
repo does not have, and that is the whole of what stands between here and an end-to-end run.
