//! Coin decimals, keyed by full Move type.
//!
//! Keyed by the full `<address>::<module>::<name>` rather than by symbol, because a symbol is not
//! unique — testnet carries two distinct coins both called USDC, from different packages, and
//! resolving by symbol would silently apply one's decimals to the other.
//!
//! Scope is only the coins Rill's own adapters touch. A coin that is not listed is not guessed at:
//! amount formatting degrades to raw base units with the type spelled out, which is honest, where
//! assuming 9 decimals would quietly misstate a balance by a factor of a thousand.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenInfo {
    pub coin_type: &'static str,
    pub symbol: &'static str,
    /// `1 <symbol> == 10^decimals` base units.
    pub decimals: u32,
}

pub const TOKENS: &[TokenInfo] = &[
    TokenInfo {
        coin_type: "0x2::sui::SUI",
        symbol: "SUI",
        decimals: 9,
    },
    TokenInfo {
        // DeepBook's testnet mock USDC. A different coin from the Cetus testnet USDC below,
        // despite the shared symbol.
        coin_type:
            "0xf7152c05930480cd740d7311b5b8b45c6f488e3a53a11c3f74a6fac36a52e0d7::DBUSDC::DBUSDC",
        symbol: "DBUSDC",
        decimals: 6,
    },
    TokenInfo {
        // Mainnet USDC — the same on-chain address serves both DeepBook and Cetus.
        coin_type: "0xdba34672e30cb065b1f93e3ab55318768fd6fef66c15942c9f7cb846e2f900e7::usdc::USDC",
        symbol: "USDC",
        decimals: 6,
    },
    TokenInfo {
        // Cetus's testnet swap-pool USDC. Its decimals are an inference, not a sourced value:
        // nothing in the DeepBook package ships metadata for it, since it is not DeepBook-listed.
        // Six matches every other USDC in this table and the ecosystem convention.
        coin_type: "0x14a71d857b34677a7d57e0feb303df1adb515a37780645ab763d42ce8d1a5e48::usdc::USDC",
        symbol: "USDC",
        decimals: 6,
    },
    TokenInfo {
        coin_type: "0x9ef7676a9f81937a52ae4b2af8d511a28a0b080477c0c2db40b0ab8882240d76::wal::WAL",
        symbol: "WAL",
        decimals: 9,
    },
    TokenInfo {
        coin_type: "0x356a26eb9e012a68958082340d4c4116e7f55615cf27affcff209cf0ae544f59::wal::WAL",
        symbol: "WAL",
        decimals: 9,
    },
];

/// Look up a token by its full Move coin type. `None` for anything not listed — callers must
/// degrade honestly rather than assume a decimal count.
pub fn find_token(coin_type: &str) -> Option<&'static TokenInfo> {
    TOKENS.iter().find(|t| t.coin_type == coin_type)
}
