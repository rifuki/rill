//! What a fullnode actually says about gas.
//!
//! Nothing here asserts a value a network can change under it — it prints what the node answers, so
//! a claim about the gRPC surface comes from a run rather than from a reading of the proto. This is
//! what caught the hardcoded gas price: testnet answers 1000 and mainnet answers 100.
//!
use sui_rpc::client::Client;
use sui_rpc::proto::sui::rpc::v2::{
    simulate_transaction_request::TransactionChecks, GetEpochRequest, ListOwnedObjectsRequest,
    SimulateTransactionRequest,
};
use sui_sdk_types::{Address, Digest};
use sui_transaction_builder::{ObjectInput, TransactionBuilder};

const TESTNET: &str = "https://fullnode.testnet.sui.io:443";
const MAINNET: &str = "https://fullnode.mainnet.sui.io:443";

const SUI_COIN_TYPE: &str = "0x0000000000000000000000000000000000000000000000000000000000000002::coin::Coin<0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI>";

/// Q1 — where the reference gas price actually comes from over gRPC.
#[tokio::test]
#[ignore = "requires network access"]
async fn reference_gas_price_from_get_epoch() {
    for endpoint in [TESTNET, MAINNET] {
        let mut client = Client::new(endpoint).expect("client");
        let mut request = GetEpochRequest::default();
        // epoch left None = the current epoch.
        request.read_mask = Some(prost_types::FieldMask {
            paths: vec!["epoch".into(), "reference_gas_price".into()],
        });
        let response = client
            .ledger_client()
            .get_epoch(request)
            .await
            .expect("get_epoch")
            .into_inner();
        let epoch = response.epoch.expect("an epoch");
        println!(
            "{endpoint}\n  epoch               : {:?}\n  reference_gas_price : {:?}",
            epoch.epoch, epoch.reference_gas_price
        );
        assert!(
            epoch.reference_gas_price.is_some(),
            "the mask asked for it, so it must come back"
        );
    }
}

/// Q2 — the pagination and server-side type filter on ListOwnedObjects.
#[tokio::test]
#[ignore = "requires network access"]
async fn list_owned_objects_paginates_and_filters_server_side() {
    let sender = std::env::var("RILL_SPIKE_ADDRESS")
        .expect("set RILL_SPIKE_ADDRESS to an address that owns SUI");
    let mut client = Client::new(TESTNET).expect("client");

    let mut page_token = None;
    let mut pages = 0;
    let mut coins = 0;
    loop {
        let mut request = ListOwnedObjectsRequest::default();
        request.owner = Some(sender.clone());
        request.page_size = Some(1); // deliberately tiny, to force the token path
        request.page_token = page_token.clone();
        request.object_type = Some(SUI_COIN_TYPE.to_owned());
        request.read_mask = Some(prost_types::FieldMask {
            paths: vec![
                "object_id".into(),
                "version".into(),
                "digest".into(),
                "object_type".into(),
                "balance".into(),
            ],
        });
        let response = client
            .state_client()
            .list_owned_objects(request)
            .await
            .expect("list_owned_objects")
            .into_inner();
        pages += 1;
        for object in &response.objects {
            coins += 1;
            println!(
                "  page {pages}: {} v{} balance={} type={:?}",
                object.object_id(),
                object.version(),
                object.balance(),
                object.object_type
            );
            assert_eq!(
                object.object_type.as_deref(),
                Some(SUI_COIN_TYPE),
                "the server-side object_type filter must be exact"
            );
        }
        page_token = response.next_page_token;
        if page_token.is_none() || pages >= 5 {
            break;
        }
    }
    println!("pages walked: {pages}, coins seen: {coins}");
    assert!(coins > 0, "the spike address should own SUI");
}

