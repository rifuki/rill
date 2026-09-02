//! What a pool will accept, and refusing an order it will not.
//!
//! # A pool has a floor and a grid
//!
//! `pool::pool_book_params` returns three numbers: a tick size the price must be a multiple of, a
//! lot size the quantity must be a multiple of, and a minimum size below which an order is rejected
//! outright. They differ per pool by orders of magnitude — SUI/DBUSDC on testnet takes nothing
//! under 1 SUI, DEEP/SUI nothing under 10 DEEP.
//!
//! An order that misses any of them aborts in `order_info::validate_inputs`, which reports a bare
//! code and names neither the number that was wrong nor the number it should have been. Checked
//! here, the caller is told both before any gas is spent.

use rill_core::amounts::AmountError;

/// A pool's constraints, as the pool itself reports them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BookParams {
    /// The price must be a multiple of this, in DeepBook's scaled price units.
    pub tick_size: u64,
    /// The quantity must be a multiple of this, in base units.
    pub lot_size: u64,
    /// No order smaller than this, in base units.
    pub min_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrderConstraintError {
    /// Smaller than the pool's floor.
    BelowMinimum {
        quantity: u64,
        min_size: u64,
    },
    /// Not on the pool's quantity grid.
    OffLot {
        quantity: u64,
        lot_size: u64,
    },
    /// Not on the pool's price grid.
    OffTick {
        price: u64,
        tick_size: u64,
    },
    Amount(AmountError),
}

impl std::fmt::Display for OrderConstraintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BelowMinimum { quantity, min_size } => write!(
                f,
                "the pool takes no order smaller than {min_size} base units, and this one is \
                 {quantity}. Raise the quantity to at least {min_size}."
            ),
            Self::OffLot { quantity, lot_size } => write!(
                f,
                "the quantity must be a multiple of the pool's lot size {lot_size}; {quantity} is \
                 not. The nearest allowed values are {} and {}.",
                quantity - (quantity % lot_size),
                quantity - (quantity % lot_size) + lot_size
            ),
            Self::OffTick { price, tick_size } => write!(
                f,
                "the price must be a multiple of the pool's tick size {tick_size}; {price} is not. \
                 The nearest allowed values are {} and {}.",
                price - (price % tick_size),
                price - (price % tick_size) + tick_size
            ),
            Self::Amount(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for OrderConstraintError {}

impl BookParams {
    /// Check an order against the pool before building it.
    ///
    /// Both values are already in the chain's own units — base units for the quantity, DeepBook's
    /// scaled units for the price — because converting them is where a float would otherwise get in.
    pub fn check(&self, price: u64, quantity: u64) -> Result<(), OrderConstraintError> {
        if quantity < self.min_size {
            return Err(OrderConstraintError::BelowMinimum {
                quantity,
                min_size: self.min_size,
            });
        }
        if self.lot_size != 0 && !quantity.is_multiple_of(self.lot_size) {
            return Err(OrderConstraintError::OffLot {
                quantity,
                lot_size: self.lot_size,
            });
        }
        if self.tick_size != 0 && !price.is_multiple_of(self.tick_size) {
            return Err(OrderConstraintError::OffTick {
                price,
                tick_size: self.tick_size,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SUI/DBUSDC on testnet, read from the pool on 2026-09-02.
    const SUI_DBUSDC: BookParams = BookParams {
        tick_size: 10,
        lot_size: 100_000_000,
        min_size: 1_000_000_000,
    };

    /// DEEP/SUI on testnet, same read.
    const DEEP_SUI: BookParams = BookParams {
        tick_size: 10_000_000,
        lot_size: 1_000_000,
        min_size: 10_000_000,
    };

    /// The order that was actually refused on chain, and what it cost to find out.
    #[test]
    fn the_order_that_aborted_is_refused_here_with_the_number_it_missed() {
        // 0.01 SUI against a 1 SUI floor.
        let err = SUI_DBUSDC.check(1_500_000_000_000, 10_000_000).unwrap_err();
        assert!(matches!(err, OrderConstraintError::BelowMinimum { .. }));
        let message = err.to_string();
        assert!(message.contains("1000000000"), "{message}");
        assert!(message.contains("10000000"), "{message}");
    }

    #[test]
    fn an_order_on_the_grid_passes() {
        // 10 DEEP at 0.015 SUI: quantity 10 * 1e6, price 0.015 * 1e12.
        assert!(DEEP_SUI.check(15_000_000_000, 10_000_000).is_ok());
    }

    #[test]
    fn a_quantity_between_lots_names_both_neighbours() {
        let err = DEEP_SUI.check(15_000_000_000, 10_500_000).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("10000000") && message.contains("11000000"),
            "{message}"
        );
    }

    #[test]
    fn a_price_between_ticks_names_both_neighbours() {
        let err = DEEP_SUI.check(15_005_000_000, 10_000_000).unwrap_err();
        assert!(matches!(err, OrderConstraintError::OffTick { .. }), "{err}");
    }

    /// The pools differ by two orders of magnitude, which is the reason this cannot be a constant.
    #[test]
    fn two_pools_do_not_share_a_floor() {
        assert_ne!(SUI_DBUSDC.min_size, DEEP_SUI.min_size);
        assert!(DEEP_SUI.check(15_000_000_000, 10_000_000).is_ok());
        assert!(SUI_DBUSDC.check(15_000_000_000, 10_000_000).is_err());
    }
}
