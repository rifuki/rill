/// Curated exact-input Cetus swaps. Witness values never escape this module.
/// The real protocol receipt determines repayment; all actual output settles to
/// the wallet owner. No generic transfer, recipient argument or caller supplied
/// package name is accepted.
module cetus_adapter::swap {
    use sui::balance;
    use sui::coin;
    use sui::clock::Clock;
    use agent_wallet::agent_wallet::{Self as aw, AgentWallet, SpendRequest};
    use agent_wallet::version::Version;
    use cetusclmm::config::GlobalConfig;
    use cetusclmm::pool::{Self, Pool};

    public struct AToB has drop {}
    public struct BToA has drop {}
    const E_INVALID_REPAYMENT: u64 = 0;

    public fun configure_a_to_b<A, B>(
        wallet: &mut AgentWallet<A>, version: &Version, pool: ID,
        min_output: u64, ctx: &TxContext,
    ) {
        aw::configure_protected<A, B, AToB>(AToB {}, wallet, version, pool, min_output, ctx);
    }
    public fun configure_b_to_a<A, B>(
        wallet: &mut AgentWallet<B>, version: &Version, pool: ID,
        min_output: u64, ctx: &TxContext,
    ) {
        aw::configure_protected<B, A, BToA>(BToA {}, wallet, version, pool, min_output, ctx);
    }

    public fun execute_a_to_b<A, B>(
        wallet: &mut AgentWallet<A>, req: SpendRequest, revision: u64,
        min_output: u64, sqrt_price_limit: u128, version: &Version,
        config: &GlobalConfig, pool: &mut Pool<A, B>, clock: &Clock,
        ctx: &mut TxContext,
    ) {
        let amount = aw::request_amount(&req);
        let (input, settlement) = aw::confirm_protected<A, B, AToB>(
            AToB {}, wallet, req, revision, object::id(pool), min_output, version, clock, ctx,
        );
        let mut payment = coin::into_balance(input);
        let (returned_a, output_b, receipt) = pool::flash_swap<A, B>(
            config, pool, true, true, amount, sqrt_price_limit, clock,
        );
        let owed = pool::swap_pay_amount(&receipt);
        assert!(owed <= amount, E_INVALID_REPAYMENT);
        let debt = balance::split(&mut payment, owed);
        pool::repay_flash_swap(config, pool, debt, balance::zero<B>(), receipt);
        balance::join(&mut payment, returned_a);
        aw::settle_protected(AToB {}, wallet, settlement,
            coin::from_balance(output_b, ctx), coin::from_balance(payment, ctx), ctx);
    }

    public fun execute_b_to_a<A, B>(
        wallet: &mut AgentWallet<B>, req: SpendRequest, revision: u64,
        min_output: u64, sqrt_price_limit: u128, version: &Version,
        config: &GlobalConfig, pool: &mut Pool<A, B>, clock: &Clock,
        ctx: &mut TxContext,
    ) {
        let amount = aw::request_amount(&req);
        let (input, settlement) = aw::confirm_protected<B, A, BToA>(
            BToA {}, wallet, req, revision, object::id(pool), min_output, version, clock, ctx,
        );
        let mut payment = coin::into_balance(input);
        let (output_a, returned_b, receipt) = pool::flash_swap<A, B>(
            config, pool, false, true, amount, sqrt_price_limit, clock,
        );
        let owed = pool::swap_pay_amount(&receipt);
        assert!(owed <= amount, E_INVALID_REPAYMENT);
        let debt = balance::split(&mut payment, owed);
        pool::repay_flash_swap(config, pool, balance::zero<A>(), debt, receipt);
        balance::join(&mut payment, returned_b);
        aw::settle_protected(BToA {}, wallet, settlement,
            coin::from_balance(output_a, ctx), coin::from_balance(payment, ctx), ctx);
    }
}
