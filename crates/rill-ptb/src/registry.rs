//! DeepBook's pool and coin registry, as data.
//!
//! The TypeScript SDK ships these tables and the Rust ecosystem has no equivalent, so they are
//! carried here rather than made the caller's problem. Generated from `@mysten/deepbook-v3` v1.5.1
//! — regenerate rather than hand-edit when DeepBook lists a new pool.
//!
//! Scalars matter more than they look. A pool's base and quote scalars decide the multiplier an
//! order price is scaled by, and one combination in this table — base 1e6 against quote 1e9 — is
//! where the reference implementation's float arithmetic lands a base unit off. `DEEP_SUI` is
//! exactly that shape and is listed on both networks, so the tables below are also the reason the
//! exact-integer money path is not theoretical.

/// One listed coin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeepBookCoin {
    pub symbol: &'static str,
    pub coin_type: &'static str,
    /// `1 <symbol>` in base units.
    pub scalar: u128,
}

/// One listed pool, by DeepBook's own key (e.g. `SUI_DBUSDC`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeepBookPool {
    pub key: &'static str,
    pub pool_id: &'static str,
    /// Symbol of the base coin — resolve through [`coin`].
    pub base: &'static str,
    pub quote: &'static str,
}

/// Which network's registry to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeepBookNetwork {
    Testnet,
    Mainnet,
}

/// `DEEPBOOK_PACKAGE_ID` on testnet.
pub const TESTNET_PACKAGE_ID: &str =
    "0x22be4cade64bf2d02412c7e8d0e8beea2f78828b948118d46735315409371a3c";
pub const TESTNET_REGISTRY_ID: &str =
    "0x7c256edbda983a2cd6f946655f4bf3f00a41043993781f8674a7046e8c0e11d1";

pub const TESTNET_COINS: &[DeepBookCoin] = &[
    DeepBookCoin {
        symbol: "DBTC",
        coin_type: "0x6502dae813dbe5e42643c119a6450a518481f03063febc7e20238e43b6ea9e86::dbtc::DBTC",
        scalar: 100000000,
    },
    DeepBookCoin {
        symbol: "DBUSDC",
        coin_type:
            "0xf7152c05930480cd740d7311b5b8b45c6f488e3a53a11c3f74a6fac36a52e0d7::DBUSDC::DBUSDC",
        scalar: 1000000,
    },
    DeepBookCoin {
        symbol: "DBUSDT",
        coin_type:
            "0xf7152c05930480cd740d7311b5b8b45c6f488e3a53a11c3f74a6fac36a52e0d7::DBUSDT::DBUSDT",
        scalar: 1000000,
    },
    DeepBookCoin {
        symbol: "DEEP",
        coin_type: "0x36dbef866a1d62bf7328989a10fb2f07d769f4ee587c0de4a0a256e57e0a58a8::deep::DEEP",
        scalar: 1000000,
    },
    DeepBookCoin {
        symbol: "SUI",
        coin_type: "0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI",
        scalar: 1000000000,
    },
    DeepBookCoin {
        symbol: "WAL",
        coin_type: "0x9ef7676a9f81937a52ae4b2af8d511a28a0b080477c0c2db40b0ab8882240d76::wal::WAL",
        scalar: 1000000000,
    },
];

pub const TESTNET_POOLS: &[DeepBookPool] = &[
    DeepBookPool {
        key: "DBTC_DBUSDC",
        pool_id: "0x0dce0aa771074eb83d1f4a29d48be8248d4d2190976a5241f66b43ec18fa34de",
        base: "DBTC",
        quote: "DBUSDC",
    },
    DeepBookPool {
        key: "DBUSDT_DBUSDC",
        pool_id: "0x83970bb02e3636efdff8c141ab06af5e3c9a22e2f74d7f02a9c3430d0d10c1ca",
        base: "DBUSDT",
        quote: "DBUSDC",
    },
    DeepBookPool {
        key: "DEEP_DBUSDC",
        pool_id: "0xe86b991f8632217505fd859445f9803967ac84a9d4a1219065bf191fcb74b622",
        base: "DEEP",
        quote: "DBUSDC",
    },
    DeepBookPool {
        key: "DEEP_SUI",
        pool_id: "0x48c95963e9eac37a316b7ae04a0deb761bcdcc2b67912374d6036e7f0e9bae9f",
        base: "DEEP",
        quote: "SUI",
    },
    DeepBookPool {
        key: "SUI_DBUSDC",
        pool_id: "0x1c19362ca52b8ffd7a33cee805a67d40f31e6ba303753fd3a4cfdfacea7163a5",
        base: "SUI",
        quote: "DBUSDC",
    },
    DeepBookPool {
        key: "WAL_DBUSDC",
        pool_id: "0xeb524b6aea0ec4b494878582e0b78924208339d360b62aec4a8ecd4031520dbb",
        base: "WAL",
        quote: "DBUSDC",
    },
    DeepBookPool {
        key: "WAL_SUI",
        pool_id: "0x8c1c1b186c4fddab1ebd53e0895a36c1d1b3b9a77cd34e607bef49a38af0150a",
        base: "WAL",
        quote: "SUI",
    },
];

