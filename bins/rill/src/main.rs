//! `rill` — one binary, every local job.
//!
//! It holds the key, trusts no bytes from the server without independent inspection, and signs only
//! an envelope that has passed every state transition in `rill-policy`.
//!
//! # Why subcommands, and why the bare command is not one of them
//!
//! An agent runs `rill mcp` and speaks JSON-RPC over stdin and stdout. A person runs `rill` and
//! wants to know whether it is working. Those are different jobs on the same stream, and the second
//! must never be able to reach the first by accident: one stray human-readable line on stdout
//! corrupts the protocol wire with nothing to indicate where the corruption came from.
//!
//! So the bare command prints status and the command list, and exits. It does not fall through to
//! the MCP loop and it does not sit waiting for input — a binary that appears to hang when run
//! without arguments is one nobody trusts enough to run again.

use std::io::{stdin, stdout, BufReader};

use rill_chain::describe::describe_function;
use rill_cli::keystore::Keystore;
use rill_cli::runset::RunSet;
use rill_cli::stdio::{serve, WalletContext};
use rill_ptb::deployments::{is_superseded, TESTNET_AGENT_WALLET};

/// The `Version` object the testnet package gates itself on, from the reference deployment.
const DEFAULT_VERSION_ID: &str =
    "0xd4f88a6dc271f923f0e55dd96eb8f8762ed4d45199c6719ae92365694478fd65";

/// The positional arguments, with `--as <address>` removed.
///
/// Written once because every subcommand that reads a position would otherwise be shifted by two
/// the moment somebody signs as a different key — and the failure is a usage message for a command
/// that was typed correctly.
fn positional() -> Vec<String> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        if argv[i] == "--as" {
            i += 2;
            continue;
        }
        out.push(argv[i].clone());
        i += 1;
    }
    out
}

/// What every subcommand needs, loaded once and reported on rather than exiting silently.
struct Loaded {
    keystore: Option<Keystore>,
    /// Why there is no key, kept rather than printed on load.
    ///
    /// Several commands need no key at all — `describe` reads a public package, `help` reads
    /// nothing — and a warning they cannot act on trains the reader to skip warnings. So the reason
    /// is carried and reported by the commands that are actually blocked by it.
    keystore_error: Option<String>,
    run_set: Option<RunSet>,
    network: String,
    mainnet_allowed: bool,
}

fn load() -> Loaded {
    // `--as <address>` selects which key signs. Owner-only calls and agent-only calls are different
    // keys by design — `add_rule` asserts the owner, `request_spend` asserts the agent — so a tool
    // that can only sign as one of them can exercise only half the contract.
    let argv: Vec<String> = std::env::args().collect();
    let requested = argv
        .iter()
        .position(|a| a == "--as")
        .and_then(|i| argv.get(i + 1))
        .map(|a| {
            a.parse::<sui_sdk_types::Address>()
                .map_err(|_| format!("--as {a} is not an address"))
        });

    let (keystore, keystore_error) = match requested {
        // An address was asked for and does not parse. Falling back to whatever key happens to be
        // first would sign as somebody the caller did not name.
        Some(Err(why)) => (None, Some(why)),
        Some(Ok(address)) => match Keystore::load_for(address) {
            Ok(store) => (Some(store), None),
            Err(e) => (None, Some(e.to_string())),
        },
        None => match Keystore::load() {
            Ok(store) => (Some(store), None),
            Err(e) => (None, Some(e.to_string())),
        },
    };

    // Loaded at startup. A run-set paired with the wrong key must fail here, where the operator is
    // looking, rather than at signing time, where it costs a user an unexplained refusal.
    let run_set = match RunSet::path_from_env() {
        Some(path) => match RunSet::from_path(&path) {
            Ok(run_set) => {
                if let Some(store) = &keystore {
                    if let Err(e) = run_set.check_key(&store.address().to_string()) {
                        eprintln!("rill: {e}");
                        std::process::exit(1);
                    }
                }
                Some(run_set)
            }
            Err(e) => {
                eprintln!("rill: {e}");
                std::process::exit(1);
            }
        },
        None => None,
    };

    Loaded {
        keystore,
        keystore_error,
        run_set,
        network: std::env::var("SUI_NETWORK").unwrap_or_else(|_| "testnet".into()),
        mainnet_allowed: std::env::var("RILL_ALLOW_MAINNET").as_deref() == Ok("true"),
    }
}