/// Q3 — what `do_gas_selection` returns, and whether it removes the need to pick coins.
#[tokio::test]
#[ignore = "requires network access"]
async fn do_gas_selection_reports_the_payment_it_chose() {
    let sender_str = std::env::var("RILL_SPIKE_ADDRESS")
        .expect("set RILL_SPIKE_ADDRESS to an address that owns SUI");
    let sender: Address = sender_str.parse().expect("address");
    let mut client = Client::new(TESTNET).expect("client");

    // A transaction with an EMPTY gas payment: built against a placeholder, then cleared, which is
    // the same trick `policy_read::policy_rules_transaction` uses.
    let mut tx = TransactionBuilder::new();
    tx.set_sender(sender);
    tx.set_gas_budget(10_000_000);
    tx.set_gas_price(1_000); // deliberately possibly-wrong, to see what comes back
    tx.add_gas_objects([ObjectInput::owned(Address::ZERO, 1, Digest::ZERO)]);
    let amount = tx.pure(&1_000u64);
    let gas = tx.gas();
    let split = tx.split_coins(gas, vec![amount]);
    let recipient = tx.pure(&sender);
    tx.transfer_objects(split, recipient);
    let mut built = tx.try_build().expect("build");
    built.gas_payment.objects.clear();

    let bytes = bcs::to_bytes(&built).expect("bcs");
    let mut transaction = sui_rpc::proto::sui::rpc::v2::Transaction::default();
    transaction.bcs = Some(bytes.into());

    let mut request = SimulateTransactionRequest::default();
    request.transaction = Some(transaction);
    request.checks = Some(TransactionChecks::Enabled as i32);
    request.do_gas_selection = Some(true);
    request.read_mask = Some(prost_types::FieldMask {
        paths: vec![
            "transaction".into(),
            "command_outputs".into(),
            "suggested_gas_price".into(),
        ],
    });

    let response = client
        .execution_client()
        .simulate_transaction(request)
        .await
        .expect("simulate_transaction")
        .into_inner();

    println!("suggested_gas_price: {:?}", response.suggested_gas_price);
    let executed = response.transaction.as_ref().expect("a transaction back");
    println!("digest             : {:?}", executed.digest);
    match executed
        .transaction
        .as_ref()
        .and_then(|t| t.gas_payment.as_ref())
    {
        Some(payment) => {
            println!("gas_payment.owner  : {:?}", payment.owner);
            println!("gas_payment.price  : {:?}", payment.price);
            println!("gas_payment.budget : {:?}", payment.budget);
            println!("gas_payment.objects: {}", payment.objects.len());
            for object in &payment.objects {
                println!(
                    "  {} v{:?} {:?}",
                    object.object_id(),
                    object.version,
                    object.digest
                );
            }
        }
        None => println!("gas_payment        : ABSENT — do_gas_selection returned no payment"),
    }
    let status = executed.effects.as_ref().and_then(|e| e.status.as_ref());
    println!("status             : {status:?}");
}

/// The control: the same simulation WITHOUT `do_gas_selection`, so the difference is attributable.
#[tokio::test]
#[ignore = "requires network access"]
async fn simulation_without_gas_selection_for_comparison() {
    let sender_str = std::env::var("RILL_SPIKE_ADDRESS")
        .expect("set RILL_SPIKE_ADDRESS to an address that owns SUI");
    let sender: Address = sender_str.parse().expect("address");
    let mut client = Client::new(TESTNET).expect("client");

    let mut tx = TransactionBuilder::new();
    tx.set_sender(sender);
    tx.set_gas_budget(10_000_000);
    tx.set_gas_price(1_000);
    tx.add_gas_objects([ObjectInput::owned(Address::ZERO, 1, Digest::ZERO)]);
    let amount = tx.pure(&1_000u64);
    let gas = tx.gas();
    let split = tx.split_coins(gas, vec![amount]);
    let recipient = tx.pure(&sender);
    tx.transfer_objects(split, recipient);
    let mut built = tx.try_build().expect("build");
    built.gas_payment.objects.clear();

    let bytes = bcs::to_bytes(&built).expect("bcs");
    let mut transaction = sui_rpc::proto::sui::rpc::v2::Transaction::default();
    transaction.bcs = Some(bytes.into());

    let mut request = SimulateTransactionRequest::default();
    request.transaction = Some(transaction);
    request.checks = Some(TransactionChecks::Enabled as i32);
    request.read_mask = Some(prost_types::FieldMask {
        paths: vec!["transaction".into(), "suggested_gas_price".into()],
    });

    match client
        .execution_client()
        .simulate_transaction(request)
        .await
    {
        Ok(response) => {
            let response = response.into_inner();
            println!("suggested_gas_price: {:?}", response.suggested_gas_price);
            let payment = response
                .transaction
                .as_ref()
                .and_then(|e| e.transaction.as_ref())
                .and_then(|t| t.gas_payment.as_ref());
            println!("gas_payment        : {payment:?}");
            let status = response
                .transaction
                .as_ref()
                .and_then(|e| e.effects.as_ref())
                .and_then(|e| e.status.as_ref());
            println!("status             : {status:?}");
        }
        Err(status) => println!("gRPC status: {:?} {}", status.code(), status.message()),
    }
}

