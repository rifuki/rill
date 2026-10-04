//! Rill's keyless server, as a library so its HTTP surface can be exercised without a socket.
//!
//! The binary is a thin `main` over this. Tests build the same router the process does and drive
//! it through `tower`, which means what they test is what ships rather than a re-declaration of it.

pub mod agent_docs;
pub mod build;
pub mod envelope;
pub mod mcp;
pub mod oauth_routes;
pub mod request;
pub mod routes;
pub mod state;
pub mod studio_auth;
pub mod studio_compile;
pub mod studio_grants;
pub mod studio_pairing;

pub mod studio_api;
pub mod studio_setup;

pub mod studio_manifest;
