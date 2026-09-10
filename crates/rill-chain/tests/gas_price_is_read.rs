//! No production code names a gas price.
//!
//! The reference gas price is a per-epoch value the validators set, and it differs by network:
//! testnet answers 1000 and mainnet answers 100. A transaction priced below it is refused before
//! execution (`Gas price 999 under reference gas price (RGP) 1000`, verbatim from a testnet node),
//! and a literal that happens to be right on the network you develop against is a refusal waiting
//! on the one you ship to. So the price is read, per build, from the chain the transaction is
//! going to, and this test is what keeps a literal from creeping back in.
//!
//! # What counts as a literal
//!
//! Three shapes, matched on whitespace-stripped source so a call split across lines is still seen:
//!
//! - `set_gas_price(<digits>)`: a builder handed a number.
//! - `gas_price: <digits>`: a struct field assigned a number. Matched at a word boundary, so the
//!   fake chain's `reference_gas_price: 1_000` is not a hit: that is the price the fake *network*
//!   reports, which a caller reads exactly as it would read a real node's, not a price a caller
//!   assumed.
//! - `const ... GAS_PRICE ... = <digits>`: a named constant, which is a literal with an alias.
//!
//! # What is excluded, and why the exclusion lives here
//!
//! Test fixtures legitimately set a price: a unit test building a transaction against the fake has
//! no node to ask. So every item under a `#[cfg(test)]` attribute is removed, `tests/` directories
//! are never walked, comments are dropped so a doc comment describing the mistake is not counted
//! as making it, and string contents are blanked so a message quoting one is not either. The
//! removal walks to the test item's own closing brace rather than to the end of the file, because
//! production code does follow inline test modules here (`rill_chain::aborts` is one), and a
//! check that stopped reading at the first `mod tests` would wave a literal in `aborts` through.
//! Excluding test code by this rule, rather than by grepping `bins/` wholesale, is what lets the
//! check run over every crate at once.

use std::fs;
use std::path::{Path, PathBuf};

/// Every `src/` directory in the workspace: each crate and each binary.
fn production_source_roots() -> Vec<PathBuf> {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/rill-chain sits two levels below the workspace root")
        .to_path_buf();

    let mut roots = Vec::new();
    for group in ["crates", "bins"] {
        let dir = workspace.join(group);
        let entries = fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("{} must be listable: {e}", dir.display()));
        for entry in entries {
            let src = entry.expect("a directory entry").path().join("src");
            if src.is_dir() {
                roots.push(src);
            }
        }
    }
    assert!(
        roots.len() >= 2,
        "expected at least one crate and one binary under {}",
        workspace.display()
    );
    roots
}

