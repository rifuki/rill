#[test_only]
module agent_wallet::recipient_allowlist_tests {
    use sui::test_scenario as ts;
    use sui::coin::{Self, Coin};
    use sui::sui::SUI;
    use sui::clock;
    use sui::package;
    use agent_wallet::agent_wallet::{Self as aw, AgentWallet, AgentCap};
    use agent_wallet::version::{Self as av, Version};
    use agent_wallet::budget;

    const OWNER: address = @0xA;
    const AGENT: address = @0xB;
    const RECIPIENT: address = @0xD;
    const STRANGER: address = @0xE;

    // Mirror agent_wallet::agent_wallet's abort codes (constants are module-private).
    const E_NOT_OWNER: u64 = 1;
    const E_REVOKED: u64 = 2;
    const E_RULE_NOT_SATISFIED: u64 = 10;
    const E_RECIPIENT_ALLOWLIST: u64 = 20;
    const E_RECIPIENT_NOT_ALLOWED: u64 = 21;
    const E_RECIPIENT_ALREADY_ALLOWED: u64 = 22;
    const E_NO_RECIPIENT_ALLOWLIST: u64 = 23;
    const VERSION_E_INVALID_PACKAGE_VERSION: u64 = 0;

    /// The live mainnet `Version` before this upgrade's `migrate`.
    const MAINNET_V2: u64 = 2;

