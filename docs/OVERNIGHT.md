# Overnight plan

The working queue. It is picked up automatically and worked straight down; each item says what it
unblocks and how you would know it worked.

**Ground rules for whoever (or whatever) picks this up.**

- Testnet only. `rill` refuses mainnet unless `RILL_ALLOW_MAINNET=true`, and nothing here should
  set it.
- Verify against the chain, not against a document. `rill describe <pkg>::<mod>::<fn>` is ground
  truth; an SDK README is not.
- A test that passes with and without the fix is worthless. Break the fix, watch the test fail,
  restore it. Record both runs.
- Every claim in a commit message must be reproducible from a command in that commit.
- Never write a private key anywhere. `rill` reads `~/.sui/sui_config/sui.keystore` and never
  prints what it read.

## State on chain (testnet)

| | |
|---|---|
| owner / agent | `0xb649a075e07c7cf0baebeaa82150416218c63943e2e767fe93a24aa5c7ce64a9` |
| agent_wallet package | `0xb02f39d682d0471344b1cc264f6f29d625280b9e73560d5beee3db3090563740` |
| Version object | `0xd4f88a6dc271f923f0e55dd96eb8f8762ed4d45199c6719ae92365694478fd65` |
| wallet | `0x20391fa91aec7a12b6657902af80036e125d1beff6621fe2eb73cfd032a04e5d` |
| AgentCap | `0x2e338177b760a1f06d05accc5b4bde68614f50fc44a5e1c5196d9700a3019e7f` |
| rules attached | `budget` 0.2 SUI · `per_tx` 0.05 SUI |

Landed: `create_wallet`, `attach rules`
(`DaVtZtYr39hTcZkTuixGELt8sT81mXNzrpLtz3swQzRv`), gated spend
(`8o4uqBDqhrLtYdeoXUYpmVBGfN9fxUjbx1KtVqTjMnAb`). A 0.06 SUI spend against the 0.05 cap was refused
by `per_tx`, and raising the cap client-side changed nothing — the limit is on chain.

## Queue

### 1. Read the wallet's live rule set from chain

**Why.** `build_manifest_gated_spend` emits one `prove` per rule in the manifest the *caller*
supplies, while `confirm_spend` counts receipts against the wallet's *real* policy. Today that
manifest is hardcoded in `main.rs`, so the two agree only by luck. Emitting a `prove` for an
unattached rule aborts in `df::borrow_mut`; omitting an attached one aborts `E_RULE_NOT_SATISFIED`
(10).

**Do.** `agent_wallet::policy_rules<T>(&AgentWallet<T>): vector<TypeName>` returns it. The
`simulate_read` + `command_returns` path in `crates/rill-ptb/src/book.rs` already reads a `u64`
return value this way; this needs the same for a `vector<TypeName>`. Map each `TypeName` back to its
rule module.

**Done when.** `rill wallet show --wallet <id>` prints `budget, per_tx` for the wallet above, read
from chain, and `rill spend` derives its prove list from that instead of from a flag.

### 2. Reconcile rules instead of only adding them

**Why.** `add_rule` aborts `E_RULE_ALREADY_SET` (11) on a rule already attached, so re-running
`rill wallet rules` fails and there is no way to change or remove a limit. Every rule module has
`remove`, and nothing emits it.

**Do.** Read the live set (item 1), diff against the manifest, emit `remove` for what is going and
`add` for what is arriving. Owner-signed — `add_rule` asserts `ctx.sender() == wallet.owner`.

**Done when.** Running `rill wallet rules` twice in a row succeeds the second time as a no-op, and
changing `--per-tx` actually changes the on-chain cap.

### 3. The lifecycle calls, and the way out

**Why.** `rill wallet create` funds a wallet and hands over its capability with an *empty* policy.
If step 2 then fails permanently, funds sit in an unbounded wallet and this repo has no call that
gets them out. `revoke` is the contract's kill switch and has no emitter.

