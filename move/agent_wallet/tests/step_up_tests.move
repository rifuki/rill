#[test_only]
module agent_wallet::step_up_tests {
    use sui::test_scenario as ts;
    use sui::coin;
    use sui::sui::SUI;
    use sui::clock;
    use agent_wallet::agent_wallet::{Self as aw, AgentWallet, AgentCap};
    use agent_wallet::version::{Self as av, Version};
    use agent_wallet::budget;
    use agent_wallet::step_up;

    const OWNER: address = @0xA;
    const AGENT: address = @0xB;
    const STRANGER: address = @0xE;

    const THRESHOLD: u64 = 300;

    // Mirror the private abort codes this file expects.
    const E_NOT_OWNER: u64 = 1;
    const E_RULE_NOT_SATISFIED: u64 = 10;
    const STEP_UP_E_NEEDS_OWNER: u64 = 1;
    const VERSION_E_INVALID_PACKAGE_VERSION: u64 = 0;

    /// A funded wallet with `budget` (so the policy is never empty) and `step_up` at THRESHOLD.
    fun setup(): ts::Scenario {
        let mut sc = ts::begin(OWNER);
        av::init_for_testing(ts::ctx(&mut sc));
        ts::next_tx(&mut sc, OWNER);
        let v = ts::take_shared<Version>(&sc);
        let funds = coin::mint_for_testing<SUI>(10_000, ts::ctx(&mut sc));
        aw::create_wallet(&v, funds, AGENT, 100_000, ts::ctx(&mut sc));
        ts::return_shared(v);
        ts::next_tx(&mut sc, OWNER);
        let v = ts::take_shared<Version>(&sc);
        let mut w = ts::take_shared<AgentWallet<SUI>>(&sc);
        budget::add(&mut w, &v, 10_000, ts::ctx(&mut sc));
        step_up::add(&mut w, &v, THRESHOLD, ts::ctx(&mut sc));
        ts::return_shared(w);
        ts::return_shared(v);
        sc
    }

    /// The agent sends the next transaction, with `sponsor` paying its gas when it is set. The
    /// reference gas price is copied from the running scenario because the builder refuses a new one
    /// inside the same epoch.
    fun next_as_agent(sc: &mut ts::Scenario, sponsor: Option<address>) {
        let rgp = ts::ctx(sc).reference_gas_price();
        let mut builder = ts::ctx_builder_from_sender(AGENT).set_reference_gas_price(rgp);
        builder = if (sponsor.is_some()) builder.set_sponsor(sponsor.destroy_some()) else builder.unset_sponsor();
        ts::next_with_context(sc, builder);
    }

    /// One transaction from the agent that releases each amount in `amounts` with every attached rule
    /// proven, `step_up` included only when `prove_step_up` holds. Returns the total released.
    fun spend_in_one_tx(sc: &mut ts::Scenario, sponsor: Option<address>, amounts: vector<u64>, prove_step_up: bool): u64 {
        next_as_agent(sc, sponsor);
        let v = ts::take_shared<Version>(sc);
        let mut w = ts::take_shared<AgentWallet<SUI>>(sc);
        let cap = ts::take_from_sender<AgentCap>(sc);
        let clk = clock::create_for_testing(ts::ctx(sc));
        let mut total = 0;
        let mut i = 0;
        while (i < amounts.length()) {
            let mut req = aw::request_spend(&w, &cap, &v, amounts[i], &clk, ts::ctx(sc));
            budget::prove(&mut req, &mut w, &v);
            if (prove_step_up) { step_up::prove(&mut req, &mut w, &v, ts::ctx(sc)); };
            let out = aw::confirm_spend(&mut w, req, &v, &clk, ts::ctx(sc));
            total = total + coin::value(&out);
            coin::burn_for_testing(out);
            i = i + 1;
        };
        clock::destroy_for_testing(clk);
        ts::return_shared(w);
        ts::return_shared(v);
        ts::return_to_sender(sc, cap);
        total
    }

    #[test]
    fun at_or_under_the_threshold_no_sponsor_is_needed() {
        let mut sc = setup();
        assert!(spend_in_one_tx(&mut sc, option::none(), vector[THRESHOLD], true) == THRESHOLD, 0);
        assert!(spend_in_one_tx(&mut sc, option::none(), vector[1], true) == 1, 1);
        ts::end(sc);
    }

    #[test, expected_failure(abort_code = STEP_UP_E_NEEDS_OWNER, location = step_up)]
    fun above_the_threshold_without_a_sponsor_aborts() {
        let mut sc = setup();
        spend_in_one_tx(&mut sc, option::none(), vector[THRESHOLD + 1], true);
        ts::end(sc);
    }

    #[test]
    fun above_the_threshold_with_the_owner_as_sponsor_releases() {
        let mut sc = setup();
        assert!(spend_in_one_tx(&mut sc, option::some(OWNER), vector[THRESHOLD + 1], true) == THRESHOLD + 1, 0);
        ts::end(sc);
    }

    #[test, expected_failure(abort_code = STEP_UP_E_NEEDS_OWNER, location = step_up)]
    fun above_the_threshold_with_another_sponsor_aborts() {
        let mut sc = setup();
        spend_in_one_tx(&mut sc, option::some(STRANGER), vector[THRESHOLD + 1], true);
        ts::end(sc);
    }

    /// The agent's own key paying its own gas is the case the rule exists to stop.
    #[test, expected_failure(abort_code = STEP_UP_E_NEEDS_OWNER, location = step_up)]
    fun above_the_threshold_with_the_agent_as_sponsor_aborts() {
        let mut sc = setup();
        spend_in_one_tx(&mut sc, option::some(AGENT), vector[THRESHOLD + 1], true);
        ts::end(sc);
    }