fn rust_files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
        let path = entry.expect("a directory entry").path();
        if path.is_dir() {
            rust_files_under(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Where a string or character literal starting at `rest` ends, as a byte length. `None` when
/// `rest` does not start one (a lifetime's `'` is not a literal).
fn literal_len(rest: &str) -> Option<usize> {
    // Raw strings: r"...", r#"..."#, br"...", with any number of hashes.
    let after_prefix = rest
        .strip_prefix("br")
        .or_else(|| rest.strip_prefix('r'))
        .filter(|s| s.starts_with('"') || s.starts_with('#'));
    if let Some(raw) = after_prefix {
        let hashes = raw.chars().take_while(|c| *c == '#').count();
        let body = &raw[hashes..];
        if let Some(opened) = body.strip_prefix('"') {
            let closer = format!("\"{}", "#".repeat(hashes));
            let close_at = opened.find(&closer)?;
            return Some((rest.len() - raw.len()) + hashes + 1 + close_at + closer.len());
        }
        return None;
    }
    let after_b = rest.strip_prefix('b').unwrap_or(rest);
    let prefix = rest.len() - after_b.len();
    if after_b.starts_with('"') {
        let mut escaped = false;
        for (offset, c) in after_b.char_indices().skip(1) {
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => return Some(prefix + offset + 1),
                _ => {}
            }
        }
        return Some(rest.len());
    }
    if after_b.starts_with('\'') {
        let mut chars = after_b.char_indices().skip(1);
        let (_, second) = chars.next()?;
        if second == '\\' {
            let close = after_b[2..].find('\'')?;
            return Some(prefix + 2 + close + 1);
        }
        if let Some((offset, '\'')) = chars.next() {
            return Some(prefix + offset + 1);
        }
        // A lifetime, not a literal.
        return None;
    }
    None
}

/// The part of a file that ships.
///
/// Comments are dropped, string and character literals are blanked, and every item under a
/// `#[cfg(test)]` attribute is removed by walking to its closing brace (or its `;`, for an item
/// without a body). Code after an inline test module is kept.
fn production_text(source: &str) -> String {
    let mut out = String::new();
    let mut i = 0;
    // Between a `#[cfg(test)]` and the item it applies to.
    let mut pending = false;
    // Inside that item's braces; the depth is how many are still open.
    let mut depth = 0usize;

    while i < source.len() {
        let rest = &source[i..];
        let emitting = !pending && depth == 0;

        if rest.starts_with("//") {
            i += rest.find('\n').unwrap_or(rest.len());
            continue;
        }
        if rest.starts_with("/*") {
            i += rest.find("*/").map_or(rest.len(), |n| n + 2);
            continue;
        }
        // Only a real literal is skipped; an identifier that merely starts with `r` or `b` is not.
        let at_identifier_boundary = source[..i]
            .chars()
            .next_back()
            .is_none_or(|c| !is_identifier_char(c));
        if at_identifier_boundary {
            if let Some(len) = literal_len(rest) {
                if emitting {
                    out.push_str("\"\"");
                }
                i += len;
                continue;
            }
        }
        if rest.starts_with("#![cfg(test)]") {
            // The whole file is a test.
            return String::new();
        }
        if rest.starts_with("#[cfg(test)]") {
            pending = true;
            i += "#[cfg(test)]".len();
            continue;
        }

        let c = rest.chars().next().expect("not at the end");
        if pending {
            match c {
                '{' => {
                    pending = false;
                    depth = 1;
                }
                ';' => pending = false,
                _ => {}
            }
        } else if depth > 0 {
            match c {
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
        } else {
            out.push(c);
        }
        i += c.len_utf8();
    }
    out
}

fn is_identifier_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Whether `needle` occurs in `haystack` followed immediately by a digit, optionally requiring
/// that the character before the match is not part of an identifier.
fn names_a_number(haystack: &str, needle: &str, at_word_boundary: bool) -> bool {
    haystack.match_indices(needle).any(|(start, _)| {
        let follows_with_digit = haystack[start + needle.len()..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit());
        let preceded_by_identifier = haystack[..start]
            .chars()
            .next_back()
            .is_some_and(is_identifier_char);
        follows_with_digit && !(at_word_boundary && preceded_by_identifier)
    })
}

/// A `const` or `static` whose name mentions `GAS_PRICE` and whose value starts with a digit.
fn is_a_priced_constant(line: &str) -> bool {
    let trimmed = line.trim_start();
    let declares = trimmed.contains("const ") || trimmed.contains("static ");
    if !declares || !line.contains("GAS_PRICE") {
        return false;
    }
    line.split_once('=')
        .map(|(_, value)| value.trim_start())
        .and_then(|value| value.chars().next())
        .is_some_and(|c| c.is_ascii_digit())
}

fn without_whitespace(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// What a hit looks like in the failure message: the path and the offending shape.
fn literals_in(path: &Path) -> Vec<String> {
    let source = fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let text = production_text(&source);
    let mut hits = Vec::new();

    // Whitespace-stripped, so `set_gas_price(\n    1_000,\n)` reads as `set_gas_price(1_000,)`.
    let stripped = without_whitespace(&text);
    if names_a_number(&stripped, "set_gas_price(", false) {
        hits.push("set_gas_price(<number>)".to_owned());
    }
    if names_a_number(&stripped, "gas_price:", true) {
        hits.push("gas_price: <number>".to_owned());
    }
    for line in text.lines() {
        if is_a_priced_constant(line) {
            hits.push(format!("constant: {}", line.trim()));
        }
    }

    hits.into_iter()
        .map(|hit| format!("{}: {hit}", path.display()))
        .collect()
}

#[test]
fn no_production_code_names_a_gas_price() {
    let mut files = Vec::new();
    for root in production_source_roots() {
        rust_files_under(&root, &mut files);
    }
    assert!(
        files.len() > 20,
        "only {} source files found; the walk is not looking where the code is",
        files.len()
    );

    let offenders: Vec<String> = files.iter().flat_map(|f| literals_in(f)).collect();
    assert!(
        offenders.is_empty(),
        "a gas price is read from the chain, never named in production code. Found:\n  {}",
        offenders.join("\n  ")
    );
}

/// The matcher itself, checked against the shapes it must and must not catch, so a green run
/// above means "no literals" and not "the matcher matches nothing".
#[test]
fn the_matcher_catches_each_shape_and_ignores_the_fake_networks_answer() {
    let stripped = |s: &str| without_whitespace(&production_text(s));

    assert!(names_a_number(
        &stripped("tx.set_gas_price(1_000);"),
        "set_gas_price(",
        false
    ));
    assert!(names_a_number(
        &stripped("tx.set_gas_price(\n    1000,\n);"),
        "set_gas_price(",
        false
    ));
    assert!(
        !names_a_number(
            &stripped("tx.set_gas_price(chain.reference_gas_price().await?);"),
            "set_gas_price(",
            false
        ),
        "a price that is read is not a literal"
    );

    assert!(names_a_number(
        &stripped("BuildRequest { gas_price: 1_000, }"),
        "gas_price:",
        true
    ));
    assert!(
        !names_a_number(
            &stripped("State { reference_gas_price: 1_000, }"),
            "gas_price:",
            true
        ),
        "the fake network's own answer is what a caller reads, not what it assumed"
    );

    assert!(is_a_priced_constant(
        "const DEFAULT_GAS_PRICE: u64 = 1_000;"
    ));
    assert!(is_a_priced_constant("pub static GAS_PRICE: u64 = 100;"));
    assert!(
        !is_a_priced_constant("const DEFAULT_GAS_BUDGET: u64 = 50_000_000;"),
        "a budget is a ceiling the signer enforces, not a price"
    );
    assert!(!is_a_priced_constant(
        "const GAS_PRICE_LABEL: &str = \"reference gas price\";"
    ));
}

/// The exclusion rule, checked from both sides: a fixture inside a test module is invisible, and
/// production code after that module is not.
#[test]
fn a_test_module_is_skipped_and_the_code_after_it_is_not() {
    let only_a_fixture = "fn build() {\n    tx.set_gas_price(price);\n}\n\n#[cfg(test)]\nmod tests {\n    fn fixture() { tx.set_gas_price(1_000); }\n}\n";
    assert!(
        !production_text(only_a_fixture).contains("1_000"),
        "a literal inside a #[cfg(test)] module is a fixture"
    );

    let literal_after_tests = "#[cfg(test)]\nmod tests {\n    // a brace in a string: \"{\"\n    const S: &str = \"}\";\n    fn f() { let c = '{'; }\n}\npub mod later {\n    pub fn build() { tx.set_gas_price(1_000); }\n}\n";
    assert!(
        production_text(literal_after_tests).contains("set_gas_price(1_000)"),
        "code after an inline test module ships, and must be scanned"
    );

    let braceless = "#[cfg(test)]\nuse std::collections::HashMap;\npub fn build() { tx.set_gas_price(1_000); }\n";
    assert!(
        production_text(braceless).contains("set_gas_price(1_000)"),
        "a #[cfg(test)] attribute on a braceless item ends at its semicolon"
    );

    let commented = "// the old code called set_gas_price(1_000) here\n/* and so did\n   set_gas_price(1_000) */\nlet x = 1;\n";
    assert!(
        !production_text(commented).contains("set_gas_price"),
        "a comment describing the mistake is not making it"
    );

    let quoted =
        "let advice = \"never write set_gas_price(1_000)\";\nlet raw = r#\"gas_price: 1000\"#;\n";
    assert!(
        !production_text(quoted).contains("1000"),
        "a string quoting the mistake is not making it"
    );

    let lifetime = "fn f<'a>(s: &'a str) { tx.set_gas_price(1_000); }\n";
    assert!(
        production_text(lifetime).contains("set_gas_price(1_000)"),
        "a lifetime's quote must not be read as an unterminated character literal"
    );
}
