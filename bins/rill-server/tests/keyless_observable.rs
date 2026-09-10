//! What a reader can check for themselves about the builder not being able to sign.
//!
//! Every other criterion in this repository can pass while this one stays invisible, because a
//! custodial server with good limits reproduces "spend inside a cap, refused by name" exactly. The
//! claim that distinguishes the design is that the thing building the transaction could not have
//! signed it, and that claim is only worth something if someone other than its author can check it.
//!
//! Three checks, in descending order of how hard they are to fake:
//!
//! 1. The crate links no signing library. A binary without one cannot sign whatever its code says.
//! 2. No source file under the server reaches for a key, a keypair, or a signing call.
//! 3. What it returns over HTTP is an unsigned transaction and carries no signature field.
//!
//! The first is also a CI gate, asserted in both directions there (the server must not have one, the
//! signer must). It is repeated here so that running the suite alone is enough to see it: a reader
//! who clones this and runs `cargo test` should not have to read a workflow file to learn the thing
//! the product is about. It nearly was not true: rill-chain and rill-auth both declared a signing
//! library and neither used it outside one live test, so it sat in this crate's graph and the claim
//! rested on no code path reaching for it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn server_sources() -> Vec<PathBuf> {
    fn walk(dir: &Path, found: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).expect("the server's src").flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, found);
            } else if path.extension().is_some_and(|e| e == "rs") {
                found.push(path);
            }
        }
    }
    let mut found = Vec::new();
    walk(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut found,
    );
    assert!(
        found.len() > 3,
        "the walk is not looking where the server lives"
    );
    found
}

/// Words that only appear in code that can produce a signature. Checked against code, not comments:
/// this crate's own prose explains why it cannot sign, and matching that would fail the build for
/// saying so.
const SIGNING_VOCABULARY: [&str; 6] = [
    "SuiSigner",
    "SimpleKeypair",
    "Ed25519PrivateKey",
    "sui_crypto",
    "from_suiprivkey",
    "RILL_SUI_PRIVATE_KEY",
];

#[test]
fn no_server_source_can_reach_a_key_or_a_signature() {
    for path in server_sources() {
        let source = fs::read_to_string(&path).expect("a source file");
        let name = path
            .strip_prefix(env!("CARGO_MANIFEST_DIR"))
            .unwrap_or(&path)
            .display()
            .to_string();
        for (index, line) in source.lines().enumerate() {
            let code = line.split("//").next().unwrap_or(line);
            for word in SIGNING_VOCABULARY {
                assert!(
                    !code.contains(word),
                    "{name}:{}: {word} belongs to code that signs, and this crate builds and \
                     simulates. See R11.",
                    index + 1
                );
            }
        }
    }
}

/// The check above is only worth something if it can see a breach.
#[test]
fn the_vocabulary_check_would_see_a_signing_call_if_one_were_added() {
    let planted =
        "    let signature = keypair.sign_transaction(&tx);".replace("keypair", "SimpleKeypair");
    assert!(
        SIGNING_VOCABULARY.iter().any(|w| planted
            .split("//")
            .next()
            .unwrap_or(&planted)
            .contains(w)),
        "the matcher no longer recognises a signing call"
    );
    let explaining = "    // this crate holds no SimpleKeypair and never will";
    assert!(
        !SIGNING_VOCABULARY.iter().any(|w| explaining
            .split("//")
            .next()
            .unwrap_or(explaining)
            .contains(w)),
        "a comment explaining the rule must not fail the rule"
    );
}

/// The strongest form, and the one a reader can run in one command.
///
/// Repeated from CI on purpose: see the module note.
#[test]
fn this_crate_links_no_signing_library() {
    let out = Command::new(env!("CARGO"))
        .args(["tree", "--locked", "-p", "rill-server", "--edges", "normal"])
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .output()
        .expect("cargo tree runs");
    assert!(
        out.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let graph = String::from_utf8_lossy(&out.stdout);
    assert!(
        !graph.contains("sui-crypto"),
        "a signing library is linked into the builder, so the keyless claim is no longer a fact \
         about the binary"
    );
    // And the asymmetry, because one direction alone is one claim rather than two: the signer must
    // still be able to sign, or nothing can complete a spend.
    let signer = Command::new(env!("CARGO"))
        .args(["tree", "--locked", "-p", "rill", "--edges", "normal"])
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .output()
        .expect("cargo tree runs");
    assert!(
        String::from_utf8_lossy(&signer.stdout).contains("sui-crypto"),
        "the signer lost its signing library, so the split is no longer about keys"
    );
}
