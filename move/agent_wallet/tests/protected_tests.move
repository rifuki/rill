#[test_only]
module agent_wallet::protected_tests {
    use sui::test_scenario as ts;
    use sui::coin;
    use sui::sui::SUI;
    use sui::clock;
    use agent_wallet::agent_wallet::{Self as aw, AgentWallet, AgentCap};
    use agent_wallet::version::{Self as av, Version};
    use agent_wallet::budget;
    public struct Adapter has drop {}
    public struct Unauthorized has drop {}
    public struct OUTPUT has drop {}
    const OWNER: address = @0xA;
    const AGENT: address = @0xB;

    fun setup(): ts::Scenario {
        let mut sc = ts::begin(OWNER);
        av::init_for_testing(ts::ctx(&mut sc));
        ts::next_tx(&mut sc, OWNER);
        let v = ts::take_shared<Version>(&sc);
        let funds = coin::mint_for_testing<SUI>(1000, ts::ctx(&mut sc));
        aw::create_wallet(&v, funds, AGENT, 100000, ts::ctx(&mut sc));
        ts::return_shared(v);
        ts::next_tx(&mut sc, OWNER);
        let v = ts::take_shared<Version>(&sc);
        let mut w = ts::take_shared<AgentWallet<SUI>>(&sc);
        budget::add(&mut w, &v, 1000, ts::ctx(&mut sc));
        aw::configure_protected<SUI, OUTPUT, Adapter>(Adapter {}, &mut w, &v, object::id_from_address(@0xC), 50, ts::ctx(&mut sc));
        ts::return_shared(w);
        ts::return_shared(v);
        sc
    }

    fun execute(sc: &mut ts::Scenario, revision: u64, pool: address, output: u64, unauthorized: bool, generic: bool) {
        ts::next_tx(sc, AGENT);
        let v = ts::take_shared<Version>(sc);
        let mut w = ts::take_shared<AgentWallet<SUI>>(sc);
        let cap = ts::take_from_sender<AgentCap>(sc);
        let clock = clock::create_for_testing(ts::ctx(sc));
        let mut req = aw::request_spend(&w, &cap, &v, 100, &clock, ts::ctx(sc));
        budget::prove(&mut req, &mut w, &v);
        if (generic) {
            coin::burn_for_testing(aw::confirm_spend(&mut w, req, &v, &clock, ts::ctx(sc)));
        } else if (unauthorized) {
            let (input, receipt) = aw::confirm_protected<SUI, OUTPUT, Unauthorized>(Unauthorized {}, &mut w, req, revision, object::id_from_address(pool), 50, &v, &clock, ts::ctx(sc));
            coin::burn_for_testing(input);
            aw::settle_protected(Unauthorized {}, &mut w, receipt, coin::mint_for_testing<OUTPUT>(output, ts::ctx(sc)), coin::zero<SUI>(ts::ctx(sc)), ts::ctx(sc));
        } else {
            let (input, receipt) = aw::confirm_protected<SUI, OUTPUT, Adapter>(Adapter {}, &mut w, req, revision, object::id_from_address(pool), 50, &v, &clock, ts::ctx(sc));
            coin::burn_for_testing(input);
            aw::settle_protected(Adapter {}, &mut w, receipt, coin::mint_for_testing<OUTPUT>(output, ts::ctx(sc)), coin::zero<SUI>(ts::ctx(sc)), ts::ctx(sc));
        };
        assert!(aw::spent(&w) == 100, 0);
        assert!(aw::remaining(&w) == 900, 1);
        ts::return_shared(w);
        ts::return_shared(v);
        ts::return_to_sender(sc, cap);
        clock::destroy_for_testing(clock);
    }

