use base64::{engine::general_purpose::STANDARD, Engine};
use rill_chain::{fake::FakeSui, ObjectRef, ObjectSummary};
use rill_core::envelope::Network;
use rill_ptb::deployments;
use rill_server::studio_setup::{attach_plan, prepare_plan, SetupContext};
use rill_store::PublishedSkill;
use serde_json::{json, Value};
use sui_sdk_types::{Address, Command, TransactionKind};

fn addr(s: &str) -> String {
    s.parse::<Address>().unwrap().to_string()
}
fn context() -> SetupContext {
    SetupContext {
        wallet_package_id: None,
        wallet_version_id: None,
        wallet_type_package: None,
        deepbook_type_packages: None,
        network: Network::Testnet,
        guard_package: Some(deployments::TESTNET_RILL_GUARD.parse().unwrap()),
        now_ms: 1000,
    }
}
fn body() -> Value {
    json!({"skillId":"skill_stake","sender":"0x1","agent":"0x2","budgetMist":"5000000000","perTxMist":"1000000000","expiresAtMs":"1000000","walletId":"0x10","agentCapId":"0x11"})
}
fn skill() -> PublishedSkill {
    PublishedSkill {
        id: "skill_stake".into(),
        name: "Stake".into(),
        description: "Stake".into(),
        flow: json!({"nodes":[{"id":"stake","type":"haedal_stake","config":{"amount":"1000000000"}}],"edges":[],"capabilityManifest":{"walletCoinType":"0x2::sui::SUI","rules":[{"kind":"budget","totalMist":"5000000000"},{"kind":"per_tx","maxMist":"1000000000"}]}}),
        tool_defs: None,
        policy_id: None,
        owner: Some(addr("0x1")),
        created_at: "2026-10-04T00:00:00Z".into(),
    }
}
fn object(chain: FakeSui, id: &str, kind: &str, fields: Value, owner: Option<&str>) -> FakeSui {
    chain.with_object(
        owner,
        ObjectSummary {
            reference: ObjectRef {
                id: addr(id),
                version: 17,
                digest: sui_sdk_types::Digest::ZERO.to_string(),
            },
            object_type: Some(kind.into()),
            fields: Some(fields),
            shared_initial_version: if owner.is_none() { Some(7) } else { None },
        },
    )
}
fn chain(balance: &str, owner: &str, agent: &str) -> FakeSui {
    chain_seeing_wallet_after(0, balance, owner, agent)
}

