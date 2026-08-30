//! Shared MCP wiring and tool definitions for both binaries.
//!
//! Tool definitions carry the annotations the reference implementation omits: the keyless
//! builders are read-only, while `execute_rill_action` — which submits a real on-chain
//! transaction — is marked destructive, so a client can tell the two apart. Tool names are
//! namespaced, so a second connected MCP server cannot collide with them.
