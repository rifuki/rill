//! The SUI coins an address can pay gas with, at the versions the ledger holds now.
//!
//! An owner listing comes from an index, and the index trails the ledger. On mainnet, one
//! transaction after a wallet create, the listing still named the owner's gas coin at the version
//! before the create spent it; the attach that followed was refused before submission ("the
//! transaction names version 1023962234 and the chain is at version 1023962235"). Re-running did
//! not help, because every run read the same lagging list.
//!
//! So the listing decides only *which* coins, and each coin's reference is read again from the
//! ledger before anything is built on it.

use crate::{ChainError, ChainResult, ObjectSummary, SuiRead};

/// The one spelling of a SUI coin's type that owner listings and object reads report.
pub const SUI_COIN_TYPE: &str =
    "0x0000000000000000000000000000000000000000000000000000000000000002::coin::Coin<0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI>";

/// Every SUI coin `owner` holds, each re-read from the ledger so its reference is current.
///
/// A coin the listing names but the ledger no longer has (merged or spent since the index was
/// written) is left out rather than failing the whole read: it is not a coin anybody can pay with.
pub async fn sui_gas_coins(chain: &impl SuiRead, owner: &str) -> ChainResult<Vec<ObjectSummary>> {
    let listed = chain.list_owned_objects(owner).await?;
    let mut coins = Vec::new();
    for coin in listed
        .iter()
        .filter(|o| o.object_type.as_deref() == Some(SUI_COIN_TYPE))
    {
        match chain.get_object(&coin.reference.id).await {
            Ok(current) => coins.push(current),
            Err(ChainError::NotFound(_)) => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(coins)
}

/// A coin's balance in base units, when the read carried its fields.
pub fn coin_balance(coin: &ObjectSummary) -> Option<u64> {
    match coin.fields.as_ref()?.get("balance")? {
        serde_json::Value::String(s) => s.parse().ok(),
        serde_json::Value::Number(n) => n.as_u64(),
        _ => None,
    }
}

/// The gas budget to name: the one asked for, or everything the paying coins hold if that is less.
///
/// A fixed budget above the sender's gas is refused by the node before it runs ("Balance of gas
/// object … is lower than the needed amount"), however little the transaction actually costs. On
/// mainnet an agent holding 0.096 SUI could not make a 0.004 SUI spend because every tool asked for
/// a 0.1 SUI budget. The simulation that follows still decides whether what is held is enough.
///
/// A coin whose balance was not read leaves the request as it was: guessing it low would refuse a
/// transaction the node would have taken, and guessing it high is the bug this exists for.
pub fn affordable_budget(requested: u64, coins: &[ObjectSummary]) -> u64 {
    let mut held: u64 = 0;
    for coin in coins {
        match coin_balance(coin) {
            Some(balance) => held = held.saturating_add(balance),
            None => return requested,
        }
    }
    if held == 0 {
        requested
    } else {
        requested.min(held)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{fake::FakeSui, ObjectRef};

    const OWNER: &str = "0xa1";
    const COIN: &str = "0xc7";

    fn coin(version: u64, digest: &str) -> ObjectSummary {
        ObjectSummary {
            reference: ObjectRef {
                id: COIN.into(),
                version,
                digest: digest.into(),
            },
            object_type: Some(SUI_COIN_TYPE.into()),
            fields: None,
            shared_initial_version: None,
        }
    }

    #[tokio::test]
    async fn a_listing_one_version_behind_is_replaced_by_the_ledger_reference() {
        let chain = FakeSui::new()
            .with_object(Some(OWNER), coin(1023962235, "new"))
            .with_listing_behind(COIN, 1023962234, "old");

        let listed = chain.list_owned_objects(OWNER).await.unwrap();
        assert_eq!(listed[0].reference.version, 1023962234, "the fake must lag");

        let coins = sui_gas_coins(&chain, OWNER).await.unwrap();
        assert_eq!(coins.len(), 1);
        assert_eq!(coins[0].reference.version, 1023962235);
        assert_eq!(coins[0].reference.digest, "new");
    }

    fn with_balance(mut coin: ObjectSummary, balance: &str) -> ObjectSummary {
        coin.fields = Some(serde_json::json!({ "balance": balance }));
        coin
    }

    #[test]
    fn a_budget_above_what_the_coins_hold_is_lowered_to_what_they_hold() {
        let coins = [with_balance(coin(1, "d"), "96160268")];
        assert_eq!(affordable_budget(100_000_000, &coins), 96_160_268);
        assert_eq!(affordable_budget(50_000_000, &coins), 50_000_000);
    }

    #[test]
    fn an_unread_balance_leaves_the_budget_as_asked() {
        assert_eq!(affordable_budget(100_000_000, &[coin(1, "d")]), 100_000_000);
    }

    #[tokio::test]
    async fn objects_that_are_not_sui_coins_are_not_gas() {
        let mut cap = coin(3, "d");
        cap.reference.id = "0xca".into();
        cap.object_type = Some("0x1::agent_wallet::AgentCap".into());
        let chain = FakeSui::new().with_object(Some(OWNER), cap);
        assert!(sui_gas_coins(&chain, OWNER).await.unwrap().is_empty());
    }
}