/// A gas price BELOW the network's RGP, submitted for simulation, to see what the node says. This
/// is the failure the hardcoded `set_gas_price(1_000)` would produce if RGP ever moved above it.
#[tokio::test]
#[ignore = "requires network access"]
async fn a_gas_price_below_rgp_is_rejected() {
    let sender_str = std::env::var("RILL_SPIKE_ADDRESS")
        .expect("set RILL_SPIKE_ADDRESS to an address that owns SUI");
    let sender: Address = sender_str.parse().expect("address");
    let mut client = Client::new(TESTNET).expect("client");

    let mut list = ListOwnedObjectsRequest::default();
    list.owner = Some(sender_str.clone());
    list.page_size = Some(10);
    list.object_type = Some(SUI_COIN_TYPE.to_owned());
    list.read_mask = Some(prost_types::FieldMask {
        paths: vec![
            "object_id".into(),
            "version".into(),
            "digest".into(),
            "balance".into(),
        ],
    });
    let owned = client
        .state_client()
        .list_owned_objects(list)
        .await
        .expect("list")
        .into_inner();
    let coin = owned.objects.first().expect("a SUI coin");

    for price in [1u64, 999, 1_000] {
        let mut tx = TransactionBuilder::new();
        tx.set_sender(sender);
        tx.set_gas_budget(10_000_000);
        tx.set_gas_price(price);
        tx.add_gas_objects([ObjectInput::owned(
            coin.object_id().parse().unwrap(),
            coin.version(),
            coin.digest().parse().unwrap(),
        )]);
        let amount = tx.pure(&1_000u64);
        let gas = tx.gas();
        let split = tx.split_coins(gas, vec![amount]);
        let recipient = tx.pure(&sender);
        tx.transfer_objects(split, recipient);
        let built = tx.try_build().expect("build");

        let mut transaction = sui_rpc::proto::sui::rpc::v2::Transaction::default();
        transaction.bcs = Some(bcs::to_bytes(&built).expect("bcs").into());
        let mut request = SimulateTransactionRequest::default();
        request.transaction = Some(transaction);
        request.checks = Some(TransactionChecks::Enabled as i32);

        match client
            .execution_client()
            .simulate_transaction(request)
            .await
        {
            Ok(response) => {
                let response = response.into_inner();
                let status = response
                    .transaction
                    .as_ref()
                    .and_then(|t| t.effects.as_ref())
                    .and_then(|e| e.status.as_ref());
                println!("price {price:5}: Ok, status={status:?}");
            }
            Err(status) => println!(
                "price {price:5}: {:?} — {}",
                status.code(),
                status.message()
            ),
        }
    }
}

