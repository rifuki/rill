//! The native Sui Bridge: a coin leaves this chain.
//!
//! `0xb::bridge::send_token<T>(bridge, target_chain, target_address, coin, ctx)`, read from the
//! deployed package rather than written from a document. It takes the whole coin and returns
//! nothing, so a gated spend releases exactly the amount to be sent and this consumes it: no
//! residual, no output to place.
//!
//! # What this does not protect, and says so
//!
//! A swap has an output coin, so a slippage floor can assert something about what came back. A bridge
//! has no output on this chain at all. The wallet's `budget` and `per_tx` rules bound how much leaves,
//! and after `send_token` there is nothing on Sui left to check: the funds are in the bridge's
//! custody until a committee signs them out on the far chain. That is a real difference from every
//! other adapter here and it belongs in the open rather than in a reader's assumption.
//!
//! # The refusals are the point
//!
//! Three of them exist because the Move signature cannot express the constraint:
//!
//! - `target_address` is a `vector<u8>`, so a 32-byte Sui address is as acceptable to the compiler as
//!   a 20-byte Ethereum one. Sent to the wrong length, the funds arrive nowhere anybody holds.
//! - `target_chain` is a `u8`. Any number type-checks, including this chain's own id, and including
//!   a chain the bridge has never heard of.
//! - The bridge carries a fixed set of token types, registered in its treasury. Anything else aborts
//!   inside the bridge with a code. At the time of writing SUI is not among them, which means a
//!   SUI-funded agent wallet cannot bridge its own funds at all. See
//!   `rill-chain/tests/bridge_signature.rs`, which asserts that rather than assuming it.

use sui_sdk_types::{Address, Identifier};
use sui_transaction_builder::{Argument, Function, TransactionBuilder};

use crate::shared::{SharedObjects, UnknownSharedVersion};

/// The bridge system package. A framework package, so this is the same address on every network.
pub const BRIDGE_PACKAGE: &str =
    "0x000000000000000000000000000000000000000000000000000000000000000b";

/// The `Bridge` shared object.
pub const BRIDGE_OBJECT: &str =
    "0x0000000000000000000000000000000000000000000000000000000000000009";

/// The chain ids the bridge uses, as it numbers them.
///
/// Named rather than passed as a bare `u8` so a caller cannot transpose a source for a destination.
/// The Sui ids are listed because they are what must be **refused** as a destination: sending from
/// Sui to Sui is not a route, and a `u8` parameter accepts it.
pub mod chain {
    pub const SUI_MAINNET: u8 = 0;
    pub const SUI_TESTNET: u8 = 1;
    pub const SUI_DEVNET: u8 = 2;
    pub const SUI_LOCAL_TEST: u8 = 3;
    pub const ETH_MAINNET: u8 = 10;
    pub const ETH_SEPOLIA: u8 = 11;
    pub const ETH_LOCAL_TEST: u8 = 12;

    /// Whether an id is one of the Sui chains, which cannot be a destination from another Sui chain.
    pub fn is_sui(id: u8) -> bool {
        matches!(id, SUI_MAINNET | SUI_TESTNET | SUI_DEVNET | SUI_LOCAL_TEST)
    }

    /// Whether an id is one of the Ethereum chains, whose addresses are 20 bytes.
    pub fn is_ethereum(id: u8) -> bool {
        matches!(id, ETH_MAINNET | ETH_SEPOLIA | ETH_LOCAL_TEST)
    }
}

/// How many bytes an address on an Ethereum chain has.
pub const ETHEREUM_ADDRESS_BYTES: usize = 20;

/// One outbound transfer.
#[derive(Debug, Clone)]
pub struct Bridge {
    /// The chain the funds are going to, as the bridge numbers chains.
    pub target_chain: u8,
    /// The recipient on that chain, as raw bytes. Twenty of them for an Ethereum chain.
    pub target_address: Vec<u8>,
    /// The coin type being sent, which must be one the bridge carries.
    pub coin_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeError {
    UnknownShared(UnknownSharedVersion),
    BadIdentifier(String),
    /// The bridge does not carry this coin type. Carries the list it does, read from the chain, so the
    /// refusal can name the alternatives instead of saying "unsupported".
    TokenNotCarried {
        coin_type: String,
        carried: Vec<String>,
    },
    /// The destination is a Sui chain, which is not somewhere this bridge sends from Sui.
    DestinationIsSui {
        target_chain: u8,
    },
    /// The destination is a chain id the bridge does not use.
    UnknownDestination {
        target_chain: u8,
    },
    /// The recipient address is the wrong length for the destination chain.
    AddressWrongLength {
        target_chain: u8,
        expected: usize,
        got: usize,
    },
    /// An empty recipient, which would send the funds nowhere.
    NoTargetAddress,
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownShared(e) => write!(f, "{e}"),
            Self::BadIdentifier(s) => write!(f, "\"{s}\" is not a valid Move identifier"),
            Self::TokenNotCarried {
                coin_type,
                carried,
            } => write!(
                f,
                "the Sui Bridge does not carry {coin_type}, so this transfer would abort inside the \
                 bridge. It carries {}. Every token it carries is an asset that originates on \
                 Ethereum and is wrapped on Sui, which is why SUI itself is not among them: an agent \
                 wallet funded in SUI cannot bridge its own funds, and would need to hold one of \
                 those types instead.",
                if carried.is_empty() {
                    "nothing this read could see".to_string()
                } else {
                    carried.join(", ")
                }
            ),
            Self::DestinationIsSui { target_chain } => write!(
                f,
                "target chain {target_chain} is a Sui chain, and this builds a transfer out of Sui. \
                 Sending from Sui to Sui is not a bridge route; a plain transfer does that."
            ),
            Self::UnknownDestination { target_chain } => write!(
                f,
                "target chain {target_chain} is not a chain id the Sui Bridge uses. Ethereum is 10 \
                 for mainnet and 11 for Sepolia."
            ),
            Self::AddressWrongLength {
                target_chain,
                expected,
                got,
            } => write!(
                f,
                "a recipient on chain {target_chain} is {expected} bytes and this one is {got}. The \
                 Move parameter is a byte vector, so the wrong length is accepted by the contract and \
                 the funds arrive at no address anybody holds. A 32-byte value here is usually a Sui \
                 address pasted in by mistake."
            ),
            Self::NoTargetAddress => write!(
                f,
                "no recipient address was given, which would send the funds nowhere recoverable"
            ),
        }
    }
}

