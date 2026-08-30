//! The money path. Every conversion from a human decimal string into on-chain base units in
//! Rill goes through this module.
//!
//! **There is no constructor here that accepts an `f64`, and there must never be one.** The
//! reference TypeScript implementation stated the same invariant in a comment and still had a
//! float reach a DeepBook order price, because a comment cannot stop `Number(value)`. Here the
//! only way in is a string, and the only way out is an integer.
//!
//! One further difference from the reference, deliberate: where a conversion cannot be
//! represented exactly, this module **rejects** rather than rounding. Silently rounding an
//! order price to the nearest base unit changes the order the caller asked for, and a caller
//! who wanted rounding can round explicitly.

use std::fmt;

/// Sui's `u64` ceiling — the maximum any on-chain amount, budget, or millisecond timestamp
/// can hold.
pub const U64_MAX: u128 = u64::MAX as u128;

/// Why a conversion was refused. Every variant echoes the offending value, because these
/// surface at an API boundary where the caller needs to know which input to fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AmountError {
    Empty,
    NotDecimal(String),
    ScientificNotation(String),
    Negative(String),
    LeadingSign(String),
    MultipleDecimalPoints(String),
    /// More fractional digits than the token can represent. Truncating would misreport the
    /// amount, so it is refused instead.
    TooPrecise {
        value: String,
        digits: usize,
        allowed: u32,
    },
    ExceedsU64(String),
    /// The scaled result is not an integer — the conversion would have to round.
    Inexact {
        value: String,
        multiplier_num: u128,
        multiplier_den: u128,
    },
    Overflow(String),
}

impl fmt::Display for AmountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "value must be a non-empty string"),
            Self::NotDecimal(v) => write!(f, "\"{v}\" is not a valid decimal number"),
            Self::ScientificNotation(v) => {
                write!(f, "\"{v}\" uses scientific notation, which is not allowed")
            }
            Self::Negative(v) => write!(f, "\"{v}\" must not be negative"),
            Self::LeadingSign(v) => write!(f, "\"{v}\" must not have a leading sign"),
            Self::MultipleDecimalPoints(v) => {
                write!(f, "\"{v}\" has more than one decimal point")
            }
            Self::TooPrecise {
                value,
                digits,
                allowed,
            } => write!(
                f,
                "\"{value}\" has {digits} fractional digits, more than the {allowed} this token \
                 supports (precision would be lost)"
            ),
            Self::ExceedsU64(v) => write!(f, "\"{v}\" exceeds the u64 maximum ({U64_MAX})"),
            Self::Inexact {
                value,
                multiplier_num,
                multiplier_den,
            } => write!(
                f,
                "\"{value}\" cannot be scaled by {multiplier_num}/{multiplier_den} without \
                 rounding; refusing to silently change the amount"
            ),
            Self::Overflow(v) => write!(f, "\"{v}\" overflowed during scaling"),
        }
    }
}

impl std::error::Error for AmountError {}

/// A decimal value parsed from a string, held exactly as `mantissa * 10^-exponent`.
///
/// This is the only representation a caller can build an amount from. It is deliberately not
/// convertible from a float in either direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decimal {
    mantissa: u128,
    exponent: u32,
}

impl Decimal {
    /// Parse a plain decimal string. Rejects anything a careless caller might expect to work:
    /// scientific notation, signs, whitespace, digit separators, multiple decimal points.
    pub fn parse(value: &str) -> Result<Self, AmountError> {
        if value.is_empty() {
            return Err(AmountError::Empty);
        }
        if value.contains('e') || value.contains('E') {
            return Err(AmountError::ScientificNotation(value.to_owned()));
        }
        if value.starts_with('-') {
            return Err(AmountError::Negative(value.to_owned()));
        }
        if value.starts_with('+') {
            return Err(AmountError::LeadingSign(value.to_owned()));
        }
        if value.matches('.').count() > 1 {
            return Err(AmountError::MultipleDecimalPoints(value.to_owned()));
        }

        let (whole, fraction) = match value.split_once('.') {
            Some((w, f)) => (w, f),
            None => (value, ""),
        };
        let valid = !whole.is_empty()
            && whole.bytes().all(|b| b.is_ascii_digit())
            && (value.find('.').is_none() && fraction.is_empty()
                || !fraction.is_empty() && fraction.bytes().all(|b| b.is_ascii_digit()));
        if !valid {
            return Err(AmountError::NotDecimal(value.to_owned()));
        }

        let digits: String = format!("{whole}{fraction}");
        let mantissa = digits
            .parse::<u128>()
            .map_err(|_| AmountError::Overflow(value.to_owned()))?;
        Ok(Self {
            mantissa,
            exponent: fraction.len() as u32,
        })
    }

