---
name: rill
description: Use when the user wants an AI agent to move money on Sui through a Rill agent wallet, such as running an action the wallet's owner granted, checking a wallet's limits, quoting or making a swap, or sending a payment. Covers setup when the signer is not ready, granted actions, reading limits before spending, never retrying a submitted transaction, and what a refusal means.
---

# Rill agent wallet

The `rill` MCP server is a local signer that holds the agent's key. The wallet's limits are a Move
contract on Sui: a budget and a per-transaction cap the chain enforces, so nothing you pass to a
tool can widen them. The wallet's owner creates it, sets the limits and funds it from their own
wallet; the agent only spends inside them.

## First, is the signer ready?

Call `rill_status`. If it reports no key, or the wrong network, the user has to configure it once,
in a terminal, because choosing which key signs is theirs to decide, not yours:

```sh
~/.rill/bin/rill-wallet setup --network mainnet --as <agent address> --api <Rill API URL>
```

`--as` names a key already in the Sui keystore (`sui client addresses` lists them). Mainnet signing
stays off until the user adds `--allow-mainnet` themselves. Do not run that flag for them, and do
not edit `~/.rill/config.json`. After setup the user restarts the client so the server reloads.

## Granted actions come first

The wallet's owner publishes actions in Rill Studio and grants them to this agent by signing them.

1. `rill_actions` lists every grant for this agent, each checked against the chain just now. Only
   entries with `usable: true` can run; an unusable one says why in `refusedBecause`.
2. `rill_run_action { actionId }` builds that action and executes it under the owner-signed limits.
   Pass `walletId` only when the same action is granted from more than one wallet.

A grant that is refused (not signed by the owner, expired, wallet revoked, agent rotated) is the
system protecting the owner. Report it; the owner fixes it by granting again in Studio.

## Before any other spend

1. `rill_wallet { wallet }` and read `largestSpendNow`. Never attempt more than it allows.
2. Tell the user the amount, the asset and the recipient before calling a tool that submits.

## Swaps

Call `rill_quote` immediately before `rill_swap` and pass its `swapArguments` through unchanged.
Never drop `minOut`. An `E_SLIPPAGE` refusal means quote again with a wider `slippageBps`.

## Rules that matter

- A tool that reports `submitted: true` is final. Do not call it again to make sure: a second call
  is a second payment. Report the digest.
- A refusal that names a rule (`per_tx`, `budget`, revoked, expired) is the wallet working. Report
  it; do not retry with the same or a larger amount, and do not look for another way around it.
- Only the owner can raise limits, top up, or revoke. Those happen in the owner's wallet, not here.