/// `DEEPBOOK_PACKAGE_ID` on mainnet.
pub const MAINNET_PACKAGE_ID: &str =
    "0x0e735f8c93a95722efd73521aca7a7652c0bb71ed1daf41b26dfd7d1ff71f748";
pub const MAINNET_REGISTRY_ID: &str =
    "0xaf16199a2dff736e9f07a845f23c5da6df6f756eddb631aed9d24a93efc4549d";

pub const MAINNET_COINS: &[DeepBookCoin] = &[
    DeepBookCoin {
        symbol: "ALKIMI",
        coin_type:
            "0x1a8f4bc33f8ef7fbc851f156857aa65d397a6a6fd27a7ac2ca717b51f2fd9489::alkimi::ALKIMI",
        scalar: 1000000000,
    },
    DeepBookCoin {
        symbol: "AUSD",
        coin_type: "0x2053d08c1e2bd02791056171aab0fd12bd7cd7efad2ab8f6b9c8902f14df2ff2::ausd::AUSD",
        scalar: 1000000,
    },
    DeepBookCoin {
        symbol: "BETH",
        coin_type: "0xd0e89b2af5e4910726fbcd8b8dd37bb79b29e5f83f7491bca830e94f7f226d29::eth::ETH",
        scalar: 100000000,
    },
    DeepBookCoin {
        symbol: "DEEP",
        coin_type: "0xdeeb7a4662eec9f2f3def03fb937a663dddaa2e215b8078a284d026b7946c270::deep::DEEP",
        scalar: 1000000,
    },
    DeepBookCoin {
        symbol: "DRF",
        coin_type: "0x294de7579d55c110a00a7c4946e09a1b5cbeca2592fbb83fd7bfacba3cfeaf0e::drf::DRF",
        scalar: 1000000,
    },
    DeepBookCoin {
        symbol: "IKA",
        coin_type: "0x7262fb2f7a3a14c888c438a3cd9b912469a58cf60f367352c46584262e8299aa::ika::IKA",
        scalar: 1000000000,
    },
    DeepBookCoin {
        symbol: "LZWBTC",
        coin_type: "0x0041f9f9344cac094454cd574e333c4fdb132d7bcc9379bcd4aab485b2a63942::wbtc::WBTC",
        scalar: 100000000,
    },
    DeepBookCoin {
        symbol: "NS",
        coin_type: "0x5145494a5f5100e645e4b0aa950fa6b68f614e8c59e17bc5ded3495123a79178::ns::NS",
        scalar: 1000000,
    },
    DeepBookCoin {
        symbol: "SEND",
        coin_type: "0xb45fcfcc2cc07ce0702cc2d229621e046c906ef14d9b25e8e4d25f6e8763fef7::send::SEND",
        scalar: 1000000,
    },
    DeepBookCoin {
        symbol: "SUI",
        coin_type: "0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI",
        scalar: 1000000000,
    },
    DeepBookCoin {
        symbol: "SUIUSDE",
        coin_type:
            "0x41d587e5336f1c86cad50d38a7136db99333bb9bda91cea4ba69115defeb1402::sui_usde::SUI_USDE",
        scalar: 1000000,
    },
    DeepBookCoin {
        symbol: "TYPUS",
        coin_type:
            "0xf82dc05634970553615eef6112a1ac4fb7bf10272bf6cbe0f80ef44a6c489385::typus::TYPUS",
        scalar: 1000000000,
    },
    DeepBookCoin {
        symbol: "USDC",
        coin_type: "0xdba34672e30cb065b1f93e3ab55318768fd6fef66c15942c9f7cb846e2f900e7::usdc::USDC",
        scalar: 1000000,
    },
    DeepBookCoin {
        symbol: "USDSUI",
        coin_type:
            "0x44f838219cf67b058f3b37907b655f226153c18e33dfcd0da559a844fea9b1c1::usdsui::USDSUI",
        scalar: 1000000,
    },
    DeepBookCoin {
        symbol: "USDT",
        coin_type: "0x375f70cf2ae4c00bf37117d0c85a2c71545e6ee05c4a5c7d282cd66a4504b068::usdt::USDT",
        scalar: 1000000,
    },
    DeepBookCoin {
        symbol: "WAL",
        coin_type: "0x356a26eb9e012a68958082340d4c4116e7f55615cf27affcff209cf0ae544f59::wal::WAL",
        scalar: 1000000000,
    },
    DeepBookCoin {
        symbol: "WBTC",
        coin_type: "0x027792d9fed7f9844eb4839566001bb6f6cb4804f66aa2da6fe1ee242d896881::coin::COIN",
        scalar: 100000000,
    },
    DeepBookCoin {
        symbol: "WETH",
        coin_type: "0xaf8cd5edc19c4512f4259f0bee101a40d41ebed738ade5874359610ef8eeced5::coin::COIN",
        scalar: 100000000,
    },
    DeepBookCoin {
        symbol: "WUSDC",
        coin_type: "0x5d4b302506645c37ff133b98c4b50a5ae14841659738d6d733d59d0d217a93bf::coin::COIN",
        scalar: 1000000,
    },
    DeepBookCoin {
        symbol: "WUSDT",
        coin_type: "0xc060006111016b8a020ad5b33834984a437aaa7d3c74c18e09a95d48aceab08c::coin::COIN",
        scalar: 1000000,
    },
    DeepBookCoin {
        symbol: "XBTC",
        coin_type: "0x876a4b7bce8aeaef60464c11f4026903e9afacab79b9b142686158aa86560b50::xbtc::XBTC",
        scalar: 100000000,
    },
];