**Do.** Emitters for `revoke`, `top_up`, `rotate_agent`, `extend_expiry`, each with its signer
asserted from the Move source rather than assumed.

**Done when.** `rill wallet revoke` returns the funds on testnet, with the digest in the commit.

### 4. Two identities

**Why.** Owner and agent are currently the same address, so the delegation the whole design rests on
has never been exercised. `request_spend` asserts `ctx.sender() == wallet.agent`; `add_rule` asserts
owner. With one key both pass for the wrong reason.

**Do.** A second address (`sui client new-address ed25519`), and a way for `rill` to select which
key it signs with. Then: owner creates and bounds the wallet, agent spends from it.

**Done when.** The agent key spends successfully, and the *owner* key attempting the same spend is
refused with `E_NOT_AGENT` (7) — which the abort table now names correctly.

### 5. DeepBook, honestly

**Why.** `balance_manager::deposit` appears to be owner-only: it calls `generate_proof_as_owner`,
which asserts `ctx.sender() == owner`. `request_spend` asserts the sender is the *agent*. One PTB
has one sender, so the combined spend-and-order transaction may be unsatisfiable as designed, and
the `TradeCap` decorative. `deposit_with_cap` is the likely answer.

**Do.** Settle it against the deployed package — signatures, not inference. Then either build the
delegated form, or write down plainly that the flow needs two transactions and why.

**Done when.** Either an order lands on testnet from a wallet-released coin, or `README.md` says
exactly which part cannot work and what the contract would need.

### 6. Gas, properly

**Why.** Gas price is hardcoded `1_000` in three places; a reference-price change makes all three
build transactions the node rejects. `list_owned_objects` pages at 50 with no cursor, so gas
selection on a busy address silently sees a partial set. Gas refs go stale between transactions and
nothing re-reads them.

**Done when.** Nothing in `bins/` names a gas price literal, and the pagination is exercised by a
test.

### 7. The MCP surface

**Why.** The three commands that work are the ones an *owner* runs by hand. The one an agent drives
— `rill_execute` — still stops at `pin_bytes()`. And the four tools exposed today do not describe a
multi-step flow at all.

**Do.** Depends on the Paybox study. The thing to keep in view: rill's difference is that its limits
are enforced by a Move contract, not by a server that could be talked out of them. The tool surface
should make that visible rather than hiding it behind the same shapes a custodial wallet uses.

**Done when.** An agent can drive create → attach → spend over MCP, and a refusal comes back naming
the rule that refused.

## Not in the queue, deliberately

Mainnet anything. Deployment cutover. The OAuth server, which works and is not on the critical
path.

## Kalau kena limit di tengah jalan

Workflow yang mati karena batas sesi **tidak perlu diulang dari nol**:

```
Workflow({ scriptPath: "<path yang dikembalikan waktu diluncurkan>",
           resumeFromRunId: "wf_xxxxxxxx" })
```

Agent yang sudah selesai dikembalikan dari cache seketika; hanya yang error yang dijalankan lagi.
Terbukti: dua run yang mati pada 2026-09-01 punya 6 dan 3 baris `result` di `journal.jsonl`, dan
resume-nya hanya menjalankan sisanya.

**Batasnya: cache itu milik sesi.** Sesi baru tidak bisa resume — run id-nya tidak dikenali, dan
riset yang sama akan dikerjakan ulang dari awal. Jadi yang harus diselamatkan bukan run id-nya,
melainkan **temuannya**: tulis hasil yang sudah masuk ke `docs/research/` sebelum sesi berakhir,
dengan status verifikasinya ditandai jelas.

Sebelum menutup sesi, cek `journal.jsonl` di
`~/.claude/projects/*/subagents/workflows/<run>/` — satu baris `{"type":"result",...}` per agent
yang selesai, berisi nilai kembaliannya utuh. Itu sumbernya, bukan ringkasan di chat.
