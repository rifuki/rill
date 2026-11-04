//! The signer's grant revision floor: once this signer has verified revision N of an owner's grant,
//! it never again uses a lower revision of the same grant, whatever the server serves and however
//! often the signer restarts.
//!
//! The server is exactly the party a grant exists to constrain. Without a floor it could drop the
//! newest signed grant and serve an older, looser one the owner signed earlier.

use std::path::PathBuf;

use rill_cli::grant_floor::GrantFloors;
use rill_cli::grants::{apply_floor, Refusal, VerifiedGrant};
use rill_cli::runset::RunSet;
use rill_core::grant::{Grant, SignedGrant};
use serde_json::{json, Value};

const OWNER: &str = "0x00000000000000000000000000000000000000000000000000000000000000a1";
const OTHER_OWNER: &str = "0x00000000000000000000000000000000000000000000000000000000000000a9";
const SIGNER: &str = "0x00000000000000000000000000000000000000000000000000000000000000a2";
const OTHER_SIGNER: &str = "0x00000000000000000000000000000000000000000000000000000000000000a8";
const WALLET: &str = "0x00000000000000000000000000000000000000000000000000000000000000b1";
const OTHER_WALLET: &str = "0x00000000000000000000000000000000000000000000000000000000000000b9";
const PACKAGE: &str = "0x00000000000000000000000000000000000000000000000000000000000000c1";

type Checked = (SignedGrant, Result<VerifiedGrant, Refusal>);

/// A fresh state root per test, so tests running in parallel never share a floor.
fn state_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rill-grant-floor-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run_set(action: &str, wallet: &str) -> Value {
    json!({
        "label": "swap",
        "network": "mainnet",
        "sender": SIGNER,
        "actionId": action,
        "walletPackageId": PACKAGE,
        "walletId": wallet,
        "agentCapId": PACKAGE,
        "versionId": PACKAGE,
        "capabilityManifest": {
            "walletCoinType": "0x2::sui::SUI",
            "rules": [{"kind": "budget", "totalMist": "100000000"}, {"kind": "per_tx", "maxMist": "50000000"}]
        },
        "allowedTargets": [format!("{PACKAGE}::agent_wallet::request_spend")],
        "allowedObjectIds": [wallet],
        "maxAmountBaseUnits": "50000000",
        "declaredSpendBaseUnits": "20000000",
        "minimumRemainingBaseUnits": "0",
        "gasCeilingBaseUnits": "100000000"
    })
}

fn signed(action: &str, wallet: &str, revision: u64) -> SignedGrant {
    SignedGrant {
        grant: Grant {
            network: "mainnet".into(),
            action_id: action.into(),
            action_name: "cetus swap".into(),
            agent: SIGNER.into(),
            wallet_id: wallet.into(),
            wallet_package_id: PACKAGE.into(),
            expires_at_ms: "9999999999999".into(),
            revision,
            run_set: run_set(action, wallet),
            build_arguments: json!({"sender": SIGNER}),
        },
        signature: "owner-signature".into(),
    }
}

/// A grant every chain check accepted, signed by `owner`.
fn verified(owner: &str, action: &str, wallet: &str, revision: u64) -> Checked {
    let signed = signed(action, wallet, revision);
    let verified = VerifiedGrant {
        grant: signed.grant.clone(),
        run_set: RunSet::from_value(&signed.grant.run_set).unwrap(),
        owner: owner.into(),
    };
    (signed, Ok(verified))
}

fn refused(revision: u64, refusal: Refusal) -> Checked {
    (signed("skill_x", WALLET, revision), Err(refusal))
}

fn floors(dir: &std::path::Path) -> GrantFloors {
    GrantFloors::open(dir, "mainnet", SIGNER).unwrap()
}

#[test]
fn a_lower_revision_after_a_higher_one_is_superseded_across_restart() {
    let dir = state_dir("restart");
    let mut first = vec![verified(OWNER, "skill_x", WALLET, 3)];
    apply_floor(&mut first, &floors(&dir)).unwrap();
    assert!(first[0].1.is_ok(), "the newest grant seen stays usable");

    // A new handle on the same directory is a restarted signer: nothing is held in memory.
    let mut rolled_back = vec![verified(OWNER, "skill_x", WALLET, 2)];
    apply_floor(&mut rolled_back, &floors(&dir)).unwrap();
    let refusal = rolled_back[0].1.as_ref().unwrap_err();
    assert_eq!(refusal, &Refusal::Superseded { floor: 3, given: 2 });
    assert_eq!(refusal.code(), "grant_superseded");
    assert!(refusal.to_string().contains("revision 3"));
}

#[test]
fn the_current_revision_stays_usable_so_workflow_pinning_works() {
    let dir = state_dir("pinning");
    let mut first = vec![verified(OWNER, "skill_x", WALLET, 3)];
    apply_floor(&mut first, &floors(&dir)).unwrap();

    let mut same = vec![verified(OWNER, "skill_x", WALLET, 3)];
    apply_floor(&mut same, &floors(&dir)).unwrap();
    assert!(same[0].1.is_ok(), "an equal revision is not superseded");

    // Both arrive together: neither is below what was already seen, so neither is refused here.
    // run_action still prefers the highest, and a workflow pins whichever it names.
    let mut both = vec![
        verified(OWNER, "skill_x", WALLET, 3),
        verified(OWNER, "skill_x", WALLET, 4),
    ];
    apply_floor(&mut both, &floors(&dir)).unwrap();
    assert!(both[1].1.is_ok());
    assert_eq!(floors(&dir).floor(OWNER, WALLET, "skill_x").unwrap(), 4);
    assert_eq!(
        both[0].1.as_ref().unwrap_err(),
        &Refusal::Superseded { floor: 4, given: 3 },
        "once 4 is verified in the same list, 3 is below the floor"
    );
}

