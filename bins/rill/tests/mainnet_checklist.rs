//! The cutover checklist cannot rot quietly.
//!
//! `docs/MAINNET.md` is the document that makes the mainnet decision legible: every precondition
//! maps to a check, and each one is marked green or explicitly not green. A table like that is worth
//! something only while it is true, and the way it stops being true is not deletion but drift: a test
//! renamed, a row quietly marked green, a count in the prose that no longer matches the rows above
//! it. Each of those leaves a document that reads as authoritative and is not.
//!
//! So the table is parsed here and checked against the tree. This cannot tell whether a check passes
//! (the suites do that) but it can tell whether the check named still exists, and whether the
//! document's own arithmetic holds.

const CHECKLIST: &str = include_str!("../../../docs/MAINNET.md");

#[derive(Debug)]
struct Row {
    number: String,
    precondition: String,
    checked_by: String,
    state: String,
}

fn rows() -> Vec<Row> {
    CHECKLIST
        .lines()
        .filter(|line| line.starts_with('|'))
        .map(|line| {
            line.trim_matches('|')
                .split('|')
                .map(str::trim)
                .collect::<Vec<&str>>()
        })
        .filter(|cells| cells.len() == 4)
        // Skip the header and its separator.
        .filter(|cells| cells[0].chars().all(|c| c.is_ascii_digit()) && !cells[0].is_empty())
        .map(|cells| Row {
            number: cells[0].to_owned(),
            precondition: cells[1].to_owned(),
            checked_by: cells[2].to_owned(),
            state: cells[3].to_owned(),
        })
        .collect()
}

#[test]
fn the_table_parses_and_is_numbered_without_gaps() {
    let rows = rows();
    assert!(
        rows.len() >= 15,
        "the checklist parsed {} rows, so either the table moved or the parser is reading the \
         wrong thing",
        rows.len()
    );
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(
            row.number,
            (index + 1).to_string(),
            "the rows must be numbered in order, or a reader cannot cite one"
        );
        assert!(
            !row.precondition.is_empty(),
            "row {} has no precondition",
            row.number
        );
        assert!(
            !row.checked_by.is_empty(),
            "row {} names no check",
            row.number
        );
    }
}

/// Every row is green or says plainly that it is not. A blank or hedged state is how a table stops
/// being a checklist.
#[test]
fn every_row_is_either_green_or_explicitly_not() {
    for row in rows() {
        let state = row.state.to_lowercase();
        let green = state.starts_with("green");
        let not = state.contains("not green");
        assert!(
            green ^ not,
            "row {} is neither clearly green nor clearly not: {:?}",
            row.number,
            row.state
        );
    }
}

/// A row claiming green must name something that exists. A renamed test leaves the row reading as
/// proof of a check nobody runs.
#[test]
fn every_green_row_names_something_that_still_exists_in_the_tree() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root");

    for row in rows() {
        if !row.state.to_lowercase().starts_with("green") {
            continue;
        }
        // Paths and test names both appear in the column, and both are findable: a path by existing,
        // a test name by appearing in some source file.
        let tokens: Vec<&str> = row
            .checked_by
            .split([' ', ',', '`', '(', ')'])
            .filter(|t| t.contains('/') || t.contains('_'))
            .filter(|t| t.len() > 6)
            .collect();
        assert!(
            !tokens.is_empty(),
            "row {} names no path and no test: {:?}",
            row.number,
            row.checked_by
        );

        let found = tokens.iter().any(|token| {
            let as_path = root.join(token.trim_end_matches(['.', ':']));
            if as_path.exists() {
                return true;
            }
            let name = token.trim_end_matches(['.', ':']);
            // Three shapes appear in that column and all three are findable: a path, a test
            // function, and a test file named after the suite it holds.
            let as_fn = format!("fn {name}");
            ["bins", "crates"]
                .iter()
                .any(|dir| grep(&root.join(dir), &as_fn))
                || ["bins", "crates", "move", "scripts", ".github"]
                    .iter()
                    .any(|dir| grep(&root.join(dir), name))
                || ["bins", "crates"]
                    .iter()
                    .any(|dir| file_named(&root.join(dir), name))
        });
        assert!(
            found,
            "row {} claims green but nothing in the tree matches any of {tokens:?}",
            row.number
        );
    }
}

fn grep(dir: &std::path::Path, needle: &str) -> bool {
    fn walk(dir: &std::path::Path, needle: &str) -> bool {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path
                    .file_name()
                    .is_some_and(|n| n == "target" || n == "build")
                {
                    continue;
                }
                if walk(&path, needle) {
                    return true;
                }
            } else if std::fs::read_to_string(&path).is_ok_and(|text| text.contains(needle)) {
                return true;
            }
        }
        false
    }
    walk(dir, needle)
}

/// The prose counts the green rows. A count that drifts from the table is the most readable part of
/// the document being the wrong part.
#[test]
fn the_written_count_matches_the_table() {
    let rows = rows();
    let green = rows
        .iter()
        .filter(|r| r.state.to_lowercase().starts_with("green"))
        .count();
    let total = rows.len();
    let words = [
        "Zero",
        "One",
        "Two",
        "Three",
        "Four",
        "Five",
        "Six",
        "Seven",
        "Eight",
        "Nine",
        "Ten",
        "Eleven",
        "Twelve",
        "Thirteen",
        "Fourteen",
        "Fifteen",
        "Sixteen",
        "Seventeen",
        "Eighteen",
        "Nineteen",
        "Twenty",
    ];
    let expected = format!(
        "{} of {}",
        words.get(green).copied().unwrap_or("?"),
        words.get(total).copied().unwrap_or("?").to_lowercase()
    );
    assert!(
        CHECKLIST.contains(&expected),
        "the table has {green} green of {total}, so the prose should say {expected:?} and does not"
    );
}

/// The refusal the code actually prints must point at this document, or the reader who hits the gate
/// never finds the preconditions.
#[test]
fn the_refusal_points_at_this_document() {
    let said = rill_core::mainnet::mainnet_refusal();
    assert!(
        said.contains("docs/MAINNET.md"),
        "the refusal must name the checklist: {said}"
    );
    assert!(
        CHECKLIST.contains(rill_core::mainnet::ALLOW_MAINNET_VAR),
        "and the checklist must name the variable the refusal does"
    );
}

/// Whether any file under `dir` is named `stem`, with or without an extension.
fn file_named(dir: &std::path::Path, stem: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path
                .file_name()
                .is_some_and(|n| n == "target" || n == "build")
            {
                continue;
            }
            if file_named(&path, stem) {
                return true;
            }
        } else if path.file_stem().is_some_and(|n| n == stem) {
            return true;
        }
    }
    false
}
