# Agent MVP verification, 2026-10-04

The package remains upgradeable. Production API and Studio run on `tencent-mgodonf` at
`https://api.rill.rifuki.dev` and `https://rill.rifuki.dev`. Public deployment receipts are in
`deploy/mainnet-v2.json`; the original `deploy/mainnet.json` remains a historical receipt.

## Live evidence

| Flow | Mainnet transaction / result |
| --- | --- |
| Wallet v2 upgrade | `8FMjfPWQuSYbrWyM2VBLZki6emXAupzSBUFLoVMG3Dit` |
| Shared Version migration | `FFRdyykDKrsKNNsRNw6b5PVY9scC3eHdwAM4Uf2xfAvV` |
| Cetus adapter publication | `AEdqwg5iqRayZS5UaxRWXT3ozpptoRwxgmbpzj7NQL2U` |
| Protected swap | `J1TMfvU4ue57Em7drCMLsphnn3RS1hU4b6xHiQ5M5N3t`: 0.005 SUI input, 5,883 USDC base units to owner, agent USDC unchanged |
| Protected recovery | `GoMDnggAPzLRVA6TRoW1KrcFAHuGp42HxBxupfSD7XmW`; wallet revoked, budget reclaimed, later grant run refused |
| Legacy swap | `3sFtRTKcN4md4LHYezunoinHARS5EZbbzz86fTHSQxCc` |
| Stake | `FrDtEcVD7pN1Rmh7bFBBDifegTVoqHzzaXXczoSfxF6R`: 1 SUI, 923,880,778 haSUI base units minted |
| Instant unstake | `DjJodav4yiV1s39M4fSVy6sukz8fhCCjjreT3SQQaoYb`: 1 haSUI redeemed, 1.082174262 SUI to owner, 1 SUI output floor |
| DeepBook order | `EqsJZzyMTM92eBz4b6YbvK3HQTXteL4KskxjkHT4aC86` |
| DeepBook recovery | `ERGZLM3Lgke6vTRJYRket7CTgsVZywxPxGLWqbERrxc9`: resting order cancelled and deposit withdrawn |
| MCP send | `JDRZKPbmB5TyjHHEpxFbnEBR9V6fAwCYutJ7Pqnf6Aes`: 0.001 SUI to owner; 0.002 SUI refused by per-tx cap |
| Send recovery | `76C3Cw1gnRGz73hBDMh6PXbyugqVo3Aod4yzKLijRCJi` |
| Portfolio | Actual local MCP read returned exact SUI, haSUI and USDC balances, without submitting a transaction |
| Pairing | Production owner prepare, separate signer proof, owner confirmation passed; replay refused; no spending permission issued |
| Historic bytecode | Fully consumed v1 gated-spend simulation aborts in original `version::check_is_valid`, code 0, after migration |

Both legacy and protected scenarios reject tampered owner grants and additional spending beyond
the remaining budget. Protected settlement tests use actual chain balances rather than the
server's description of the action. Test allocations were revoked and recovered.

## Local checks

- Rust workspace: 775 passed, 0 failed, 70 ignored at the recorded full run. Added production
  pairing and protected-swap assertions were also run separately against the live API.
- Move wallet: 55 passed, including protected custody, revision, pool/output checks and owner exit.
- Cetus adapter compiled against pinned official interfaces; actual execution proven above.
- Frontend: 180 passed; TypeScript check and production build passed.
- Clippy workspace/all targets with warnings denied passed. Build-script dependency audit passed.
- The appended gas-coin transfer regression failed before the fix and passed afterwards. The
  signer now rejects gas-coin action operands and requires MoveCall-only protected envelopes.

## Boundaries

The first protected strategy uses a SUI-funded vault and one terminal Cetus swap. Send, stake and
DeepBook use existing bounded-wallet paths and retain their declared enforcement layers. Direct
owned assets can be redeemed through the unstake signer tool. Multiasset vault onboarding, delayed
unstake/claim and external lending-position aggregation are outside this MVP.

Published skills have immutable IDs, definition lineage, versions and workflow digests. Existing
grants stay pinned to their approved publication. Pairing registers an agent public address;
owner-approved onchain policy and AgentCap remain the spending authority.

Machine-local receipts and logs reside under `/tmp/rill-mainnet-e2e-20261004-corrected/`,
`/tmp/rill-protected-mainnet-e2e/` and `/tmp/rill-mcp-send-smoke/`. No private key or bearer token is
part of these publication receipts. Browser wallet approvals are performed by the user.