const COMMANDS: &[(&str, &str)] = &[
    ("mcp", "speak MCP over stdio — this is what an agent runs"),
    ("status", "report readiness and exit"),
    ("address", "print the signing address, nothing else"),
    ("capabilities", "show what the loaded run-set permits"),
    (
        "describe",
        "read a Move function's signature from chain: rill describe <pkg>::<module>::<fn>",
    ),
    (
        "wallet create",
        "mint an agent wallet + capability (add --submit to send it)",
    ),
    (
        "wallet revoke",
        "owner-only kill switch: stop the wallet and take the remaining balance back",
    ),
    (
        "wallet rules",
        "attach the manifest's rules to a wallet — without this it has no limits",
    ),
    (
        "deepbook provision",
        "create a DeepBook BalanceManager and delegate it with a TradeCap + DepositCap",
    ),
    (
        "order",
        "the hero path: gated spend -> deposit -> proof -> DeepBook limit order",
    ),
    (
        "spend",
        "release a gated spend: request_spend -> prove x N -> confirm_spend",
    ),
    ("help", "this list"),
];

fn print_commands() {
    println!("\nCommands");
    for (name, description) in COMMANDS {
        println!("  rill {name:14} {description}");
    }
}

/// Readiness, in the order someone debugging would ask for it.
///
/// Returns the exit code: not-ready is a failure, because a connector that cannot sign should not
/// report success to whatever is checking on it.
fn status(loaded: &Loaded) -> i32 {
    println!("rill");
    match &loaded.keystore {
        Some(store) => {
            println!("  status : ready");
            println!("  address: {}", store.address());
        }
        None => {
            println!("  status : not ready — no key loaded");
            if let Some(reason) = &loaded.keystore_error {
                println!("  reason : {reason}");
            }
        }
    }
    println!("  network: {}", loaded.network);
    println!(
        "  mainnet signing: {}",
        if loaded.mainnet_allowed {
            "allowed"
        } else {
            "off"
        }
    );
    match &loaded.run_set {
        Some(run_set) => {
            println!("  run-set: {} ({})", run_set.label, run_set.action_id);
            // A capability minted by the superseded package cannot authorise a call in the current
            // one, and the failure otherwise arrives as a Move abort at signing time. Said here, it
            // is read before anything is attempted.
            if is_superseded(&run_set.wallet_package_id) {
                println!(
                    "  WARNING: this run-set names the superseded agent_wallet package.\n\
                     \x20          It has spend(), not request_spend()/confirm_spend(), so every\n\
                     \x20          execution will abort. Rebind to {}.",
                    TESTNET_AGENT_WALLET
                );
            }
        }
        None => println!("  run-set: none — execution will refuse. Set RILL_RUN_SET_PATH."),
    }
    i32::from(loaded.keystore.is_none())
}

