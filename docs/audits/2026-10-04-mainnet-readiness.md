# Rill mainnet readiness audit

Reviewed: Rust `d4122df`, frontend `4438eae`, current Move source. Toolchain: `sui 1.78.0-homebrew`.

Verdict: **Not ready for mainnet publication or activation.** This is a targeted internal audit with source inspection and reproduced invariant failures, not an independent third-party contract audit. No package was published and no funds were spent.

## Pattern comparison

The requested references are in `~/mgodonf/web3/sui/hackathon-bootcamp/`, not `hackathon/`:

- `narnia-realm/contract/sources/version.move` has the same shared Version + constant + check + Publisher-gated migrate shape.
- `narnia-realm/contract/sources/nft.move:49` actually claims its Publisher from an NFT one-time witness and transfers it to the publisher at initialization.
- `sui-move-bootcamp/H1/package_upgrade/sources/hero.move:32` also claims and keeps its Publisher.
- Rill copied the version gate but omitted the production Publisher creation.

Sui publication creates an UpgradeCap; it does not automatically create the Publisher required by Rill's migrate function. Old package versions remain callable, and upgrade initializers do not run again. The official versioned-object example additionally requires migration to move the object version forward.

## Reproduced findings

| Severity | Finding | Evidence | Required change |
|---|---|---|---|
| P1 | Production Publisher is never created | `version.move:41` only shares Version; no production source claims a Publisher. The new initializer inventory test aborts with `EEmptyInventory=3`. Existing migrate tests fabricate one with test-only `package::test_claim`. | Claim a real one-time-witness Publisher on the initial mainnet publish, or introduce an explicitly bound AdminCap. Keep its custody separate from the agent signer. |
| P1 | Migration accepts version downgrade | `version.move:55-57` assigns the compiled constant without a monotonicity check. A Version set to 2 is reset to 1, and old `check_is_valid` succeeds; rejection test fails because no abort occurs. | Require the current object version to be less than the target compiled version, authorize migration, and emit a migration event. Test old-package rejection after an actual upgrade and migration. |
| P1 | Per-transaction cap is only per-request | `per_tx.move:31-35` checks only one SpendRequest amount. Two spends of 200 in one transaction both succeed against a cap of 300, releasing 400 total; rejection test fails because no abort occurs. | Enforce an aggregate cap per transaction, or explicitly change the product contract to a per-request limit. The current per-transaction claim requires the former. |
| P1 | CLI creation exposes funded empty-policy wallet before attachment | `bins/rill/src/wallet.rs:208-230` funds during create; `create_and_bound_json_on:376-393` attaches rules in a second transaction. The contract test spends 500 from a newly funded wallet before any rule is attached. | Bring CLI/MCP creation onto the Studio empty-create -> attach-and-fund-atomically sequence, or reject empty-policy spends at the contract boundary. Studio already uses the safe funding sequence. |

## Mainnet integration blockers

- `bins/rill-server/src/studio_setup.rs:94` deliberately refuses non-testnet setup. Creation and attachment read testnet wallet/Version and DeepBook deployment constants.
- Frontend `src/routes/__root.tsx:18-20,146` configures only testnet. Pointing its API at a mainnet server cannot switch browser wallet operations to mainnet.
- CLI creation paths still default to testnet wallet/Version constants. Mainnet package IDs, Version ID, guard ID, and per-type DeepBook origins need an explicit network-specific deployment configuration verified against chain.
- The reviewed cutover document still requires independent audit findings closed, hosted endpoint evidence and recorded authorization. Its historical green rows were not all re-executed in this audit and are not fresh deployment evidence.

## What passed

- Baseline Move suites: agent_wallet 37/37, rill_guard 2/2.
- Both packages compile with `sui move build --build-env mainnet` in a scratch copy. This proves compilation only, not mainnet publication or RPC/contract compatibility of a deployed stack.
- Existing owner-only checks, wallet/cap binding, hot-potato receipts and eager budget reservation are present. The owner recovery path remains usable while the agent version gate is stale.
- Four new audit tests compiled and ran: 0 passed, 4 failed for the behaviors above. Initial test attribute syntax errors were corrected before treating any output as behavioral evidence.

## Reproduction

`2026-10-04-mainnet-repros.patch` contains only audit tests. Apply it to a scratch copy, not the production checkout, and run:

```sh
sui move test --path /path/to/scratch/agent_wallet audit_
```

Observed: missing Publisher inventory, migration did not reject a downgrade, two spends bypassed the aggregate per-tx cap, and empty-policy funding was spendable. Logs for this run: `/tmp/rill-mainnet-repros.log`, `/tmp/rill-mainnet-agent_wallet-baseline.log`, `/tmp/rill-mainnet-rill_guard-baseline.log`.

## New owner/deployer wallet

- Alias: `rill-mainnet-owner-20261004-a374`
- Address: `0xa1938b420508d960597153e7e3ec7dafce5fddc8747c071220570b93cc426870`
- Scheme: Ed25519, generated with the Sui CLI's 24-word setting.
- Custody: native Sui CLI keystore at `~/.sui/sui_config/sui.keystore`, file permission 0600, configuration directory 0700. No private key or recovery phrase was printed or added to this repository.
- Verified mainnet balance: zero coins.
- Existing active address and active testnet environment were preserved.
- This is the owner/deployer identity. A separate agent identity will be used for bounded spends. Production UpgradeCap custody should be a named, governed owner (preferably multisig/timelock), not an agent-held key.

## Required sequence before publication

1. Fix the four reproduced contract/CLI findings and add passing regression coverage.
2. Rehearse initial publish, actual package upgrade, monotonic Version migration, old-version rejection, owner emergency recovery and bounded agent spend on a disposable/testnet deployment.
3. Wire explicit mainnet deployments into Rust and frontend, including network checks and original type identities.
4. Close independent audit and production hosting gates. Review the concrete publish transaction, UpgradeCap custody and gas estimate after this wallet is funded.
5. Publish only the reviewed build and record package IDs, UpgradeCaps, Publisher/AdminCap and Version objects. Verify bytecode/source on chain before enabling the agent.

## First-party references

- [Sui package upgrades and versioned shared objects](https://docs.sui.io/develop/publish-upgrade-packages/upgrade)
- [Sui security best practices](https://docs.sui.io/develop/security/best-practices)
- [Publisher and UpgradeCap implementation](https://github.com/MystenLabs/sui/blob/main/crates/sui-framework/packages/sui-framework/sources/package.move)