#[test]
fn only_a_verified_grant_raises_the_floor() {
    let dir = state_dir("verified-only");
    let mut forged = vec![
        refused(9, Refusal::BadSignature("not the owner".into())),
        refused(9, Refusal::Expired),
    ];
    apply_floor(&mut forged, &floors(&dir)).unwrap();
    assert_eq!(floors(&dir).floor(OWNER, WALLET, "skill_x").unwrap(), 0);

    let mut honest = vec![verified(OWNER, "skill_x", WALLET, 3)];
    apply_floor(&mut honest, &floors(&dir)).unwrap();
    assert!(honest[0].1.is_ok());
    assert_eq!(floors(&dir).floor(OWNER, WALLET, "skill_x").unwrap(), 3);
}

#[test]
fn slots_are_independent() {
    let dir = state_dir("slots");
    floors(&dir).raise(OWNER, WALLET, "skill_x", 5).unwrap();
    assert_eq!(floors(&dir).floor(OWNER, WALLET, "skill_x").unwrap(), 5);
    assert_eq!(
        floors(&dir).floor(OWNER, OTHER_WALLET, "skill_x").unwrap(),
        0
    );
    assert_eq!(floors(&dir).floor(OWNER, WALLET, "skill_y").unwrap(), 0);
    assert_eq!(
        floors(&dir).floor(OTHER_OWNER, WALLET, "skill_x").unwrap(),
        0
    );
    let testnet = GrantFloors::open(&dir, "testnet", SIGNER).unwrap();
    assert_eq!(testnet.floor(OWNER, WALLET, "skill_x").unwrap(), 0);
    let other_signer = GrantFloors::open(&dir, "mainnet", OTHER_SIGNER).unwrap();
    assert_eq!(other_signer.floor(OWNER, WALLET, "skill_x").unwrap(), 0);

    // One spelling per address: a short form names the same slot as the full one.
    assert_eq!(floors(&dir).floor("0xa1", "0xb1", "skill_x").unwrap(), 5);
}

#[test]
fn concurrent_raises_never_lower_the_floor() {
    let dir = state_dir("concurrent");
    let up = {
        let dir = dir.clone();
        std::thread::spawn(move || {
            let floors = floors(&dir);
            for revision in 1..=50 {
                floors.raise(OWNER, WALLET, "skill_x", revision).unwrap();
            }
        })
    };
    let down = {
        let dir = dir.clone();
        std::thread::spawn(move || {
            let floors = floors(&dir);
            for revision in (1..=50).rev() {
                floors.raise(OWNER, WALLET, "skill_x", revision).unwrap();
            }
        })
    };
    up.join().unwrap();
    down.join().unwrap();
    assert_eq!(floors(&dir).floor(OWNER, WALLET, "skill_x").unwrap(), 50);
}

#[test]
fn a_malformed_floor_entry_fails_closed_and_a_path_like_action_id_is_refused() {
    let dir = state_dir("malformed");
    let store = floors(&dir);
    store.raise(OWNER, WALLET, "skill_x", 2).unwrap();
    let slot = dir
        .join("grant-floors")
        .join("mainnet")
        .join(SIGNER)
        .join(OWNER)
        .join(WALLET)
        .join("skill_x");
    assert!(
        slot.join("2").exists(),
        "the floor lives where it is documented"
    );
    std::fs::write(slot.join("abc"), b"").unwrap();
    assert!(
        store.floor(OWNER, WALLET, "skill_x").is_err(),
        "an entry that is not a revision must not read as no floor"
    );

    for bad in ["../x", "", "a/b", "x.y", &"a".repeat(97)] {
        assert!(store.raise(OWNER, WALLET, bad, 1).is_err(), "{bad:?}");
        assert!(store.floor(OWNER, WALLET, bad).is_err(), "{bad:?}");
    }
    assert!(!dir
        .join("grant-floors")
        .join("mainnet")
        .join(SIGNER)
        .join(OWNER)
        .join("x")
        .exists());
    assert!(!dir.join("x").exists());
    assert!(GrantFloors::open(&dir, "../mainnet", SIGNER).is_err());
    assert!(GrantFloors::open(&dir, "mainnet", "../x").is_err());
    assert!(store.raise("not-an-address", WALLET, "skill_x", 1).is_err());
}

#[cfg(unix)]
#[test]
fn the_floor_is_readable_by_this_user_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = state_dir("modes");
    floors(&dir).raise(OWNER, WALLET, "skill_x", 7).unwrap();
    let root = dir.join("grant-floors");
    let slot = root
        .join("mainnet")
        .join(SIGNER)
        .join(OWNER)
        .join(WALLET)
        .join("skill_x");
    for directory in [&root, &slot] {
        let mode = std::fs::metadata(directory).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{}", directory.display());
    }
    let mode = std::fs::metadata(slot.join("7"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn a_relative_config_path_gives_an_absolute_state_root() {
    let dir = rill_cli::grant_floor::state_dir_for(std::path::Path::new("rill.json"))
        .expect("a relative config still has a directory");
    assert!(
        dir.is_absolute(),
        "floors must not follow the launch directory: {}",
        dir.display()
    );
    assert_eq!(dir, std::env::current_dir().unwrap());
}
