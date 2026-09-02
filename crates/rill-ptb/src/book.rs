//! Reading DeepBook's order book.
//!
//! `pool::mid_price` is a Move function with a return value, so it is read by simulating a
//! transaction that calls it and taking the value back out — no key, no submission, nothing on
//! chain changes. The same keyless simulation the build path already depends on.
//!
//! # The price comes back as an integer and stays one
//!
//! The TypeScript SDK does this at the end of its read:
//!
//! ```text
//! Number(bcs.U64.parse(bytes)) * baseScalar / quoteScalar / FLOAT_SCALAR
//! ```
//!
//! So the price you read off the book has already been through a double before you use it — and
//! the usual next step is to feed it straight back in as an order price, where it goes through a
//! second one. Two roundings on a number that decides what an order costs.
//!
//! Here the u64 the chain returns is kept as a u64. Converting it to something human-readable is a
//! display concern, and display is the only place it belongs.

use rill_core::amounts::AmountError;
use sui_sdk_types::{Address, Identifier};
use sui_transaction_builder::{Function, TransactionBuilder};

use crate::shared::{SharedObjects, UnknownSharedVersion};

use crate::deepbook::{PoolSpec, FLOAT_SCALAR};

/// A mid price exactly as the chain reports it.
///
/// The raw value is scaled by `FLOAT_SCALAR * quote_scalar / base_scalar`, which is the same
/// convention an order price uses — so this can be handed back to an order builder without any
/// conversion at all, which is the whole point of not converting it here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MidPrice {
    /// What `pool::mid_price` returned, untouched.
    pub raw: u64,
    pub base_scalar: u128,
    pub quote_scalar: u128,
}

