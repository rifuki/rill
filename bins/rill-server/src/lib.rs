//! Rill's keyless server, as a library so its HTTP surface can be exercised without a socket.
//!
//! The binary is a thin `main` over this. Tests build the same router the process does and drive
//! it through `tower`, which means what they test is what ships rather than a re-declaration of it.

pub mod envelope;
pub mod routes;
pub mod state;
