//! Initial shared versions, resolved rather than assumed.
//!
//! # Why a shared object cannot be entered as version zero
//!
//! A shared object is referenced in a transaction by its id *and the version at which it was first
//! shared* — not its current version, and not zero. Get it wrong and the node refuses the whole
//! transaction before executing anything:
//!
//! ```text
//! Error checking transaction input objects: Could not find the referenced object
//! 0xb663828d…79fc22 at version None
//! ```
//!
//! Every shared input in this crate once passed `0`. That is not a version any object was shared
//! at, so nothing built here could have reached a real validator — and the failure arrives as a
//! "could not find the object" message that reads like a wrong address, which is the wrong thing
//! to go looking at.
//!
//! # Resolved, and refused when unknown
//!
//! The version is a fact about the chain, so it is read from the chain and passed in. A lookup that
//! comes up empty is an error rather than a fallback: defaulting to zero is what produced the bug,
//! and defaulting to *any* number would build a transaction whose rejection says nothing about why.
//!
//! The two framework singletons are exceptions with real answers rather than guesses — `0x5` and
//! `0x6` are shared in the genesis transaction, at version 1, on every Sui network.

use std::collections::BTreeMap;

use sui_sdk_types::Address;
use sui_transaction_builder::ObjectInput;

/// Sui's framework singletons are shared at genesis, so their initial version is 1 everywhere.
pub const GENESIS_SHARED_VERSION: u64 = 1;

/// The initial shared version of every shared object a transaction is about to reference.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SharedObjects {
    versions: BTreeMap<Address, u64>,
}

/// A shared object was referenced without its initial version being known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownSharedVersion {
    pub object_id: Address,
}

impl std::fmt::Display for UnknownSharedVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the initial shared version of {} is not known; it must be read from the chain before \
             the object can be referenced, and guessing at it builds a transaction the node will \
             reject with a message about the object being missing",
            self.object_id
        )
    }
}

impl std::error::Error for UnknownSharedVersion {}

impl SharedObjects {
    /// Start with the framework singletons already known, since their answer is not network-specific.
    pub fn new() -> Self {
        let mut versions = BTreeMap::new();
        for id in ["0x5", "0x6"] {
            let addr: Address = id.parse().expect("framework ids are valid addresses");
            versions.insert(addr, GENESIS_SHARED_VERSION);
        }
        Self { versions }
    }

    /// Record a version read from the chain.
    pub fn insert(&mut self, object_id: Address, initial_shared_version: u64) -> &mut Self {
        self.versions.insert(object_id, initial_shared_version);
        self
    }

    pub fn get(&self, object_id: Address) -> Result<u64, UnknownSharedVersion> {
        self.versions
            .get(&object_id)
            .copied()
            .ok_or(UnknownSharedVersion { object_id })
    }

    /// Build the input for a shared object, refusing rather than defaulting when unknown.
    pub fn input(
        &self,
        object_id: Address,
        mutable: bool,
    ) -> Result<ObjectInput, UnknownSharedVersion> {
        Ok(ObjectInput::shared(
            object_id,
            self.get(object_id)?,
            mutable,
        ))
    }

    /// Every id whose version is still missing, so a caller can fetch them in one pass.
    pub fn missing<'a>(&self, ids: impl IntoIterator<Item = &'a Address>) -> Vec<Address> {
        ids.into_iter()
            .filter(|id| !self.versions.contains_key(id))
            .copied()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> Address {
        s.parse().unwrap()
    }

    #[test]
    fn the_framework_singletons_are_known_without_a_lookup() {
        let shared = SharedObjects::new();
        assert_eq!(shared.get(addr("0x5")).unwrap(), 1);
        assert_eq!(shared.get(addr("0x6")).unwrap(), 1);
    }

    /// The whole point: an unknown version stops the build instead of becoming a zero.
    #[test]
    fn an_unresolved_object_is_refused_rather_than_defaulted() {
        let shared = SharedObjects::new();
        let pool = addr("0xb663828d6217467c8a1838a03793da896cbe745b150ebd57d82f814ca579fc22");
        assert_eq!(
            shared.get(pool),
            Err(UnknownSharedVersion { object_id: pool })
        );
        assert!(shared.input(pool, true).is_err());
    }

    #[test]
    fn the_refusal_names_the_object_and_says_it_must_be_read_from_chain() {
        let pool = addr("0x20");
        let message = UnknownSharedVersion { object_id: pool }.to_string();
        assert!(
            message.contains(&pool.to_string()),
            "a refusal that does not name the object leaves nothing to look up: {message}"
        );
        assert!(
            message.contains("read from the chain"),
            "the refusal must say where the answer comes from: {message}"
        );
    }

    #[test]
    fn a_resolved_version_is_used_verbatim() {
        let mut shared = SharedObjects::new();
        let pool = addr("0x20");
        shared.insert(pool, 419_123);
        assert_eq!(shared.get(pool).unwrap(), 419_123);
        assert!(shared.input(pool, true).is_ok());
    }

    #[test]
    fn missing_lists_only_what_is_unresolved() {
        let mut shared = SharedObjects::new();
        let known = addr("0x20");
        let unknown = addr("0x21");
        shared.insert(known, 7);
        assert_eq!(
            shared.missing([&addr("0x6"), &known, &unknown]),
            vec![unknown]
        );
    }
}