pub const MAINNET_POOLS: &[DeepBookPool] = &[
    DeepBookPool {
        key: "ALKIMI_SUI",
        pool_id: "0x84752993c6dc6fce70e25ddeb4daddb6592d6b9b0912a0a91c07cfff5a721d89",
        base: "ALKIMI",
        quote: "SUI",
    },
    DeepBookPool {
        key: "AUSD_USDC",
        pool_id: "0x5661fc7f88fbeb8cb881150a810758cf13700bb4e1f31274a244581b37c303c3",
        base: "AUSD",
        quote: "USDC",
    },
    DeepBookPool {
        key: "BETH_USDC",
        pool_id: "0x1109352b9112717bd2a7c3eb9a416fff1ba6951760f5bdd5424cf5e4e5b3e65c",
        base: "BETH",
        quote: "USDC",
    },
    DeepBookPool {
        key: "DEEP_SUI",
        pool_id: "0xb663828d6217467c8a1838a03793da896cbe745b150ebd57d82f814ca579fc22",
        base: "DEEP",
        quote: "SUI",
    },
    DeepBookPool {
        key: "DEEP_USDC",
        pool_id: "0xf948981b806057580f91622417534f491da5f61aeaf33d0ed8e69fd5691c95ce",
        base: "DEEP",
        quote: "USDC",
    },
    DeepBookPool {
        key: "DRF_SUI",
        pool_id: "0x126865a0197d6ab44bfd15fd052da6db92fd2eb831ff9663451bbfa1219e2af2",
        base: "DRF",
        quote: "SUI",
    },
    DeepBookPool {
        key: "IKA_USDC",
        pool_id: "0xfa732993af2b60d04d7049511f801e79426b2b6a5103e22769c0cead982b0f47",
        base: "IKA",
        quote: "USDC",
    },
    DeepBookPool {
        key: "LZWBTC_USDC",
        pool_id: "0xf5142aafa24866107df628bf92d0358c7da6acc46c2f10951690fd2b8570f117",
        base: "LZWBTC",
        quote: "USDC",
    },
    DeepBookPool {
        key: "NS_SUI",
        pool_id: "0x27c4fdb3b846aa3ae4a65ef5127a309aa3c1f466671471a806d8912a18b253e8",
        base: "NS",
        quote: "SUI",
    },
    DeepBookPool {
        key: "NS_USDC",
        pool_id: "0x0c0fdd4008740d81a8a7d4281322aee71a1b62c449eb5b142656753d89ebc060",
        base: "NS",
        quote: "USDC",
    },
    DeepBookPool {
        key: "SEND_USDC",
        pool_id: "0x1fe7b99c28ded39774f37327b509d58e2be7fff94899c06d22b407496a6fa990",
        base: "SEND",
        quote: "USDC",
    },
    DeepBookPool {
        key: "SUIUSDE_USDC",
        pool_id: "0x0fac1cebf35bde899cd9ecdd4371e0e33f44ba83b8a2902d69186646afa3a94b",
        base: "SUIUSDE",
        quote: "USDC",
    },
    DeepBookPool {
        key: "SUI_AUSD",
        pool_id: "0x183df694ebc852a5f90a959f0f563b82ac9691e42357e9a9fe961d71a1b809c8",
        base: "SUI",
        quote: "AUSD",
    },
    DeepBookPool {
        key: "SUI_SUIUSDE",
        pool_id: "0x034f3a42e7348de2084406db7a725f9d9d132a56c68324713e6e623601fb4fd7",
        base: "SUI",
        quote: "SUIUSDE",
    },
    DeepBookPool {
        key: "SUI_USDC",
        pool_id: "0xe05dafb5133bcffb8d59f4e12465dc0e9faeaa05e3e342a08fe135800e3e4407",
        base: "SUI",
        quote: "USDC",
    },
    DeepBookPool {
        key: "SUI_USDSUI",
        pool_id: "0x826eeacb2799726334aa580396338891205a41cf9344655e526aae6ddd5dc03f",
        base: "SUI",
        quote: "USDSUI",
    },
    DeepBookPool {
        key: "TYPUS_SUI",
        pool_id: "0xe8e56f377ab5a261449b92ac42c8ddaacd5671e9fec2179d7933dd1a91200eec",
        base: "TYPUS",
        quote: "SUI",
    },
    DeepBookPool {
        key: "USDSUI_USDC",
        pool_id: "0xa374264d43e6baa5aa8b35ff18ff24fdba7443b4bcb884cb4c2f568d32cdac36",
        base: "USDSUI",
        quote: "USDC",
    },
    DeepBookPool {
        key: "USDT_USDC",
        pool_id: "0xfc28a2fb22579c16d672a1152039cbf671e5f4b9f103feddff4ea06ef3c2bc25",
        base: "USDT",
        quote: "USDC",
    },
    DeepBookPool {
        key: "WAL_SUI",
        pool_id: "0x81f5339934c83ea19dd6bcc75c52e83509629a5f71d3257428c2ce47cc94d08b",
        base: "WAL",
        quote: "SUI",
    },
    DeepBookPool {
        key: "WAL_USDC",
        pool_id: "0x56a1c985c1f1123181d6b881714793689321ba24301b3585eec427436eb1c76d",
        base: "WAL",
        quote: "USDC",
    },
    DeepBookPool {
        key: "WUSDC_USDC",
        pool_id: "0xa0b9ebefb38c963fd115f52d71fa64501b79d1adcb5270563f92ce0442376545",
        base: "WUSDC",
        quote: "USDC",
    },
    DeepBookPool {
        key: "WUSDT_USDC",
        pool_id: "0x4e2ca3988246e1d50b9bf209abb9c1cbfec65bd95afdacc620a36c67bdb8452f",
        base: "WUSDT",
        quote: "USDC",
    },
    DeepBookPool {
        key: "XBTC_USDC",
        pool_id: "0x20b9a3ec7a02d4f344aa1ebc5774b7b0ccafa9a5d76230662fdc0300bb215307",
        base: "XBTC",
        quote: "USDC",
    },
];

