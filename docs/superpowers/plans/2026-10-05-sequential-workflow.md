# Sequential Agent Workflow Implementation Plan

> **For agentic workers:** Execute this plan task-by-task in the current session. Steps use checkbox (`- [x]`) syntax for tracking. The named superpowers execution skills are unavailable; native execution is authorized by the user.

**Goal:** Run separately approved Sui actions in one ordered agent workflow without repeating successful transactions.

**Architecture:** The local MCP signer preflights every pinned grant before starting, then rechecks each action through the existing signing path. A durable local receipt claims the workflow run before execution and checkpoints each step; retries return the receipt and never submit again. Studio exports selected grants in an explicit order with one owner, network and signer. Outputs are not automatically reinvested and this is not atomic.

**Tech Stack:** Rust, serde, existing MCP transport, React, TypeScript, Vitest.

---

## Task 1: Durable execution

Files: `bins/rill/src/workflow.rs`, `bins/rill/src/lib.rs`, `bins/rill/src/stdio.rs`, `bins/rill/tests/workflow.rs`, `crates/rill-mcp/src/lib.rs`, `crates/rill-mcp/tests/surface_split.rs`.

- [x] Test sequential success, stop after refusal, uncertain submission, interrupted receipts, restart replay, changed definition, and concurrent claims.
- [x] Run `cargo test -p rill --test workflow` before implementation and record the missing-module failure.
- [x] Define an immutable workflow with runId, network, signer, owner and 1 to 10 steps. Each step pins actionId, walletId, revision and optional node params. Reject unknown fields and malformed identifiers.
- [x] Claim a run using create_new, sync a started checkpoint before invoking a step, and sync its response before advancing. Return partial results on any failure. Existing receipts never execute again, even after a restart.
- [x] Preflight all exact grant revisions and owners; use existing run_action for each step with a revision selector. Advertise rill_run_workflow as destructive on the signer only.
- [x] Run workflow tests, signer/MCP suites and Clippy.

## Task 2: Studio export

Files: `rill-frontend/src/lib/agent-workflow.ts`, its test, `rill-frontend/src/components/agent-wallet/workflow-export.tsx`, `rill-frontend/src/routes/agent-wallet.tsx` in the frontend repository.

- [x] Test rejection of mixed owners, networks/signers, unactivated grants and duplicate vaults.
- [x] Add a compact ordered selection beside saved budgets. Selecting budgets adds steps; moving or removing a step changes the export order. Never execute or approve funds in this component.
- [x] Export a fresh runId and exact grant references for rill_run_workflow, with clear separate-budget and partial-success copy.
- [x] Run frontend tests, TypeScript, scoped lint, production build and browser checks.

## Task 3: Deliver

- [ ] Update the agent skill with no retries, no automatic reinvestment, and receipt inspection guidance.
- [ ] Validate a combined workflow through MCP using isolated test fixtures and refusal paths against production grants. Do not claim another real three-step mainnet execution unless fresh approved grants exist.
- [ ] Commit only owned files, deploy frontend to both hosts, install the locally built signer without altering its key/config, and record evidence in `_handoff/journey.md`.