    /// Splitting one large payout into requests each under the threshold must not slip past it.
    #[test, expected_failure(abort_code = STEP_UP_E_NEEDS_OWNER, location = step_up)]
    fun two_requests_under_the_threshold_but_over_it_together_abort() {
        let mut sc = setup();
        spend_in_one_tx(&mut sc, option::none(), vector[200, 200], true);
        ts::end(sc);
    }

    #[test]
    fun two_requests_over_the_threshold_together_release_with_the_owner_as_sponsor() {
        let mut sc = setup();
        assert!(spend_in_one_tx(&mut sc, option::some(OWNER), vector[200, 200], true) == 400, 0);
        ts::end(sc);
    }

    /// The running total belongs to one transaction: the next one starts again from zero.
    #[test]
    fun the_running_total_resets_in_the_next_transaction() {
        let mut sc = setup();
        assert!(spend_in_one_tx(&mut sc, option::none(), vector[200], true) == 200, 0);
        assert!(spend_in_one_tx(&mut sc, option::none(), vector[200], true) == 200, 1);
        ts::end(sc);
    }

    #[test, expected_failure(abort_code = E_RULE_NOT_SATISFIED, location = aw)]
    fun a_prove_list_that_omits_step_up_aborts() {
        let mut sc = setup();
        spend_in_one_tx(&mut sc, option::some(OWNER), vector[1], false);
        ts::end(sc);
    }

    #[test, expected_failure(abort_code = E_NOT_OWNER, location = aw)]
    fun the_agent_cannot_attach_step_up() {
        let mut sc = ts::begin(OWNER);
        av::init_for_testing(ts::ctx(&mut sc));
        ts::next_tx(&mut sc, OWNER);
        let v = ts::take_shared<Version>(&sc);
        let funds = coin::mint_for_testing<SUI>(1_000, ts::ctx(&mut sc));
        aw::create_wallet(&v, funds, AGENT, 100_000, ts::ctx(&mut sc));
        ts::return_shared(v);
        ts::next_tx(&mut sc, AGENT);
        let v = ts::take_shared<Version>(&sc);
        let mut w = ts::take_shared<AgentWallet<SUI>>(&sc);
        step_up::add(&mut w, &v, THRESHOLD, ts::ctx(&mut sc));
        ts::return_shared(w);
        ts::return_shared(v);
        ts::end(sc);
    }

    #[test, expected_failure(abort_code = E_NOT_OWNER, location = aw)]
    fun the_agent_cannot_remove_step_up() {
        let mut sc = setup();
        ts::next_tx(&mut sc, AGENT);
        let v = ts::take_shared<Version>(&sc);
        let mut w = ts::take_shared<AgentWallet<SUI>>(&sc);
        step_up::remove(&mut w, &v, ts::ctx(&mut sc));
        ts::return_shared(w);
        ts::return_shared(v);
        ts::end(sc);
    }

    #[test]
    fun the_owner_removes_step_up_and_large_spends_need_no_sponsor_again() {
        let mut sc = setup();
        ts::next_tx(&mut sc, OWNER);
        let v = ts::take_shared<Version>(&sc);
        let mut w = ts::take_shared<AgentWallet<SUI>>(&sc);
        step_up::remove(&mut w, &v, ts::ctx(&mut sc));
        assert!(!aw::has_rule<SUI, step_up::Rule>(&w), 0);
        ts::return_shared(w);
        ts::return_shared(v);
        assert!(spend_in_one_tx(&mut sc, option::none(), vector[THRESHOLD + 1], false) == THRESHOLD + 1, 1);
        ts::end(sc);
    }

    #[test]
    fun the_threshold_is_readable() {
        let mut sc = setup();
        ts::next_tx(&mut sc, OWNER);
        let w = ts::take_shared<AgentWallet<SUI>>(&sc);
        assert!(step_up::threshold(step_up::config(&w)) == THRESHOLD, 0);
        ts::return_shared(w);
        ts::end(sc);
    }

    /// Stale bytecode must not be able to prove a rule the live version may have changed.
    #[test, expected_failure(abort_code = VERSION_E_INVALID_PACKAGE_VERSION, location = av)]
    fun prove_is_version_gated() {
        let mut sc = setup();
        ts::next_tx(&mut sc, OWNER);
        let mut v = ts::take_shared<Version>(&sc);
        av::set_for_testing(&mut v, 2);
        ts::return_shared(v);
        next_as_agent(&mut sc, option::some(OWNER));
        let v = ts::take_shared<Version>(&sc);
        let mut w = ts::take_shared<AgentWallet<SUI>>(&sc);
        let cap = ts::take_from_sender<AgentCap>(&sc);
        let clk = clock::create_for_testing(ts::ctx(&mut sc));
        // request_spend is gated too, so the agent cannot even reach prove: either abort proves the
        // point, and request_spend is the one that fires first.
        let mut req = aw::request_spend(&w, &cap, &v, 1, &clk, ts::ctx(&mut sc));
        step_up::prove(&mut req, &mut w, &v, ts::ctx(&mut sc));
        let out = aw::confirm_spend(&mut w, req, &v, &clk, ts::ctx(&mut sc));
        coin::burn_for_testing(out);
        clock::destroy_for_testing(clk);
        ts::return_shared(w);
        ts::return_shared(v);
        ts::return_to_sender(&sc, cap);
        ts::end(sc);
    }
}