impl DeepBookNetwork {
    pub fn package_id(self) -> &'static str {
        match self {
            Self::Testnet => TESTNET_PACKAGE_ID,
            Self::Mainnet => MAINNET_PACKAGE_ID,
        }
    }

    pub fn coins(self) -> &'static [DeepBookCoin] {
        match self {
            Self::Testnet => TESTNET_COINS,
            Self::Mainnet => MAINNET_COINS,
        }
    }

    pub fn pools(self) -> &'static [DeepBookPool] {
        match self {
            Self::Testnet => TESTNET_POOLS,
            Self::Mainnet => MAINNET_POOLS,
        }
    }
}

/// A coin by symbol. `None` for anything unlisted — never guessed, because a wrong scalar
/// misstates every amount that passes through it.
pub fn coin(network: DeepBookNetwork, symbol: &str) -> Option<&'static DeepBookCoin> {
    network.coins().iter().find(|c| c.symbol == symbol)
}

/// A pool by DeepBook's key.
pub fn pool(network: DeepBookNetwork, key: &str) -> Option<&'static DeepBookPool> {
    network.pools().iter().find(|p| p.key == key)
}

/// Everything needed to price an order on a pool, resolved from the tables.
///
/// Returns `None` when the pool or either of its coins is unlisted, rather than substituting a
/// default scalar — a scalar that is wrong by a factor of a thousand produces an order that is
/// wrong by a factor of a thousand.
pub fn pool_spec(network: DeepBookNetwork, key: &str) -> Option<crate::deepbook::PoolSpec> {
    let p = pool(network, key)?;
    let base = coin(network, p.base)?;
    let quote = coin(network, p.quote)?;
    Some(crate::deepbook::PoolSpec {
        pool_id: p.pool_id.parse().ok()?,
        base_coin_type: base.coin_type.to_string(),
        quote_coin_type: quote.coin_type.to_string(),
        base_scalar: base.scalar,
        quote_scalar: quote.scalar,
    })
}