    /// Same-package stand-in for the real one-time witness: `from_package` compares addresses only.
    public struct TEST_OTW has drop {}

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
        ts::return_shared(w);
        ts::return_shared(v);
        sc
    }

    fun allow_as(sc: &mut ts::Scenario, sender: address, recipient: address) {
        ts::next_tx(sc, sender);
        let v = ts::take_shared<Version>(sc);
        let mut w = ts::take_shared<AgentWallet<SUI>>(sc);
        aw::allow_recipient(&mut w, &v, recipient, ts::ctx(sc));
        ts::return_shared(w);
        ts::return_shared(v);
    }

    fun allow(sc: &mut ts::Scenario, recipient: address) { allow_as(sc, OWNER, recipient) }

    fun disallow_as(sc: &mut ts::Scenario, sender: address, recipient: address) {
        ts::next_tx(sc, sender);
        let mut w = ts::take_shared<AgentWallet<SUI>>(sc);
        aw::disallow_recipient(&mut w, recipient, ts::ctx(sc));
        ts::return_shared(w);
    }

    fun clear_as(sc: &mut ts::Scenario, sender: address) {
        ts::next_tx(sc, sender);
        let mut w = ts::take_shared<AgentWallet<SUI>>(sc);
        aw::clear_recipient_allowlist(&mut w, ts::ctx(sc));
        ts::return_shared(w);
    }

    /// The agent's payout. `prove` false leaves the budget receipt off, which must still abort.
    fun spend_to(sc: &mut ts::Scenario, recipient: address, prove: bool) {
        ts::next_tx(sc, AGENT);
        let v = ts::take_shared<Version>(sc);
        let mut w = ts::take_shared<AgentWallet<SUI>>(sc);
        let cap = ts::take_from_sender<AgentCap>(sc);
        let clock = clock::create_for_testing(ts::ctx(sc));
        let mut req = aw::request_spend(&w, &cap, &v, 100, &clock, ts::ctx(sc));
        if (prove) { budget::prove(&mut req, &mut w, &v); };
        aw::confirm_spend_to(&mut w, req, recipient, &v, &clock, ts::ctx(sc));
        ts::return_shared(w);
        ts::return_shared(v);
        ts::return_to_sender(sc, cap);
        clock::destroy_for_testing(clock);
    }

    /// The generic release: the coin comes back to the transaction, which can send it anywhere.
    fun spend_generic(sc: &mut ts::Scenario): u64 {
        ts::next_tx(sc, AGENT);
        let v = ts::take_shared<Version>(sc);
        let mut w = ts::take_shared<AgentWallet<SUI>>(sc);
        let cap = ts::take_from_sender<AgentCap>(sc);
        let clock = clock::create_for_testing(ts::ctx(sc));
        let mut req = aw::request_spend(&w, &cap, &v, 100, &clock, ts::ctx(sc));
        budget::prove(&mut req, &mut w, &v);
        let out = aw::confirm_spend(&mut w, req, &v, &clock, ts::ctx(sc));
        let value = coin::value(&out);
        coin::burn_for_testing(out);
        ts::return_shared(w);
        ts::return_shared(v);
        ts::return_to_sender(sc, cap);
        clock::destroy_for_testing(clock);
        value
    }

    fun set_version(sc: &mut ts::Scenario, value: u64) {
        ts::next_tx(sc, OWNER);
        let mut v = ts::take_shared<Version>(sc);
        av::set_for_testing(&mut v, value);
        ts::return_shared(v);
    }

    #[test]
    fun version_is_three() {
        assert!(av::current_for_testing() == 3, 0);
    }

    // ── the coin path closes ──

    #[test, expected_failure(abort_code = E_RECIPIENT_ALLOWLIST, location = aw)]
    fun confirm_spend_aborts_once_allowlist_set() {
        let mut sc = setup();
        allow(&mut sc, RECIPIENT);
        spend_generic(&mut sc);
        ts::end(sc);
    }

    #[test, expected_failure(abort_code = E_RECIPIENT_ALLOWLIST, location = aw)]
    fun removing_last_recipient_keeps_coin_path_closed() {
        let mut sc = setup();
        allow(&mut sc, RECIPIENT);
        disallow_as(&mut sc, OWNER, RECIPIENT);
        spend_generic(&mut sc);
        ts::end(sc);
    }

    #[test, expected_failure(abort_code = E_RECIPIENT_NOT_ALLOWED, location = aw)]
    fun spend_to_after_removal_aborts() {
        let mut sc = setup();
        allow(&mut sc, RECIPIENT);
        disallow_as(&mut sc, OWNER, RECIPIENT);
        spend_to(&mut sc, RECIPIENT, true);
        ts::end(sc);
    }

    #[test]
    fun clear_reopens_generic_release() {
        let mut sc = setup();
        allow(&mut sc, RECIPIENT);
        clear_as(&mut sc, OWNER);
        ts::next_tx(&mut sc, OWNER);
        let w = ts::take_shared<AgentWallet<SUI>>(&sc);
        assert!(!aw::has_recipient_allowlist(&w), 0);
        assert!(aw::recipient_allowlist(&w).is_none(), 1);
        assert!(!aw::is_recipient_allowed(&w, RECIPIENT), 2);
        ts::return_shared(w);
        assert!(spend_generic(&mut sc) == 100, 3);
        ts::end(sc);
    }

    // ── payouts go only to listed recipients ──

    #[test]
    fun spend_to_listed_recipient_delivers_coin() {
        let mut sc = setup();
        allow(&mut sc, RECIPIENT);
        spend_to(&mut sc, RECIPIENT, true);
        ts::next_tx(&mut sc, RECIPIENT);
        let out = ts::take_from_sender<Coin<SUI>>(&sc);
        assert!(coin::value(&out) == 100, 0);
        coin::burn_for_testing(out);
        let w = ts::take_shared<AgentWallet<SUI>>(&sc);
        assert!(aw::spent(&w) == 100, 1);
        assert!(aw::remaining(&w) == 900, 2);
        ts::return_shared(w);
        ts::end(sc);
    }

    #[test, expected_failure(abort_code = E_RECIPIENT_NOT_ALLOWED, location = aw)]
    fun spend_to_unlisted_recipient_aborts() {
        let mut sc = setup();
        allow(&mut sc, RECIPIENT);
        spend_to(&mut sc, STRANGER, true);
        ts::end(sc);
    }

    #[test, expected_failure(abort_code = E_RECIPIENT_NOT_ALLOWED, location = aw)]
    fun spend_to_agent_itself_aborts_unless_listed() {
        let mut sc = setup();
        allow(&mut sc, RECIPIENT);
        spend_to(&mut sc, AGENT, true);
        ts::end(sc);
    }

    #[test]
    fun spend_to_agent_succeeds_when_owner_lists_it() {
        let mut sc = setup();
        allow(&mut sc, AGENT);
        spend_to(&mut sc, AGENT, true);
        ts::end(sc);
    }

    #[test, expected_failure(abort_code = E_NO_RECIPIENT_ALLOWLIST, location = aw)]
    fun spend_to_without_allowlist_aborts() {
        let mut sc = setup();
        spend_to(&mut sc, RECIPIENT, true);
        ts::end(sc);
    }

    #[test, expected_failure(abort_code = E_RULE_NOT_SATISFIED, location = aw)]
    fun spend_to_still_requires_every_rule_receipt() {
        let mut sc = setup();
        allow(&mut sc, RECIPIENT);
        spend_to(&mut sc, RECIPIENT, false);
        ts::end(sc);
    }

    #[test, expected_failure(abort_code = E_REVOKED, location = aw)]
    fun spend_to_on_revoked_wallet_aborts() {
        let mut sc = setup();
        allow(&mut sc, RECIPIENT);
        ts::next_tx(&mut sc, OWNER);
        let mut w = ts::take_shared<AgentWallet<SUI>>(&sc);
        coin::burn_for_testing(aw::revoke(&mut w, ts::ctx(&mut sc)));
        ts::return_shared(w);
        spend_to(&mut sc, RECIPIENT, true);
        ts::end(sc);
    }

    // ── only the owner manages the list ──

    #[test, expected_failure(abort_code = E_NOT_OWNER, location = aw)]
    fun allow_by_agent_aborts() {
        let mut sc = setup();
        allow_as(&mut sc, AGENT, AGENT);
        ts::end(sc);
    }

    #[test, expected_failure(abort_code = E_NOT_OWNER, location = aw)]
    fun disallow_by_agent_aborts() {
        let mut sc = setup();
        allow(&mut sc, RECIPIENT);
        disallow_as(&mut sc, AGENT, RECIPIENT);
        ts::end(sc);
    }

    #[test, expected_failure(abort_code = E_NOT_OWNER, location = aw)]
    fun clear_by_agent_aborts() {
        let mut sc = setup();
        allow(&mut sc, RECIPIENT);
        clear_as(&mut sc, AGENT);
        ts::end(sc);
    }

    #[test, expected_failure(abort_code = E_RECIPIENT_ALREADY_ALLOWED, location = aw)]
    fun allow_twice_aborts() {
        let mut sc = setup();
        allow(&mut sc, RECIPIENT);
        allow(&mut sc, RECIPIENT);
        ts::end(sc);
    }

    #[test, expected_failure(abort_code = E_RECIPIENT_NOT_ALLOWED, location = aw)]
    fun disallow_unlisted_aborts() {
        let mut sc = setup();
        allow(&mut sc, RECIPIENT);
        disallow_as(&mut sc, OWNER, STRANGER);
        ts::end(sc);
    }

    #[test, expected_failure(abort_code = E_NO_RECIPIENT_ALLOWLIST, location = aw)]
    fun disallow_without_allowlist_aborts() {
        let mut sc = setup();
        disallow_as(&mut sc, OWNER, RECIPIENT);
        ts::end(sc);
    }

    #[test, expected_failure(abort_code = E_NO_RECIPIENT_ALLOWLIST, location = aw)]
    fun clear_without_allowlist_aborts() {
        let mut sc = setup();
        clear_as(&mut sc, OWNER);
        ts::end(sc);
    }

    #[test]
    fun views_report_the_list() {
        let mut sc = setup();
        ts::next_tx(&mut sc, OWNER);
        let w = ts::take_shared<AgentWallet<SUI>>(&sc);
        assert!(!aw::has_recipient_allowlist(&w), 0);
        assert!(aw::recipient_allowlist(&w).is_none(), 1);
        ts::return_shared(w);
        allow(&mut sc, RECIPIENT);
        allow(&mut sc, STRANGER);
        disallow_as(&mut sc, OWNER, STRANGER);
        ts::next_tx(&mut sc, OWNER);
        let w = ts::take_shared<AgentWallet<SUI>>(&sc);
        assert!(aw::has_recipient_allowlist(&w), 2);
        assert!(aw::recipient_allowlist(&w) == option::some(vector[RECIPIENT]), 3);
        assert!(aw::is_recipient_allowed(&w, RECIPIENT), 4);
        assert!(!aw::is_recipient_allowed(&w, STRANGER), 5);
        ts::return_shared(w);
        ts::end(sc);
    }

    #[test]
    fun emptied_list_reads_as_present_and_empty() {
        let mut sc = setup();
        allow(&mut sc, RECIPIENT);
        disallow_as(&mut sc, OWNER, RECIPIENT);
        ts::next_tx(&mut sc, OWNER);
        let w = ts::take_shared<AgentWallet<SUI>>(&sc);
        assert!(aw::has_recipient_allowlist(&w), 0);
        assert!(aw::recipient_allowlist(&w) == option::some(vector[]), 1);
        ts::return_shared(w);
        ts::end(sc);
    }

    // ── the upgrade: activation is gated, tightening and leaving are not ──

    #[test, expected_failure(abort_code = VERSION_E_INVALID_PACKAGE_VERSION, location = av)]
    fun allow_with_stale_version_aborts() {
        let mut sc = setup();
        set_version(&mut sc, MAINNET_V2);
        allow(&mut sc, RECIPIENT);
        ts::end(sc);
    }

    #[test]
    fun disallow_and_clear_work_with_stale_version() {
        let mut sc = setup();
        allow(&mut sc, RECIPIENT);
        allow(&mut sc, STRANGER);
        set_version(&mut sc, MAINNET_V2);
        disallow_as(&mut sc, OWNER, STRANGER);
        clear_as(&mut sc, OWNER);
        ts::next_tx(&mut sc, OWNER);
        let w = ts::take_shared<AgentWallet<SUI>>(&sc);
        assert!(!aw::has_recipient_allowlist(&w), 0);
        ts::return_shared(w);
        ts::end(sc);
    }

    #[test, expected_failure(abort_code = VERSION_E_INVALID_PACKAGE_VERSION, location = av)]
    fun spend_to_with_stale_version_aborts() {
        let mut sc = setup();
        allow(&mut sc, RECIPIENT);
        set_version(&mut sc, MAINNET_V2);
        spend_to(&mut sc, RECIPIENT, true);
        ts::end(sc);
    }

    /// v2 bytecode stays callable on Sui; what disables it is the live `Version` moving past the
    /// `VERSION = 2` its `check_is_valid` compares against. Without that, v2's `confirm_spend`
    /// knows nothing of the allowlist and would pay out a coin the PTB can send anywhere.
    #[test]
    fun migrate_moves_mainnet_past_the_version_v2_bytecode_accepts() {
        let mut sc = setup();
        set_version(&mut sc, MAINNET_V2);
        ts::next_tx(&mut sc, OWNER);
        let mut v = ts::take_shared<Version>(&sc);
        let publisher = package::test_claim(TEST_OTW {}, ts::ctx(&mut sc));
        av::migrate(&publisher, &mut v);
        assert!(av::value_for_testing(&v) == 3, 0);
        assert!(av::value_for_testing(&v) != MAINNET_V2, 1);
        package::burn_publisher(publisher);
        ts::return_shared(v);
        allow(&mut sc, RECIPIENT);
        spend_to(&mut sc, RECIPIENT, true);
        ts::end(sc);
    }
}