/// Q2 follow-up — the shape of a truncated page. An address with N objects asked for `page_size`
/// N-1 answers with N-1 objects **and** a `next_page_token`; the remainder is only reachable
/// through that token. This is the same shape a 50-object page has on an address holding more.
#[tokio::test]
#[ignore = "requires network access"]
async fn a_short_page_returns_a_token_and_the_rest_is_invisible_without_it() {
    let sender = std::env::var("RILL_SPIKE_ADDRESS").expect("set RILL_SPIKE_ADDRESS");
    let mut client = Client::new(TESTNET).expect("client");

    // First: how many objects does this address actually own?
    let mut all = ListOwnedObjectsRequest::default();
    all.owner = Some(sender.clone());
    all.page_size = Some(1000);
    all.read_mask = Some(prost_types::FieldMask {
        paths: vec!["object_id".into(), "object_type".into()],
    });
    let full = client
        .state_client()
        .list_owned_objects(all)
        .await
        .expect("list")
        .into_inner();
    println!(
        "owned in one page of 1000: {} (next_page_token={:?})",
        full.objects.len(),
        full.next_page_token.is_some()
    );
    assert!(
        full.objects.len() >= 2,
        "need at least 2 objects to truncate"
    );

    // Now the truncated page.
    let short = (full.objects.len() - 1) as u32;
    let mut request = ListOwnedObjectsRequest::default();
    request.owner = Some(sender.clone());
    request.page_size = Some(short);
    request.read_mask = Some(prost_types::FieldMask {
        paths: vec!["object_id".into(), "object_type".into()],
    });
    let page = client
        .state_client()
        .list_owned_objects(request)
        .await
        .expect("list")
        .into_inner();
    println!(
        "page_size={short}: objects={} next_page_token={:?}",
        page.objects.len(),
        page.next_page_token.as_ref().map(|t| t.len())
    );
    assert_eq!(page.objects.len(), short as usize);
    assert!(
        page.next_page_token.is_some(),
        "a truncated page MUST carry a token — dropping it silently loses the remainder"
    );

    // And the remainder, only via the token.
    let mut rest = ListOwnedObjectsRequest::default();
    rest.owner = Some(sender.clone());
    rest.page_size = Some(1000);
    rest.page_token = page.next_page_token.clone();
    rest.read_mask = Some(prost_types::FieldMask {
        paths: vec!["object_id".into(), "object_type".into()],
    });
    let rest = client
        .state_client()
        .list_owned_objects(rest)
        .await
        .expect("list")
        .into_inner();
    println!("after the token: {} more object(s)", rest.objects.len());
    assert_eq!(page.objects.len() + rest.objects.len(), full.objects.len());
}

/// Q3 follow-up — does `do_gas_selection` OVERRIDE the price and budget in the submitted bytes, or
/// echo them back? Testnet RGP happens to equal the hardcoded 1_000, so the earlier run could not
/// tell the two apart. This one sends values that are unmistakably not the network's.
#[tokio::test]
#[ignore = "requires network access"]
async fn does_gas_selection_override_price_and_budget_or_echo_them() {
    let sender_str = std::env::var("RILL_SPIKE_ADDRESS").expect("set RILL_SPIKE_ADDRESS");
    let sender: Address = sender_str.parse().expect("address");
    let mut client = Client::new(TESTNET).expect("client");

    for (price, budget) in [
        (1_000u64, 10_000_000u64),
        (1_500, 10_000_000),
        (1_000, 1_000_000),
        (1_000, 2_000_000),
    ] {
        let mut tx = TransactionBuilder::new();
        tx.set_sender(sender);
        tx.set_gas_budget(budget);
        tx.set_gas_price(price);
        tx.add_gas_objects([ObjectInput::owned(Address::ZERO, 1, Digest::ZERO)]);
        let amount = tx.pure(&1_000u64);
        let gas = tx.gas();
        let split = tx.split_coins(gas, vec![amount]);
        let recipient = tx.pure(&sender);
        tx.transfer_objects(split, recipient);
        let mut built = tx.try_build().expect("build");
        built.gas_payment.objects.clear();

        let mut transaction = sui_rpc::proto::sui::rpc::v2::Transaction::default();
        transaction.bcs = Some(bcs::to_bytes(&built).expect("bcs").into());
        let mut request = SimulateTransactionRequest::default();
        request.transaction = Some(transaction);
        request.checks = Some(TransactionChecks::Enabled as i32);
        request.do_gas_selection = Some(true);
        request.read_mask = Some(prost_types::FieldMask {
            paths: vec![
                "transaction.transaction".into(),
                "transaction.effects".into(),
                "suggested_gas_price".into(),
            ],
        });

        match client
            .execution_client()
            .simulate_transaction(request)
            .await
        {
            Ok(response) => {
                let response = response.into_inner();
                let executed = response.transaction.as_ref();
                let payment = executed
                    .and_then(|e| e.transaction.as_ref())
                    .and_then(|t| t.gas_payment.as_ref());
                let gas_used = executed
                    .and_then(|e| e.effects.as_ref())
                    .and_then(|e| e.gas_used.as_ref())
                    .map(|g| (g.computation_cost(), g.storage_cost(), g.storage_rebate()));
                let inner = executed.and_then(|e| e.transaction.as_ref());
                let status = executed
                    .and_then(|e| e.effects.as_ref())
                    .and_then(|e| e.status.as_ref())
                    .map(|s| {
                        (
                            s.success,
                            s.error.as_ref().and_then(|e| e.description.clone()),
                        )
                    });
                println!(
                    "sent price={price} budget={budget}\n  back price={:?} budget={:?} objects={} suggested={:?}\n  gas_used(comp,store,rebate)={gas_used:?}\n  status={status:?}\n  tx.bcs present={} bytes={:?}  tx.digest={:?}  executed.digest={:?}",
                    payment.and_then(|p| p.price),
                    payment.and_then(|p| p.budget),
                    payment.map(|p| p.objects.len()).unwrap_or(0),
                    response.suggested_gas_price,
                    inner.and_then(|t| t.bcs.as_ref()).is_some(),
                    inner.and_then(|t| t.bcs.as_ref()).and_then(|b| b.value.as_ref()).map(|v| v.len()),
                    inner.and_then(|t| t.digest.clone()),
                    executed.and_then(|e| e.digest.clone()),
                );
            }
            Err(status) => println!(
                "sent price={price} budget={budget}\n  gRPC {:?}: {}",
                status.code(),
                status.message()
            ),
        }
    }
}

