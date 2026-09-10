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