impl std::error::Error for BridgeError {}

impl From<UnknownSharedVersion> for BridgeError {
    fn from(e: UnknownSharedVersion) -> Self {
        Self::UnknownShared(e)
    }
}

fn ident(s: &str) -> Result<Identifier, BridgeError> {
    Identifier::new(s).map_err(|_| BridgeError::BadIdentifier(s.to_owned()))
}

/// A coin type in the form a `TypeTag` will parse.
///
/// The bridge's treasury stores types with no `0x` on the package address, so a caller that reads the
/// registry and hands a type straight back, which is the obvious thing to do and what the live test
/// did, has a string that `carries` accepts and `TypeTag::parse` refuses. The two disagreeing is the
/// bug this exists to remove: a token the bridge demonstrably carries was refused as a bad identifier.
pub fn normalise_coin_type(coin_type: &str) -> String {
    let trimmed = coin_type.trim();
    match trimmed.strip_prefix("0x") {
        Some(_) => trimmed.to_owned(),
        None => format!("0x{trimmed}"),
    }
}

/// Whether the bridge carries this coin type, given the registry read from its treasury.
///
/// Compared after normalising both sides, for the same reason: the registry's form and a caller's form
/// differ by a prefix, and a comparison that missed that would refuse every token the bridge actually
/// carries, which is the most expensive direction for this bug because it reads as "supports nothing".
pub fn carries(carried: &[String], coin_type: &str) -> bool {
    let needle = normalise_coin_type(coin_type).to_ascii_lowercase();
    carried
        .iter()
        .any(|c| normalise_coin_type(c).to_ascii_lowercase() == needle)
}

/// Emit `bridge::send_token`, consuming the coin.
///
/// `carried` is the bridge's own token registry, read from chain by the caller. Passed in rather than
/// hardcoded here: a list in this file would go stale the moment the bridge registers a token, and
/// the staleness would show up as a refusal of something that works.
pub fn send_token(
    tx: &mut TransactionBuilder,
    bridge: &Bridge,
    carried: &[String],
    coin: Argument,
    shared: &SharedObjects,
) -> Result<(), BridgeError> {
    if bridge.target_address.is_empty() {
        return Err(BridgeError::NoTargetAddress);
    }
    if chain::is_sui(bridge.target_chain) {
        return Err(BridgeError::DestinationIsSui {
            target_chain: bridge.target_chain,
        });
    }
    if !chain::is_ethereum(bridge.target_chain) {
        return Err(BridgeError::UnknownDestination {
            target_chain: bridge.target_chain,
        });
    }
    if bridge.target_address.len() != ETHEREUM_ADDRESS_BYTES {
        return Err(BridgeError::AddressWrongLength {
            target_chain: bridge.target_chain,
            expected: ETHEREUM_ADDRESS_BYTES,
            got: bridge.target_address.len(),
        });
    }
    if !carries(carried, &bridge.coin_type) {
        return Err(BridgeError::TokenNotCarried {
            coin_type: bridge.coin_type.clone(),
            carried: carried.to_vec(),
        });
    }

    let coin_type: sui_sdk_types::TypeTag = normalise_coin_type(&bridge.coin_type)
        .parse()
        .map_err(|_| BridgeError::BadIdentifier(bridge.coin_type.clone()))?;
    let package: Address = BRIDGE_PACKAGE
        .parse()
        .expect("the bridge package id is a valid address");
    let bridge_id: Address = BRIDGE_OBJECT
        .parse()
        .expect("the bridge object id is a valid address");

    // The Bridge is mutated by a send, so it is taken mutably at the version the chain reports.
    let bridge_object = tx.object(shared.input(bridge_id, true)?);
    let target_chain = tx.pure(&bridge.target_chain);
    let target_address = tx.pure(&bridge.target_address);

    tx.move_call(
        Function::new(package, ident("bridge")?, ident("send_token")?)
            .with_type_args(vec![coin_type]),
        vec![bridge_object, target_chain, target_address, coin],
    );
    Ok(())
}

/// The Move call a bridge emits, for the signer's pinned sequence.
pub fn expected_bridge_targets() -> Vec<String> {
    vec![format!("{BRIDGE_PACKAGE}::bridge::send_token")]
}