    #[test]
    fun settles_only_to_owner() {
        let mut sc = setup();
        execute(&mut sc, 1, @0xC, 60, false, false);
        ts::next_tx(&mut sc, OWNER);
        let out = ts::take_from_sender<sui::coin::Coin<OUTPUT>>(&sc);
        assert!(coin::value(&out) == 60, 0);
        coin::burn_for_testing(out);
        ts::end(sc);
    }
    #[test, expected_failure(abort_code = 12, location = aw)]
    fun generic_release_blocked() { let mut sc = setup(); execute(&mut sc, 1, @0xC, 60, false, true); ts::end(sc); }
    #[test, expected_failure(abort_code = 14, location = aw)]
    fun wrong_adapter_blocked() { let mut sc = setup(); execute(&mut sc, 1, @0xC, 60, true, false); ts::end(sc); }
    #[test, expected_failure(abort_code = 15, location = aw)]
    fun stale_revision_blocked() { let mut sc = setup(); execute(&mut sc, 0, @0xC, 60, false, false); ts::end(sc); }
    #[test, expected_failure(abort_code = 16, location = aw)]
    fun wrong_pool_blocked() { let mut sc = setup(); execute(&mut sc, 1, @0xD, 60, false, false); ts::end(sc); }
    #[test, expected_failure(abort_code = 17, location = aw)]
    fun minimum_output_enforced() { let mut sc = setup(); execute(&mut sc, 1, @0xC, 49, false, false); ts::end(sc); }
    #[test]
    fun edit_preserves_spend_and_owner_exit_ignores_version() {
        let mut sc = setup();
        execute(&mut sc, 1, @0xC, 60, false, false);
        ts::next_tx(&mut sc, OWNER);
        let mut v = ts::take_shared<Version>(&sc);
        let mut w = ts::take_shared<AgentWallet<SUI>>(&sc);
        aw::configure_protected<SUI, OUTPUT, Adapter>(Adapter {}, &mut w, &v, object::id_from_address(@0xC), 70, ts::ctx(&mut sc));
        assert!(aw::protected_revision(&w) == 2, 0);
        assert!(aw::spent(&w) == 100, 1);
        av::set_for_testing(&mut v, 1);
        coin::burn_for_testing(aw::withdraw(&mut w, 100, ts::ctx(&mut sc)));
        assert!(aw::remaining(&w) == 800, 2);
        coin::burn_for_testing(aw::revoke(&mut w, ts::ctx(&mut sc)));
        assert!(aw::remaining(&w) == 0, 3);
        ts::return_shared(w);
        ts::return_shared(v);
        ts::end(sc);
    }
    #[test, expected_failure(abort_code = 1, location = aw)]
    fun agent_cannot_edit() {
        let mut sc = setup();
        ts::next_tx(&mut sc, AGENT);
        let v = ts::take_shared<Version>(&sc);
        let mut w = ts::take_shared<AgentWallet<SUI>>(&sc);
        aw::configure_protected<SUI, OUTPUT, Adapter>(Adapter {}, &mut w, &v, object::id_from_address(@0xD), 1, ts::ctx(&mut sc));
        ts::return_shared(w);
        ts::return_shared(v);
        ts::end(sc);
    }
    #[test, expected_failure(abort_code = 0, location = av)]
    fun version_one_is_not_authoritative_after_upgrade() {
        let mut sc = setup();
        ts::next_tx(&mut sc, OWNER);
        let mut v = ts::take_shared<Version>(&sc);
        av::set_for_testing(&mut v, 1);
        ts::return_shared(v);
        execute(&mut sc, 1, @0xC, 60, false, false);
        ts::end(sc);
    }

    fun execute_output<Out>(sc: &mut ts::Scenario, min_output: u64, change: u64, prove: bool) {
        ts::next_tx(sc, AGENT);
        let v = ts::take_shared<Version>(sc);
        let mut w = ts::take_shared<AgentWallet<SUI>>(sc);
        let cap = ts::take_from_sender<AgentCap>(sc);
        let clock = clock::create_for_testing(ts::ctx(sc));
        let mut req = aw::request_spend(&w, &cap, &v, 100, &clock, ts::ctx(sc));
        if (prove) { budget::prove(&mut req, &mut w, &v); };
        let (input, receipt) = aw::confirm_protected<SUI, Out, Adapter>(Adapter {}, &mut w, req, 1, object::id_from_address(@0xC), min_output, &v, &clock, ts::ctx(sc));
        coin::burn_for_testing(input);
        aw::settle_protected(Adapter {}, &mut w, receipt, coin::mint_for_testing<Out>(60, ts::ctx(sc)), coin::mint_for_testing<SUI>(change, ts::ctx(sc)), ts::ctx(sc));
        assert!(aw::spent(&w) == 100, 0);
        assert!(aw::remaining(&w) == 900 + change, 1);
        ts::return_shared(w);
        ts::return_shared(v);
        ts::return_to_sender(sc, cap);
        clock::destroy_for_testing(clock);
    }
    #[test, expected_failure(abort_code = 18, location = aw)]
    fun wrong_output_asset_blocked() { let mut sc = setup(); execute_output<SUI>(&mut sc, 50, 0, true); ts::end(sc); }
    #[test, expected_failure(abort_code = 17, location = aw)]
    fun caller_cannot_lower_owner_floor() { let mut sc = setup(); execute_output<OUTPUT>(&mut sc, 49, 0, true); ts::end(sc); }
    #[test, expected_failure(abort_code = 19, location = aw)]
    fun change_cannot_exceed_reserved_input() { let mut sc = setup(); execute_output<OUTPUT>(&mut sc, 50, 101, true); ts::end(sc); }
    #[test, expected_failure(abort_code = 10, location = aw)]
    fun protected_path_requires_all_rule_receipts() { let mut sc = setup(); execute_output<OUTPUT>(&mut sc, 50, 0, false); ts::end(sc); }
    #[test]
    fun change_returns_to_vault_without_resetting_spend() { let mut sc = setup(); execute_output<OUTPUT>(&mut sc, 50, 20, true); ts::end(sc); }

}
