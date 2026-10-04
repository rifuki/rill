//! Exact Cetus pool quotes through a keyless, read-only simulation.
//!
//! The call package is explicit: a pool's type origin need not be its current package.
//! No price approximation or wallet funding is used when the quote is unavailable.

use rill_chain::{ChainError, SimulationOutcome, SuiRead, Verification};
use sui_sdk_types::{bcs::ToBcs, Address, Identifier, Transaction, TypeTag};
use sui_transaction_builder::{Function, ObjectInput, TransactionBuilder};

use crate::{cetus::pool_coin_types, shared::SharedObjects};

/// A pool-calculated quote in exact base units, including its separately reported fee.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CetusQuote {
    /// Input consumed by the pool excluding `fee_amount`. For exact-input quotes,
    /// compare requested input with the checked sum of these two fields.
    pub amount_in: u64,
    pub amount_out: u64,
    pub fee_amount: u64,
}

/// A quote that could not be safely obtained or used.
#[derive(Debug, thiserror::Error)]
pub enum QuoteError {
    #[error("Cetus quote chain read failed: {0}")]
    Chain(#[from] ChainError),
    #[error("invalid Cetus quote input: {0}")]
    InvalidInput(String),
    #[error("Cetus quote simulation failed or was unverified: {0}")]
    Simulation(String),
    #[error("Cetus quote returned malformed or missing {0}")]
    Malformed(&'static str),
    #[error("Cetus quote exceeds available pool liquidity")]
    Exceeded,
    #[error("Cetus quote returned zero output")]
    ZeroOutput,
}

fn identifier(name: &str) -> Result<Identifier, QuoteError> {
    Identifier::new(name).map_err(|_| QuoteError::InvalidInput(name.to_owned()))
}

/// Build the pool read followed by four scalar getters.
///
/// The pool is immutable in this transaction and the gas payment is empty. The only
/// command return positions consumed by [`parse_quote`] are these four getters.
#[expect(
    clippy::too_many_arguments,
    reason = "explicit pool read inputs prevent package/type guessing"
)]
pub fn quote_transaction(
    pool_package_id: Address,
    pool_id: Address,
    coin_type_a: &str,
    coin_type_b: &str,
    shared: &SharedObjects,
    gas_price: u64,
    a2b: bool,
    by_amount_in: bool,
    amount: u64,
) -> Result<Transaction, QuoteError> {
    if amount == 0 {
        return Err(QuoteError::InvalidInput("amount must be nonzero".into()));
    }
    let types: Vec<TypeTag> = [coin_type_a, coin_type_b]
        .into_iter()
        .map(|raw| {
            raw.parse()
                .map_err(|_| QuoteError::InvalidInput(raw.to_owned()))
        })
        .collect::<Result<_, _>>()?;
    let mut tx = TransactionBuilder::new();
    tx.set_sender(Address::ZERO);
    tx.set_gas_budget(100_000_000);
    tx.set_gas_price(gas_price);
    let pool = tx.object(
        shared
            .input(pool_id, false)
            .map_err(|e| QuoteError::InvalidInput(e.to_string()))?,
    );
    let direction = tx.pure(&a2b);
    let exact_input = tx.pure(&by_amount_in);
    let value = tx.pure(&amount);
    let calculated = tx.move_call(
        Function::new(
            pool_package_id,
            identifier("pool")?,
            identifier("calculate_swap_result")?,
        )
        .with_type_args(types),
        vec![pool, direction, exact_input, value],
    );
    for getter in [
        "calculated_swap_result_amount_out",
        "calculated_swap_result_amount_in",
        "calculated_swap_result_is_exceed",
        "calculated_swap_result_fee_amount",
    ] {
        tx.move_call(
            Function::new(pool_package_id, identifier("pool")?, identifier(getter)?),
            vec![calculated],
        );
    }
    let placeholder = crate::book::PLACEHOLDER_GAS_OBJECT
        .parse()
        .map_err(|_| QuoteError::InvalidInput("placeholder gas object".into()))?;
    tx.add_gas_objects([ObjectInput::owned(
        placeholder,
        1,
        sui_sdk_types::Digest::ZERO,
    )]);
    let mut built = tx
        .try_build()
        .map_err(|e| QuoteError::InvalidInput(e.to_string()))?;
    built.gas_payment.objects.clear();
    Ok(built)
}

/// Decode only successful, verified simulations and exact BCS scalar values.
pub fn parse_quote(outcome: &SimulationOutcome) -> Result<CetusQuote, QuoteError> {
    if !outcome.ok || outcome.verification != Verification::Verified {
        return Err(QuoteError::Simulation(
            outcome
                .error
                .clone()
                .unwrap_or_else(|| "unverified read".into()),
        ));
    }
    if outcome.command_returns.len() != 5 || outcome.command_output_count != 5 {
        return Err(QuoteError::Malformed("getter command results"));
    }
    let scalar = |index: usize, name| -> Result<&[u8], QuoteError> {
        let values = &outcome.command_returns[index];
        if values.len() != 1 {
            return Err(QuoteError::Malformed(name));
        }
        Ok(&values[0])
    };
    let integer = |index, name| -> Result<u64, QuoteError> {
        let bytes: [u8; 8] = scalar(index, name)?
            .try_into()
            .map_err(|_| QuoteError::Malformed(name))?;
        Ok(u64::from_le_bytes(bytes))
    };
    let amount_out = integer(1, "amount_out")?;
    let amount_in = integer(2, "amount_in")?;
    let exceeded = match scalar(3, "is_exceed")? {
        [0] => false,
        [1] => true,
        _ => return Err(QuoteError::Malformed("is_exceed")),
    };
    let fee_amount = integer(4, "fee_amount")?;
    if exceeded {
        return Err(QuoteError::Exceeded);
    }
    if amount_out == 0 {
        return Err(QuoteError::ZeroOutput);
    }
    if amount_in == 0 {
        return Err(QuoteError::Malformed("nonzero amount_in"));
    }
    Ok(CetusQuote {
        amount_in,
        amount_out,
        fee_amount,
    })
}

