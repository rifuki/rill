# Protected Cetus swap contract

## Authority and custody

An existing `AgentWallet<T>` can be permanently placed into protected mode by its owner. The configuration is a dynamic field on the wallet UID, separate from ordinary restriction rules. Existing published struct layouts do not change. Removing a budget or time rule does not remove protection; there is deliberately no disable operation.

The owner pins the adapter witness type, exact output coin type, exact pool object ID, and a positive minimum output. The adapter witness additionally identifies the action and direction (`AToB` versus `BToA`). `configure_a_to_b` and `configure_b_to_a` expose only the audited Cetus witnesses. They require an owner transaction. A wallet owner remains able to deliberately authorize another adapter, so clients must display and pin the audited adapter package identity.

The agent must first obtain a normal `SpendRequest` and prove every attached rule. For protected wallets, generic `confirm_spend` aborts. The adapter exclusively constructs its witness to call `confirm_protected`, which checks the policy revision, pool, adapter, output asset and owner floor. It obtains the input together with a non-droppable, non-storable `Settlement` obligation. The actual protocol result must satisfy that obligation in the same transaction.

The concrete adapter calls official Cetus `pool::flash_swap` in exact-input mode, reads the actual protocol receipt repayment amount, pays through `repay_flash_swap`, then settles. Output goes directly to `wallet.owner`; neither the agent nor a caller-supplied recipient receives it. All unused input returns to the wallet. The policy counts the full reserved input against spend limits, conservatively, even when Cetus returns input change. This prevents refund paths from resetting lifetime or per-transaction counters.

Rill cannot vouch for another module merely because its creator labels a witness as Cetus. The production client must use the exact deployed `cetus_adapter::swap` package and the pinned witness identity. The protected entrypoints return no coins or receipts to their PTB caller.

## Interfaces

Owner configuration:

- `cetus_adapter::swap::configure_a_to_b<A,B>(wallet: &mut AgentWallet<A>, version: &Version, pool: ID, min_output: u64, ctx: &TxContext)`
- `cetus_adapter::swap::configure_b_to_a<A,B>(wallet: &mut AgentWallet<B>, version: &Version, pool: ID, min_output: u64, ctx: &TxContext)`

Execution after `request_spend` and the normal rule proofs:

- `execute_a_to_b<A,B>(wallet: &mut AgentWallet<A>, req: SpendRequest, revision: u64, min_output: u64, sqrt_price_limit: u128, version: &Version, config: &GlobalConfig, pool: &mut Pool<A,B>, clock: &Clock, ctx: &mut TxContext)`
- `execute_b_to_a<A,B>` has the same parameters with `AgentWallet<B>` as the funded wallet.

There is no return value. The adapter forces exact-input mode; the PTB caller cannot substitute an exact-output mode or choose an output destination. The caller may tighten the minimum output but cannot lower the owner's configured floor.

Wallet views:

- `is_protected<T>(&AgentWallet<T>): bool`
- `protected_revision<T>(&AgentWallet<T>): u64` (zero for legacy mode)
- `protected_policy<T>(&AgentWallet<T>): (TypeName, TypeName, ID, u64, u64)` in adapter, output asset, pool, minimum output, revision order.

Every configuration update increments the policy revision and preserves all existing wallet/rule counters. The normal agent cap identity and sender checks remain in `request_spend`. Owner `withdraw`, `revoke`, and `rotate_agent` remain available independently of package migration.

## Upgrade sequencing

Package behavior version is **2**. Merely upgrading code is insufficient: old bytecode can still be called until the authoritative shared `Version` is migrated. The original v1 entrypoints check their compiled version and consequently reject the shared object once its value becomes 2.

Required order:

1. Upgrade the existing wallet package, retaining the UpgradeCap and published type origins.
2. Call its publisher-authorized `version::migrate` on the existing shared Version object.
3. Publish the Cetus adapter linked to that upgraded wallet package.
4. Owner configures protected mode and grants the pinned action binding.
5. Execute a restricted swap and verify owner output and vault change.

`configure_protected` itself checks version 2 so protection cannot be advertised as active while v1 remains authoritative. Owner emergency exits do not require version 2. UpgradeCaps stay upgradeable.

An actual old-package call after migration must be tested on a chain deployment; setting version values in a unit test is not equivalent to executing historic bytecode.

## Dependencies and verification

The official [Cetus interface](https://github.com/CetusProtocol/cetus-clmm-interface/tree/5ef409e5aa97cf0c545c5ff64560cb65eaa981e7/sui/cetus_clmm) is pinned to immutable commit `5ef409e5aa97cf0c545c5ff64560cb65eaa981e7`. IntegerMate and MoveSTL transitive dependencies are pinned from its official lockfile. The dependency uses the published Cetus original package ID `0x1eabed72c53feb3805120a081dc15963c204dc8d091542592abaf7a35689b2fb`. The upstream interface commit records mainnet implementation `0x260693ec785a6e6c9d81d58c7d2ff72f1288ae0fa6a9725abe05a6478b11f084`, version 15. Those upstream metadata values are not live chain verification.

The upstream Cetus implementation bodies in this repository are intentionally aborting interfaces. Successful compilation proves the adapter uses those published interfaces; it does not prove an actual swap or create a working local Cetus pool. The test-only adapter in `protected_tests.move` is solely for wallet policy/settlement invariants and is never shipped as a protocol adapter.

Verified locally:

- `sui move test --path move/agent_wallet`: 55 tests passed, including 14 new protected-mode tests.
- `PATH=/tmp/rill-move-tools:$PATH sui move build --path move/cetus_adapter`: adapter compiled against official interfaces. MVR v0.1.0 was downloaded from the official MystenLabs release to a temporary tool directory; it is required by upstream dependency manifests.
- An isolated copy of the package containing `tests/compile_fail/missing_settlement.move.fixture` fails with `EC06001: unused value without 'drop'`. This verifies a missing settlement cannot be compiled away.

The tests cover owner-only configuration, generic release rejection, unauthorized adapter witness, stale revision, wrong pool, wrong output asset, owner minimum output, complete rule proofs, residual custody, preserved counters across edits, owner settlement destination, and owner emergency exit during stale package state. Existing tests additionally cover revoked/expired wallets, rotated caps, per-transaction aggregate budget, and rule substitution.

Still requiring deployment evidence: compatibility check against deployed package, publisher migration, historic-v1 rejection, both protocol directions against real Cetus pools, and Rust/FE end-to-end execution and policy edit flow.
