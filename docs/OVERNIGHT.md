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

## Sudah selesai sejak rencana ini ditulis

- Daftar `prove` dibaca hidup dari chain lewat `policy_rules` (item 1).
- Rule bisa direkonsiliasi, bukan cuma dipasang sekali (item 2) — dibuktikan dengan menurunkan
  per-tx cap 0.05 → 0.02 dan spend 0.03 yang tadinya lolos jadi ditolak.
- Lifecycle lengkap: `revoke`, `top_up`, `rotate_agent`, `extend_expiry` (item 3).
- Permukaan MCP bisa didorong agent: `rill_status`, `rill_wallet`, `rill_spend`, `rill_execute`.
- Tabel abort diperbaiki (geser satu di semua kode `agent_wallet`) dan dikunci ke sumber Move.

Digest yang mendarat, urut: `DaVtZtYr…QzRv` (rules), `8o4uqBDq…MnAb` (spend), `7Hf7EGyR…HSzX`
(spend dari policy chain), `CTkxPAZX…b1jF` (reconcile ulang), `6biB5gaM…ahKC` (turunkan cap),
`DpTPdMKb…WhmW` (spend lewat MCP).

## Riset yang sudah ada, dan statusnya

`docs/research/2026-09-01-defi-addresses-unverified.md` — alamat Cetus, Haedal, DeepBook
BalanceManager, pool testnet, dan framework Sui. **Satu lapis, belum diverifikasi ulang.** Jangan
masuk registry sebelum dicek dengan `rill describe`. Header file itu menjelaskan kenapa.

The queue that lived here is superseded by `docs/plans/2026-09-10-001-feat-rill-operational-mcp-plan.md`; the state table above stays live.

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
