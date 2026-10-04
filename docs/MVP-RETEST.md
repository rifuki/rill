# Production MVP retest, 2026-10-04

API: https://api.rill.rifuki.dev. Websites: https://rill.rifuki.dev and https://rill.naisu.one.
Signer: verified published v0.10.0 release, configured for the separate mainnet agent.

| Flow | Result | Evidence |
| --- | --- | --- |
| swap | PASS | `8Nz2EX8yT9niebda2pJy2NbytsDEw916XaiKVskdkea1` |
| stake | PASS | `5robxTCyki57sgNVYgHv88Gs4DViviuCngVPUGThdMyP` |
| deepbook | PASS | `HJMKPxz4etgJ8tohSdDKmMDPZkpHKs4zLEnbLocBXzkF` |
| instant unstake | PASS | `ngVhcFrG5CCR17Ta6YuAfXMXBCDcZAsvtGehydsw8Bn` |
| send + per-tx refusal + recovery | PASS | `6PyB9G4qFBqTrEfcF3GGnuYWWavm5wukXKxWvojus3Vm` |
| portfolio | PASS | Read-only MCP exact balances |
| browser pairing / create / fund / grant / swap | PASS | `GQGk5JMuyhcEKMEcpztBvBhmchbZSkd7uMiByyhHYXKh` |

Fresh automated mainnet scenarios also rejected altered owner grants, over-budget attempts and
post-revoke execution. DeepBook resting orders were cancelled and the deposit recovered.
Instant unstake redeemed 0.93 haSUI for 1.006422064 SUI to the owner, with a 1 SUI onchain floor.

The first browser allocation (0.0075 SUI / 0.005 per transaction) could not meet its original
10,000 USDC-base-unit output floor. Simulation refused with code 17 and spent nothing. The floor
was preserved, the unused allocation reclaimed in `9WHeJgwFABfsGYqFuvKZZ5vT5tkruwArPf4xVYKudkto`,
and a new owner-approved 0.015 SUI allocation / 0.01 per-transaction cap was created.
The browser swap sent 11,746 USDC base units to the Backpack owner and charged the agent only gas.
Wallet lifetime spend became 10,000,000 input base units; 5,000,000 remained. A second run was refused
before submission for insufficient vault funds. Browser revoke/reclaim passed in `D8o2h8oo6y4TYFtrADmGkXu6hJan7rRhrLWYmcLtxpN5`;
chain confirms revoked=true and budget=0. Post-revoke execution is refused before signing.

Fresh local checks: Rust 776 passed, 70 ignored; frontend 180 passed and TypeScript check passed;
Move wallet 55 passed. Ignored network tests are not counted as local coverage. Live receipts are
separate from unit-test evidence.

Scope: protected SUI-funded single Cetus swap; legacy bounded send/stake/DeepBook; immediate owned
haSUI redemption; directly owned portfolio. Delayed redemption tickets/claims, external lending
positions and multiasset vault onboarding are not implemented and are not claimed tested.

Machine-local receipts: /tmp/rill-full-retest-mainnet/, /tmp/rill-mcp-send-retest/,
/tmp/rill-browser-release-retest/ and /tmp/rill-browser-over-budget-retest/.