fn main() {
    // The first argument that is not `--as <address>`. Without this the flag becomes the command,
    // and `rill --as 0x… status` reports that `--as` is not a command it knows.
    let command = positional().first().cloned();
    let loaded = load();

    // `--status` predates the subcommands and is kept working: a flag that quietly stops being
    // honoured is worse than one that was never offered.
    let command = match command.as_deref() {
        Some("--status") | Some("-s") => Some("status"),
        other => other,
    };

    match command {
        None => {
            let code = status(&loaded);
            print_commands();
            std::process::exit(code);
        }
        Some("status") => std::process::exit(status(&loaded)),
        Some("help") | Some("--help") | Some("-h") => {
            println!("rill — the local half of Rill: holds the key, checks the work, signs.");
            print_commands();
        }
        Some("address") => match &loaded.keystore {
            // Bare, so it composes: `export SENDER=$(rill address)`.
            Some(store) => println!("{}", store.address()),
            None => {
                eprintln!(
                    "rill: {}",
                    loaded
                        .keystore_error
                        .as_deref()
                        .unwrap_or("no key loaded, so there is no address to print")
                );
                std::process::exit(1);
            }
        },
        Some("capabilities") => match &loaded.run_set {
            Some(run_set) => {
                println!("action        : {}", run_set.action_id);
                println!("network       : {:?}", run_set.network);
                println!("sender        : {}", run_set.sender);
                println!("wallet package: {}", run_set.wallet_package_id);
                println!("max amount    : {}", run_set.max_amount_base_units);
                println!("declared spend: {}", run_set.declared_spend_base_units);
                println!("must remain   : {}", run_set.minimum_remaining_base_units);
                println!("gas ceiling   : {}", run_set.gas_ceiling_base_units);
                println!("\nAllowed call sequence, in order:");
                for target in &run_set.allowed_targets {
                    println!("  {target}");
                }
            }
            None => {
                eprintln!("rill: no run-set loaded. Set RILL_RUN_SET_PATH.");
                std::process::exit(1);
            }
        },
        // Integrating a protocol needs the exact shape of its call, and the chain publishes that.
        // This is the whole of what a per-protocol SDK would otherwise be consulted for, without
        // waiting for one to exist or trusting it to be current.
        Some("describe") => {
            let Some(target) = positional().get(1).cloned() else {
                eprintln!("usage: rill describe <package>::<module>::<function>");
                std::process::exit(1);
            };
            let parts: Vec<&str> = target.split("::").collect();
            let [package, module, function] = parts.as_slice() else {
                eprintln!("rill: expected <package>::<module>::<function>, got {target}");
                std::process::exit(1);
            };
            let endpoint = std::env::var("SUI_RPC_URL")
                .unwrap_or_else(|_| format!("https://fullnode.{}.sui.io:443", loaded.network));

            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(e) => {
                    eprintln!("rill: {e}");
                    std::process::exit(1);
                }
            };
            match runtime.block_on(describe_function(&endpoint, package, module, function)) {
                Ok(signature) => {
                    println!("{signature}");
                    println!(
                        "\n{} argument(s) a PTB command must carry:",
                        signature.arity()
                    );
                    for (i, parameter) in signature.call_arguments().iter().enumerate() {
                        println!("  {i:2}  {parameter}");
                    }
                    if signature.parameters.len() != signature.arity() {
                        println!("\n  (TxContext is declared but supplied by the runtime)");
                    }
                }
                Err(e) => {
                    eprintln!("rill: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("wallet") => {
            let action = positional().get(1).cloned().unwrap_or_default();
            if action != "create" && action != "rules" && action != "revoke" {
                eprintln!(
                    "rill: usage:\n  rill wallet create [--submit] [--amount 0.2]\n  \
                     rill wallet rules --wallet <id> [--submit]\n  \
                     rill wallet revoke --wallet <id> [--submit]"
                );
                std::process::exit(1);
            }
            let Some(keystore) = &loaded.keystore else {
                eprintln!(
                    "rill: {}",
                    loaded.keystore_error.as_deref().unwrap_or("no key loaded")
                );
                std::process::exit(1);
            };
            let argv: Vec<String> = std::env::args().collect();
            let flag = |name: &str| {
                argv.iter()
                    .position(|a| a == name)
                    .and_then(|i| argv.get(i + 1))
                    .cloned()
            };
            let submit = argv.iter().any(|a| a == "--submit");
            if submit && loaded.network == "mainnet" && !loaded.mainnet_allowed {
                eprintln!(
                    "rill: refusing to submit on mainnet. Set RILL_ALLOW_MAINNET=true if that is \
                     really the intent."
                );
                std::process::exit(1);
            }

            let manifest = rill_core::manifest::CapabilityManifest {
                wallet_coin_type: "0x2::sui::SUI".into(),
                rules: vec![
                    rill_core::manifest::CapabilityRule::Budget {
                        total_mist: flag("--budget").unwrap_or_else(|| "200000000".into()),
                    },
                    rill_core::manifest::CapabilityRule::PerTx {
                        max_mist: flag("--per-tx").unwrap_or_else(|| "50000000".into()),
                    },
                ],
            };

            let args = rill_cli::wallet::CreateArgs {
                package_id: flag("--package")
                    .or_else(|| std::env::var("AGENT_WALLET_PACKAGE_ID").ok())
                    .unwrap_or_else(|| TESTNET_AGENT_WALLET.into()),
                version_id: flag("--version-object")
                    .or_else(|| std::env::var("AGENT_WALLET_VERSION_ID").ok())
                    .unwrap_or_else(|| DEFAULT_VERSION_ID.into()),
                agent: flag("--agent"),
                amount: flag("--amount").unwrap_or_else(|| "0.2".into()),
                expires_in_days: flag("--days").and_then(|d| d.parse().ok()).unwrap_or(30),
                manifest,
                gas_budget: flag("--gas-budget")
                    .and_then(|g| g.parse().ok())
                    .unwrap_or(100_000_000),
                dry_run: !submit,
            };

            let endpoint = std::env::var("SUI_RPC_URL")
                .unwrap_or_else(|_| format!("https://fullnode.{}.sui.io:443", loaded.network));
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("the clock is after 1970")
                .as_millis() as u64;

            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a single-threaded runtime");
            let outcome = if action == "revoke" {
                let Some(wallet_id) = flag("--wallet") else {
                    eprintln!("rill: rill wallet revoke needs --wallet <id>");
                    std::process::exit(1);
                };
                runtime.block_on(rill_cli::revoke_cmd::revoke(
                    &endpoint,
                    keystore,
                    &rill_cli::revoke_cmd::RevokeArgs {
                        package_id: args.package_id.clone(),
                        wallet_id,
                        recipient: flag("--to"),
                        gas_budget: args.gas_budget,
                        dry_run: args.dry_run,
                    },
                ))
            } else if action == "create" {
                runtime.block_on(rill_cli::wallet::create(&endpoint, keystore, &args, now_ms))
            } else {
                let Some(wallet_id) = flag("--wallet") else {
                    eprintln!("rill: rill wallet rules needs --wallet <id>");
                    std::process::exit(1);
                };
                runtime.block_on(rill_cli::rules_cmd::attach(
                    &endpoint,
                    keystore,
                    &rill_cli::rules_cmd::RulesArgs {
                        package_id: args.package_id.clone(),
                        version_id: args.version_id.clone(),
                        wallet_id,
                        manifest: args.manifest.clone(),
                        gas_budget: args.gas_budget,
                        dry_run: args.dry_run,
                    },
                ))
            };
            if let Err(e) = outcome {
                eprintln!("\nrill: {e}");
                std::process::exit(1);
            }
        }
        Some("spend") => {
            let Some(keystore) = &loaded.keystore else {
                eprintln!(
                    "rill: {}",
                    loaded.keystore_error.as_deref().unwrap_or("no key loaded")
                );
                std::process::exit(1);
            };
            let argv: Vec<String> = std::env::args().collect();
            let flag = |name: &str| {
                argv.iter()
                    .position(|a| a == name)
                    .and_then(|i| argv.get(i + 1))
                    .cloned()
            };
            let (Some(wallet_id), Some(cap_id)) = (flag("--wallet"), flag("--cap")) else {
                eprintln!("rill: rill spend needs --wallet <id> --cap <id> [--amount 0.01]");
                std::process::exit(1);
            };
            let submit = argv.iter().any(|a| a == "--submit");

            let args = rill_cli::spend_cmd::SpendArgs {
                package_id: flag("--package")
                    .or_else(|| std::env::var("AGENT_WALLET_PACKAGE_ID").ok())
                    .unwrap_or_else(|| TESTNET_AGENT_WALLET.into()),
                version_id: flag("--version-object")
                    .or_else(|| std::env::var("AGENT_WALLET_VERSION_ID").ok())
                    .unwrap_or_else(|| DEFAULT_VERSION_ID.into()),
                wallet_id,
                cap_id,
                amount: flag("--amount").unwrap_or_else(|| "0.01".into()),
                recipient: flag("--to"),
                gas_budget: flag("--gas-budget")
                    .and_then(|g| g.parse().ok())
                    .unwrap_or(100_000_000),
                dry_run: !submit,
            };

            let endpoint = std::env::var("SUI_RPC_URL")
                .unwrap_or_else(|_| format!("https://fullnode.{}.sui.io:443", loaded.network));
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a single-threaded runtime");
            if let Err(e) = runtime.block_on(rill_cli::spend_cmd::spend(&endpoint, keystore, &args))
            {
                eprintln!("\nrill: {e}");
                std::process::exit(1);
            }
        }
        Some("deepbook") => {
            let action = positional().get(1).cloned().unwrap_or_default();
            if action != "provision" {
                eprintln!("rill: usage: rill deepbook provision [--agent <addr>] [--submit]");
                std::process::exit(1);
            }
            let Some(keystore) = &loaded.keystore else {
                eprintln!("rill: no key loaded");
                std::process::exit(1);
            };
            let argv: Vec<String> = std::env::args().collect();
            let flag = |name: &str| {
                argv.iter()
                    .position(|a| a == name)
                    .and_then(|i| argv.get(i + 1))
                    .cloned()
            };
            let network = if loaded.network == "mainnet" {
                rill_ptb::registry::DeepBookNetwork::Mainnet
            } else {
                rill_ptb::registry::DeepBookNetwork::Testnet
            };
            let args = rill_cli::manager_cmd::ProvisionArgs {
                deepbook_package: flag("--package")
                    .unwrap_or_else(|| network.package_id().to_string()),
                agent: flag("--agent"),
                gas_budget: 200_000_000,
                dry_run: !argv.iter().any(|a| a == "--submit"),
            };
            let endpoint = std::env::var("SUI_RPC_URL")
                .unwrap_or_else(|_| format!("https://fullnode.{}.sui.io:443", loaded.network));
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a single-threaded runtime");
            if let Err(e) =
                runtime.block_on(rill_cli::manager_cmd::provision(&endpoint, keystore, &args))
            {
                eprintln!("\nrill: {e}");
                std::process::exit(1);
            }
        }
        Some("order") => {
            let Some(keystore) = &loaded.keystore else {
                eprintln!("rill: no key loaded");
                std::process::exit(1);
            };
            let argv: Vec<String> = std::env::args().collect();
            let flag = |name: &str| {
                argv.iter()
                    .position(|a| a == name)
                    .and_then(|i| argv.get(i + 1))
                    .cloned()
            };
            let need = |name: &str| match flag(name) {
                Some(v) => v,
                None => {
                    eprintln!("rill: rill order needs {name}");
                    std::process::exit(1);
                }
            };
            let network = if loaded.network == "mainnet" {
                rill_ptb::registry::DeepBookNetwork::Mainnet
            } else {
                rill_ptb::registry::DeepBookNetwork::Testnet
            };
            let args = rill_cli::order_cmd::OrderArgs {
                package_id: flag("--package")
                    .or_else(|| std::env::var("AGENT_WALLET_PACKAGE_ID").ok())
                    .unwrap_or_else(|| TESTNET_AGENT_WALLET.into()),
                version_id: flag("--version-object")
                    .or_else(|| std::env::var("AGENT_WALLET_VERSION_ID").ok())
                    .unwrap_or_else(|| DEFAULT_VERSION_ID.into()),
                wallet_id: need("--wallet"),
                cap_id: need("--cap"),
                deepbook_package: flag("--deepbook")
                    .unwrap_or_else(|| network.package_id().to_string()),
                pool_key: flag("--pool").unwrap_or_else(|| "SUI_DBUSDC".into()),
                network,
                balance_manager_id: need("--manager"),
                trade_cap_id: need("--trade-cap"),
                deposit_cap_id: need("--deposit-cap"),
                spend: flag("--spend").unwrap_or_else(|| "0.01".into()),
                price: need("--price"),
                quantity: need("--quantity"),
                is_bid: argv.iter().any(|a| a == "--bid"),
                gas_budget: 200_000_000,
                dry_run: !argv.iter().any(|a| a == "--submit"),
            };
            let endpoint = std::env::var("SUI_RPC_URL")
                .unwrap_or_else(|_| format!("https://fullnode.{}.sui.io:443", loaded.network));
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a single-threaded runtime");
            if let Err(e) = runtime.block_on(rill_cli::order_cmd::order(&endpoint, keystore, &args))
            {
                eprintln!("\nrill: {e}");
                std::process::exit(1);
            }
        }
        Some("mcp") => {
            if loaded.run_set.is_none() {
                eprintln!("rill: no run-set configured; execution will refuse.");
            }
            match &loaded.keystore {
                Some(store) => eprintln!("rill ready — {} ({})", loaded.network, store.address()),
                None => {
                    eprintln!("rill started with no key; only read-only tools will answer");
                    if let Some(reason) = &loaded.keystore_error {
                        eprintln!("rill: {reason}");
                    }
                }
            }
            let mut context =
                WalletContext::new(loaded.keystore, loaded.network, loaded.mainnet_allowed)
                    .with_run_set(loaded.run_set);
            if let Err(e) = serve(&mut context, BufReader::new(stdin()), stdout()) {
                eprintln!("rill: transport stopped: {e}");
                std::process::exit(1);
            }
        }
        Some(unknown) => {
            eprintln!("rill: no such command: {unknown}");
            print_commands();
            std::process::exit(1);
        }
    }
}
