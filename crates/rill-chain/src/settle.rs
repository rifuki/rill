//! Waiting until the node can answer for what a transaction just created.
//!
//! # Certified is not the same as visible
//!
//! `execute` returns when the validators have certified the transaction, and the fullnode this
//! client is talking to may not have indexed its objects yet. Reading one back immediately answers
//! `not found on chain` for an object that certainly exists.
//!
//! That never showed up while creating a wallet and bounding it were two commands a person typed:
//! the seconds spent reading an id off the terminal and typing the next line were the wait. It
//! showed up the first time an agent drove both calls back to back, on testnet, with the wallet the
//! create had just minted: `reading the wallet object: not found on chain: object 0x78f283fe…`, one
//! call after the effects had named that very id. A flow whose second step fails on its first
//! attempt is not a flow an agent can drive.
//!
//! # Why this blocks the thread
//!
//! The callers are the CLI commands and the stdio tools, and each of those owns a current-thread
//! runtime built for one command, with nothing else scheduled on it. Blocking it is therefore
//! blocking nothing, and the alternative is a timer feature on the async runtime for a sleep that
//! happens once per created object. It must not be called from a shared runtime that is serving
//! other work, which is why no path in `rill-server` calls it.

use std::time::Duration;

use crate::SuiRead;

/// How long to wait for the node to catch up, and how often to ask.
///
/// Thirty seconds in half-second steps, the same budget the live tests use for the same question. A
/// testnet fullnode usually answers on the first or second ask; the budget exists for the time it
/// does not.
///
/// Public because the condition is not always "this object exists": an attach waits until the rule
/// list it wrote is the one a read answers with, which is its own question asked on the same
/// schedule. One budget, named once, so two waits cannot drift apart.
pub const TRIES: usize = 60;
pub const PAUSE: Duration = Duration::from_millis(500);

/// Wait until `object_id` can be read, or give up. `false` means it never became readable.
///
/// Giving up is not an error: the transaction is on chain and its digest is the proof. What the
/// caller has lost is the ability to promise that the next step will find the object, which is
/// something it should say rather than something it should pretend.
pub async fn wait_until_readable(chain: &impl SuiRead, object_id: &str) -> bool {
    wait_until_readable_within(chain, object_id, TRIES, PAUSE).await
}

/// The same wait with its budget named, so a test can exercise the giving-up path in milliseconds
/// rather than in half a minute.
pub async fn wait_until_readable_within(
    chain: &impl SuiRead,
    object_id: &str,
    tries: usize,
    pause: Duration,
) -> bool {
    for attempt in 0..tries {
        if chain.get_object(object_id).await.is_ok() {
            return true;
        }
        if attempt + 1 < tries {
            std::thread::sleep(pause);
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeSui;
    use crate::{ObjectRef, ObjectSummary};

    const ID: &str = "0x0000000000000000000000000000000000000000000000000000000000000abc";

    fn object() -> ObjectSummary {
        ObjectSummary {
            reference: ObjectRef {
                id: ID.to_owned(),
                version: 1,
                digest: String::new(),
            },
            object_type: None,
            fields: None,
            shared_initial_version: Some(1),
        }
    }

    fn run<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime")
            .block_on(future)
    }

    #[test]
    fn an_object_the_node_already_has_needs_no_waiting() {
        let chain = FakeSui::new().with_object(None, object());
        assert!(run(wait_until_readable_within(
            &chain,
            ID,
            1,
            Duration::from_millis(0)
        )));
    }

    /// Giving up is reported, not raised: the caller has a digest and has to say what it cannot
    /// promise.
    #[test]
    fn an_object_the_node_never_indexes_is_reported_as_not_readable() {
        let chain = FakeSui::new();
        assert!(!run(wait_until_readable_within(
            &chain,
            ID,
            2,
            Duration::from_millis(1)
        )));
    }
}