/// Read a real pool quote without a signer, funding, or transaction submission.
pub async fn quote(
    chain: &impl SuiRead,
    pool_package_id: Address,
    pool_id: Address,
    a2b: bool,
    by_amount_in: bool,
    amount: u64,
) -> Result<CetusQuote, QuoteError> {
    if amount == 0 {
        return Err(QuoteError::InvalidInput("amount must be nonzero".into()));
    }
    let pool = chain.get_object(&pool_id.to_string()).await?;
    let (coin_a, coin_b) = pool
        .object_type
        .as_deref()
        .and_then(pool_coin_types)
        .ok_or_else(|| QuoteError::InvalidInput("pool coin types unavailable".into()))?;
    let version = pool
        .shared_initial_version
        .filter(|version| *version > 0)
        .ok_or_else(|| {
            QuoteError::InvalidInput("pool initial shared version unavailable".into())
        })?;
    let mut shared = SharedObjects::new();
    shared.insert(pool_id, version);
    let gas_price = chain.reference_gas_price().await?;
    let transaction = quote_transaction(
        pool_package_id,
        pool_id,
        &coin_a,
        &coin_b,
        &shared,
        gas_price,
        a2b,
        by_amount_in,
        amount,
    )?;
    let encoded = transaction
        .to_bcs_base64()
        .map_err(|e| QuoteError::InvalidInput(e.to_string()))?;
    parse_quote(&chain.simulate_read(&encoded).await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rill_chain::{SimulationOutcome, Verification};

    fn outcome(out: u64, input: u64, fee: u64, exceed: Vec<u8>) -> SimulationOutcome {
        SimulationOutcome {
            ok: true,
            verification: Verification::Verified,
            error: None,
            gas_used_mist: 0,
            balance_changes: vec![],
            command_output_count: 5,
            command_returns: vec![
                vec![],
                vec![out.to_le_bytes().to_vec()],
                vec![input.to_le_bytes().to_vec()],
                vec![exceed],
                vec![fee.to_le_bytes().to_vec()],
            ],
        }
    }

    #[test]
    fn zero_amount_is_refused_before_building_a_read() {
        let result = quote_transaction(
            Address::ZERO,
            Address::ZERO,
            "0x2::sui::SUI",
            "0x2::sui::SUI",
            &SharedObjects::new(),
            1000,
            true,
            true,
            0,
        );
        assert!(matches!(result, Err(QuoteError::InvalidInput(_))));
    }

    #[test]
    fn quote_preserves_exact_amounts_above_javascript_integer_precision() {
        let exact = (1u64 << 53) + 1;
        assert_eq!(
            parse_quote(&outcome(exact, exact + 2, 3, vec![0])).unwrap(),
            CetusQuote {
                amount_out: exact,
                amount_in: exact + 2,
                fee_amount: 3
            }
        );
    }
    #[test]
    fn exceeded_quote_is_refused() {
        assert!(matches!(
            parse_quote(&outcome(42, 10, 1, vec![1])),
            Err(QuoteError::Exceeded)
        ));
    }
    #[test]
    fn zero_output_is_refused() {
        assert!(matches!(
            parse_quote(&outcome(0, 10, 1, vec![0])),
            Err(QuoteError::ZeroOutput)
        ));
    }
    #[test]
    fn malformed_or_missing_returns_are_refused() {
        let mut result = outcome(42, 10, 1, vec![0]);
        result.command_returns[1][0].push(0);
        assert!(parse_quote(&result).is_err());
        result.command_returns.pop();
        assert!(parse_quote(&result).is_err());
        assert!(parse_quote(&outcome(42, 10, 1, vec![2])).is_err());
    }
    #[test]
    fn failed_or_unverified_simulation_cannot_supply_quote() {
        let mut result = outcome(42, 10, 1, vec![0]);
        result.ok = false;
        assert!(parse_quote(&result).is_err());
        result.ok = true;
        result.verification = Verification::Unverified;
        assert!(parse_quote(&result).is_err());
    }
    #[tokio::test]
    #[ignore = "read-only live mainnet pool; requires network"]
    async fn live_mainnet_quotes_cross_the_example_output_floor() {
        let chain = rill_chain::grpc::GrpcSui::new("https://fullnode.mainnet.sui.io:443").unwrap();
        let package: Address = "0x260693ec785a6e6c9d81d58c7d2ff72f1288ae0fa6a9725abe05a6478b11f084"
            .parse()
            .unwrap();
        let pool: Address = "0xb8d7d9e66a60c239e7a60110efcf8de6c705580ed924d0dde141f4a0e2c90105"
            .parse()
            .unwrap();
        let small = quote(&chain, package, pool, false, true, 5_000_000)
            .await
            .unwrap();
        let larger = quote(&chain, package, pool, false, true, 10_000_000)
            .await
            .unwrap();
        println!("read-only .005 SUI quote: {small:?}; .01 SUI quote: {larger:?}");
        assert_eq!(
            small.amount_in.checked_add(small.fee_amount),
            Some(5_000_000)
        );
        assert_eq!(
            larger.amount_in.checked_add(larger.fee_amount),
            Some(10_000_000)
        );
        // These floor comparisons are a live regression scenario, not a fixed price fixture.
        assert!(
            small.amount_out < 10_000,
            "example floor no longer brackets current market: {small:?}"
        );
        assert!(
            larger.amount_out >= 10_000,
            "example floor no longer brackets current market: {larger:?}"
        );
    }
}
