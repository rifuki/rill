# Rill

**The transaction layer for AI agents on Sui** — rebuilt in Rust.

Any agent can safely transact with any Sui protocol without hallucinating parameters or
risking the whole wallet. The server builds and simulates transactions **without ever
holding a key**; a local signer holds the key and trusts nothing the server sends without
independently re-deriving it; two on-chain Move contracts bound every action.

## Status

Scaffold. The implementation plan is
[`docs/plans/2026-08-30-001-feat-rill-rust-greenfield-plan.md`](docs/plans/2026-08-30-001-feat-rill-rust-greenfield-plan.md).

The Bun/TypeScript implementation this replaces remains the **specification** — its 746
passing tests define the behavior being re-expressed here, and it keeps running until each
component is cut over.

## Why a rebuild rather than a port

Three problems in the TypeScript version are the same problem: invariants the code
documents but the language does not enforce.

- A float reached the money path, in a codebase whose own stated rule is that no IEEE-754
  value may touch a token amount. Every test passed.
- The signer drifted off the deployed contract — it still required a `spend()` entry point
  the contract no longer has, so the only run-set the product could create could never
  validate.
- One capability-declaration contract had three implementations, two of which had to agree
  exactly with nothing enforcing that they did.

Here, each is structurally impossible: integer-only money types with no `f64` constructor,
validation state carried in the type so an unchecked envelope cannot reach `sign()`, and a
single declaration producer.

## Layout

| Path | Role |
|---|---|
| `crates/rill-core` | Pure domain logic — **no I/O, enforced in CI** |
| `crates/rill-chain` | The only crate that talks to Sui (nine methods behind a trait) |
| `crates/rill-ptb` | Transaction building; direct Move calls, no protocol SDK |
| `crates/rill-policy` | Type-state envelope verification |
| `crates/rill-mcp` | Shared MCP wiring and tool definitions |
| `crates/rill-auth` | OAuth 2.1 authorization server + Sign-In With Sui |
| `crates/rill-store` | Persistence behind a trait |
| `bins/rill-server` | axum — REST, MCP, OAuth |
| `bins/rill-wallet` | The local signer that holds the key |
| `move/` | On-chain contracts, carried over unchanged |

## Build

```sh
cargo build --workspace
cargo test --workspace
cargo tree -p rill-core --edges normal   # must show no tokio / axum / sui-rpc
```