/// Q3 follow-up — is the transaction that comes back from `do_gas_selection` the SIGNABLE one?
/// If `transaction.transaction.bcs` decodes to a `Transaction` whose `gas_payment.objects` is
/// populated, then the node has handed back exactly the bytes to sign, and no client-side coin
/// picking is needed at all.
#[tokio::test]
#[ignore = "requires network access"]
async fn the_bcs_returned_by_gas_selection_carries_the_chosen_coins() {
    let sender_str = std::env::var("RILL_SPIKE_ADDRESS").expect("set RILL_SPIKE_ADDRESS");
    let sender: Address = sender_str.parse().expect("address");
    let mut client = Client::new(TESTNET).expect("client");

    let mut tx = TransactionBuilder::new();
    tx.set_sender(sender);
    tx.set_gas_budget(10_000_000);
    tx.set_gas_price(1_000);
    tx.add_gas_objects([ObjectInput::owned(Address::ZERO, 1, Digest::ZERO)]);
    let amount = tx.pure(&1_000u64);
    let gas = tx.gas();
    let split = tx.split_coins(gas, vec![amount]);
    let recipient = tx.pure(&sender);
    tx.transfer_objects(split, recipient);
    let mut built = tx.try_build().expect("build");
    built.gas_payment.objects.clear();
    let sent_bytes = bcs::to_bytes(&built).expect("bcs");
    println!("sent    : {} bytes, gas objects = 0", sent_bytes.len());

    let mut transaction = sui_rpc::proto::sui::rpc::v2::Transaction::default();
    transaction.bcs = Some(sent_bytes.clone().into());
    let mut request = SimulateTransactionRequest::default();
    request.transaction = Some(transaction);
    request.checks = Some(TransactionChecks::Enabled as i32);
    request.do_gas_selection = Some(true);
    request.read_mask = Some(prost_types::FieldMask {
        paths: vec!["transaction.transaction.bcs".into()],
    });

    let response = client
        .execution_client()
        .simulate_transaction(request)
        .await
        .expect("simulate")
        .into_inner();

    let bcs_back = response
        .transaction
        .as_ref()
        .and_then(|e| e.transaction.as_ref())
        .and_then(|t| t.bcs.as_ref())
        .and_then(|b| b.value.as_ref())
        .expect("bcs back");
    println!("returned: {} bytes", bcs_back.len());
    assert_ne!(
        bcs_back.as_ref(),
        sent_bytes.as_slice(),
        "if the bytes were identical the node changed nothing"
    );

    let decoded: sui_sdk_types::Transaction =
        bcs::from_bytes(bcs_back).expect("the returned bcs must decode as a Transaction");
    println!("decoded gas_payment.owner : {}", decoded.gas_payment.owner);
    println!("decoded gas_payment.price : {}", decoded.gas_payment.price);
    println!("decoded gas_payment.budget: {}", decoded.gas_payment.budget);
    for object in &decoded.gas_payment.objects {
        println!(
            "  object {} v{} {}",
            object.object_id(),
            object.version(),
            object.digest()
        );
    }
    assert!(
        !decoded.gas_payment.objects.is_empty(),
        "gas selection must have filled the payment"
    );
}

