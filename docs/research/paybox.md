# PayBox, read properly

MoonPay's agent wallet, launched 2026-07-29. Researched 2026-09-03 from `docs.paybox.sh`, the
connector attached to this session, and press coverage. Everything below is quoted or paraphrased
from a source; where a claim is inference it says so.

The reason to read it: PayBox is the closest thing to a competitor rill has, it is well designed,
and the places where rill should **not** copy it are more instructive than the places where it
should.

## What it is

> "the non-custodial wallet for AI agents"

A credential control plane, not a custodian. You vault a credential once; an agent then acts against
it without ever receiving the underlying material. MoonPay's own framing in the help centre is
"1Password meets Stripe Link, built for agentic workflows".

Three credential types, each held by a different system:

| Type | Held by |
|---|---|
| Card | Basis Theory, tokenised |
| Wallet | MoonX MPC key shares inside a TEE |
| Secret | Envelope-encrypted via an external KMS |

What an agent receives is never the credential: "a one-time virtual card", "a signed transaction",
or "a short-lived token".

## The architecture, and the part worth stealing

**One agent, one key.** An agent — Claude Code, an OpenAI agent — "runs under its own client key.
One agent, one key." That is what makes an agent revocable without touching the credential
underneath, and it is the same instinct as one `AgentCap` per wallet.

**Signing is 2-of-2, and the model holds neither half.** The private key lives in MoonX MPC shares
inside a TEE. The authorisation to use it comes from a signing window rendered as an MCP UI resource
(`ui://paybox/wallet-sign`) that holds its own keypair. The docs are explicit:

> "signs **client-side** via MoonX MPC. The private key never reaches PayBox, the agent, or the
> model."

So `request_*` is called by the model and `submit_*` is called by the signing window — a split of
**principals**, not of steps. Every submit tool is prefixed "Signing-app only." The model cannot
call them because it cannot produce the window's signature. The barrier is cryptographic, not a
written instruction.

**The server pins the bytes.** `submit_envelopes` "sends NO transaction bytes (the server already
holds them, pinned at park time), so it cannot substitute a different transaction." The window
proves it approved a digest; the server proves that digest is the plan it quoted. Neither side picks
the transaction alone.

## Approvals

Per-credential **mode**, chosen at consent, not per call:

- **App approval** — every operation pauses for a passkey in the PayBox app, plus a mobile push.
- **iframe** — "the user confirms in the in-chat signing window, which then signs and submits
  immediately." Wallets only; cards and secrets cannot use it.
- **Autonomous** — "the agent acts without per-op approval, within the grant."

Two properties do the real work:

> "Approvals are **operation-bound**: change a parameter (amount, merchant, payload) and it's a new
> request — an approval can't be replayed against different operation details."

> "Sensitive operations still pause for the **user's passkey** regardless of the app's token — the
> token alone never bypasses step-up."

A **step-up** tier — revealing secrets, signing, issuing or revoking keys — needs "a **fresh**
passkey assertion (within a short step-up window)". Even in autonomous mode.

## The request lifecycle

    pending_approval → pending_signature → pending_settlement → pending_confirmation
                                                              → success | denied | error

The agent's whole job is: call one `request_*`, keep the `request_id`, show `approval_url` if the
state is `pending_approval`, then poll `get_request` until terminal.

> "submit once, then poll — never re-issue the original tool call to 'finish' it."

Money tools are non-idempotent and say so in their own descriptions: `request_transfer` reads "never
re-call this tool for the same request because that can send a second transfer." Recovery from a
lost signing window is its own tool — `reopen_signing_window` — which "does not quote, rebuild, or
create another operation. Use this instead of re-calling any money or wallet-sign tool."

## OAuth 2.1

`https://api.paybox.sh/mcp`, Streamable HTTP, protocol `2025-06-18`.

| | |
|---|---|
| PKCE | S256 required; other methods rejected |
| Registration | `POST /oauth/register`, public client, `token_endpoint_auth_method: none` |
| Scopes | `mcp`, `offline_access` |
| Authorization code | 5 min, opaque, single use |
| Access token | 60 min, JWT HS256, audience-bound to the MCP resource |
| Refresh token | 30 days sliding, opaque, rotated on every use |

> "Replaying a used code revokes the client (treated as compromise)."

## The tool surface

Roughly thirty tools, grouped:

- **Read** — `list_credentials`, `get_portfolio`, `get_request`, `list_requests`, `get_contract`,
  `resolve_username`, `verify_solana_balance`
- **Request** — `request_payment`, `request_transfer`, `request_swap`, `request_wallet_sign`,
  `request_secret`, `request_account_change`, `pay_x402`
- **Signing-app only** — `submit_envelopes`, `submit_signature`, `moonx_sign`,
  `moonx_resolve_binding`, `reopen_signing_window`
- **Discovery** — `discover_services`, `discover_plugins`, `use_service`, `use_plugin`
- **Onboarding** — `get_buy_link`, `claim_payment_credentials`

Reads and writes are never the same tool.

## What rill should take, and what it should not

**Take: the request/submit split as a split of principals.** rill already has this shape — a keyless
server proposes, a local binary re-derives and signs — but rill's version is stronger in one respect
and weaker in another. Stronger: the limits are a Move contract, so no server can be talked out of
them. Weaker: nothing sits between the model and the local signer except the run-set, because the
signer and the thing driving it are on the same machine.

**Take: non-idempotence stated in the tool description.** rill does this now, and it came from
reading PayBox. A money tool that does not say "do not retry" gets retried.

**Take: a dedicated recovery tool.** `reopen_signing_window` exists so that "lost the window" never
becomes "call the money tool again". rill has no equivalent, and its failure mode is the same.

**Take: operation-bound approval.** rill's byte pin is the same idea — the digest covers the exact
transaction — but rill has no human in the loop to bind an approval *to*.

**Do not take: the modes.** PayBox's three approval modes exist because its limits live in a
server that could otherwise be asked for anything. rill's limits are on chain: `budget`, `per_tx`,
`rate_limit`, `time_window` are enforced by Move, and a client that raises them locally changes
nothing. That is a different guarantee and the surface should say so rather than hiding it behind
the same abstractions a custodial wallet uses.

**Do not take: hiding the refusal.** PayBox returns `denied` with a reason. rill can do better,
because the refusing party is a named rule in a contract: "per_tx refused it: this spend is larger
than the per-transaction cap. The limit is on chain, not in this client."

**The honest gap: rill has no human step at all.** PayBox's passkey is a real second factor; rill's
equivalent is the owner holding a different key from the agent, which bounds what the agent can do
but never pauses it. For a wallet with a budget of 0.2 SUI that is proportionate. For anything
larger it is not, and the answer is probably not a passkey but a smaller budget.

## Sources

- <https://docs.paybox.sh> — overview, navigation
- <https://docs.paybox.sh/concepts/model> — credentials, agents, grants
- <https://docs.paybox.sh/concepts/approvals> — modes, operation binding, step-up
- <https://docs.paybox.sh/concepts/requests> — lifecycle, states, signing window
- <https://docs.paybox.sh/connect/mcp> — endpoint, transport, tools
- <https://docs.paybox.sh/connect/oauth> — OAuth 2.1 in full
- <https://fortune.com/2026/07/23/moonpay-launches-universal-ai-shopping-wallet-for-non-technical-claude-and-chatgpt-consumers/>
- <https://www.unite.ai/moonpays-paybox-lets-ai-agents-spend-without-taking-custody/>
- The PayBox MCP connector's own server instructions, attached to this session
