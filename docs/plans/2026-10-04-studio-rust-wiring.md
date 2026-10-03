# Connect Studio to the Rust server

Goal: the existing TypeScript Studio uses the Rust server for discovery, graph preview, publication, wallet identity, consent, and bounded wallet onboarding. No TypeScript backend proxy or server signing.

Implementation units:
1. Wallet authentication: reuse single-use OAuth request storage, verify the exact personal message, derive identity from the signature, redirect agent authorization to Studio consent. Preserve PKCE, resource binding and token rotation.
2. Graph compilation: compose existing Rust protocol adapters, resolve shared objects from the chain, serialize BCS TransactionKind for the browser. Preview simulation is explicitly unverified when input checks are disabled. Strict agent execution retains its signing gate.
3. Studio HTTP contract: protocol defaults, real package introspection, semantic descriptors, capability projection, compile/simulate, owned publication and per-skill MCP. Store the published flow and capability manifest together.
4. Onboarding: create an empty wallet and optional DeepBook manager/capabilities. A second owner-signed transaction attaches rules and funds the wallet atomically. Return a Rust-compatible run set only after resolving actual objects.
5. Frontend: default local backend to port 3939, preserve exact decimal strings, decode BCS kinds, include manifests in publication and cache identity, complete and retry onboarding, export the run set.
6. Verification: reproduce contract failures before production edits; run Rust workspace tests and clippy, frontend tests/typecheck/build, HTTP integration and browser desktop/mobile checks. No live spend or mainnet cutover.

Ownership during the implementation wave:
- Auth worker: oauth_routes.rs, studio_auth.rs, auth/siws.rs, studio_auth tests.
- Compiler worker: studio_compile module and its tests.
- Frontend worker: frontend sources and tests only.
- Coordinator: route integration, state/config, dependencies, chain introspection, Studio HTTP/setup, documentation, authoritative verification.

Existing unrelated image deletions and warden.html in the TypeScript repository are excluded. Existing frontend features and contracts determine compatibility; production hosting is outside this local integration request.
