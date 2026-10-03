# Owner-signed action grants: one MCP server runs Studio actions

Goal: an agent with only the `rill` plugin can list and run the actions its wallet's owner published
in Studio, with no run-set file copied by hand, and without trusting the Rill server for what it may
sign.

## Why a grant

Today a Studio action reaches an agent in three manual pieces: the owner downloads a run set and
build arguments after onboarding, sets `RILL_RUN_SET_PATH` for the signer, and the agent calls the
hosted `rill_build_action` before handing the envelope to the local `rill_execute`. The run set is
what stops a compromised server from getting the signer to sign anything the owner did not intend
(an extra transfer, another pool, a larger amount), so it cannot simply be fetched from the server.

A grant is that run set and its build arguments, signed by the wallet's owner as a personal message.
The server stores and serves it. The signer trusts it only after checking, against the chain, that
the signature is from the address the wallet names as its owner and that the wallet names this
signer as its agent. The trust anchor moves from "a file a person copied" to "the owner's key".

## Message

`rill_core::grant` builds the exact text, from the grant alone, on both sides:

```
Rill action grant v1
Agent 0x… may run "<action name>" (<action id>) from wallet 0x… on <network>,
at most <per-tx> MIST per transaction.
Digest: <sha256 of the grant's canonical JSON>
```

Canonical JSON is `serde_json` with sorted keys and no whitespace, computed in Rust only. Studio
never canonicalizes: it signs the message the server's prepare endpoint returns, and the signer
recomputes the same message from the grant it received. Any change to the grant changes the digest
and the signature no longer verifies.

## Verification on the signer

1. Fetch grants for the signer's own address from `<apiUrl>/api/grants/<agent>`.
2. For each: read the wallet object; its `owner` is the expected signer of the grant and its `agent`
   must equal this signer's address. A wallet that is revoked or expired is listed as unusable.
3. Verify the signature over the recomputed message with the node's `VerifySignature`, passing the
   owner address. The node covers ed25519, secp256k1/r1, multisig and zkLogin, which a browser
   wallet may use.
4. Only a verified grant's run set is used as the pinned run set for that action's execution, through
   the same validation, re-simulation and signing path as `rill_execute`.

## Surface

- Signer: `rill_actions` (read-only) lists verified grants with their limits; `rill_run_action
  { actionId, params? }` (destructive) builds through the hosted builder with the grant's build
  arguments and executes under the grant's run set. `apiUrl` joins `~/.rill/config.json`.
- Server: `POST /api/grants/prepare` returns the message for a grant the owner is about to sign;
  `POST /api/grants` stores a signed grant after checking the bearer owns the action and the wallet;
  `GET /api/grants/{agent}` lists them publicly (public ids only, nothing secret).
- Owner without a browser: `rill-wallet grant --wallet <id> --action <id>` under the owner key does
  the prepare, sign and store round trip.
- Studio: after the attach step succeeds, ask the owner to sign the grant instead of downloading a
  run-set file.

## Not in scope

- Revoking a grant without revoking the wallet. A revoked or expired wallet already ends every grant
  on it, which is the kill switch the contract provides.
- Hosting. The API URL is configuration; a public deployment is a separate step.

## Architecture extension: owner-controlled vaults and agent pairing

Planning addition, 2026-10-04. The implementation described above remains a bounded transport
and local-signer improvement. The architecture below is proposed work, not a claim that it is
implemented or deployed. No tests or transactions were executed for this planning addition.

### Trust boundaries

- The user's main wallet owns the funds and approves vault funding and permissions. Its private
  key never enters the agent runtime or the Rill server.
- A vault is a Move object holding a limited allocation, not a new keypair. Administrative control
  and permission to execute are separate authorities.
- Default to one signer per agent runtime, reusable across skills. Use separate signers for separate
  runtime installations. Never use one service-wide signer for all customers.
- Start with one isolated vault per agent/strategy. Several skills can use that strategy only when
  their combined activity fits its shared budget. Do not present each skill as having an independent
  budget when the onchain counter is shared.
- The Rust server stores metadata and builds unsigned transactions. A local signer validates,
  simulates, signs and submits. A remote-MCP-only client requires a separately provisioned signer
  runner; connecting the MCP endpoint does not supply transaction-signing capability.
- Package upgrade authority is separate from user vault authority and routine execution keys.
  Upgrade governance remains part of the trust model even when Rill does not hold user keys.

### Two different permissions

An owner-signed action grant authorizes the cooperating local signer to execute a pinned workflow.
It protects that signer from an altered server response. It does not constrain an attacker who
controls the agent key and submits a different transaction directly.

The onchain AgentCap and vault policy determine what that key can actually spend. Current budget,
per-transaction, rate and time rules must not be described as enforcing protocol, recipient or
settlement restrictions merely because a signed run set contains those restrictions.

Keep these concepts explicit in names and UI:

| Concept | Authority |
| --- | --- |
| Studio login / MCP token | Access to application metadata and tools |
| Pairing proof | Proof that a runtime controls an agent address |
| Owner-signed action grant | Approval of a workflow for the validating local signer |
| Onchain vault policy and AgentCap | Spending permission enforced against the transaction signer |

### Skill lifecycle and execution binding

Separate reusable skill definitions from execution bindings. A definition contains its workflow,
input schema, instructions and immutable published version. A binding references the definition
version, chain identifier, exact deployment, owner, agent address, vault and capability identifiers,
and the applicable policy revision. The database is an index of these relationships, not their
spending authority; execution rechecks the chain.