#[tokio::test]
async fn single_swap_runtime_default_fits_the_owner_allocation() {
    let mut published = skill();
    published.flow["nodes"] = json!([{"id":"swap","type":"cetus_swap","config":{
        "pool":"0x123","amount_in":"100000000","inputCoinType":"0x2::sui::SUI","min_amount_out":"1"
    }}]);
    let mut input = body();
    input["budgetMist"] = json!("7500000");
    input["perTxMist"] = json!("5000000");
    let c = object(
        chain("0", "0x1", "0x2"),
        "0x123",
        "0x3::pool::Pool<0x2::sui::SUI,0x3::usdc::USDC>",
        json!({}),
        None,
    );
    let c = object(
        c,
        deployments::TESTNET_CETUS_GLOBAL_CONFIG,
        "0x3::config::GlobalConfig",
        json!({}),
        None,
    );
    let result = attach_plan(&input, &published, &addr("0x1"), &context(), &c)
        .await
        .unwrap();
    assert_eq!(result["runSet"]["declaredSpendBaseUnits"], "5000000");
    assert_eq!(
        result["buildArguments"]["params"]["swap"]["amount_in"],
        "5000000"
    );
    assert_eq!(
        published.flow["nodes"][0]["config"]["amount_in"],
        "100000000"
    );
    assert_eq!(published.flow["nodes"][0]["config"]["min_amount_out"], "1");
}
fn chain_listing_cap_after(listings: usize) -> FakeSui {
    lagging_chain(0, listings, "0", "0x1", "0x2")
}
/// The same chain, with a node that answers "not found" for the wallet `reads` times first: the
/// owner's create landed through another fullnode and this one has not indexed it yet.
fn chain_seeing_wallet_after(reads: usize, balance: &str, owner: &str, agent: &str) -> FakeSui {
    lagging_chain(reads, 0, balance, owner, agent)
}
fn lagging_chain(
    wallet_reads: usize,
    cap_listings: usize,
    balance: &str,
    owner: &str,
    agent: &str,
) -> FakeSui {
    let reads = wallet_reads;
    let chain = object(
        FakeSui::new(),
        deployments::TESTNET_AGENT_WALLET_VERSION,
        &format!("{}::version::Version", deployments::TESTNET_AGENT_WALLET),
        json!({}),
        None,
    );
    let chain = chain.with_object_after(
        reads,
        None,
        ObjectSummary {
            reference: ObjectRef {
                id: addr("0x10"),
                version: 17,
                digest: sui_sdk_types::Digest::ZERO.to_string(),
            },
            object_type: Some(format!(
                "{}::agent_wallet::AgentWallet<0x2::sui::SUI>",
                deployments::TESTNET_AGENT_WALLET
            )),
            fields: Some(
                json!({"owner":owner,"agent":agent,"cap_id":"0x11","budget":balance,"spent":"0","policy":{"rules":{"contents":[]}},"revoked":false,"expires_at_ms":"1000000"}),
            ),
            shared_initial_version: Some(7),
        },
    );
    let chain = chain.with_listing_after(
        cap_listings,
        &addr(agent),
        ObjectSummary {
            reference: ObjectRef {
                id: addr("0x11"),
                version: 17,
                digest: sui_sdk_types::Digest::ZERO.to_string(),
            },
            object_type: Some(format!(
                "{}::agent_wallet::AgentCap",
                deployments::TESTNET_AGENT_WALLET
            )),
            fields: Some(json!({"wallet":"0x10"})),
            shared_initial_version: None,
        },
    );
    let chain = object(
        chain,
        deployments::TESTNET_HAEDAL_STAKING,
        "0x3::staking::Staking",
        json!({}),
        None,
    );
    object(
        chain,
        "0x5",
        "0x3::sui_system::SuiSystemState",
        json!({}),
        None,
    )
}
fn commands(result: &Value, key: &str) -> Vec<Command> {
    let kind: TransactionKind =
        bcs::from_bytes(&STANDARD.decode(result[key].as_str().unwrap()).unwrap()).unwrap();
    let TransactionKind::ProgrammableTransaction(ptb) = kind else {
        panic!("PTB")
    };
    ptb.commands
}
#[tokio::test]
async fn prepare_creates_an_empty_wallet_with_version() {
    let result = prepare_plan(
        &body(),
        &skill(),
        &addr("0x1"),
        &context(),
        &chain("0", "0x1", "0x2"),
    )
    .await
    .unwrap();
    let commands = commands(&result, "setupPtb");
    assert!(matches!(&commands[0],Command::MoveCall(c) if c.function.as_str()=="zero"));
    assert!(
        matches!(&commands[1],Command::MoveCall(c) if c.function.as_str()=="create_wallet" && c.arguments.len()==4)
    );
    assert!(!commands.iter().any(|c| matches!(c, Command::SplitCoins(_))));
}
/// Found by the end-to-end run on testnet: the attach request arrived before this server's node had
/// seen the wallet the owner had just created, and was refused as "not found on chain".
#[tokio::test]
async fn attach_waits_for_a_wallet_the_node_has_not_indexed_yet() {
    let result = attach_plan(
        &body(),
        &skill(),
        &addr("0x1"),
        &context(),
        &chain_seeing_wallet_after(3, "0", "0x1", "0x2"),
    )
    .await
    .expect("a wallet that appears within the wait is attached");
    assert!(result["attachPtb"].is_string());
}
/// The next lag the same run met: the wallet was readable, the agent's capability not yet in the
/// agent's owner listing.
#[tokio::test]
async fn attach_waits_for_a_capability_the_owner_index_has_not_listed_yet() {
    attach_plan(
        &body(),
        &skill(),
        &addr("0x1"),
        &context(),
        &chain_listing_cap_after(3),
    )
    .await
    .expect("a capability listed within the wait is accepted");
}
#[tokio::test]
async fn attach_places_all_rules_before_funding_and_exports_exact_runset() {
    let result = attach_plan(
        &body(),
        &skill(),
        &addr("0x1"),
        &context(),
        &chain("0", "0x1", "0x2"),
    )
    .await
    .unwrap();
    let commands = commands(&result, "attachPtb");
    assert!(
        matches!(&commands[0],Command::MoveCall(c) if c.module.as_str()=="budget" && c.function.as_str()=="add")
    );
    assert!(
        matches!(&commands[1],Command::MoveCall(c) if c.module.as_str()=="per_tx" && c.function.as_str()=="add")
    );
    assert!(
        matches!(commands.last().unwrap(),Command::MoveCall(c) if c.function.as_str()=="top_up")
    );
    let runset = &result["runSet"];
    let typed: rill_cli::runset::RunSet = serde_json::from_value(runset.clone()).unwrap();
    assert_eq!(
        typed.to_policy().unwrap().declared_spend_base_units,
        1_000_000_000
    );
    assert_eq!(result["buildArguments"]["actionId"], "skill_stake");
    assert_eq!(
        result["buildArguments"]["agentWallet"]["capId"],
        addr("0x11")
    );
    let mut keys = runset
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    keys.sort();
    let mut expected = vec![
        "label",
        "network",
        "sender",
        "actionId",
        "walletPackageId",
        "walletId",
        "agentCapId",
        "versionId",
        "capabilityManifest",
        "allowedTargets",
        "allowedObjectIds",
        "maxAmountBaseUnits",
        "declaredSpendBaseUnits",
        "minimumRemainingBaseUnits",
        "gasCeilingBaseUnits",
    ];
    expected.sort();
    assert_eq!(keys, expected);
    assert_eq!(runset["declaredSpendBaseUnits"], "1000000000");
    assert!(runset["allowedTargets"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t.as_str().unwrap().ends_with("::interface::request_stake")));
}
#[tokio::test]
async fn attach_rejects_wrong_owner_agent_and_already_funded_wallet() {
    for (balance, owner, agent, reason) in [
        ("1", "0x1", "0x2", "funded"),
        ("0", "0x3", "0x2", "owner"),
        ("0", "0x1", "0x3", "agent"),
    ] {
        let error = attach_plan(
            &body(),
            &skill(),
            &addr("0x1"),
            &context(),
            &chain(balance, owner, agent),
        )
        .await
        .unwrap_err();
        assert!(error.contains(reason), "{error}");
    }
}
#[tokio::test]
async fn prepare_rejects_widened_published_limits_and_mismatched_session() {
    let mut body = body();
    body["budgetMist"] = json!("5000000001");
    assert!(prepare_plan(
        &body,
        &skill(),
        &addr("0x1"),
        &context(),
        &chain("0", "0x1", "0x2")
    )
    .await
    .unwrap_err()
    .contains("budget"));
    assert!(prepare_plan(
        &body,
        &skill(),
        &addr("0x2"),
        &context(),
        &chain("0", "0x1", "0x2")
    )
    .await
    .unwrap_err()
    .contains("sender"));
}

