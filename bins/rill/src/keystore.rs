//! The key, and everything that keeps it from leaking.
//!
//! # It never becomes a String you can print
//!
//! The private key is read once from the environment, parsed, and the parsed keypair is what is
//! kept. There is no accessor that returns the secret, no `Debug` that could render it into a log
//! line, and no `Display`. The only thing this module hands out is a signature and a public
//! address.
//!
//! That is a deliberately narrower surface than the reference's, which held the raw secret on a
//! config object and deleted it after derivation. Deleting a field is a runtime step that a new
//! code path can skip; not having the field is not.
//!
//! # Where it comes from
//!
//! Two sources, in this order:
//!
//! 1. `RILL_SUI_PRIVATE_KEY` in the environment of the process that launched the signer.
//! 2. The `sui` CLI's own keystore at `~/.sui/sui_config/sui.keystore`, if the machine has one.
//!
//! The second exists because a developer who has used Sui already has a funded key, and asking them
//! to export it into an environment variable is asking them to put a secret somewhere new — into a
//! shell history, a `.env`, a screenshot. Reading the file they already have adds no copy.
//!
//! The environment still wins, so a deliberate override is never silently ignored in favour of
//! whatever key happens to be on the machine.
//!
//! Never an MCP tool argument — `rill-mcp`'s keyless guard refuses those by name — never a config
//! file the agent can read, and never a command-line argument, which is visible in `ps` to every
//! user on the machine.
//!
//! # Which key, when there is more than one
//!
//! Owner and agent are different keys by design: `add_rule` asserts the owner, `request_spend`
//! asserts the agent, and the delegation is only proved when the two are not the same address. So
//! a machine that runs both halves holds two keys, and `--as <address>` (or `RILL_SIGN_AS`, which
//! is the same thing for a launch config that cannot pass a flag) says which one signs.
//!
//! When neither is given and the keystore holds more than one key, this refuses rather than taking
//! the first entry. "Whichever key comes first" is not a choice anybody made: `wallet create` mints
//! a wallet's ownership from the key that signs it, and a key picked by file order can be the agent
//! as easily as the owner. The refusal lists the candidates, because the fix is to name one.
//! Exactly one key needs no naming and loads as it always has.

use sui_crypto::simple::SimpleKeypair;
use sui_crypto::SuiSigner;
use sui_sdk_types::{Address, Transaction, UserSignature};

/// The environment variable the launching shell or secret manager sets.
pub const PRIVATE_KEY_VAR: &str = "RILL_SUI_PRIVATE_KEY";

/// The address to sign as, for a launch that cannot pass `--as`. Holds an address, never a key.
pub const SIGN_AS_VAR: &str = "RILL_SIGN_AS";

/// The `sui` CLI's keystore, relative to the home directory.
pub const SUI_KEYSTORE_PATH: &str = ".sui/sui_config/sui.keystore";

/// Where the loaded key came from, so `rill status` can say it without saying anything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    Environment,
    SuiKeystore,
}

impl std::fmt::Display for KeySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Environment => write!(f, "{PRIVATE_KEY_VAR}"),
            Self::SuiKeystore => write!(f, "~/{SUI_KEYSTORE_PATH}"),
        }
    }
}

pub struct Keystore {
    keypair: SimpleKeypair,
    address: Address,
    source: KeySource,
}

/// Deliberately opaque. A `Debug` that printed the key is exactly the accident this prevents, and
/// a signer's struct ends up in an error message sooner or later.
impl std::fmt::Debug for Keystore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keystore")
            .field("address", &self.address)
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeystoreError {
    /// No key was provided. Distinct from a malformed one, because the fixes differ.
    NotConfigured,
    /// The value was present but not a key. The value itself is **never** echoed.
    Malformed,
    /// A specific address was asked for and the keystore does not hold its key.
    NoSuchAddress(Address),
    /// The keystore holds more than one key and nothing said which should sign. Carries every
    /// candidate's address, because naming one is the fix.
    Ambiguous(Vec<Address>),
    /// `--as` or `RILL_SIGN_AS` was given something that is not an address. The value is echoed:
    /// it is a public address attempt, not a secret, and seeing it is how a typo gets found.
    NotAnAddress { via: String, value: String },
}

