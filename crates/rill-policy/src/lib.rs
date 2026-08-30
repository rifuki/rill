//! Local, fail-closed verification of an ExecutionEnvelope before a key is ever used.
//!
//! Validation state lives in the type rather than in a boolean: an envelope moves through
//! `RawEnvelope` -> `Validated` -> `BytePinned` -> `Simulated`, each transition consuming
//! the previous value, and the signing function accepts only the last of them. Skipping a
//! check is a compile error instead of something review has to catch.
//!
//! Only checks the chain cannot make are carried here — which protocol a released coin
//! flows into, whether an off-chain simulation actually succeeded, and byte-level pinning
//! of the transaction. Everything `request_spend` and `confirm_spend` already enforce
//! unbypassably is left to them.