#[tokio::test]
async fn deepbook_setup_mints_both_caps_before_sharing_and_never_funds() {
    let mut skill = skill();
    skill.flow["nodes"] = json!([{"id":"order","type":"deepbook_limit_order","config":{"poolKey":"SUI_DBUSDC","price":"1.25","quantity":"1","depositSui":"1","isBid":false}}]);
    let result = prepare_plan(
        &body(),
        &skill,
        &addr("0x1"),
        &context(),
        &chain("0", "0x1", "0x2"),
    )
    .await
    .unwrap();
    assert_eq!(result["requiresTradeCap"], true);
    let commands = commands(&result, "setupPtb");
    let targets = commands
        .iter()
        .filter_map(|c| {
            if let Command::MoveCall(call) = c {
                Some(call.function.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    let share = commands
        .iter()
        .find_map(|command| match command {
            Command::MoveCall(call) if call.function.as_str() == "public_share_object" => {
                Some(call)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(
        share.type_arguments[0].to_string(),
        format!(
            "{}::balance_manager::BalanceManager",
            deployments::TESTNET_DEEPBOOK_MANAGER_TYPE_PACKAGE
        )
    );
    assert_eq!(
        targets,
        vec![
            "zero",
            "create_wallet",
            "new",
            "mint_trade_cap",
            "mint_deposit_cap",
            "public_share_object"
        ]
    );
    assert!(!commands.iter().any(|c| matches!(c, Command::SplitCoins(_))));
}

#[tokio::test]
async fn prepare_preserves_other_rules_and_keeps_u64_limits_as_strings() {
    let mut skill = skill();
    skill.flow["capabilityManifest"]["rules"] = json!([
        {"kind":"budget","totalMist":"18446744073709551615"},
        {"kind":"per_tx","maxMist":"18446744073709551615"},
        {"kind":"rate_limit","windowMs":"60000","maxMist":"1000000000"},
        {"kind":"recipient_allowlist","addresses":[addr("0x2")]}
    ]);
    let mut body = body();
    body["budgetMist"] = json!("18446744073709551615");
    let result = prepare_plan(
        &body,
        &skill,
        &addr("0x1"),
        &context(),
        &chain("0", "0x1", "0x2"),
    )
    .await
    .unwrap();
    assert_eq!(
        result["capabilityManifest"]["rules"][0]["totalMist"],
        "18446744073709551615"
    );
    assert_eq!(
        result["capabilityManifest"]["rules"][2],
        skill.flow["capabilityManifest"]["rules"][2]
    );
    assert_eq!(
        result["capabilityManifest"]["rules"][3],
        skill.flow["capabilityManifest"]["rules"][3]
    );
}

#[tokio::test]
async fn attach_rejects_rotated_cap_revoked_expired_or_already_configured_wallet() {
    for (key, value, reason) in [
        ("cap_id", json!("0x12"), "active cap"),
        ("revoked", json!(true), "revoked"),
        ("expires_at_ms", json!("1000"), "expired"),
        (
            "policy",
            json!({"rules":{"contents":["budget"]}}),
            "attached rules",
        ),
    ] {
        let mut fields = json!({"owner":"0x1","agent":"0x2","cap_id":"0x11","budget":"0","spent":"0","expires_at_ms":"1000000","revoked":false,"policy":{"rules":{"contents":[]}}});
        fields[key] = value;
        let chain = object(
            chain("0", "0x1", "0x2"),
            "0x10",
            &format!(
                "{}::agent_wallet::AgentWallet<0x2::sui::SUI>",
                deployments::TESTNET_AGENT_WALLET
            ),
            fields,
            None,
        );
        let error = attach_plan(&body(), &skill(), &addr("0x1"), &context(), &chain)
            .await
            .unwrap_err();
        assert!(error.contains(reason), "{error}");
    }
}

#[tokio::test]
#[ignore = "read-only testnet object schema verification"]
async fn live_deepbook_capability_fields_match_setup_validation() {
    use rill_chain::SuiRead;
    let chain = rill_chain::grpc::GrpcSui::new("https://fullnode.testnet.sui.io:443").unwrap();
    let manager = "0xd817b421de7a65e054160faf3063460ff85d2c76536aa7f6ef8865a7fd12dfe5";
    let mut observed = Vec::new();
    let mut expected = Vec::new();
    for (id, kind, defining_package) in [
        (
            manager,
            "BalanceManager",
            deployments::TESTNET_DEEPBOOK_MANAGER_TYPE_PACKAGE,
        ),
        (
            "0xf1e2693c1b5d78c2768faa89c7947320e905f9b34b2fd9ed44295b500608d5ac",
            "TradeCap",
            deployments::TESTNET_DEEPBOOK_TRADE_CAP_TYPE_PACKAGE,
        ),
        (
            "0x954fbdd985a13f18ebc871e0434becb2c1bc74ae341a2e5066abb578e9774ad7",
            "DepositCap",
            deployments::TESTNET_DEEPBOOK_DEPOSIT_CAP_TYPE_PACKAGE,
        ),
    ] {
        let object = chain.get_object(id).await.unwrap();
        let fields = object.fields.as_ref().unwrap();
        let binding = if kind == "BalanceManager" {
            fields["owner"].as_str().map(|_| "owner present".to_owned())
        } else {
            fields["balance_manager_id"].as_str().map(str::to_owned)
        };
        observed.push((kind, object.object_type, binding));
        expected.push((
            kind,
            Some(format!("{defining_package}::balance_manager::{kind}")),
            Some(if kind == "BalanceManager" {
                "owner present".into()
            } else {
                manager.into()
            }),
        ));
    }
    assert_eq!(observed, expected);
}

#[tokio::test]
async fn deepbook_attach_pins_the_full_order_and_exports_runtime_bindings() {
    let mut skill = skill();
    skill.flow["nodes"] = json!([{"id":"order","type":"deepbook_limit_order","config":{"poolKey":"DEEP_SUI","price":"0.01","quantity":"1","depositSui":"1","isBid":true}}]);
    let mut body = body();
    body["price"] = json!("0.123456789012");
    body["balanceManagerId"] = json!("0x20");
    body["tradeCapId"] = json!("0x21");
    body["depositCapId"] = json!("0x22");
    let package = deployments::TESTNET_DEEPBOOK_MANAGER_TYPE_PACKAGE;
    let deposit_package = deployments::TESTNET_DEEPBOOK_DEPOSIT_CAP_TYPE_PACKAGE;
    let chain = object(
        chain("0", "0x1", "0x2"),
        "0x20",
        &format!("{package}::balance_manager::BalanceManager"),
        json!({"owner":"0x1"}),
        None,
    );
    let chain = object(
        chain,
        "0x21",
        &format!("{package}::balance_manager::TradeCap"),
        json!({"balance_manager_id":"0x20"}),
        Some(&addr("0x2")),
    );
    let chain = object(
        chain,
        "0x22",
        &format!("{deposit_package}::balance_manager::DepositCap"),
        json!({"balance_manager_id":"0x20"}),
        Some(&addr("0x2")),
    );
    let pool =
        rill_ptb::registry::pool_spec(rill_ptb::registry::DeepBookNetwork::Testnet, "DEEP_SUI")
            .unwrap();
    let chain = object(
        chain,
        &pool.pool_id.to_string(),
        "0x3::pool::Pool",
        json!({}),
        None,
    );
    let chain = object(chain, "0x6", "0x2::clock::Clock", json!({}), None);
    prepare_plan(&body, &skill, &addr("0x1"), &context(), &chain)
        .await
        .unwrap();
    let result = attach_plan(&body, &skill, &addr("0x1"), &context(), &chain)
        .await
        .unwrap();
    let _: rill_cli::runset::RunSet = serde_json::from_value(result["runSet"].clone()).unwrap();
    let args = &result["buildArguments"];
    assert_eq!(args["params"]["order"]["price"], "0.123456789012");
    assert_eq!(args["params"]["order"]["depositCapId"], addr("0x22"));
    assert!(result["runSet"]["allowedTargets"]
        .as_array()
        .unwrap()
        .iter()
        .any(|target| target
            .as_str()
            .unwrap()
            .ends_with("::pool::place_limit_order")));
    for id in ["0x20", "0x21", "0x22"] {
        assert!(result["runSet"]["allowedObjectIds"]
            .as_array()
            .unwrap()
            .contains(&json!(addr(id))));
    }
    let bad = object(
        chain,
        "0x22",
        &format!("{deposit_package}::balance_manager::DepositCap"),
        json!({"balance_manager_id":"0x23"}),
        Some(&addr("0x2")),
    );
    assert!(attach_plan(&body, &skill, &addr("0x1"), &context(), &bad)
        .await
        .unwrap_err()
        .contains("another balance manager"));
}

#[tokio::test]
async fn mainnet_setup_uses_explicit_wallet_deployment() {
    let mut ctx = context();
    ctx.network = Network::Mainnet;
    ctx.wallet_package_id = Some("0x123".into());
    ctx.wallet_version_id = Some("0x456".into());
    ctx.wallet_type_package = Some("0x123".into());
    let c = object(
        chain("0", "0x1", "0x2"),
        "0x456",
        "0x123::version::Version",
        json!({}),
        None,
    );
    let result = prepare_plan(&body(), &skill(), &addr("0x1"), &ctx, &c)
        .await
        .unwrap();
    assert_eq!(result["walletPackageId"], addr("0x123"));
    assert_eq!(result["versionId"], addr("0x456"));
    assert_eq!(
        result["deepbookPackageId"],
        rill_ptb::registry::MAINNET_PACKAGE_ID
    );
}

#[test]
fn mainnet_never_falls_back_to_testnet_wallet_ids() {
    let (package, version) = deployments::wallet_deployment(Network::Mainnet, None, None)
        .expect("mainnet defaults to its own published pair");
    assert_eq!(package.to_string(), deployments::MAINNET_AGENT_WALLET);
    assert_eq!(
        version.to_string(),
        deployments::MAINNET_AGENT_WALLET_VERSION
    );
    assert!(
        deployments::wallet_deployment(
            Network::Mainnet,
            Some(deployments::MAINNET_AGENT_WALLET),
            None
        )
        .is_err(),
        "half a pair is refused rather than completed"
    );
    assert!(deployments::wallet_deployment(
        Network::Mainnet,
        Some(deployments::TESTNET_AGENT_WALLET),
        Some(deployments::TESTNET_AGENT_WALLET_VERSION)
    )
    .is_err());
}