    /// How many digits sit after the decimal point.
    pub fn fractional_digits(&self) -> u32 {
        self.exponent
    }

    /// Scale to base units for a token with `decimals` places.
    ///
    /// Refuses when the value carries more precision than the token can hold, rather than
    /// truncating — the reference implementation makes the same choice, and it is the right
    /// one: a silently truncated amount is not the amount the caller asked for.
    pub fn to_base_units(&self, decimals: u32) -> Result<u64, AmountError> {
        if self.exponent > decimals {
            return Err(AmountError::TooPrecise {
                value: self.to_string(),
                digits: self.exponent as usize,
                allowed: decimals,
            });
        }
        let shift = decimals - self.exponent;
        let factor = pow10(shift).ok_or_else(|| AmountError::Overflow(self.to_string()))?;
        let scaled = self
            .mantissa
            .checked_mul(factor)
            .ok_or_else(|| AmountError::Overflow(self.to_string()))?;
        u64::try_from(scaled).map_err(|_| AmountError::ExceedsU64(self.to_string()))
    }

    /// Scale by an arbitrary rational `numerator / denominator`, exactly.
    ///
    /// This is what an order price needs: DeepBook wants
    /// `price * FLOAT_SCALAR * quote_scalar / base_scalar`, and every one of those is an
    /// integer. Computing it exactly makes the result independent of magnitude — which is the
    /// whole difference from the reference, where a large enough product silently lands one
    /// base unit off because a double had no bit left to hold it.
    pub fn scale_by_ratio(&self, numerator: u128, denominator: u128) -> Result<u64, AmountError> {
        let den = denominator
            .checked_mul(
                pow10(self.exponent).ok_or_else(|| AmountError::Overflow(self.to_string()))?,
            )
            .ok_or_else(|| AmountError::Overflow(self.to_string()))?;
        let num = self
            .mantissa
            .checked_mul(numerator)
            .ok_or_else(|| AmountError::Overflow(self.to_string()))?;
        if den == 0 || num % den != 0 {
            return Err(AmountError::Inexact {
                value: self.to_string(),
                multiplier_num: numerator,
                multiplier_den: denominator,
            });
        }
        u64::try_from(num / den).map_err(|_| AmountError::ExceedsU64(self.to_string()))
    }
}

impl fmt::Display for Decimal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.exponent == 0 {
            return write!(f, "{}", self.mantissa);
        }
        let s = self.mantissa.to_string();
        let width = self.exponent as usize;
        let padded = if s.len() <= width {
            format!("{}{}", "0".repeat(width + 1 - s.len()), s)
        } else {
            s
        };
        let split = padded.len() - width;
        write!(f, "{}.{}", &padded[..split], &padded[split..])
    }
}

fn pow10(exp: u32) -> Option<u128> {
    10u128.checked_pow(exp)
}

/// Convert a human decimal string into base units for a token with `decimals` places.
///
/// The direct equivalent of the reference's `decimalToBaseUnits`, and the function every
/// caller should reach for first.
pub fn decimal_to_base_units(value: &str, decimals: u32) -> Result<u64, AmountError> {
    Decimal::parse(value)?.to_base_units(decimals)
}

/// Parse a field that is already denominated in base units — a spend amount, a budget, a
/// millisecond timestamp. No decimal point, no sign, no scientific notation.
pub fn parse_u64_string(value: &str) -> Result<u64, AmountError> {
    if value.is_empty() {
        return Err(AmountError::Empty);
    }
    if !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(AmountError::NotDecimal(value.to_owned()));
    }
    value
        .parse::<u128>()
        .map_err(|_| AmountError::ExceedsU64(value.to_owned()))
        .and_then(|v| u64::try_from(v).map_err(|_| AmountError::ExceedsU64(value.to_owned())))
}

/// A DeepBook order price in base units: `price * float_scalar * quote_scalar / base_scalar`,
/// computed exactly.
pub fn deepbook_price_to_base_units(
    price: &str,
    float_scalar: u128,
    quote_scalar: u128,
    base_scalar: u128,
) -> Result<u64, AmountError> {
    let numerator = float_scalar
        .checked_mul(quote_scalar)
        .ok_or_else(|| AmountError::Overflow(price.to_owned()))?;
    Decimal::parse(price)?.scale_by_ratio(numerator, base_scalar)
}

/// A DeepBook order quantity in base units: `quantity * base_scalar`, computed exactly.
pub fn deepbook_quantity_to_base_units(
    quantity: &str,
    base_scalar: u128,
) -> Result<u64, AmountError> {
    Decimal::parse(quantity)?.scale_by_ratio(base_scalar, 1)
}