impl MidPrice {
    /// Render for a human, and only for a human.
    ///
    /// Integer division and remainder rather than a float: this is the one place a decimal point
    /// appears, and it appears in a string that nothing reads back.
    pub fn to_decimal_string(&self) -> Result<String, AmountError> {
        // raw = price * FLOAT_SCALAR * quote / base, so price = raw * base / (FLOAT_SCALAR * quote)
        let numerator = (self.raw as u128).saturating_mul(self.base_scalar);
        let denominator = FLOAT_SCALAR.saturating_mul(self.quote_scalar);
        if denominator == 0 {
            return Ok("0".into());
        }
        let whole = numerator / denominator;
        let remainder = numerator % denominator;
        if remainder == 0 {
            return Ok(whole.to_string());
        }
        // Nine fractional digits, then trailing zeros trimmed — enough for any Sui coin.
        let scaled = remainder.saturating_mul(1_000_000_000) / denominator;
        let fraction = format!("{scaled:09}");
        let trimmed = fraction.trim_end_matches('0');
        Ok(if trimmed.is_empty() {
            whole.to_string()
        } else {
            format!("{whole}.{trimmed}")
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BookError {
    /// A shared object was referenced before its initial version was known.
    UnknownShared(UnknownSharedVersion),
    BadIdentifier(String),
    /// The simulation ran but returned nothing to read.
    NoReturnValue,
    /// The bytes were not the u64 the function is declared to return.
    UnreadableValue,
}

impl std::fmt::Display for BookError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownShared(e) => write!(f, "{e}"),
            Self::BadIdentifier(s) => write!(f, "\"{s}\" is not a valid Move identifier or type"),
            Self::NoReturnValue => write!(
                f,
                "the mid-price simulation returned no value; the pool may not be registered on \
                 this network"
            ),
            Self::UnreadableValue => write!(
                f,
                "the mid-price call returned something that is not a u64; refusing to guess at it"
            ),
        }
    }
}

impl std::error::Error for BookError {}

impl From<UnknownSharedVersion> for BookError {
    fn from(e: UnknownSharedVersion) -> Self {
        Self::UnknownShared(e)
    }
}

fn ident(s: &str) -> Result<Identifier, BookError> {
    Identifier::new(s).map_err(|_| BookError::BadIdentifier(s.to_owned()))
}

/// Stands in for the gas coin a read does not have. Never resolved; see the note where it is used.
pub const PLACEHOLDER_GAS_OBJECT: &str =
    "0x0000000000000000000000000000000000000000000000000000000000000001";

/// Build the transaction whose simulation returns a pool's mid price.
///
/// Nothing here needs a sender with funds — it is never submitted. A zero sender is used so the
/// call cannot be mistaken for something meant to execute.
pub fn mid_price_transaction(
    deepbook_package: Address,
    pool: &PoolSpec,
    clock_id: Address,
    // Initial shared versions read from the chain; a missing one refuses the build.
    shared: &SharedObjects,
) -> Result<sui_sdk_types::Transaction, BookError> {
    let mut tx = TransactionBuilder::new();
    tx.set_sender(Address::ZERO);
    tx.set_gas_budget(10_000_000);
    // A literal is correct here and only here: this transaction is never submitted, and its gas
    // payment is emptied below so the node prices it itself. Reading the reference price for a
    // read would be a round trip that changes nothing.
    tx.set_gas_price(1_000);

    let pool_object = tx.object(shared.input(pool.pool_id, false)?);
    let clock = tx.object(shared.input(clock_id, false)?);

    let base: sui_sdk_types::TypeTag = pool
        .base_coin_type
        .parse()
        .map_err(|_| BookError::BadIdentifier(pool.base_coin_type.clone()))?;
    let quote: sui_sdk_types::TypeTag = pool
        .quote_coin_type
        .parse()
        .map_err(|_| BookError::BadIdentifier(pool.quote_coin_type.clone()))?;

    tx.move_call(
        Function::new(deepbook_package, ident("pool")?, ident("mid_price")?)
            .with_type_args(vec![base, quote]),
        vec![pool_object, clock],
    );

    // A read has no payer, but the builder will not produce a transaction without a gas object. So
    // one is supplied to satisfy the builder and then removed: an empty gas payment is what asks
    // the node to select gas itself, and it is the only shape a public fullnode accepts for a
    // transaction whose sender owns nothing.
    //
    // Naming a real object here instead would be worse than pointless — the node looks it up, finds
    // it at a different version, and refuses with a message about rebuilding the transaction.
    tx.add_gas_objects([sui_transaction_builder::ObjectInput::owned(
        PLACEHOLDER_GAS_OBJECT
            .parse()
            .expect("the placeholder is a valid address"),
        1,
        sui_sdk_types::Digest::ZERO,
    )]);

    let mut built = tx
        .try_build()
        .map_err(|_| BookError::BadIdentifier("mid_price transaction".into()))?;
    built.gas_payment.objects.clear();
    Ok(built)
}

/// Read a u64 out of a command's BCS return value.
///
/// BCS encodes a u64 as eight little-endian bytes and nothing else, so anything of a different
/// length is a different type — and reading it anyway would produce a plausible number from the
/// wrong bytes, which is worse than refusing.
pub fn parse_u64_return(bytes: &[u8]) -> Result<u64, BookError> {
    let eight: [u8; 8] = bytes.try_into().map_err(|_| BookError::UnreadableValue)?;
    Ok(u64::from_le_bytes(eight))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(base_scalar: u128, quote_scalar: u128) -> PoolSpec {
        PoolSpec {
            pool_id: Address::ZERO,
            base_coin_type: "0x2::sui::SUI".into(),
            quote_coin_type: "0x2::sui::SUI".into(),
            base_scalar,
            quote_scalar,
        }
    }

    #[test]
    fn a_mid_price_renders_without_a_float() {
        // 2.5 on a base 1e9 / quote 1e6 pool: raw = 2.5 * 1e9 * 1e6 / 1e9 = 2_500_000
        let price = MidPrice {
            raw: 2_500_000,
            base_scalar: 1_000_000_000,
            quote_scalar: 1_000_000,
        };
        assert_eq!(price.to_decimal_string().unwrap(), "2.5");
    }

    #[test]
    fn a_whole_number_price_has_no_decimal_point() {
        let price = MidPrice {
            raw: 3_000_000,
            base_scalar: 1_000_000_000,
            quote_scalar: 1_000_000,
        };
        assert_eq!(price.to_decimal_string().unwrap(), "3");
    }

    /// The pool shape where the reference's arithmetic goes wrong, read back exactly.
    #[test]
    fn the_deep_sui_shape_renders_exactly() {
        // 2362.123456 on base 1e6 / quote 1e9 → raw = 2362123456000000
        let price = MidPrice {
            raw: 2_362_123_456_000_000,
            base_scalar: 1_000_000,
            quote_scalar: 1_000_000_000,
        };
        assert_eq!(
            price.to_decimal_string().unwrap(),
            "2362.123456",
            "the value read off the book must survive the round trip intact"
        );
    }

    /// And the rendered string feeds straight back into an order at the same exact value.
    #[test]
    fn a_price_read_from_the_book_round_trips_into_an_order() {
        use rill_core::amounts::deepbook_price_to_base_units;
        let price = MidPrice {
            raw: 2_362_123_456_000_000,
            base_scalar: 1_000_000,
            quote_scalar: 1_000_000_000,
        };
        let rendered = price.to_decimal_string().unwrap();
        let back = deepbook_price_to_base_units(
            &rendered,
            FLOAT_SCALAR,
            price.quote_scalar,
            price.base_scalar,
        )
        .expect("the rendered price must be an exact order price");
        assert_eq!(
            back, price.raw,
            "read a price, place an order at it, and it must be the same number"
        );
    }

    #[test]
    fn a_zero_price_is_zero_not_an_error() {
        let price = MidPrice {
            raw: 0,
            base_scalar: 1_000_000_000,
            quote_scalar: 1_000_000,
        };
        assert_eq!(price.to_decimal_string().unwrap(), "0");
    }

    #[test]
    fn a_u64_return_value_is_read_little_endian() {
        assert_eq!(
            parse_u64_return(&2_500_000u64.to_le_bytes()).unwrap(),
            2_500_000
        );
    }

    /// Reading the wrong number of bytes anyway would produce a plausible number from the wrong
    /// value, which is worse than refusing.
    #[test]
    fn a_return_value_of_the_wrong_size_is_refused() {
        assert!(matches!(
            parse_u64_return(&[1, 2, 3]),
            Err(BookError::UnreadableValue)
        ));
        assert!(matches!(
            parse_u64_return(&[0u8; 16]),
            Err(BookError::UnreadableValue)
        ));
    }

    #[test]
    fn the_mid_price_transaction_builds() {
        let pkg: Address = "0x000000000000000000000000000000000000000000000000000000000000dee9"
            .parse()
            .unwrap();
        let clock: Address = "0x6".parse().unwrap();
        let mut pool = spec(1_000_000_000, 1_000_000);
        pool.pool_id = "0x0000000000000000000000000000000000000000000000000000000000000020"
            .parse()
            .unwrap();
        let mut shared = SharedObjects::new();
        shared.insert(pool.pool_id, 419_123);
        assert!(mid_price_transaction(pkg, &pool, clock, &shared).is_ok());
    }

    /// The bug this module was written against: a pool entered at version zero is not a pool the
    /// node can find, and it must be refused here rather than discovered as a missing object.
    #[test]
    fn a_pool_with_no_resolved_shared_version_is_refused() {
        let pkg: Address = "0x000000000000000000000000000000000000000000000000000000000000dee9"
            .parse()
            .unwrap();
        let clock: Address = "0x6".parse().unwrap();
        let mut pool = spec(1_000_000_000, 1_000_000);
        pool.pool_id = "0x0000000000000000000000000000000000000000000000000000000000000020"
            .parse()
            .unwrap();
        let shared = SharedObjects::new();
        assert!(matches!(
            mid_price_transaction(pkg, &pool, clock, &shared),
            Err(BookError::UnknownShared(_))
        ));
    }
}
