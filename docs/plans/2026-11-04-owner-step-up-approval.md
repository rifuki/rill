# Owner step-up approval: the owner approves each large granted transaction

Goal: an owner who grants an action can name a threshold, and above it every transaction waits for
the owner's own approval of those exact bytes, given out of band in Studio, before the signer signs.
Below it a granted action keeps running without a chat yes, as before.

## Phase 1: the signer gate (built)

The threshold is `stepUpAboveBaseUnits` inside the grant's run set, never a top-level `Grant` field.
`Grant` refuses unknown fields and signers parse the grant list whole, so a new field would make an
old signer refuse every grant at once. Inside the run set it is covered by the grant's digest, a
grant without it keeps the exact message and digest it was signed over, and an old signer refuses
only a stepped grant (its run set refuses the unknown key), so it never runs one without the pause.
The grant message gains one line, only when the key is present:
`above <n> base units, each transaction also needs my separate approval,`.

On `rill_run_action`, after the bytes are pinned and before the key or a node is touched:

1. The spend compared is the larger of the envelope's declared spend and what the bytes request:
   every `agent_wallet::request_spend` amount, read as a pure `u64` from the bytes
   (`rill_policy::decode::pure_u64_argument`). A builder that understates the spend still meets the
   gate. Bytes whose amount is not a literal are refused (`step_up_amount_unreadable`), since nobody
   could be shown what they approve.
2. Above the threshold the signer answers `pending_owner_approval` / `step_up_required`, signs a
   `Rill step-up request v1` personal message with the agent key, and opens the request on
   `POST /api/step-up`. The server checks the bytes against the pinned digest, Sui's transaction
   digest and the amount, that the request expires within five minutes, that the wallet names the
   agent, that it holds the named grant revision and digest, and the agent's signature. It answers an
   id and a Studio link, `/approve?request=<id>`.
3. The owner opens the link. Studio decodes the bytes, recomputes Sui's digest and the requested
   amount, and offers the sign button only when both match. The owner signs the
   `Rill step-up approval v1` message; `POST /api/step-up/<id>/approve` stores it only when it is the
   wallet owner's signature, and only once.
4. The agent calls `rill_run_action` again with the `approvalId` and a new `idempotencyKey`: the
   first key's stored outcome is the pause. The signer re-reads its grants and requires the approval's
   grant to still be the latest usable one (`grant_changed`), verifies the owner's signature over its
   own copy of the approval text against the owner the chain names now (`approval_not_owner`), and
   executes the held envelope with the approved pinned digest (`step_up_digest_mismatch`). The
   envelope's own five-minute TTL bounds the window (`step_up_expired`).

A run set file with a threshold is refused on `rill_execute` (`step_up_requires_grant`): it carries
no owner's signature and no route to the owner's approval.

What Phase 1 binds: `rill_run_action` and `rill_run_workflow`. `rill_spend`, `rill_swap`,
`rill_stake` and a hand-supplied `rill_execute` are already refused with `use_granted_action` on a
wallet holding a usable grant when a Rill API is configured. Anyone who can read the agent's
keystore (an agent with a shell, the `rill-wallet` CLI) signs without asking the signer and is bound
only by the wallet's on-chain rules.

## Phase 2: a `step_up` rule on chain (deferred, needs a mainnet package upgrade)

Not built. It is the variant that binds whoever holds the agent key, and it changes the published
`agent_wallet` package, which cannot be undone, so it waits for an explicit decision.

- A new module `agent_wallet::step_up` with `add`, `remove` (owner-only, through `add_rule` and
  `remove_rule`) and `prove`. `prove` keeps a running total per transaction digest, as `per_tx`
  does, and above the threshold asserts `tx_context::sponsor() == some(owner)`.
- The owner approves by signing the exact `TransactionData` as gas sponsor on the same Studio page,
  so the approval is the transaction signature itself; the signer submits both signatures.
- The builder must build with the owner's gas above the threshold, and Studio and the signer keep
  refusing any command that touches the gas coin, since the gas is then the owner's.
- `RuleKind::StepUp` and its prove call in the builder. An older builder omits the prove and
  `confirm_spend` aborts, which fails closed.
- It is a compatible upgrade (a new module, config in a dynamic field like the other rules), but it
  needs the upgrade capability, a testnet check that `sponsor()` returns the gas owner in simulation
  and in execution, and puts gas cost on the owner above the threshold.