impl std::fmt::Display for KeystoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ambiguous(candidates) => {
                write!(
                    f,
                    "~/{SUI_KEYSTORE_PATH} holds {} keys and none was named, so there is no way \
                     to know which one should sign. Signing with whichever comes first would \
                     mint a wallet's ownership, or a spend, from a key nobody chose. Pass \
                     `--as <address>` or set {SIGN_AS_VAR}=<address>. The keys there:",
                    candidates.len()
                )?;
                for address in candidates {
                    write!(f, "\n  {address}")?;
                }
                Ok(())
            }
            Self::NotAnAddress { via, value } => write!(
                f,
                "{via}{value} is not a Sui address. Both --as and {SIGN_AS_VAR} take the 0x \
                 address of a key in ~/{SUI_KEYSTORE_PATH}; `sui client addresses` lists them."
            ),
            Self::NotConfigured => write!(
                f,
                "no signing key is configured. Set {PRIVATE_KEY_VAR} in the shell or secret \
                 manager that launches this process, or run `sui client new-address ed25519` so \
                 there is a key at ~/{SUI_KEYSTORE_PATH}. Never put it in an MCP config file, a \
                 command-line argument, or anything the agent can read."
            ),
            // The malformed value is not included on purpose: an error message is the most likely
            // place for a secret to end up somewhere it should not be.
            Self::NoSuchAddress(address) => write!(
                f,
                "no key for {address} in ~/{SUI_KEYSTORE_PATH}. Run `sui client addresses` to see \
                 which ones are there, or `sui client new-address ed25519` to add one."
            ),
            Self::Malformed => write!(
                f,
                "{PRIVATE_KEY_VAR} is set but could not be parsed as a Sui private key. Expected a \
                 `suiprivkey1...` bech32 string. The value is not echoed here."
            ),
        }
    }
}

impl std::error::Error for KeystoreError {}

