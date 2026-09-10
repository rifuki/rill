//! The enforcement labels this binary emits come from the producer, never from a constant.
//!
//! Checked on the source rather than on the output. The output is produced by a function that can
//! be handed any input, so a test that gives it a pre-flight rule and reads back "pre-flight"
//! proves that path and nothing else. The constant this replaces did not sit on a path a test could
//! reach: it sat in the one JSON literal every read went through, and every read agreed with it.
//! So the literal is what is asserted against, on the code that ships.

const WALLET_READ: &str = include_str!("../src/wallet_read.rs");
const STDIO: &str = include_str!("../src/stdio.rs");

/// Everything before the first `#[cfg(test)]`: the code that ships. A test is allowed to write the
/// word it expects; the code under test is not.
fn shipped(source: &str) -> &str {
    source.split("#[cfg(test)]").next().unwrap_or(source)
}

/// A label written out where the producer's answer should be, however it is spaced.
fn is_constant_label(line: &str) -> bool {
    let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    compact.contains("\"enforcement\":\"on-chain\"")
        || compact.contains("\"enforcement\":\"pre-flight\"")
}

#[test]
fn no_shipped_line_writes_an_enforcement_label_as_a_constant() {
    for (file, source) in [("wallet_read.rs", WALLET_READ), ("stdio.rs", STDIO)] {
        for (index, line) in shipped(source).lines().enumerate() {
            assert!(
                !is_constant_label(line),
                "{file}:{}: an enforcement label written as a constant: {}",
                index + 1,
                line.trim()
            );
        }
    }
}

/// The check above is only worth something if the label is emitted at all, and emitted from the
/// producer. Deleting the label would pass it; this is what stops that.
#[test]
fn the_wallet_read_emits_the_label_and_asks_the_producer_for_it() {
    let code = shipped(WALLET_READ);
    assert!(
        code.contains("\"enforcement\":"),
        "the wallet read no longer emits an enforcement label at all"
    );
    assert!(
        code.contains(".enforcement()"),
        "the wallet read must take its label from RuleKind::enforcement()"
    );
}

/// The detector itself, so a reformatted constant cannot slip past it.
#[test]
fn the_detector_recognises_a_constant_label_however_it_is_spaced() {
    assert!(is_constant_label(r#"        "enforcement": "on-chain","#));
    assert!(is_constant_label(r#""enforcement":"pre-flight""#));
    assert!(is_constant_label(r#"  "enforcement" :  "pre-flight" ,"#));
    assert!(!is_constant_label(
        r#""enforcement": enforcement.as_str(),"#
    ));
    assert!(!is_constant_label(r#"// nothing about enforcement here"#));
}

/// The wallet tool hands the loaded run-set's manifest to the read.
///
/// This is the one line that decides whether an agent is ever shown a pre-flight rule, and it is
/// not reachable from an offline test: `wallet()` builds its own client from an endpoint, so a
/// version of it that passes `None` answers every offline test exactly as the correct one does,
/// and only a live testnet read tells them apart. Verified by making the edit: `None` in place of
/// the mapping leaves `cargo test -p rill` fully green.
///
/// The assembly itself is covered offline against a fake node in `wallet_read::assembly_tests`.
/// What is left over, and what this holds, is the wiring between them.
#[test]
fn the_wallet_tool_passes_the_run_sets_manifest_to_the_read() {
    let shipped = shipped(STDIO);
    let call = shipped
        .find("read_limits(")
        .map(|at| &shipped[at..])
        .expect("stdio.rs must call read_limits; the wallet tool is what it is for");
    let args = balanced_arguments(call);
    assert!(
        args.contains("local"),
        "the read must be given the run-set's manifest, not a literal: {args}"
    );
    assert!(
        shipped.contains("capability_manifest"),
        "stdio.rs must map the loaded run-set to its manifest; without it `local` is always None"
    );
    assert!(
        !shipped.contains("let local: Option<&rill_core::manifest::CapabilityManifest> = None"),
        "the manifest was stubbed out; a run-set's pre-flight rules would be invisible"
    );
}

/// The argument list of a call, from its opening parenthesis to the parenthesis that closes it.
///
/// Depth-counted rather than cut at the first `)`, because the first argument here is itself a
/// call: stopping there reads `&endpoint(context` as the whole list and the check fails on
/// correct code.
fn balanced_arguments(call: &str) -> &str {
    let open = call.find('(').expect("a call has an opening parenthesis");
    let mut depth = 0usize;
    for (offset, ch) in call[open..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return &call[open + 1..open + offset];
                }
            }
            _ => {}
        }
    }
    panic!("the call to read_limits is never closed");
}
