//! `rill spend` — the gated spend, which is the whole point of the wallet.
//!
//! `request_spend` mints a hot potato that cannot be dropped. Every attached rule's `prove` must
//! stamp a receipt on it, and `confirm_spend` counts them against the wallet's policy before
//! releasing a single coin. Miss one and the transaction aborts; the funds do not move.
//!
//! # The released coin must be consumed
//!
//! `confirm_spend` returns a `Coin<T>`, and a coin left unconsumed aborts execution with
//! `UnusedValueWithoutDrop`. So this transfers it — to a recipient if one is named, back to the
//! sender otherwise. There is no path here that builds a spend and forgets the coin.
//!
//! # Unless the owner listed the recipients
//!
//! A wallet whose owner set an on-chain recipient allowlist refuses `confirm_spend`, because a coin
//! handed back to the transaction could be sent anywhere. Its payouts end in `confirm_spend_to`,
//! which transfers inside Move and checks the recipient against the list. The list is read before
//! building, and a recipient that is not on it is refused here, but that refusal is only courtesy:
//! the contract is what enforces it.

use rill_chain::{grpc::GrpcSui, SuiRead, SuiWrite};
use rill_ptb::policy_read::{
    attached_modules, parse_recipient_allowlist, parse_type_names, policy_rules_transaction,
    recipient_allowlist_transaction,
};
use rill_ptb::shared::SharedObjects;
use rill_ptb::spend::{
    build_gated_spend_for_modules, build_gated_spend_to_for_modules, expected_spend_to_targets,
    WalletBinding,
};
use rill_ptb::transfer::transfer_coin;
use sui_sdk_types::{Address, Digest};
use sui_transaction_builder::{ObjectInput, TransactionBuilder};

use crate::keystore::Keystore;
use crate::verdict::{did_fail, no_verdict, submit_failed, would_fail, Failure};

pub struct SpendArgs {
    pub package_id: String,
    pub version_id: String,
    pub wallet_id: String,
    pub cap_id: String,
    /// Decimal SUI to release from the wallet.
    pub amount: String,
    /// Where the released coin goes. The sender when absent.
    pub recipient: Option<String>,
    pub gas_budget: u64,
    pub dry_run: bool,
}

/// The printing command. Everything it knows comes from [`spend_json`]; it only renders.
pub async fn spend(endpoint: &str, keystore: &Keystore, args: &SpendArgs) -> Result<(), String> {
    let result = spend_json(endpoint, keystore, args)
        .await
        .map_err(|failure| failure.to_string())?;
    for (label, key) in [
        ("wallet   ", "wallet"),
        ("amount   ", "amount"),
        ("recipient", "recipient"),
        ("rules    ", "rules"),
    ] {
        if let Some(value) = result.get(key) {
            println!("{label}: {}", render(value));
        }
    }
    if let Some(sequence) = result.get("callSequence").and_then(|s| s.as_array()) {
        println!("call sequence:");
        for target in sequence {
            println!("  {}", render(target));
        }
    }
    println!(
        "\nsimulation: ok=true gas={}",
        render(result.get("gasUsed").unwrap_or(&serde_json::Value::Null))
    );
    if args.dry_run {
        println!("\ndry run, nothing signed. Re-run with --submit.");
        return Ok(());
    }
    println!(
        "\ndigest  : {}",
        render(result.get("digest").unwrap_or(&serde_json::Value::Null))
    );
    println!("success : true");
    Ok(())
}

fn render(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(a) => a.iter().map(render).collect::<Vec<_>>().join(", "),
        other => other.to_string(),
    }
}

/// Build, simulate, sign, submit — and return what happened as structured data.
///
/// Returns `Err` for a refusal as well as a failure, with the rule named when the contract is what
/// refused. A caller that renders this for an agent must not present a refusal as an error to
/// retry: the amount has to change, or nothing will.
pub async fn spend_json(
    endpoint: &str,
    keystore: &Keystore,
    args: &SpendArgs,
) -> Result<serde_json::Value, Failure> {
    let chain = GrpcSui::new(endpoint).map_err(|e| e.to_string())?;
    spend_json_on(&chain, keystore, args).await
}

