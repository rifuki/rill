use base64::{engine::general_purpose::STANDARD, Engine};
use rill_chain::{fake::FakeSui, ObjectRef, ObjectSummary};
use rill_core::{envelope::Network, flow::FlowGraph};
use rill_ptb::deployments;
use rill_server::studio_compile::{compile, CompileOptions};
use serde_json::json;
use sui_sdk_types::{Address, Command, TransactionKind};

fn options() -> CompileOptions {
    CompileOptions {
        sender: None,
        agent_wallet: None,
        network: Network::Testnet,
        guard_package: Some(deployments::TESTNET_RILL_GUARD.parse().unwrap()),
    }
}
fn shared(chain: FakeSui, id: &str, kind: &str) -> FakeSui {
    let id = id.parse::<Address>().unwrap().to_string();
    chain.with_object(
        None,
        ObjectSummary {
            reference: ObjectRef {
                id,
                version: 17,
                digest: sui_sdk_types::Digest::ZERO.to_string(),
            },
            object_type: Some(kind.into()),
            fields: None,
            shared_initial_version: Some(7),
        },
    )
}
fn chain() -> FakeSui {
    let chain = shared(FakeSui::new(), "0x5", "0x3::sui_system::SuiSystemState");
    let chain = shared(chain, "0x6", "0x2::clock::Clock");
    let chain = shared(
        chain,
        deployments::TESTNET_HAEDAL_STAKING,
        "0x3::staking::Staking",
    );
    let chain = shared(
        chain,
        deployments::TESTNET_CETUS_GLOBAL_CONFIG,
        "0x3::config::GlobalConfig",
    );
    shared(
        chain,
        "0x123",
        "0x3::pool::Pool<0x2::sui::SUI, 0x3::usdc::USDC>",
    )
}
fn flow(value: serde_json::Value) -> FlowGraph {
    serde_json::from_value(value).unwrap()
}

#[tokio::test]
async fn standalone_stake_returns_real_kind_with_empty_preview_gas() {
    let flow = flow(
        json!({"nodes":[{"id":"stake","type":"haedal_stake","config":{"amount":"1000000000"}}],"edges":[]}),
    );
    let result = compile(&flow, &options(), &chain()).await.unwrap();
    let kind: TransactionKind =
        bcs::from_bytes(&STANDARD.decode(&result.unsigned_ptb).unwrap()).unwrap();
    let TransactionKind::ProgrammableTransaction(ptb) = kind else {
        panic!("expected PTB")
    };
    assert!(ptb.commands.iter().any(
        |c| matches!(c, Command::MoveCall(call) if call.function.as_str() == "request_stake")
    ));
    assert!(result.transaction.gas_payment.objects.is_empty());
    assert_eq!(result.root_spend_mist, 1_000_000_000);
}

