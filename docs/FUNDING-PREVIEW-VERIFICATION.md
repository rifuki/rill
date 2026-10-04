# Funding preview verification

The single-Cetus mainnet setup now obtains a read-only quote before creating an empty wallet and rechecks it before producing the funding transaction. The owner-published minimum stays unchanged. An insufficient quote returns a refusal instead of a funding PTB.

## Source and checks

- Rust main: `0882cd9`; frontend main: `a4b4fcc`.
- Rust workspace: 788 passed, 71 ignored. Clippy and Linux server build passed.
- Frontend: 182 passed; direct TypeScript compiler and production build passed.
- Read-only live mainnet quote regression passed: 5,000,000 SUI base units returned 5,870 USDC base units; 10,000,000 returned 11,741. The published floor was 10,000. Fees were respectively 12,500 and 25,000 SUI base units, included in the gross input.
- Native serial review found no remaining actionable findings. No independent cross-model coverage was performed.
- Production unauthenticated `POST /api/setup/preview` returned 401.
- Tencent API and web deployed; Vercel deployment `dpl_D1QhzynswDLGzRkkv9aEhwNrtyx9` Ready and aliased to `rill.naisu.one`.

## Boundaries

Quotes are snapshots. A later execution can still refuse because prices changed; the on-chain floor remains authoritative. Required quotes apply to single-Cetus mainnet setup. Existing multi-action behavior and testnet without an explicitly configured quote package remain unchanged.

Known SUI and USDC amounts use exact decimal formatting; unknown assets stay explicitly in base units. Changing the setup form hides its previous quote. A preview does not sign or submit a transaction.

Signer status distinguishes owner-signed `rill_run_action`, which requires a valid grant, from generic `rill_execute`, which requires a run-set. Protected abort codes 12 through 19 identify adapter, revision, pool, output-floor, asset, and change failures without proposing an automatic policy bypass.

## Signer DNS regression found during release smoke

The released signer initially timed out listing actions twice because DNS returned an unreachable synthesized IPv6 address before the working IPv4 address. `curl -4` returned HTTP 200 in 0.177 seconds; `curl -6` timed out. The production regression failed before the patch at three seconds.

Commit `569f101` races resolved transport connections and cancels unused attempts. Certificate verification and SNI remain unchanged. Only the winning connection sends an HTTP request. The live regression passed in 0.31 seconds after the patch; actual MCP action listing and portfolio passed in 1.09 and 0.73 seconds. Signer package checks: 211 passed, 13 ignored; both live HTTPS tests and Clippy passed. Native source review had no actionable findings.