/// Q2 follow-up — the ORDER `ListOwnedObjects` returns objects in, one page at a time. Which
/// objects fall off the end of an unpaginated 50-item page depends entirely on this.
#[tokio::test]
#[ignore = "requires network access"]
async fn what_order_does_list_owned_objects_return() {
    let sender = std::env::var("RILL_SPIKE_ADDRESS").expect("set RILL_SPIKE_ADDRESS");
    let mut client = Client::new(TESTNET).expect("client");
    let mut page_token = None;
    let mut n = 0;
    loop {
        let mut request = ListOwnedObjectsRequest::default();
        request.owner = Some(sender.clone());
        request.page_size = Some(1);
        request.page_token = page_token.clone();
        request.read_mask = Some(prost_types::FieldMask {
            paths: vec!["object_id".into(), "object_type".into()],
        });
        let page = client
            .state_client()
            .list_owned_objects(request)
            .await
            .expect("list")
            .into_inner();
        for object in &page.objects {
            n += 1;
            println!("{n:>3}. {} {:?}", object.object_id(), object.object_type);
        }
        page_token = page.next_page_token;
        if page_token.is_none() || n > 200 {
            break;
        }
    }
    println!("total: {n}");
}

/// Q2 — what `GrpcSui::list_owned_objects` (the production path) actually sees on an address that
/// owns more than one page of objects, against a full paginated walk of the same address.
#[tokio::test]
#[ignore = "requires network access"]
async fn the_production_lister_against_the_truth() {
    use rill_chain::SuiRead as _;
    let sender = std::env::var("RILL_SPIKE_ADDRESS").expect("set RILL_SPIKE_ADDRESS");

    // The truth: every page, followed to the end.
    let mut client = Client::new(TESTNET).expect("client");
    let mut page_token = None;
    let mut all: Vec<(String, String)> = Vec::new();
    loop {
        let mut request = ListOwnedObjectsRequest::default();
        request.owner = Some(sender.clone());
        request.page_size = Some(1000);
        request.page_token = page_token.clone();
        request.read_mask = Some(prost_types::FieldMask {
            paths: vec!["object_id".into(), "object_type".into()],
        });
        let page = client
            .state_client()
            .list_owned_objects(request)
            .await
            .expect("list")
            .into_inner();
        for object in &page.objects {
            all.push((
                object.object_id().to_owned(),
                object.object_type().to_owned(),
            ));
        }
        page_token = page.next_page_token;
        if page_token.is_none() {
            break;
        }
    }
    let truth_sui = all.iter().filter(|(_, t)| t == SUI_COIN_TYPE).count();
    println!(
        "full walk        : {} objects, {truth_sui} of them SUI coins",
        all.len()
    );
    println!(
        "first type       : {}",
        all.first().map(|(_, t)| t.as_str()).unwrap_or("-")
    );
    println!(
        "last  type       : {}",
        all.last().map(|(_, t)| t.as_str()).unwrap_or("-")
    );
    let sorted_by_type: Vec<&String> = all.iter().map(|(_, t)| t).collect();
    let mut expected = sorted_by_type.clone();
    expected.sort();
    println!("ordered by type? : {}", sorted_by_type == expected);

    // What the production path sees.
    let chain = rill_chain::grpc::GrpcSui::new(TESTNET).expect("client");
    let seen = chain.list_owned_objects(&sender).await.expect("list");
    let seen_sui = seen
        .iter()
        .filter(|o| o.object_type.as_deref() == Some(SUI_COIN_TYPE))
        .count();
    println!(
        "production path  : {} objects, {seen_sui} of them SUI coins",
        seen.len()
    );
    println!(
        "invisible        : {} object(s), {} SUI coin(s)",
        all.len() - seen.len(),
        truth_sui - seen_sui
    );
}

