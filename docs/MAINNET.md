# The mainnet cutover

Nothing in this repository has spent real money, and this document is what stands between it and
doing so. It exists to make the decision legible rather than to make it: every precondition below
maps to a check that can be run and is green on testnet today, except the ones that cannot be green
without spending money, which are named as exactly that.

`RILL_ALLOW_MAINNET=true` is the only thing the code asks for, and it is not a configuration step.
Setting it asserts everything on this page. The refusal says so, from one producer in
`crates/rill-core/src/mainnet.rs`.

## Who decides, and on what

**The trigger clause.** All of the following, together, and not a majority of them:

1. Every precondition in the table below is green.
2. A Move audit of `move/agent_wallet` and `move/rill_guard` has been funded and its findings closed.
3. A named person authorises the cutover in writing, and that name is recorded here.

Nobody is named yet. That is the honest state, and an unnamed authoriser is the difference between a
gate and a speed bump.

**What the audit costs, so the number is not a surprise.** Roughly three to six person-days at a firm
with a published Sui record, and commonly four to twelve weeks of lead time between commissioning and
a report. The lead time is the part that decides a schedule: there is no version of this where the
audit is booked in the same week the decision is taken.

## What mainnet unlocks that testnet cannot

Two things, and both are reasons rather than justifications.

- **The withheld half of the hackathon prize**, which is released on mainnet deployment. It is a real
  incentive and it is also exactly the kind of incentive that gets an unaudited contract deployed, so
  it is written down here next to the audit rather than somewhere the audit is not.
- **Blockaid's Sui scanner**, whose endpoints accept only `mainnet`. Transaction screening cannot be
  evaluated at all until then, so "we will add screening" stays a plan and not a tested property.

## Preconditions

| # | Precondition | How it is checked | State |
|---|---|---|---|
| 1 | The contracts pass their own suites | `sui move test` in `move/agent_wallet` and `move/rill_guard` | green, 37 and 2 |
| 2 | The delegation holds in both directions on a live chain | `cargo test -p rill --test delegation_live -- --ignored` | green on testnet, digests in `docs/OVERNIGHT.md` |
| 3 | `E_NOT_AGENT` is covered, which no owner-signed transaction can reach | `request_spend_by_cap_holder_who_is_not_the_agent_aborts` in the Move suite | green |
| 4 | Every enforcement label comes from one producer, never a constant | `bins/rill/tests/enforcement_claims.rs` | green |
| 5 | The builder cannot sign | `cargo test -p rill-server --test keyless_observable`, plus the CI graph check in both directions | green |
| 6 | A chain client is never built in one runtime and used in another | `the_execute_tool_keeps_its_chain_client_inside_one_runtime` | green |
| 7 | No production code names a gas price | `crates/rill-chain/tests/gas_price_is_read.rs` | green |
| 8 | Gas selection sees every coin on a busy address | `cargo test -p rill-chain --test gas_read_live -- --ignored` | green, 60 objects over a 50-object page |
| 9 | Every build path refuses a lockfile change | `bins/rill/tests/supply_chain.rs` | green |
| 10 | A new build script in the dependency tree fails the build | `scripts/audit-build-scripts.sh` against its baseline | green, 40 on the reviewed list |
| 11 | The release workflow produces three assets and six files | `workflow_dispatch` dry run | green, run 34505161179 |
| 12 | A published checksum verifies against a published asset | the same two commands the README gives a reader | **not green: no release is published.** Needs a `rill-wallet-v*` tag, which is a publication decision |
| 13 | The mainnet refusal names what the override asserts | `crates/rill-core/src/mainnet.rs` tests | green |
| 14 | A long-lived credential is revocable, and revoking stops the next call | `crates/rill-auth/tests/long_lived_token.rs` | green |
| 15 | The hosted endpoint answers from outside this network | `GET /health` against the deployed host | **not green: there is no host.** `api.rill.naisu.one` has no DNS record and its droplet is unreachable |
| 16 | A Move audit has been funded and its findings closed | a report from the firm | **not green, and not a test** |
| 17 | A named person authorises the cutover | this document | **not green: nobody is named** |

Thirteen of seventeen are green. The four that are not divide cleanly: two need a decision that costs
money (16, and the host behind 15), one needs a publication (12), and one needs a person to put their
name to it (17). None of them can be closed by writing more code, which is the useful thing this
table says.

## What does not change at cutover

The guard stays. `RILL_ALLOW_MAINNET` is not removed once mainnet is live, because the reason it
exists is not that mainnet is unready: it is that a testnet run and a mainnet run are one typo apart,
and the variable is what makes the difference deliberate. Every signing path checks it, and
`bins/rill-server` refuses to start on mainnet without a durable signing secret and a deployed guard
package for the same reason.
