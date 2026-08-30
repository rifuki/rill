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

use rill_cli::keystore::Keystore;
use rill_cli::runset::RunSet;
use rill_cli::stdio::{serve, WalletContext};
use rill_ptb::deployments::{is_superseded, TESTNET_AGENT_WALLET};

/// What every subcommand needs, loaded once and reported on rather than exiting silently.
struct Loaded {
    keystore: Option<Keystore>,
    run_set: Option<RunSet>,
    network: String,
    mainnet_allowed: bool,
}

fn load() -> Loaded {
    let keystore = match Keystore::from_env() {
        Ok(store) => Some(store),
        Err(e) => {
            // stderr, always. See the module note on why stdout is untouchable here.
            eprintln!("rill: {e}");
            None
        }
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
            println!("           set RILL_SUI_PRIVATE_KEY, then run `rill status` again");
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
    let command = std::env::args().nth(1);
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
                eprintln!("rill: no key loaded, so there is no address to print");
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
        Some("mcp") => {
            if loaded.run_set.is_none() {
                eprintln!("rill: no run-set configured; execution will refuse.");
            }
            match &loaded.keystore {
                Some(store) => eprintln!("rill ready — {} ({})", loaded.network, store.address()),
                None => eprintln!("rill started with no key; only read-only tools will answer"),
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