/// Does `do_gas_selection` make a keyless read WORSE? `simulate_read` sets it, and gas selection
/// needs the sender to actually hold SUI. A sender that holds nothing is exactly the keyless case
/// the doc comment describes.
#[tokio::test]
#[ignore = "requires network access"]
async fn a_penniless_sender_read_with_and_without_gas_selection() {
    let mut client = Client::new(TESTNET).expect("client");
    // An address that holds nothing.
    for (label, sender) in [
        (
            "0x0 (the sender policy_rules_transaction actually uses)",
            Address::ZERO,
        ),
        (
            "0x..ff (an address nobody funds)",
            "0x00000000000000000000000000000000000000000000000000000000000000ff"
                .parse::<Address>()
                .unwrap(),
        ),
    ] {
        println!("--- {label}");
        for selection in [false, true] {
            let mut tx = TransactionBuilder::new();
            tx.set_sender(sender);
            tx.set_gas_budget(10_000_000);
            tx.set_gas_price(1_000);
            tx.add_gas_objects([ObjectInput::owned(Address::ZERO, 1, Digest::ZERO)]);
            // A pure read: no owned inputs at all.
            let a = tx.pure(&1u64);
            let b = tx.pure(&2u64);
            tx.move_call(
                sui_transaction_builder::Function::new(
                    "0x0000000000000000000000000000000000000000000000000000000000000001"
                        .parse()
                        .unwrap(),
                    sui_sdk_types::Identifier::new("u64").unwrap(),
                    sui_sdk_types::Identifier::new("max").unwrap(),
                ),
                vec![a, b],
            );
            let mut built = tx.try_build().expect("build");
            built.gas_payment.objects.clear();

            let mut transaction = sui_rpc::proto::sui::rpc::v2::Transaction::default();
            transaction.bcs = Some(bcs::to_bytes(&built).expect("bcs").into());
            let mut request = SimulateTransactionRequest::default();
            request.transaction = Some(transaction);
            request.checks = Some(TransactionChecks::Enabled as i32);
            if selection {
                request.do_gas_selection = Some(true);
            }

            match client
                .execution_client()
                .simulate_transaction(request)
                .await
            {
                Ok(response) => {
                    let response = response.into_inner();
                    let status = response
                        .transaction
                        .as_ref()
                        .and_then(|e| e.effects.as_ref())
                        .and_then(|e| e.status.as_ref())
                        .map(|s| s.success);
                    println!(
                        "do_gas_selection={selection:<5} OK  success={status:?} outputs={}",
                        response.command_outputs.len()
                    );
                }
                Err(status) => println!(
                    "do_gas_selection={selection:<5} ERR {:?}: {}",
                    status.code(),
                    status.message()
                ),
            }
        }
    }
}

/// How much SUI does `0x0` — the sender every keyless read in this repo uses — actually hold?
#[tokio::test]
#[ignore = "requires network access"]
async fn what_does_0x0_hold() {
    use sui_rpc::proto::sui::rpc::v2::GetBalanceRequest;
    for endpoint in [TESTNET, MAINNET] {
        let mut client = Client::new(endpoint).expect("client");
        let mut request = GetBalanceRequest::default();
        request.owner = Some(Address::ZERO.to_string());
        request.coin_type = Some("0x2::sui::SUI".to_owned());
        match client.state_client().get_balance(request).await {
            Ok(response) => println!(
                "{endpoint}  0x0 SUI balance = {:?}",
                response.into_inner().balance.and_then(|b| b.balance)
            ),
            Err(status) => println!("{endpoint}  ERR {:?}: {}", status.code(), status.message()),
        }
    }
}

/// Q1 — is the read_mask actually required for `reference_gas_price`, or is it returned anyway?
#[tokio::test]
#[ignore = "requires network access"]
async fn get_epoch_needs_the_mask_to_return_the_rgp() {
    let mut client = Client::new(TESTNET).expect("client");
    for mask in [
        None,
        Some(vec!["epoch"]),
        Some(vec!["reference_gas_price"]),
        Some(vec!["epoch", "reference_gas_price"]),
        Some(vec!["*"]),
    ] {
        let mut request = GetEpochRequest::default();
        request.read_mask = mask.as_ref().map(|paths| prost_types::FieldMask {
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
        });
        match client.ledger_client().get_epoch(request).await {
            Ok(response) => {
                let epoch = response.into_inner().epoch.unwrap_or_default();
                println!(
                    "mask={:<40?} epoch={:?} reference_gas_price={:?}",
                    mask, epoch.epoch, epoch.reference_gas_price
                );
            }
            Err(status) => println!(
                "mask={mask:<40?} ERR {:?}: {}",
                status.code(),
                status.message()
            ),
        }
    }
}