- Editing instructions or descriptions creates a metadata revision without granting new spending
  rights. Do not allow a metadata edit to silently change execution semantics.
- Editing executable workflow content creates a new version and requires a fresh owner-signed
  action grant when the pinned workflow changes, even if its spending bounds are unchanged.
- Increasing authority requires an owner-approved onchain policy change in addition to any
  offchain grant update. Creating or publishing a skill alone never grants authority.
- Shared skill templates contain no private key, authentication token or transferable user grant.
- Replacing a signer creates a new binding and invalidates the old executor's onchain access.
  Existing users' pinned versions must not follow a mutable latest-version pointer automatically.

### Delivery sequence and verification gates

1. **Record the current baseline and reconcile WIP.** Inspect the existing grant work in
   `crates/rill-core/src/grant.rs`, `crates/rill-chain/src/`, and the current Move package. Identify
   which endpoints and frontend flows actually exist. Read current deployment receipts and onchain
   ownership before planning migration. Preserve unrelated edits and do not recreate funded objects.
   Gate: a factual inventory distinguishing implemented, tested, deployed and proposed behavior.

2. **Finish signed action delivery as its own bounded slice.** Use the existing plan above. Pin the
   workflow digest, network/deployment, wallet, agent, expiry and revision in the signed authority.
   Present the material action, asset, destination and limits in the owner's approval UI, not only
   a digest. Recheck current vault revocation, expiry and signer identity before every execution.
   Gate: modified workflow, wrong owner, wrong network, rotated agent and expired/revoked wallet
   are rejected. Label remaining local-only enforcement explicitly.

3. **Implement pairing without importing keys.** Add a short-lived pairing request, generated by
   either Studio or the runtime, and proof of possession by the agent signer. Bind the challenge
   to the request ID, application domain, intended owner session, agent address, network, purpose,
   expiry and unpredictable nonce. Consume it atomically once. The owner's authenticated session
   reviews the resulting address and separately approves the onchain grant.
   Gate: replay, expiry, address substitution, cross-session claiming and concurrent redemption
   fail; a pairing proof or MCP token alone cannot spend or modify the vault.

4. **Separate definitions, versions and bindings in storage/API.** Extend
   `crates/rill-store/src/lib.rs`, its persistence implementation, and the Studio/MCP surfaces in
   `bins/rill-server/src/`. Preserve existing IDs and URLs with an explicit legacy mapping.
   Gate: two owners using the same template cannot access each other's bindings; a new definition
   version cannot mutate an existing authorized execution; editing a name cannot reset a budget.

5. **Close the onchain action and settlement boundary.** Select one supported swap adapter for
   the first protected action. It must enforce actual input asset, amount, supported target/pool,
   minimum output and owner-controlled destination. A caller-supplied target label is insufficient.
   Do not leave a generic coin-release path that bypasses these restrictions for the same protected
   vault. Return swap output and unused funds to the owner for the single-funding-asset MVP.
   Gate: direct agent-signed PTBs attempting arbitrary transfer, alternate protocol, wrong asset,
   missing settlement, incorrect destination or insufficient output abort onchain. Successful swap
   receipts prove both the output destination and counter updates.

6. **Design the contract transition before publishing.** Audit the deployed struct layouts and
   upgrade policy against the intended changes. Prefer a successor type/package and explicit
   owner-authorized migration when compatibility cannot preserve the intended invariants. Keep a
   usable owner withdrawal/revocation path for legacy vaults; moving funds requires owner authority.
   Consider vault-bound OwnerCap for the successor design rather than changing existing ownership
   semantics implicitly. Separate package version, policy revision and action-grant revision.
   Gate: edits preserve spending/rate counters, stale authority fails, rotated agents lose access,
   and owner exit works during pause or version transitions. Do not claim per-grant revocation
   before a mechanism actually enforces it; the current wallet-wide kill switch is the baseline.

7. **Wire the frontend and local plugin.** Extend the sibling frontend's
   `../rill-ts/rill-frontend/src/routes/agent-wallet.tsx` and its API/transaction helpers after reading
   its local instructions. Provide connect, pair, review/fund, monitor, rotate and revoke flows.
   Show owner and agent addresses separately, active network, policy enforcement layer and grant
   state. Reconnect or refresh MCP tool discovery after publication when client support requires it.
   Gate: a user can create a skill with the main wallet, pair a separate signer, execute one action,
   edit a workflow through a new approval, revoke access, and recover the remaining allocation.

8. **Verify end-to-end before another mainnet transaction.** Run targeted Rust, Move and frontend
   regressions for the preceding gates, then exercise localnet or a suitable fork/integration setup.
   Mainnet smoke remains a separate execution step under the user's existing total $10 limit,
   after reconciling prior spending and the current deployment state. Record transaction digests,
   actual gas, input/output amounts, recipients and remaining balances. This plan does not restart
   deployment or authorize spending beyond that limit.

### Scope and completion

Defer multiasset treasury accounting, several independently budgeted agents sharing one vault,
embedded wallets, sponsored gas and a permission marketplace. The first complete vertical slice
is one owner, one separately paired agent signer, one isolated allocation, one protected swap,
versioned skill delivery, owner revocation and withdrawal.

The earlier plan's exclusion of per-action revocation applies to its initial signed-grant slice.
Do not expose an offchain delete button as effective onchain revocation. Stronger independent
revocation requires additional onchain state or separately revocable vaults.

Planning confidence: role separation and the delivery sequence are settled. Exact adapter design,
compatible contract evolution and current WIP/deployment state require evidence in steps 1 and 5-6
before implementation claims or release readiness can be established.
