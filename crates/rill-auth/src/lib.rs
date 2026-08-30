//! OAuth 2.1 authorization server and Sign-In With Sui.
//!
//! Hand-rolled deliberately: no maintained Rust crate provides the authorization-server
//! side with dynamic client registration, PKCE S256, rotating refresh tokens, and resource
//! binding together. What Rust contributes here is grant and token state expressed in
//! types, not a library.
//!
//! Token type and audience are both inside the MAC, so a refresh token replayed as a
//! bearer fails closed. The authenticated address is derived from the signature, never
//! read from the request body.

pub mod oauth;
pub mod siws;
pub mod tokens;
