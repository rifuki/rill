//! Persistence behind a trait, with a file-backed implementation.
//!
//! The file implementation reads the reference deployment's existing `skills.json` and
//! `oauth.json` unchanged, so going live needs no migration. Note that `tool_defs` are
//! recomputed on load rather than trusted from disk, so this is a port of behavior and not
//! merely of a format.
//!
//! A Postgres implementation drops in later to remove the single-replica constraint the
//! file stores impose — two replicas would each hold half the authorization codes.
