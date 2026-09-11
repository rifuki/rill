/// Rill guard: the on-chain slippage chokepoint for agent swaps.
///
/// Protocol-agnostic: `assert_min_value` aborts if a swap's output coin holds less than the caller's
/// floor. `rill_swap` emits it on the bought coin before that coin goes anywhere, so an agent cannot
/// accept a worse-than-`min` fill even when a sandwich moves the pool under the trade.
///
/// This is the bound, not a second opinion on one. `sqrt_price_limit` is not defence in depth here:
/// on the funded side the only value that does not abort inside Cetus is the extreme one, which
/// bounds nothing. An earlier version of this comment said the floor was injected after every swap
/// output and that the price bound sat on top of it; neither was true of the code, which emitted no
/// floor at all on any path and set the price bound to its widest value.
module rill_guard::guard {
    use sui::coin::Coin;

    /// Output is below the caller's minimum (slippage floor breached).
    const E_SLIPPAGE: u64 = 1;

    /// Abort unless `coin` holds at least `min`. Borrows the coin (immutable), so it stays usable
    /// downstream in the same PTB.
    public fun assert_min_value<T>(coin: &Coin<T>, min: u64) {
        assert!(coin.value() >= min, E_SLIPPAGE);
    }
}