impl Keystore {
    /// Parse a `suiprivkey1...` string.
    ///
    /// Takes the secret by reference and keeps none of it — only the derived keypair survives the
    /// call.
    pub fn from_suiprivkey(value: &str) -> Result<Self, KeystoreError> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(KeystoreError::NotConfigured);
        }
        let keypair =
            SimpleKeypair::from_suiprivkey(trimmed).map_err(|_| KeystoreError::Malformed)?;
        let address = keypair.verifying_key().derive_address();
        Ok(Self {
            keypair,
            address,
            source: KeySource::Environment,
        })
    }

    /// Every key the `sui` CLI's keystore holds, decoded.
    ///
    /// The file is a JSON array of base64 `flag || secret` strings, which `sui-crypto` already
    /// parses: `from_base64` is documented as being for exactly this file. Decoding it by hand
    /// would mean re-deciding which schemes are valid, and getting that wrong produces an address
    /// that is not the user's, which is worse than refusing: funds sent to it are gone.
    ///
    /// An entry that does not decode is skipped rather than aborting the read, since one unreadable
    /// key should not hide the others. A file with entries and no readable key is `Malformed`; an
    /// empty or missing file is `NotConfigured`, because the fixes differ.
    fn keys_in_sui_keystore(path: &std::path::Path) -> Result<Vec<SimpleKeypair>, KeystoreError> {
        let contents = std::fs::read_to_string(path).map_err(|_| KeystoreError::NotConfigured)?;
        let entries: Vec<String> =
            serde_json::from_str(&contents).map_err(|_| KeystoreError::Malformed)?;
        if entries.is_empty() {
            return Err(KeystoreError::NotConfigured);
        }
        let keys: Vec<SimpleKeypair> = entries
            .iter()
            .filter_map(|e| SimpleKeypair::from_base64(e.trim()).ok())
            .collect();
        if keys.is_empty() {
            return Err(KeystoreError::Malformed);
        }
        Ok(keys)
    }

    /// Read the one key in the `sui` CLI's keystore.
    ///
    /// One, not the first. With two or more keys and nothing to say which, this refuses and names
    /// them all: see the module note on why taking the first entry is a choice nobody made. The
    /// caller that has an address to name goes through [`Keystore::for_address`] instead.
    pub fn from_sui_keystore(path: &std::path::Path) -> Result<Self, KeystoreError> {
        let mut keys = Self::keys_in_sui_keystore(path)?;
        if keys.len() > 1 {
            return Err(KeystoreError::Ambiguous(
                keys.iter()
                    .map(|k| k.verifying_key().derive_address())
                    .collect(),
            ));
        }
        let keypair = keys.pop().expect("exactly one key");
        let address = keypair.verifying_key().derive_address();
        Ok(Self {
            keypair,
            address,
            source: KeySource::SuiKeystore,
        })
    }

    /// Every address the `sui` keystore holds, without exposing any key.
    ///
    /// Derived rather than read from `sui.aliases`, because the aliases file is a label a human
    /// edits and the address is a fact about the key. A label that has drifted from its key would
    /// select the wrong signer, and selecting the wrong signer is how funds move from the wrong
    /// wallet.
    pub fn addresses_in_sui_keystore(
        path: &std::path::Path,
    ) -> Result<Vec<Address>, KeystoreError> {
        Ok(Self::keys_in_sui_keystore(path)?
            .iter()
            .map(|k| k.verifying_key().derive_address())
            .collect())
    }

    /// Select the key for one specific address.
    ///
    /// Each entry is decoded and its address derived, then compared. There is no index to trust and
    /// no name to trust: the address a key produces is the only thing that identifies it.
    pub fn for_address(path: &std::path::Path, wanted: Address) -> Result<Self, KeystoreError> {
        for keypair in Self::keys_in_sui_keystore(path)? {
            let address = keypair.verifying_key().derive_address();
            if address == wanted {
                return Ok(Self {
                    keypair,
                    address,
                    source: KeySource::SuiKeystore,
                });
            }
        }
        Err(KeystoreError::NoSuchAddress(wanted))
    }

    /// The `sui` CLI's keystore in this user's home directory, if there is one.
    pub fn from_default_sui_keystore() -> Result<Self, KeystoreError> {
        let home = std::env::var("HOME").map_err(|_| KeystoreError::NotConfigured)?;
        Self::from_sui_keystore(&std::path::Path::new(&home).join(SUI_KEYSTORE_PATH))
    }

    /// Which source this key came from.
    pub fn source(&self) -> KeySource {
        self.source
    }

    /// Read from the process environment.
    ///
    /// The variable is removed from this process's environment immediately after reading, so a
    /// later `std::env::vars()` — in a crash reporter, a diagnostic dump, a child process — cannot
    /// pick it up. It cannot be un-leaked from a parent's environment, but it stops here.
    pub fn from_env() -> Result<Self, KeystoreError> {
        let raw = std::env::var(PRIVATE_KEY_VAR).map_err(|_| KeystoreError::NotConfigured)?;
        let result = Self::from_suiprivkey(&raw);
        // SAFETY-equivalent note: single-threaded startup, before any task is spawned.
        unsafe { std::env::remove_var(PRIVATE_KEY_VAR) };
        result
    }

    /// The environment first, then the `sui` CLI's keystore, when nothing named an address.
    ///
    /// A malformed environment variable is an error rather than a reason to fall through: someone
    /// who set it meant to use that key, and quietly signing with a different one is the worst
    /// possible recovery. A keystore with more than one key is an error for the same reason, and
    /// the error names them: see [`Keystore::from_sui_keystore`].
    pub fn load() -> Result<Self, KeystoreError> {
        match Self::from_env() {
            Ok(store) => Ok(store),
            Err(KeystoreError::NotConfigured) => Self::from_default_sui_keystore(),
            Err(other) => Err(other),
        }
    }

    /// Load the key for one address, environment first.
    ///
    /// The environment variable wins only when it holds the key that was asked for. Signing as
    /// whoever the environment happens to name, when a specific address was requested, is how an
    /// owner-only call ends up signed by the agent — which aborts, and the abort names the wrong
    /// thing.
    pub fn load_for(wanted: Address) -> Result<Self, KeystoreError> {
        if let Ok(store) = Self::from_env() {
            if store.address() == wanted {
                return Ok(store);
            }
        }
        let home = std::env::var("HOME").map_err(|_| KeystoreError::NotConfigured)?;
        Self::for_address(&std::path::Path::new(&home).join(SUI_KEYSTORE_PATH), wanted)
    }

    /// The public address this key controls. Safe to log, and the only identity anything else
    /// needs.
    pub fn address(&self) -> Address {
        self.address
    }

    /// Sign a transaction.
    ///
    /// Takes a fully-formed [`Transaction`] rather than bytes. Signing arbitrary bytes is how a
    /// "sign this login message" flow becomes a transaction signature — the intent prefix is what
    /// separates them, and letting a caller supply raw bytes would put that separation in the
    /// caller's hands.
    pub fn sign(&self, transaction: &Transaction) -> Result<UserSignature, KeystoreError> {
        self.keypair
            .sign_transaction(transaction)
            .map_err(|_| KeystoreError::Malformed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway key built from fixed bytes rather than a committed `suiprivkey1...` string.
    ///
    /// Encoding it here instead of pasting one means there is no key-shaped literal in the
    /// repository for anyone — or any scanner — to mistake for a real one, and none that could
    /// ever be funded by accident.
    fn generated_suiprivkey(seed: u8) -> String {
        use sui_crypto::ed25519::Ed25519PrivateKey;
        Ed25519PrivateKey::new([seed; 32])
            .to_suiprivkey()
            .expect("encode")
    }

    #[test]
    fn a_generated_key_round_trips_and_yields_an_address() {
        let encoded = generated_suiprivkey(7);
        let store = Keystore::from_suiprivkey(&encoded).expect("parse");
        let address = store.address();
        assert_eq!(
            format!("{address}").len(),
            66,
            "a Sui address is 0x plus 64 hex characters"
        );
    }

    #[test]
    fn the_same_key_always_derives_the_same_address() {
        let encoded = generated_suiprivkey(7);
        let a = Keystore::from_suiprivkey(&encoded).unwrap().address();
        let b = Keystore::from_suiprivkey(&encoded).unwrap().address();
        assert_eq!(a, b);
    }

    #[test]
    fn an_empty_value_reads_as_not_configured_not_as_malformed() {
        assert!(
            matches!(
                Keystore::from_suiprivkey("   "),
                Err(KeystoreError::NotConfigured)
            ),
            "the two have different fixes, so they are different errors"
        );
    }

    #[test]
    fn a_value_that_is_not_a_key_is_refused() {
        for bad in ["not-a-key", "suiprivkey1garbage", "0xdeadbeef"] {
            assert!(
                matches!(
                    Keystore::from_suiprivkey(bad),
                    Err(KeystoreError::Malformed)
                ),
                "{bad} should be refused"
            );
        }
    }

    /// The most likely place for a secret to end up somewhere it should not be is an error string.
    #[test]
    fn no_error_message_ever_contains_the_offending_value() {
        let secret = generated_suiprivkey(9);
        // Corrupt it so it fails to parse while still looking like a key.
        let broken = format!("{}xx", &secret[..secret.len() - 2]);
        let message = Keystore::from_suiprivkey(&broken).unwrap_err().to_string();
        assert!(
            !message.contains(&broken) && !message.contains(&secret[10..30]),
            "the rejected value must not be echoed: {message}"
        );
    }

    /// A Debug that printed the key is precisely the accident this guards against — and a signer's
    /// struct reaches a log line sooner or later.
    #[test]
    fn debug_output_shows_the_address_and_nothing_else() {
        let encoded = generated_suiprivkey(7);
        let store = Keystore::from_suiprivkey(&encoded).unwrap();
        let rendered = format!("{store:?}");
        assert!(rendered.contains("address"));
        assert!(
            !rendered.contains("suiprivkey"),
            "the key must not be renderable: {rendered}"
        );
        assert!(!rendered.contains(&encoded[10..30]));
    }
}

/// Selecting among keys in a `sui.keystore` file.
///
/// Every file here is a throwaway written to the temp directory from generated keys, and removed
/// when the test ends. The real keystore is never read: a test that read it would pass or fail on
/// what happens to be on the machine, and a test that wrote it would be a disaster.
#[cfg(test)]
mod selection_tests {
    use super::*;
    use sui_crypto::ed25519::Ed25519PrivateKey;

    /// A keystore entry the way the `sui` CLI writes one: base64 `flag || secret`.
    fn entry(seed: u8) -> String {
        Ed25519PrivateKey::new([seed; 32]).to_base64()
    }

    fn address_of(seed: u8) -> Address {
        Ed25519PrivateKey::new([seed; 32])
            .public_key()
            .derive_address()
    }

    /// A temp keystore file, deleted on drop so a failing assertion does not leave key-shaped
    /// material lying in the temp directory.
    ///
    /// Named by a counter, not by its contents: tests run in parallel in one process, and two
    /// tests holding the same seeds would otherwise share a path and delete it from under each
    /// other.
    struct TempKeystore(std::path::PathBuf);

    impl TempKeystore {
        fn holding(seeds: &[u8]) -> Self {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let name = format!(
                "rill-keystore-test-{}-{}.json",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            );
            let path = std::env::temp_dir().join(name);
            let entries: Vec<String> = seeds.iter().copied().map(entry).collect();
            std::fs::write(&path, serde_json::to_string(&entries).unwrap()).unwrap();
            Self(path)
        }
    }

    impl Drop for TempKeystore {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn one_key_and_no_address_named_loads_as_before() {
        let file = TempKeystore::holding(&[11]);
        let store = Keystore::from_sui_keystore(&file.0).expect("one key is not ambiguous");
        assert_eq!(store.address(), address_of(11));
        assert_eq!(store.source(), KeySource::SuiKeystore);
    }

    /// The pattern the design disowns: with two keys and nothing to choose between them, taking
    /// the first would sign as whichever key happened to be written first.
    #[test]
    fn two_keys_and_no_address_named_is_refused_naming_both() {
        let file = TempKeystore::holding(&[11, 12]);
        match Keystore::from_sui_keystore(&file.0) {
            Err(KeystoreError::Ambiguous(candidates)) => {
                assert_eq!(candidates, vec![address_of(11), address_of(12)]);
            }
            other => panic!("two keys must be refused, not chosen from: {other:?}"),
        }
    }

    #[test]
    fn the_refusal_says_both_ways_to_choose_and_lists_every_candidate() {
        let file = TempKeystore::holding(&[11, 12, 13]);
        let message = Keystore::from_sui_keystore(&file.0)
            .unwrap_err()
            .to_string();
        assert!(message.contains("--as <address>"), "{message}");
        assert!(message.contains("RILL_SIGN_AS=<address>"), "{message}");
        assert!(message.contains("3 keys"), "{message}");
        for seed in [11, 12, 13] {
            assert!(
                message.contains(&address_of(seed).to_string()),
                "every candidate must be named, so the fix is a copy-paste: {message}"
            );
        }
    }

    /// The refusal lists addresses. It must not list what derived them.
    #[test]
    fn the_refusal_carries_no_key_material() {
        let file = TempKeystore::holding(&[11, 12]);
        let message = Keystore::from_sui_keystore(&file.0)
            .unwrap_err()
            .to_string();
        for seed in [11, 12] {
            let secret = entry(seed);
            assert!(!message.contains(&secret[..16]), "{message}");
        }
    }

    /// Selection is by address, not by position: asking for the second key gets the second key.
    #[test]
    fn naming_an_address_selects_it_from_a_two_key_keystore() {
        let file = TempKeystore::holding(&[11, 12]);
        let store = Keystore::for_address(&file.0, address_of(12)).expect("the second key");
        assert_eq!(store.address(), address_of(12));
        assert_ne!(
            store.address(),
            address_of(11),
            "the first entry must not win by being first"
        );
    }

    #[test]
    fn naming_an_address_that_is_not_there_is_refused_by_name() {
        let file = TempKeystore::holding(&[11, 12]);
        let wanted = address_of(99);
        assert_eq!(
            Keystore::for_address(&file.0, wanted).unwrap_err(),
            KeystoreError::NoSuchAddress(wanted)
        );
    }

    #[test]
    fn every_address_is_listed_without_a_key() {
        let file = TempKeystore::holding(&[11, 12]);
        assert_eq!(
            Keystore::addresses_in_sui_keystore(&file.0).unwrap(),
            vec![address_of(11), address_of(12)]
        );
    }
}

#[cfg(test)]
mod signing_tests {
    use super::*;
    use sui_sdk_types::Digest;
    use sui_transaction_builder::{ObjectInput, TransactionBuilder};

    fn key() -> Keystore {
        use sui_crypto::ed25519::Ed25519PrivateKey;
        let encoded = Ed25519PrivateKey::new([3u8; 32]).to_suiprivkey().unwrap();
        Keystore::from_suiprivkey(&encoded).unwrap()
    }

    fn a_transaction(store: &Keystore) -> Transaction {
        let mut tx = TransactionBuilder::new();
        tx.set_sender(store.address());
        tx.set_gas_budget(10_000_000);
        tx.set_gas_price(1_000);
        tx.add_gas_objects([ObjectInput::owned(store.address(), 1, Digest::ZERO)]);
        let amount = tx.pure(&1_000u64);
        let gas = tx.gas();
        let split = tx.split_coins(gas, vec![amount]);
        let recipient = tx.pure(&store.address());
        tx.transfer_objects(split, recipient);
        tx.try_build().expect("build")
    }

    #[test]
    fn a_transaction_can_actually_be_signed() {
        let store = key();
        let transaction = a_transaction(&store);
        assert!(
            store.sign(&transaction).is_ok(),
            "the key must produce a usable signature, not merely parse"
        );
    }

    /// Two different transactions must not produce the same signature — the obvious property, and
    /// the one that would silently break if the wrong bytes were ever signed.
    #[test]
    fn signing_covers_the_transaction_rather_than_something_fixed() {
        let store = key();
        let first = store.sign(&a_transaction(&store)).unwrap();

        let mut tx = TransactionBuilder::new();
        tx.set_sender(store.address());
        tx.set_gas_budget(20_000_000); // different budget, therefore different bytes
        tx.set_gas_price(1_000);
        tx.add_gas_objects([ObjectInput::owned(store.address(), 1, Digest::ZERO)]);
        let amount = tx.pure(&2_000u64);
        let gas = tx.gas();
        let split = tx.split_coins(gas, vec![amount]);
        let recipient = tx.pure(&store.address());
        tx.transfer_objects(split, recipient);
        let second = store.sign(&tx.try_build().unwrap()).unwrap();

        assert_ne!(
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&second).unwrap(),
            "a signature that does not change with the transaction is not signing the transaction"
        );
    }
}
