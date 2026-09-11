# Rill

**The transaction layer for AI agents on Sui** — in Rust.

An agent can transact with any Sui protocol without inventing parameters or risking the whole
wallet. The server builds and simulates transactions **without ever holding a key**; a local
binary holds the key and trusts nothing the server sends without re-deriving it independently;
two on-chain Move contracts bound every action.

## Where things stand

The workspace builds and 567 Rust tests pass. Another 51, which need a live fullnode, are
`#[ignore]` and run explicitly (see [Build](#build)). The two Move packages pass their own 37
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
| the same path built by the keyless server and signed locally, over MCP | `dxzyeAfW5eRdGUobBNUGeu2mnmaN4xyzY7J8dZxL5fZ` |
| a gated spend swapped on Cetus, by `rill_swap` over MCP | `8M5VjFgZ9Gx2N2CVq6MrUSnmNmq3ZVWqHLCvsFTDTYsZ` |
| the same swap, run through the published binary a stranger downloads | `AXxZMPVruaCAVoFXZwRjncseuS8e9Tedu6uBcbvYLN6K` |
| a swap over the per-transaction cap | refused, and the refusal names `per_tx` |

The sequence that produced each digest is in the commit that records it. The first three are no
longer a record to be trusted, either: `cargo test -p rill --test delegation_live -- --ignored`
builds a fresh wallet, its rules, the spend the agent makes and the owner cannot, and a revoke, on
every run. `docs/OVERNIGHT.md` holds the addresses and objects, including what that test last
produced.

The Bun/TypeScript implementation this replaces is the **specification**. Its behaviour is
re-expressed here, with conformance fixtures in `fixtures/` checked against it by
`ts/verify-reference.ts`.

## Install

The release is produced by a tag on `github.com/rifuki/rill`, and that is the one origin that
publishes this binary. Pushing a `v*` tag there runs `.github/workflows/release.yaml`, which builds
one asset per platform, checks that each one starts, and attaches it with a checksum beside it.
Nothing else publishes `rill-wallet`; an install line that names any other repository is wrong.

> **Published as `rill-wallet-v0.3.0`.** Three assets and three checksums, built by
> `.github/workflows/release.yaml` from a tag on this repository. The commands below were run against
> them exactly as written: all three checksums verify, and a file with one byte appended is rejected.
> The series continues `rill-wallet-v0.2.0`, which `naisu-one/rill` published on 2026-07-19 with the
> same three asset names.

One block per platform. Paste the whole of yours: the checksum is verified first and the rest is
chained to it, so a file that does not match is never made executable and never run. What the check
prints, and what to do when it fails, is under [Verifying the download](#verifying-the-download).

**macOS, Apple silicon**

```sh
curl -fsSLO https://github.com/rifuki/rill/releases/latest/download/rill-wallet-darwin-arm64
curl -fsSLO https://github.com/rifuki/rill/releases/latest/download/rill-wallet-darwin-arm64.sha256
shasum -a 256 -c rill-wallet-darwin-arm64.sha256 \
  && chmod +x rill-wallet-darwin-arm64 \
  && mv rill-wallet-darwin-arm64 rill-wallet
```

**macOS, Intel**

```sh
curl -fsSLO https://github.com/rifuki/rill/releases/latest/download/rill-wallet-darwin-x64
curl -fsSLO https://github.com/rifuki/rill/releases/latest/download/rill-wallet-darwin-x64.sha256
shasum -a 256 -c rill-wallet-darwin-x64.sha256 \
  && chmod +x rill-wallet-darwin-x64 \
  && mv rill-wallet-darwin-x64 rill-wallet
```

**Linux, x86_64**

```sh
curl -fsSLO https://github.com/rifuki/rill/releases/latest/download/rill-wallet-linux-x64
curl -fsSLO https://github.com/rifuki/rill/releases/latest/download/rill-wallet-linux-x64.sha256
sha256sum -c rill-wallet-linux-x64.sha256 \
  && chmod +x rill-wallet-linux-x64 \
  && mv rill-wallet-linux-x64 rill-wallet
```

Then, on any of them:

```sh
./rill-wallet --status
```

`--status` prints `rill-wallet` on its first line and then whether it can sign. With no key it says
`not ready` and exits 1: that is the expected answer on a fresh machine, not a broken download. The
key comes from `RILL_SUI_PRIVATE_KEY` or the `sui` CLI's own keystore, never from an argument.

### Verifying the download

This binary signs transactions with a key on your machine, so it is worth the twenty seconds. The
release attaches a `.sha256` beside every asset, and it is one line, `<hex>  <asset>`, exactly what
`shasum -a 256 <asset>` printed on the runner that built it. The name inside the file is how the
checker finds the asset, so download both into the same directory and verify before renaming
anything.

Pick the tool your machine actually has. `shasum` is present on macOS and on any Linux with the full
perl package, which a slim container image usually lacks; `sha256sum` comes from coreutils on Linux
and, on recent macOS, ships as an Apple-signed binary in `/sbin` (an earlier version of this sentence
said macOS has no `sha256sum`, which was simply wrong). Both read the same file, so on a machine with
both, either works:

```sh
shasum -a 256 -c rill-wallet-darwin-arm64.sha256   # macOS
shasum -a 256 -c rill-wallet-darwin-x64.sha256
shasum -a 256 -c rill-wallet-linux-x64.sha256

sha256sum -c rill-wallet-linux-x64.sha256          # Linux
```

A pass is one line naming the file, and exit 0:

```console
rill-wallet-darwin-arm64: OK
```

A failure says so in words, and exits non-zero:

```console
rill-wallet-darwin-arm64: FAILED
shasum: WARNING: 1 computed checksum did NOT match
```

If that happens, delete both files and download them again; a truncated transfer is the likely
cause. If it happens twice, do not run the binary: report it on the repository rather than
chmod-ing it, because the only thing a mismatch can tell you is that what arrived is not what was
built. With neither checker installed, `openssl dgst -sha256 <asset>` prints the same hex to compare
with the `.sha256` file by eye.

Checksums prove the bytes are the ones the release published. What produced them is the other half:
a tag on this repository runs `.github/workflows/release.yaml`, which builds with `--locked` against
the committed `Cargo.lock`, on the exact toolchain `rust-toolchain.toml` names, with the set of
dependencies that run code at build time diffed against `scripts/build-scripts.baseline`. The
container image is built the same way, by the same flags, which is checked by
`bins/rill/tests/supply_chain.rs` rather than by reading the files and hoping.

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

## From nothing to a bounded wallet

One command, on a machine with no Sui setup.

```sh
./rill-wallet init --wait
```

What it does, and what it refuses:

1. **Checks for two keys** and stops if there are not two, naming the command that makes them. Two,
   not one: the owner creates the wallet and can revoke it, the agent spends inside its rules, and
   the delegation is only proved when they are different addresses. This never writes a key. When
   keys are missing it tells you to run `sui client new-address ed25519`, which is the sui CLI's own
   job and already the thing you would otherwise be told to run.
2. **Refuses an empty rule set**, because a capability with no rules is bounded by nothing:
   `confirm_spend` on an empty policy requires zero receipts, so the wallet would hand out its whole
   balance on request. Pass `--budget` and `--per-tx`, or take the defaults.
3. **Prints a faucet link with your owner address already in it** and, with `--wait`, polls until the
   coins land instead of exiting. There is no faucet this process can call: the CLI one was removed
   and the web one is interactive, so this is the one step a human has to do.
4. **Mints the wallet and attaches the rules**, then reports the wallet id, the capability id, and
   both digests.
5. **Does nothing on a second run.** Minting again would leave the first wallet funded and forgotten,
   which on testnet is waste and on mainnet is money, so a run-set that already names a wallet is
   reported and nothing is sent.

Flags: `--wait`, `--budget <mist>`, `--per-tx <mist>`, `--amount <SUI>`, `--run-set <path>`,
`--gas-budget <mist>`, `--package`, `--version-object`.

The whole path is covered offline in `bins/rill/tests/cold_start.rs`, including both refusals and the
second-run no-op, so the command that onboards a stranger is not itself only tested by onboarding a
stranger.

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

### Two surfaces, and only one of them can finish a spend

There are two MCP endpoints and they are deliberately not the same. Which one an agent is talking to
decides what it can do, so it is worth being blunt about it.

| | `rill mcp`, over stdio | the hosted endpoint, over HTTP |
|---|---|---|
| holds the key | yes | **no, and it is not linked against a signing library at all** |
| tools | `rill_status`, `rill_wallet`, `rill_create_wallet`, `rill_attach_rules`, `rill_spend`, `rill_execute` | `rill_list_actions`, `rill_describe_action`, `rill_build_action` |
| can complete a spend | yes | no |
| what it returns instead | a submitted transaction and its digest | an unsigned `ExecutionEnvelope`, inert until a local signer validates and signs it |

The two share no tool name: the builder speaks a catalogue vocabulary and the signer speaks a wallet
one. So an agent on the hosted endpoint alone can plan a transaction and prove by simulation that it
would execute, and then it stops. Finishing needs the local binary, which re-reads the bytes, checks
them against its own pinned run-set, re-simulates against live state, and only then signs.

That asymmetry is checked rather than asserted. `cargo tree -p rill-server` must contain no signing
library and `cargo tree -p rill` must contain one, both in CI, and
`crates/rill-mcp/tests/surface_split.rs` holds the tool split and the protocol negotiation from the
one producer that emits both surfaces.

### Checking the builder cannot sign, yourself

Two commands, neither of which needs a key, a wallet, or trust in this README.

```sh
# 1. The binary that builds transactions links no signing library. Nothing it does can sign.
cargo tree --locked -p rill-server --edges normal | grep sui-crypto   # prints nothing
cargo tree --locked -p rill        --edges normal | grep sui-crypto   # prints a line

# 2. And the whole claim, asserted in one suite that needs no network:
cargo test -p rill-server --test keyless_observable
```

The asymmetry is the point. The builder cannot sign because it has nothing to sign with; the signer
can because it does. CI asserts both directions on every push, since an asymmetry checked one way is
one claim rather than two.

Against a running deployment the same thing is visible from outside: `rill_build_action` returns an
`ExecutionEnvelope` whose `unsignedPtb` is exactly that, with no signature anywhere in the response,
and submitting it to a fullnode without one is refused. What turns it into a transaction is the local
binary, which re-derives the digest from the bytes, checks the call sequence against its own pinned
run-set, re-simulates against live state, and signs only then.

### Over stdio

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

## Swapping, under the wallet's rules

A swap from the signer's own coins is an ordinary swap with extra steps. What this does instead is
release the SUI from the agent wallet first, against the rules attached to it on chain, so the agent
swaps money it does not own and cannot swap more than the owner allowed.

One transaction, six calls:

```
request_spend -> budget::prove -> per_tx::prove -> confirm_spend -> coin::zero -> router::swap
```

Over MCP that is `rill_swap`, and the refusal works the same way as every other: 0.09 SUI against a
0.05 per-transaction cap comes back `rule_refused`, `rule: per_tx`, with the reminder that the limit
is on chain and raising it in the client changes nothing.

Two things about the Cetus path are worth knowing, because both cost a real transaction to learn:

- **`router::swap` returns two coins**, and the builder hands back one `Argument` for the whole
  return tuple. Returning that as the output coin is accepted by `try_build` and refused by the VM
  with `InvalidResultArity`, so the swap could not execute at all until both were taken. Arity is not
  a thing a build can check, which is why `crates/rill-ptb/tests/cetus_swap_live.rs` reproduces the
  refusal against a node before showing the fix.
- **The price bound has a side.** Funding A pushes the price down so the bound is a floor; funding B
  pushes it up so it is a ceiling. A bound on the wrong side aborts in `flash_swap_internal` with a
  bare 11 that names neither the value nor the field, so the adapter derives it from the direction and
  refuses a caller's wrong-sided bound by name.

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

## Running the server

One replica, one volume, one URL. That is the whole deployment contract, and it is written down in
`compose.yaml` rather than in somebody's shell history.

```sh
export RILL_OAUTH_SECRET=$(openssl rand -hex 32)
export PUBLIC_BASE_URL=https://whatever-address-clients-will-use
docker compose up --build
```

Three things about it are load-bearing:

- **One replica.** The server holds its state in a file it rewrites whole, so two replicas would each
  hold half the authorization codes and reject the other's. There is no clustering story here and
  `compose.yaml` is not the place to invent one.
- **A volume at `/app/data`.** `skills.json` and `oauth.json` live there. Lose it and every client
  re-registers.
- **`PUBLIC_BASE_URL` is the one value everything derives from**: both discovery documents, the
  `WWW-Authenticate` challenge, the audience tokens are bound to, and the instructions the server
  hands agents. Set it to the address clients will actually use. A trailing slash is fine; seven
  places read it and they all agree, which `bins/rill-server/tests/base_url.rs` holds by minting a
  token on a trailing-slash deployment and presenting it back.

Without `RILL_OAUTH_SECRET` compose refuses to start rather than generate a per-boot secret, because
a server that quietly regenerates it invalidates every token on restart and tells nobody.

**There is no hosted deployment today.** `api.rill.naisu.one` has no DNS record and its droplet is
unreachable, so the hosted half of this is a running command and not a URL you can curl. Everything
that does not depend on a permanent address is verified: the container boots, passes its own
healthcheck, serves both discovery documents, answers an unauthenticated `/mcp` with a challenge
pointing at them, and keeps tokens valid across a restart with the volume and the same secret.

## Mainnet

Nothing here has spent real money, and the guard that keeps it that way is not coming out.

`RILL_ALLOW_MAINNET=true` is the only thing the code asks for, and setting it is not a configuration
step: it asserts that the contracts holding the money have been audited, that every precondition in
[`docs/MAINNET.md`](docs/MAINNET.md) is green, and that a named person authorised the cutover. The
refusal says exactly that, from one producer, because the sentence it replaced named the override and
nothing else, which is a refusal whose only content is how to get past it.

Thirteen of seventeen preconditions are green today. The four that are not divide cleanly and none of
them can be closed by writing more code: a Move audit has to be funded (roughly three to six
person-days, commonly four to twelve weeks of lead time), a host has to exist, a release has to be
tagged, and somebody has to put their name to the decision. Nobody is named yet, and an unnamed
authoriser is the difference between a gate and a speed bump.

The guard stays after cutover too. Its reason is not that mainnet is unready; it is that a testnet run
and a mainnet run are one typo apart.

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