/// The spend itself, against any chain.
///
/// Split out so the whole path can be driven offline against [`rill_chain::fake::FakeSui`]: five
/// round trips, a rule list read from the wallet rather than assumed, a gas price that has to be
/// read, and a refusal that has to name the rule. Every one of those was reachable only through a
/// live testnet node and therefore not exercised in CI at all.
///
/// The client is created by the caller and used here, which keeps it inside the caller's runtime.
/// A tonic channel built in one runtime and used in another is already closed.
pub async fn spend_json_on(
    chain: &(impl SuiRead + SuiWrite),
    keystore: &Keystore,
    args: &SpendArgs,
) -> Result<serde_json::Value, Failure> {
    let sender = keystore.address();

    let wallet_id: Address = args
        .wallet_id
        .parse()
        .map_err(|_| format!("{} is not an address", args.wallet_id))?;
    let version_id: Address = args
        .version_id
        .parse()
        .map_err(|_| format!("{} is not an address", args.version_id))?;

    let mut shared = SharedObjects::new();
    for (label, id, raw) in [
        ("wallet", wallet_id, args.wallet_id.as_str()),
        ("version", version_id, args.version_id.as_str()),
    ] {
        let summary = chain
            .get_object(raw)
            .await
            .map_err(|e| format!("reading the {label} object: {e}"))?;
        shared.insert(
            id,
            summary
                .shared_initial_version
                .ok_or_else(|| format!("the {label} object {raw} is not shared"))?,
        );
    }

    // The capability is owned, so it enters by reference — id, version, digest, all current.
    let cap = chain
        .get_object(&args.cap_id)
        .await
        .map_err(|e| format!("reading the AgentCap: {e}"))?;

    let gas = rill_chain::gas::sui_gas_coins(chain, &sender.to_string())
        .await
        .map_err(|e| format!("listing the sender's gas coins: {e}"))?;
    if gas.is_empty() {
        return Err(Failure::Failed(format!(
            "{sender} holds no SUI to pay for this"
        )));
    }

    // Read, not assumed. Testnet answers 1000 and mainnet answers 100, so a literal that is right
    // on one network is ten times the price on the other, and a price below the reference is
    // rejected outright rather than merely running slow. Read once and used twice: the node
    // refuses a read priced below the reference exactly as it refuses a submission.
    let gas_price = chain
        .reference_gas_price()
        .await
        .map_err(|e| format!("reading the reference gas price: {e}"))?;

    // The prove list must name exactly the rules this wallet carries — not what a flag says. Too
    // many aborts inside df::borrow_mut with no code of its own; too few aborts at the last
    // command, after everything else has passed.
    let read_tx = policy_rules_transaction(
        args.package_id
            .parse()
            .map_err(|_| format!("{} is not an address", args.package_id))?,
        wallet_id,
        "0x2::sui::SUI",
        &shared,
        gas_price,
    )
    .map_err(|e| e.to_string())?;
    let read_b64 = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .encode(bcs::to_bytes(&read_tx).map_err(|e| e.to_string())?)
    };
    let read = chain
        .simulate_read(&read_b64)
        .await
        .map_err(|e| format!("reading the wallet's rules: {e}"))?;
    let names = read
        .command_returns
        .iter()
        .flatten()
        .next()
        .ok_or("the wallet did not report its rules")
        .and_then(|b| parse_type_names(b).map_err(|_| "the rule list did not decode"))?;
    let modules: Vec<String> = attached_modules(&names)
        .into_iter()
        .map(str::to_owned)
        .collect();
    if modules.len() != names.len() {
        return Err(Failure::Failed(format!(
            "this wallet carries {} rule(s), and only {} of them have an emitter here: {names:?}\n\
             Every attached rule must be proved, so this spend cannot be built.",
            names.len(),
            modules.len()
        )));
    }

    let package_id: Address = args
        .package_id
        .parse()
        .map_err(|_| format!("{} is not an address", args.package_id))?;
    let allowlist_tx =
        recipient_allowlist_transaction(package_id, wallet_id, "0x2::sui::SUI", &shared, gas_price)
            .map_err(|e| e.to_string())?;
    let allowlist_b64 = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .encode(bcs::to_bytes(&allowlist_tx).map_err(|e| e.to_string())?)
    };
    let allowlist = allowlist_from_read(
        &chain
            .simulate_read(&allowlist_b64)
            .await
            .map_err(|e| format!("reading the wallet's recipient allowlist: {e}"))?,
    )?;
    let to = match &args.recipient {
        Some(r) => Some(r.parse().map_err(|_| format!("{r} is not an address"))?),
        None => None,
    };
    let payout = payout(allowlist.as_deref(), to, sender)?;

    let amount_mist = rill_core::amounts::decimal_to_base_units(&args.amount, 9)
        .map_err(|e| format!("the amount: {e}"))?;

    let mut tx = TransactionBuilder::new();
    tx.set_sender(sender);
    tx.set_gas_budget(rill_chain::gas::affordable_budget(args.gas_budget, &gas));
    tx.set_gas_price(gas_price);
    tx.add_gas_objects(gas.iter().map(|c| {
        ObjectInput::owned(
            c.reference.id.parse().expect("an id from the chain"),
            c.reference.version,
            c.reference.digest.parse::<Digest>().expect("a digest"),
        )
    }));

    let binding = WalletBinding {
        package_id: args
            .package_id
            .parse()
            .map_err(|_| format!("{} is not an address", args.package_id))?,
        wallet_id,
        cap: ObjectInput::owned(
            cap.reference.id.parse().expect("an id from the chain"),
            cap.reference.version,
            cap.reference.digest.parse::<Digest>().expect("a digest"),
        ),
        version_id,
        coin_type: "0x2::sui::SUI".into(),
        // Only used for the targets projection, which this path no longer takes — the rules come
        // from the chain above.
        manifest: rill_core::manifest::CapabilityManifest {
            wallet_coin_type: "0x2::sui::SUI".into(),
            rules: Vec::new(),
        },
    };

    let module_refs: Vec<&str> = modules.iter().map(String::as_str).collect();
    let (recipient, call_sequence) = match payout {
        Payout::Transfer(recipient) => {
            let coin = build_gated_spend_for_modules(
                &mut tx,
                &binding,
                amount_mist,
                &module_refs,
                &shared,
            )
            .map_err(|e| e.to_string())?;
            // The released coin, consumed. See the module note.
            transfer_coin(&mut tx, coin, recipient);
            let sequence: Vec<String> = std::iter::once(format!(
                "{}::agent_wallet::request_spend",
                binding.package_id
            ))
            .chain(
                modules
                    .iter()
                    .map(|m| format!("{}::{m}::prove", binding.package_id)),
            )
            .chain(std::iter::once(format!(
                "{}::agent_wallet::confirm_spend",
                binding.package_id
            )))
            .collect();
            (recipient, sequence)
        }
        Payout::SpendTo(recipient) => {
            build_gated_spend_to_for_modules(
                &mut tx,
                &binding,
                amount_mist,
                &module_refs,
                recipient,
                &shared,
            )
            .map_err(|e| e.to_string())?;
            (
                recipient,
                expected_spend_to_targets(binding.package_id, &module_refs),
            )
        }
    };

    let built = tx.try_build().map_err(|e| format!("compiling: {e}"))?;
    let b64 = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .encode(bcs::to_bytes(&built).map_err(|e| e.to_string())?)
    };

    let outcome = chain.simulate(&b64).await.map_err(no_verdict)?;

    if !outcome.ok {
        // A rule refusing is the wallet working. Saying so is the difference between a caller who
        // trusts the limits and one who thinks the tool is broken — and between an agent that
        // changes the amount and one that retries the same call forever. The naming lives in
        // `verdict` now, because create and attach need exactly the same answer and used to give a
        // quoted abort instead.
        return Err(would_fail(outcome.error));
    }

    let mut result = serde_json::json!({
        "wallet": wallet_id.to_string(),
        "amount": format!("{} SUI ({amount_mist} mist)", args.amount),
        "recipient": recipient.to_string(),
        "rules": modules,
        "rulesSource": "read from chain",
        "callSequence": call_sequence,
        "gasUsed": outcome.gas_used_mist,
        "submitted": false,
    });
    if allowlist.is_some() {
        result["recipientAllowlist"] = serde_json::Value::String("on-chain".into());
    }

    if args.dry_run {
        result["note"] = serde_json::Value::String(
            "Simulated only. Nothing was signed and nothing was submitted.".into(),
        );
        return Ok(result);
    }

    let signature = keystore.sign(&built).map_err(|e| e.to_string())?;
    let outcome = chain
        .execute(&b64, &[signature.to_base64()])
        .await
        .map_err(submit_failed)?;

    if let Some(error) = &outcome.error {
        return Err(did_fail(error));
    }

    result["submitted"] = serde_json::Value::Bool(true);
    result["digest"] = serde_json::Value::String(outcome.digest.clone());
    result["gasUsed"] = serde_json::json!(outcome.gas_used_mist);
    result["balanceChanges"] = serde_json::json!(outcome
        .balance_changes
        .iter()
        .map(|d| serde_json::json!({ "amount": d.amount, "coinType": d.coin_type }))
        .collect::<Vec<_>>());
    result["note"] = serde_json::Value::String(
        "Submitted and confirmed. This cannot be undone, and calling again sends a second payment."
            .into(),
    );
    Ok(result)
}

