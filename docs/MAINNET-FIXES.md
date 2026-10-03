# Mainnet audit fixes

The four internal audit regressions now pass. Initial publication creates a real one-time-witness Publisher. Version migration checks forward progress and emits Migrated. The per_tx rule reserves aggregate spend against the current transaction digest before confirming requests. Empty-policy wallets cannot release coins.

CLI and MCP creation now mint an empty wallet, then attach rules and fund in one owner-signed transaction. Failed attachment leaves no initial funding exposed. Studio uses explicitly configured wallet/Version IDs on mainnet and resolves upgrade-aware type origins from the deployed package ABI. The signer never falls back to testnet deployments on mainnet. Frontend selects VITE_SUI_NETWORK, validates backend registry network and isolates browser state between networks.

## Verification

- Move: 41 agent-wallet tests, including the four reproduced regressions; guard tests unchanged.
- Rust: 705 tests passing, 64 live tests ignored; Clippy with warnings denied.
- Frontend: 166 tests, TypeScript check, testnet and mainnet builds.
- Real isolated localnet publish, compatible upgrade, Publisher-authorized migration, latest-version success, old-version rejection, and downgrade rejection. The public proof is docs/audits/2026-10-04-upgrade-rehearsal.json. Ephemeral publication files and local keys remain outside the repository.

The per_tx Config layout and prove signature changed before the first mainnet publication. This is a fresh package release; do not try to upgrade the old testnet deployment to this layout. Later upgrades must preserve public signatures and struct layouts, bump VERSION, retain the Publisher and UpgradeCap, and migrate forward.

## Deployment inputs

Use deploy/mainnet.env.example after publishing the reviewed packages. Mainnet package and Version IDs are deliberately unset until publication; an unset pair causes a clear configuration refusal. Set VITE_SUI_NETWORK=mainnet and point VITE_RILL_API_URL to the matching Rust backend. Keep the owner/deployer identity separate from the agent signer.

Owner/deployer: 0xa1938b420508d960597153e7e3ec7dafce5fddc8747c071220570b93cc426870. Key custody is the existing protected native Sui CLI keystore, not this repository.

Publication has not occurred on mainnet. Rehearsal used synthetic localnet funds only. The existing independent audit, upgrade-cap custody, production hosting and cutover requirements in MAINNET.md still apply.

Use the owner explicitly when preparing publication; do not depend on whichever address is active:

```sh
sui client --client.env mainnet publish move/agent_wallet --sender 0xa1938b420508d960597153e7e3ec7dafce5fddc8747c071220570b93cc426870 --dry-run --json
```

The dry run needs mainnet gas in that address. Capture the concrete gas estimate and review it before removing --dry-run. Record the package, UpgradeCap, Publisher and Version IDs from publication, verify the deployed bytecode/source, then supply those IDs to the Rust server and signer.