#[tokio::test]
async fn swap_output_guard_uses_coin_b_and_settles_both_coins_once() {
    let flow = flow(json!({"nodes":[
        {"id":"swap","type":"cetus_swap","config":{"pool":"0x123","amount_in":"1000000000","inputCoinType":"0x2::sui::SUI"}},
        {"id":"guard","type":"guardrail","config":{"minValue":"100"}}
    ],"edges":[{"source":"swap","sourceHandle":"coin_out","target":"guard","targetHandle":"in"}]}));
    let result = compile(&flow, &options(), &chain()).await.unwrap();
    let TransactionKind::ProgrammableTransaction(ptb) = result.transaction.kind else {
        panic!("expected PTB")
    };
    let swap_index = ptb
        .commands
        .iter()
        .position(|c| matches!(c, Command::MoveCall(call) if call.function.as_str() == "swap"))
        .unwrap() as u16;
    let guard = ptb
        .commands
        .iter()
        .find_map(|c| match c {
            Command::MoveCall(call) if call.function.as_str() == "assert_min_value" => Some(call),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        guard.arguments[0],
        sui_sdk_types::Argument::NestedResult(swap_index, 1)
    );
    let settled: usize = ptb
        .commands
        .iter()
        .filter_map(|c| match c {
            Command::TransferObjects(t) => Some(t.objects.len()),
            _ => None,
        })
        .sum();
    assert_eq!(settled, 2);
}

#[tokio::test]
async fn unknown_node_is_refused_instead_of_silently_skipped() {
    let flow = flow(json!({"nodes":[{"id":"typo","type":"cetus_swpa"}],"edges":[]}));
    assert!(compile(&flow, &options(), &chain())
        .await
        .unwrap_err()
        .to_string()
        .contains("unsupported"));
}

#[tokio::test]
async fn fanout_is_refused_before_a_coin_can_be_spent_twice() {
    let flow = flow(
        json!({"nodes":[{"id":"a","type":"cetus_swap"},{"id":"b","type":"guardrail"},{"id":"c","type":"guardrail"}],"edges":[
        {"source":"a","sourceHandle":"coin_out","target":"b","targetHandle":"in"},
        {"source":"a","sourceHandle":"coin_out","target":"c","targetHandle":"in"}]}),
    );
    assert!(compile(&flow, &options(), &chain())
        .await
        .unwrap_err()
        .to_string()
        .contains("multiple consumers"));
}

#[tokio::test]
async fn swap_without_an_effective_floor_is_refused() {
    let flow = flow(
        json!({"nodes":[{"id":"swap","type":"cetus_swap","config":{"pool":"0x123","amount_in":"1000"}}],"edges":[]}),
    );
    assert!(compile(&flow, &options(), &chain())
        .await
        .unwrap_err()
        .to_string()
        .contains("floor"));
}

#[tokio::test]
async fn decimal_json_numbers_are_not_accepted_for_exact_order_prices() {
    let flow = flow(
        json!({"nodes":[{"id":"order","type":"deepbook_limit_order","config":{"price":0.1,"quantity":"1","depositSui":"0.1"}}],"edges":[]}),
    );
    assert!(compile(&flow, &options(), &chain())
        .await
        .unwrap_err()
        .to_string()
        .contains("price"));
}

fn wallet_chain() -> FakeSui {
    let c = shared(
        chain(),
        "0xa1",
        "0xa0::agent_wallet::AgentWallet<0x2::sui::SUI>",
    );
    let c = shared(c, "0xa2", "0xa0::version::Version");
    let c = shared(c, "0xb1", "0xb0::balance_manager::BalanceManager");
    let c = shared(
        c,
        rill_ptb::registry::pool(rill_ptb::registry::DeepBookNetwork::Testnet, "DEEP_SUI")
            .unwrap()
            .pool_id,
        "0xb0::pool::Pool<0x3::deep::DEEP, 0x2::sui::SUI>",
    );
    ["0xa3", "0xb2", "0xb3"].iter().fold(c, |c, id| {
        c.with_object(
            Some(&"0xc1".parse::<Address>().unwrap().to_string()),
            ObjectSummary {
                reference: ObjectRef {
                    id: id.parse::<Address>().unwrap().to_string(),
                    version: 9,
                    digest: sui_sdk_types::Digest::ZERO.to_string(),
                },
                object_type: Some("0xa0::agent_wallet::AgentCap".into()),
                fields: None,
                shared_initial_version: None,
            },
        )
    })
}
fn wallet_options(extra_rule: Option<serde_json::Value>) -> CompileOptions {
    let mut options = options();
    let mut rules = vec![
        json!({"kind":"budget","totalMist":"5000000000"}),
        json!({"kind":"per_tx","maxMist":"2000000000"}),
    ];
    if let Some(rule) = extra_rule {
        rules.push(rule);
    }
    options.sender = Some("0xc1".parse().unwrap());
    options.agent_wallet = Some(serde_json::from_value(json!({
        "packageId":"0xa0", "walletId":"0xa1", "versionId":"0xa2", "capId":"0xa3", "coinType":"0x2::sui::SUI",
        "capabilityManifest":{"walletCoinType":"0x2::sui::SUI","rules":rules}
    })).unwrap());
    options
}

#[tokio::test]
async fn protected_swap_has_no_coin_escape_and_checks_owner_binding() {
    use rill_server::studio_compile::ProtectedSwapInput;
    let flow = flow(json!({"nodes":[{"id":"swap","type":"cetus_swap","config":{
        "pool":"0x123","inputCoinType":"0x2::sui::SUI","amount_in":"1000000","min_amount_out":"100"
    }}],"edges":[]}));
    let mut options = wallet_options(None);
    options.agent_wallet.as_mut().unwrap().protected_swap = Some(ProtectedSwapInput {
        adapter_package_id: "0xd1".into(),
        revision: 1,
        owner: "0xc2".into(),
    });
    let chain = wallet_chain().with_object(
        None,
        ObjectSummary {
            reference: ObjectRef {
                id: "0xa1".parse::<Address>().unwrap().to_string(),
                version: 17,
                digest: sui_sdk_types::Digest::ZERO.to_string(),
            },
            object_type: Some("0xa0::agent_wallet::AgentWallet<0x2::sui::SUI>".into()),
            shared_initial_version: Some(7),
            fields: Some(json!({"owner":"0xc2","agent":"0xc1"})),
        },
    );
    let compiled = compile(&flow, &options, &chain).await.unwrap();
    let TransactionKind::ProgrammableTransaction(ptb) = compiled.transaction.kind else {
        panic!("PTB")
    };
    assert!(ptb
        .commands
        .iter()
        .all(|command| matches!(command, Command::MoveCall(_))));
    let functions: Vec<_> = ptb
        .commands
        .iter()
        .filter_map(|command| match command {
            Command::MoveCall(call) => Some(call.function.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        functions,
        ["request_spend", "prove", "prove", "execute_a_to_b"]
    );
    let mut guarded = serde_json::to_value(&flow).unwrap();
    guarded["nodes"].as_array_mut().unwrap().push(json!({"id":"floor","type":"guardrail","config":{"minValue":"200","coinType":"0x3::usdc::USDC"}}));
    guarded["edges"] =
        json!([{"source":"swap","sourceHandle":"coin_out","target":"floor","targetHandle":"in"}]);
    let built = compile(
        &serde_json::from_value(guarded.clone()).unwrap(),
        &options,
        &chain,
    )
    .await
    .unwrap();
    let TransactionKind::ProgrammableTransaction(guarded_ptb) = built.transaction.kind else {
        panic!("PTB")
    };
    let Command::MoveCall(call) = guarded_ptb.commands.last().unwrap() else {
        panic!("adapter")
    };
    let sui_sdk_types::Argument::Input(index) = call.arguments[3] else {
        panic!("floor input")
    };
    let sui_sdk_types::Input::Pure(value) = &guarded_ptb.inputs[index as usize] else {
        panic!("pure floor")
    };
    assert_eq!(bcs::from_bytes::<u64>(value).unwrap(), 200);
    for (field, value) in [("coinType", "0x2::sui::SUI"), ("minValue", "0")] {
        let mut rejected = guarded.clone();
        rejected["nodes"][1]["config"][field] = json!(value);
        assert!(
            compile(&serde_json::from_value(rejected).unwrap(), &options, &chain)
                .await
                .is_err()
        );
    }
    let mut rejected = guarded.clone();
    rejected["edges"][0]["sourceHandle"] = json!("residual");
    assert!(
        compile(&serde_json::from_value(rejected).unwrap(), &options, &chain)
            .await
            .is_err()
    );
    let mut rejected = guarded.clone();
    rejected["edges"] = json!([]);
    assert!(
        compile(&serde_json::from_value(rejected).unwrap(), &options, &chain)
            .await
            .is_err()
    );
    let mut rejected = guarded;
    rejected["nodes"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id":"extra","type":"guardrail","config":{"minValue":"1"}}));
    rejected["edges"].as_array_mut().unwrap().push(
        json!({"source":"floor","sourceHandle":"coin_out","target":"extra","targetHandle":"in"}),
    );
    assert!(
        compile(&serde_json::from_value(rejected).unwrap(), &options, &chain)
            .await
            .is_err()
    );
    options
        .agent_wallet
        .as_mut()
        .unwrap()
        .protected_swap
        .as_mut()
        .unwrap()
        .owner = "0xdead".into();
    assert!(compile(&flow, &options, &chain)
        .await
        .unwrap_err()
        .to_string()
        .contains("vault owner"));
}
#[tokio::test]
async fn wallet_stake_emits_hot_potato_before_staking() {
    let flow = flow(
        json!({"nodes":[{"id":"stake","type":"haedal_stake","config":{"amount":"1000000000"}}],"edges":[]}),
    );
    let result = compile(&flow, &wallet_options(None), &wallet_chain())
        .await
        .unwrap();
    let TransactionKind::ProgrammableTransaction(ptb) = result.transaction.kind else {
        panic!("PTB")
    };
    let functions = ptb
        .commands
        .iter()
        .filter_map(|c| match c {
            Command::MoveCall(c) => Some(c.function.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        functions,
        [
            "request_spend",
            "prove",
            "prove",
            "confirm_spend",
            "request_stake"
        ]
    );
}
#[tokio::test]
async fn manifest_protocol_scope_checks_real_compiled_targets() {
    let flow = flow(
        json!({"nodes":[{"id":"stake","type":"haedal_stake","config":{"amount":"1000000000"}}],"edges":[]}),
    );
    let options = wallet_options(Some(
        json!({"kind":"protocol_scope","allowedPackages":["0xdead"]}),
    ));
    assert!(compile(&flow, &options, &wallet_chain())
        .await
        .unwrap_err()
        .to_string()
        .contains("protocol_scope"));
}
/// An owner refused here has to know what to change: the action's amount or the limit.
#[tokio::test]
async fn a_spend_over_a_limit_names_the_amount_and_the_limit() {
    let flow = flow(
        json!({"nodes":[{"id":"stake","type":"haedal_stake","config":{"amount":"3000000000"}}],"edges":[]}),
    );
    let message = compile(&flow, &wallet_options(None), &wallet_chain())
        .await
        .unwrap_err()
        .to_string();
    assert!(message.contains("per_tx rule exceeded"), "{message}");
    assert!(message.contains("spends 3 SUI per run"), "{message}");
    assert!(
        message.contains("above the 2 SUI per-transaction limit"),
        "{message}"
    );

    let mut options = wallet_options(None);
    let wallet = options.agent_wallet.as_mut().unwrap();
    wallet.capability_manifest.rules =
        serde_json::from_value(json!([{"kind":"budget","totalMist":"500000000"}])).unwrap();
    let flow = flow_with_stake("1000000000");
    let message = compile(&flow, &options, &wallet_chain())
        .await
        .unwrap_err()
        .to_string();
    assert!(
        message.contains("spends 1 SUI per run, above the 0.5 SUI budget"),
        "{message}"
    );
}
fn flow_with_stake(amount: &str) -> FlowGraph {
    flow(
        json!({"nodes":[{"id":"stake","type":"haedal_stake","config":{"amount":amount}}],"edges":[]}),
    )
}
#[tokio::test]
async fn manifest_floor_cannot_be_bypassed_through_downstream_guard() {
    let flow = flow(json!({"nodes":[
        {"id":"swap","type":"cetus_swap","config":{"pool":"0x123","amount_in":"1000000000"}},
        {"id":"guard","type":"guardrail","config":{"minValue":"100"}}
    ],"edges":[{"source":"swap","sourceHandle":"coin_out","target":"guard","targetHandle":"in"}]}));
    let options = wallet_options(Some(json!({"kind":"slippage_floor","minOutMist":"101"})));
    assert!(compile(&flow, &options, &wallet_chain())
        .await
        .unwrap_err()
        .to_string()
        .contains("slippage_floor"));
}
/// A floor names the coin it is counted in. Against a swap that outputs that coin it binds, and
/// is reported in that coin's units; against a swap that outputs another coin it says nothing,
/// because 101 base units of SUI and of USDC are not the same amount.
#[tokio::test]
async fn a_floor_binds_only_swaps_that_output_its_coin() {
    let flow = flow(json!({"nodes":[
        {"id":"swap","type":"cetus_swap","config":{"pool":"0x123","amount_in":"1000000000","min_amount_out":"100"}}
    ],"edges":[]}));
    let floor = |coin: &str| {
        wallet_options(Some(
            json!({"kind":"slippage_floor","minOutMist":"101","coinType":coin}),
        ))
    };
    let refused = compile(&flow, &floor("0x3::usdc::USDC"), &wallet_chain())
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("slippage_floor"), "{refused}");
    assert!(
        refused.contains("swap swap accepts as little as"),
        "{refused}"
    );

    compile(&flow, &floor("0x2::sui::SUI"), &wallet_chain())
        .await
        .expect("a SUI floor says nothing about a swap that outputs USDC");
    let met = wallet_options(Some(
        json!({"kind":"slippage_floor","minOutMist":"100","coinType":"0x3::usdc::USDC"}),
    ));
    compile(&flow, &met, &wallet_chain())
        .await
        .expect("a floor the swap meets passes");
}
#[tokio::test]
async fn deepbook_price_preserves_the_exact_base_unit() {
    let flow = flow(
        json!({"nodes":[{"id":"order","type":"deepbook_limit_order","config":{
        "poolKey":"DEEP_SUI", "balanceManagerId":"0xb1", "tradeCapId":"0xb2", "depositCapId":"0xb3",
        "price":"0.123456789012", "quantity":"1", "depositSui":"1", "isBid":true
    }}],"edges":[]}),
    );
    let result = compile(&flow, &wallet_options(None), &wallet_chain())
        .await
        .unwrap();
    let TransactionKind::ProgrammableTransaction(ptb) = result.transaction.kind else {
        panic!("PTB")
    };
    let call = ptb
        .commands
        .iter()
        .find_map(|c| match c {
            Command::MoveCall(c) if c.function.as_str() == "place_limit_order" => Some(c),
            _ => None,
        })
        .unwrap();
    let sui_sdk_types::Argument::Input(index) = call.arguments[6] else {
        panic!("pure price input")
    };
    let sui_sdk_types::Input::Pure(bytes) = &ptb.inputs[index as usize] else {
        panic!("pure price")
    };
    assert_eq!(bcs::from_bytes::<u64>(bytes).unwrap(), 123456789012);
}
#[tokio::test]
async fn swap_cannot_feed_non_sui_into_haedal() {
    let flow = flow(json!({"nodes":[
        {"id":"swap","type":"cetus_swap","config":{"pool":"0x123","amount_in":"1000000000","min_amount_out":"100"}},
        {"id":"stake","type":"haedal_stake","config":{"amount":"1000000000"}}
    ],"edges":[{"source":"swap","sourceHandle":"coin_out","target":"stake","targetHandle":"sui_coin"}]}));
    assert!(compile(&flow, &options(), &chain())
        .await
        .unwrap_err()
        .to_string()
        .contains("Haedal input must be SUI"));
}

fn funded_chain() -> FakeSui {
    wallet_chain().with_object(
        Some(&"0xc1".parse::<Address>().unwrap().to_string()),
        ObjectSummary {
            reference: ObjectRef {
                id: "0xf1".parse::<Address>().unwrap().to_string(),
                version: 18,
                digest: sui_sdk_types::Digest::ZERO.to_string(),
            },
            object_type: Some("0x2::coin::Coin<0x2::sui::SUI>".into()),
            fields: Some(json!({"balance":"1000000000"})),
            shared_initial_version: None,
        },
    )
}
#[tokio::test]
async fn published_stake_builds_a_strict_funded_envelope_with_pinned_bytes() {
    let flow = flow(
        json!({"nodes":[{"id":"stake","type":"haedal_stake","config":{"amount":"1000000000"}}],"edges":[]}),
    );
    let result = rill_server::studio_compile::build_action(
        &flow,
        &wallet_options(None),
        "skill_stake",
        &funded_chain(),
        1_700_000_000_000,
    )
    .await
    .unwrap();
    result.validate_shape().unwrap();
    let policy = rill_policy::LocalPolicy {
        network: result.network,
        sender: result.sender.clone(),
        action_id: result.action_id.clone(),
        wallet_package_id: result.wallet_package_id.clone(),
        wallet_id: result.wallet_id.clone(),
        agent_cap_id: result.agent_cap_id.clone(),
        allowed_targets: result.allowed_targets.clone(),
        required_object_ids: result.required_object_ids.clone(),
        max_amount_base_units: 1_000_000_000,
        declared_spend_base_units: 1_000_000_000,
        minimum_remaining_base_units: 0,
        gas_ceiling_base_units: 100_000_000,
    };
    let validated = rill_policy::RawEnvelope::new(result.clone())
        .validate(&policy, 1_700_000_000_000)
        .unwrap();
    assert_eq!(validated.spend_base_units(), 1_000_000_000);
    validated.pin_bytes(&policy).unwrap();
    let decoded = rill_policy::decode::decode(&result.unsigned_ptb).unwrap();
    assert_eq!(decoded.targets, result.allowed_targets);
    let tx: sui_sdk_types::Transaction =
        bcs::from_bytes(&STANDARD.decode(&result.unsigned_ptb).unwrap()).unwrap();
    assert_eq!(
        tx.gas_payment.objects[0].object_id().to_string(),
        "0xf1".parse::<Address>().unwrap().to_string()
    );
    assert_eq!(
        result.simulation.verification,
        rill_core::envelope::Verification::Verified
    );
}
#[tokio::test]
async fn strict_failure_cannot_return_a_signable_envelope() {
    let flow = flow(
        json!({"nodes":[{"id":"stake","type":"haedal_stake","config":{"amount":"1000000000"}}],"edges":[]}),
    );
    let chain = funded_chain().with_simulation(rill_chain::fake::SimulationBehavior::Fails {
        error: "MoveAbort budget".into(),
    });
    assert!(rill_server::studio_compile::build_action(
        &flow,
        &wallet_options(None),
        "skill_stake",
        &chain,
        1_700_000_000_000
    )
    .await
    .unwrap_err()
    .to_string()
    .contains("simulation"));
}
#[tokio::test]
async fn strict_build_refuses_missing_sender_gas() {
    let flow = flow(
        json!({"nodes":[{"id":"stake","type":"haedal_stake","config":{"amount":"1000000000"}}],"edges":[]}),
    );
    assert!(rill_server::studio_compile::build_action(
        &flow,
        &wallet_options(None),
        "skill_stake",
        &wallet_chain(),
        1_700_000_000_000
    )
    .await
    .unwrap_err()
    .to_string()
    .contains("gas"));
}

#[tokio::test]
async fn legacy_ptb_marker_alone_cannot_publish_an_empty_action() {
    let flow = flow(json!({"nodes":[{"id":"ptb","type":"ptb"}],"edges":[]}));
    assert!(compile(&flow, &options(), &chain()).await.is_err());
}
#[tokio::test]
async fn zero_deepbook_price_cannot_create_a_zero_price_order() {
    let flow = flow(
        json!({"nodes":[{"id":"order","type":"deepbook_limit_order","config":{
        "poolKey":"DEEP_SUI", "balanceManagerId":"0xb1", "tradeCapId":"0xb2", "depositCapId":"0xb3",
        "price":"0", "quantity":"1", "depositSui":"1", "isBid":true
    }}],"edges":[]}),
    );
    assert!(compile(&flow, &wallet_options(None), &wallet_chain())
        .await
        .is_err());
}

#[tokio::test]
async fn inconclusive_simulation_cannot_return_a_signable_envelope() {
    let flow = flow(
        json!({"nodes":[{"id":"stake","type":"haedal_stake","config":{"amount":"1000000000"}}],"edges":[]}),
    );
    let chain = funded_chain().with_simulation(rill_chain::fake::SimulationBehavior::Fails {
        error: format!(
            "MoveAbort in {}::config::checked_package_version",
            rill_chain::CETUS_PACKAGE_IDS[0].1
        ),
    });
    assert!(rill_server::studio_compile::build_action(
        &flow,
        &wallet_options(None),
        "skill_stake",
        &chain,
        1_700_000_000_000
    )
    .await
    .unwrap_err()
    .to_string()
    .contains("unverified"));
}

#[tokio::test]
async fn configured_haedal_minimum_is_not_silently_lowered() {
    let flow = flow(
        json!({"nodes":[{"id":"stake","type":"haedal_stake","config":{"amount":"1000000000","minStakeMist":"2000000000"}}],"edges":[]}),
    );
    assert!(compile(&flow, &options(), &chain()).await.is_err());
}
#[tokio::test]
async fn configured_directional_sqrt_limit_is_preserved() {
    let flow = flow(
        json!({"nodes":[{"id":"swap","type":"cetus_swap","config":{"pool":"0x123","amount_in":"1000","min_amount_out":"1","minSqrtPrice":"5000000000"}}],"edges":[]}),
    );
    let result = compile(&flow, &options(), &chain()).await.unwrap();
    let TransactionKind::ProgrammableTransaction(ptb) = result.transaction.kind else {
        panic!("PTB")
    };
    let call = ptb
        .commands
        .iter()
        .find_map(|c| match c {
            Command::MoveCall(c) if c.function.as_str() == "swap" => Some(c),
            _ => None,
        })
        .unwrap();
    let sui_sdk_types::Argument::Input(index) = call.arguments[7] else {
        panic!("sqrt input")
    };
    let sui_sdk_types::Input::Pure(bytes) = &ptb.inputs[index as usize] else {
        panic!("pure sqrt")
    };
    assert_eq!(bcs::from_bytes::<u128>(bytes).unwrap(), 5_000_000_000);
}

#[tokio::test]
async fn paying_deep_fees_must_be_in_the_allowed_asset_scope() {
    use rill_ptb::registry::{self, DeepBookNetwork};
    let pool = registry::pool_spec(DeepBookNetwork::Testnet, "SUI_DBUSDC").unwrap();
    let c = shared(
        wallet_chain(),
        &pool.pool_id.to_string(),
        "0xb0::pool::Pool<0x2::sui::SUI,0x3::usdc::USDC>",
    );
    let options = wallet_options(Some(
        json!({"kind":"asset_scope","allowedCoinTypes":[pool.base_coin_type,pool.quote_coin_type]}),
    ));
    let f = flow(
        json!({"nodes":[{"id":"order","type":"deepbook_limit_order","config":{"poolKey":"SUI_DBUSDC","balanceManagerId":"0xb1","tradeCapId":"0xb2","depositCapId":"0xb3","price":"1","quantity":"1","depositSui":"1","isBid":false,"payWithDeep":true}}],"edges":[]}),
    );
    assert!(compile(&f, &options, &c)
        .await
        .unwrap_err()
        .to_string()
        .contains("asset_scope"));
}

#[tokio::test]
async fn published_actions_skip_gas_coins_deleted_since_the_owner_listing() {
    let owner = "0xc1".parse::<Address>().unwrap().to_string();
    let gas = |id: &str| ObjectSummary {
        reference: ObjectRef {
            id: id.into(),
            version: 1,
            digest: sui_sdk_types::Digest::ZERO.to_string(),
        },
        object_type: Some("0x2::coin::Coin<0x2::sui::SUI>".into()),
        fields: Some(json!({"balance":"1000000000"})),
        shared_initial_version: None,
    };
    let chain = wallet_chain()
        .with_object_after(100, Some(&owner), gas("0xc3"))
        .with_object(Some(&owner), gas("0xc4"));
    let flow = flow(
        json!({"nodes":[{"id":"stake","type":"haedal_stake","config":{"amount":"1000000000"}}],"edges":[]}),
    );
    let envelope = rill_server::studio_compile::build_action(
        &flow,
        &wallet_options(None),
        "skill_stake",
        &chain,
        1_700_000_000_000,
    )
    .await
    .unwrap();
    let transaction: sui_sdk_types::Transaction =
        bcs::from_bytes(&STANDARD.decode(envelope.unsigned_ptb).unwrap()).unwrap();
    assert_eq!(transaction.gas_payment.objects.len(), 1);
    assert_eq!(
        transaction.gas_payment.objects[0].object_id().to_string(),
        "0xc4".parse::<Address>().unwrap().to_string()
    );
}