/// Where a released coin goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Payout {
    /// `confirm_spend`, then a `TransferObjects` this signer builds.
    Transfer(Address),
    /// `confirm_spend_to`: the contract transfers, to an owner-listed address only.
    SpendTo(Address),
}

/// Decide the payout from the wallet's allowlist (`None` when it has none) and the caller's `to`.
///
/// With a list there is no default. The signer is the agent, and paying the agent is a destination
/// the owner did not choose unless they listed it.
fn payout(
    allowlist: Option<&[Address]>,
    to: Option<Address>,
    sender: Address,
) -> Result<Payout, String> {
    let Some(list) = allowlist else {
        return Ok(Payout::Transfer(to.unwrap_or(sender)));
    };
    let named = if list.is_empty() {
        "no recipients: its owner emptied the list, so it pays out to nobody".to_string()
    } else {
        format!(
            "owner-approved recipients only: [{}]",
            list.iter()
                .map(Address::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    match to {
        None => Err(format!(
            "this wallet pays out to {named}. Name one of them with `to`; a spend without one \
             would pay the agent, which the owner did not list."
        )),
        Some(to) if list.contains(&to) => Ok(Payout::SpendTo(to)),
        Some(to) => Err(format!(
            "{to} is not a recipient this wallet may pay. It pays out to {named}. Only the \
             wallet's owner can change that list, on chain. Do not retry with another address."
        )),
    }
}

/// The allowlist, from a simulation of `agent_wallet::recipient_allowlist`.
///
/// Only a package that has no `recipient_allowlist` view is taken as no list: the view exists from
/// package version 3, and a package without it has no `confirm_spend` that could honour a list
/// either. Any other failed read (a node error, an abort, an unknown reason) says nothing about the
/// list and is refused, as is a read that ran and answered something undecodable.
pub(crate) fn allowlist_from_read(
    outcome: &rill_chain::SimulationOutcome,
) -> Result<Option<Vec<Address>>, String> {
    if !outcome.ok {
        let error = outcome.error.as_deref().unwrap_or("no reason given");
        let lower = error.to_ascii_lowercase();
        let no_such_view = [
            "functionnotfound",
            "function not found",
            "linker_error",
            "linkererror",
        ]
        .iter()
        .any(|marker| lower.contains(marker));
        return if no_such_view {
            Ok(None)
        } else {
            Err(format!(
                "the recipient allowlist could not be read: {error}"
            ))
        };
    }
    outcome
        .command_returns
        .iter()
        .flatten()
        .next()
        .ok_or_else(|| "the wallet did not report its recipient allowlist".to_string())
        .and_then(|b| {
            parse_recipient_allowlist(b)
                .map_err(|_| "the recipient allowlist did not decode".to_string())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rill_chain::{SimulationOutcome, Verification};

    fn a(n: u8) -> Address {
        format!("0x{n:064x}").parse().unwrap()
    }

    #[test]
    fn without_a_list_the_coin_goes_where_it_always_did() {
        assert_eq!(payout(None, None, a(0xb)), Ok(Payout::Transfer(a(0xb))));
        assert_eq!(
            payout(None, Some(a(0xe)), a(0xb)),
            Ok(Payout::Transfer(a(0xe)))
        );
    }

    /// Defaulting to the signer would pay the agent itself, which is exactly the destination an
    /// owner who set a list did not choose.
    #[test]
    fn with_a_list_and_no_recipient_the_spend_is_refused_naming_the_list() {
        let refusal = payout(Some(&[a(0xd)]), None, a(0xb)).unwrap_err();
        assert!(refusal.contains("owner-approved recipients"), "{refusal}");
        assert!(refusal.contains(&a(0xd).to_string()), "{refusal}");
        assert!(refusal.contains("`to`"), "{refusal}");
    }

    #[test]
    fn with_a_list_an_unlisted_recipient_is_refused_and_only_the_owner_can_change_it() {
        let refusal = payout(Some(&[a(0xd)]), Some(a(0xe)), a(0xb)).unwrap_err();
        assert!(refusal.contains(&a(0xd).to_string()), "{refusal}");
        assert!(refusal.contains(&a(0xe).to_string()), "{refusal}");
        assert!(refusal.contains("Only the wallet's owner"), "{refusal}");
        assert!(refusal.contains("Do not retry"), "{refusal}");
    }

    #[test]
    fn with_a_list_a_listed_recipient_is_paid_inside_move() {
        assert_eq!(
            payout(Some(&[a(0xd)]), Some(a(0xd)), a(0xb)),
            Ok(Payout::SpendTo(a(0xd)))
        );
    }

    #[test]
    fn an_emptied_list_still_pays_nobody() {
        let refusal = payout(Some(&[]), Some(a(0xd)), a(0xb)).unwrap_err();
        assert!(refusal.contains("no recipients"), "{refusal}");
    }

    fn read(ok: bool, returns: Vec<Vec<Vec<u8>>>) -> SimulationOutcome {
        SimulationOutcome {
            ok,
            verification: Verification::Verified,
            error: None,
            gas_used_mist: 0,
            balance_changes: Vec::new(),
            command_output_count: returns.len(),
            command_returns: returns,
        }
    }

    /// A package older than v3 has no `recipient_allowlist` view, and no `confirm_spend` that could
    /// honour a list either, so a read that does not run means no list to route around. The chain
    /// still enforces: on a v3 wallet with a list, `confirm_spend` aborts 20.
    #[test]
    fn a_read_the_package_cannot_answer_means_no_list() {
        for missing in [
            "FunctionNotFound in command 0",
            "LINKER_ERROR: Cannot find ModuleId ... recipient_allowlist",
        ] {
            let mut outcome = read(false, vec![]);
            outcome.error = Some(missing.into());
            assert_eq!(allowlist_from_read(&outcome), Ok(None), "{missing}");
        }
    }

    /// Any other failure (a node error, a wallet that does not exist, an abort) says nothing about
    /// the list, so it must not be taken for "no list": a spend would then skip the owner's list
    /// on the signer's side and be refused on chain only by luck of the route.
    #[test]
    fn any_other_failed_read_is_refused_not_taken_for_no_list() {
        for other in [
            None,
            Some("MoveAbort in 0x2::dynamic_field, code 1"),
            Some("timeout"),
        ] {
            let mut outcome = read(false, vec![]);
            outcome.error = other.map(str::to_owned);
            assert!(allowlist_from_read(&outcome).is_err(), "{other:?}");
        }
    }

    #[test]
    fn a_read_that_answers_is_decoded_strictly() {
        assert_eq!(
            allowlist_from_read(&read(true, vec![vec![vec![0]]])),
            Ok(None)
        );
        let mut listed = vec![1, 1];
        listed.extend_from_slice(a(0xd).as_bytes());
        assert_eq!(
            allowlist_from_read(&read(true, vec![vec![listed]])),
            Ok(Some(vec![a(0xd)]))
        );
        assert!(allowlist_from_read(&read(true, vec![vec![vec![2]]])).is_err());
        assert!(allowlist_from_read(&read(true, vec![])).is_err());
    }
}
