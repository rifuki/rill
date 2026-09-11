//! The document this server hands agents, checked for the four ways it has already been wrong.
//!
//! Every failure asserted here happened in the reference implementation's generators, which this
//! one replaces: a host typed into the document instead of read from the configuration, an
//! enforcement label written as a word, a scoping rule handed to the chain, and an install line
//! pointing at a repository that has never published the binary. None of those is hypothetical and
//! none of them failed a test, because the generator lived in a repository that could not see the
//! code it was describing.
//!
//! The document is rendered twice, against two different configured base URLs, so a check that
//! passes only because the test's own host happens to match cannot pass.
//!
//! Routes are exercised through `tower::ServiceExt::oneshot`, as in `http_contract.rs`: no socket,
//! and what is tested is the router the binary serves.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::Value;
use tower::ServiceExt as _;

use rill_core::manifest::{
    to_declaration, CapabilityManifest, CapabilityRule, Enforcement, RuleKind,
};
use rill_core::release::{CHECKSUM_SUFFIX, LATEST_DOWNLOAD_URL, RELEASE_REPO, WALLET_ASSETS};
use rill_server::agent_docs::agent_instructions;
use rill_server::routes;
use rill_server::state::{AppState, Config, Network};
use rill_store::{PublishedSkill, SkillStore};

/// The module's own source, read as text.
///
/// One check here is about what is *written* rather than what is rendered: a host that reaches the
/// document only on one deployment's configuration would pass every rendering check made against
/// that same configuration.
const GENERATOR_SOURCE: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/agent_docs.rs"));

const SECTION_FIXTURE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/reference-doc-sections.json"
));

/// Two deployments, so nothing can pass by coincidence with the one the test happens to name.
const BASES: [&str; 2] = ["https://api.rill.test", "http://localhost:3939"];

/// Hosts a generated document may name besides this deployment's own and the release origin's.
///
/// Written out here on purpose. Adding one is a deliberate edit to a test, which is the only place
/// a reviewer would think to look for "which other servers does our own documentation send people
/// to".
const ALLOWED_FOREIGN_URLS: [&str; 1] = ["https://opencode.ai/config.json"];

fn config(base: &str) -> Config {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "rill-generated-docs-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("create the store directory");
    Config {
        port: 3939,
        network: Network::Testnet,
        public_base_url: base.into(),
        sui_rpc_url: "https://fullnode.testnet.sui.io:443".into(),
        oauth_secret: "test-secret".into(),
        oauth_secret_from_env: true,
        guard_package_id: Some("0xguard".into()),
        // The long-lived credential U8 added is configured, not defaulted, so a document
        // generated on a deployment that never sets an owner is the ordinary case here.
        owner_secret: None,
        owner_address: None,
        skills_store_path: dir.join("skills.json").to_string_lossy().into(),
        oauth_store_path: dir.join("oauth.json").to_string_lossy().into(),
    }
}

/// The document as the route serves it, with nothing attached.
fn document(base: &str) -> String {
    agent_instructions(&config(base), None, None)
}

/// One rule of every kind there is, so the grant table has a row per enforcement layer.
///
/// Amounts are decimal strings throughout, which is the money path's rule and not a formatting
/// preference: a JSON number here would be a float on the way to a cap.
fn every_kind_manifest() -> CapabilityManifest {
    CapabilityManifest {
        wallet_coin_type: "0x2::sui::SUI".into(),
        rules: vec![
            CapabilityRule::Budget {
                total_mist: "200000000".into(),
            },
            CapabilityRule::PerTx {
                max_mist: "50000000".into(),
            },
            CapabilityRule::RateLimit {
                window_ms: "3600000".into(),
                max_mist: "100000000".into(),
            },
            CapabilityRule::ProtocolScope {
                allowed_packages: vec!["0xdeep".into()],
            },
            CapabilityRule::SlippageFloor {
                min_out_mist: "9500000".into(),
            },
            CapabilityRule::AssetScope {
                allowed_coin_types: vec!["0x2::sui::SUI".into()],
            },
            CapabilityRule::RecipientAllowlist {
                addresses: vec!["0xb649".into()],
            },
            CapabilityRule::TimeWindow {
                not_before_ms: "1757000000000".into(),
                not_after_ms: "1790000000000".into(),
            },
        ],
    }
}

fn a_skill() -> PublishedSkill {
    PublishedSkill {
        id: "skill_demo".into(),
        name: "rill_skill_demo".into(),
        description: "A 10 DEEP bid on DeepBook, funded by a gated spend.".into(),
        flow: serde_json::json!({ "nodes": [], "edges": [] }),
        tool_defs: None,
        policy_id: None,
        owner: None,
        created_at: "2026-09-11T00:00:00.000Z".into(),
    }
}

/// Every absolute URL the document names.
fn urls(document: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = document;
    while let Some(at) = rest.find("http") {
        let candidate = &rest[at..];
        if candidate.starts_with("http://") || candidate.starts_with("https://") {
            let end = candidate
                .find(|c: char| c.is_whitespace() || "\"'`,)<>".contains(c))
                .unwrap_or(candidate.len());
            // Trailing sentence punctuation is not part of the URL. Without this, a URL that ends
            // a sentence is collected with its full stop attached and then fails the allowlist for
            // a reason that has nothing to do with its host.
            let url = candidate[..end].trim_end_matches(['.', ';', ':', '!', '?']);
            found.push(url.to_owned());
            rest = &candidate[end..];
        } else {
            rest = &candidate["http".len()..];
        }
    }
    found
}

// ── Scenario: the configured base URL, and no literal host ─────────────────────────────────────

/// Every URL that is not an allowed foreign one is under the configured base, and the endpoint the
/// reader is told to connect to moves when the configuration moves.
#[test]
fn a_generated_document_interpolates_the_configured_base_url_and_no_other() {
    for base in BASES {
        let doc = document(base);
        let endpoint = format!("{base}/mcp");
        assert!(
            doc.contains(&endpoint),
            "the document must hand the reader this deployment's endpoint {endpoint}:\n{doc}"
        );

        let found = urls(&doc);
        assert!(
            found.iter().any(|url| url.starts_with(base)),
            "no URL in the document belongs to the configured deployment:\n{doc}"
        );
        for url in found {
            // The releases page as well as the download path. It is the same origin and strictly
            // broader: rill_core::release asserts LATEST_DOWNLOAD_URL is under RELEASES_URL, and
            // the document names the page so a reader whose download 404s has somewhere to look
            // rather than concluding the binary is broken.
            let allowed = url.starts_with(base)
                || url.starts_with(LATEST_DOWNLOAD_URL)
                || url.starts_with(rill_core::release::RELEASES_URL)
                || ALLOWED_FOREIGN_URLS.contains(&url.as_str());
            assert!(
                allowed,
                "{url} is neither this deployment's nor an allowed foreign URL. A host written \
                 into the document reaches every reader of every deployment."
            );
        }

        // And the other deployment's host must be nowhere in it, which is what a typed-in host
        // would look like from here.
        let other = BASES.iter().find(|b| **b != base).expect("two bases");
        let other_host = other.split("//").nth(1).expect("a host");
        assert!(
            !doc.contains(other_host),
            "the document names {other_host} while configured for {base}:\n{doc}"
        );
    }
}

/// The same claim about the source rather than the rendering: no URL is typed into the text.
///
/// A literal host in a `format!` would survive every check above on the deployment whose host it
/// happens to be. The allowance is narrow: a constant declaration, or a comment.
#[test]
fn the_generator_writes_no_url_into_the_document_text() {
    for (number, line) in GENERATOR_SOURCE.lines().enumerate() {
        if !line.contains("://") {
            continue;
        }
        let trimmed = line.trim_start();
        let declared = line.contains("const ") && line.contains('=');
        let commented = trimmed.starts_with("//");
        assert!(
            declared || commented,
            "agent_docs.rs:{} writes a URL into the document instead of reading one: {}",
            number + 1,
            trimmed
        );
    }
}

// ── Scenario: each rule's computed enforcement label ───────────────────────────────────────────

/// Every rule kind there is appears with the label its producer computes, and with the other
/// layer's label absent from its row.
///
/// This is the test that fails if a label is ever written as a word again. It is also the reason
/// `RuleKind::all` exists: a kind added to the manifest without a row here fails rather than being
/// quietly left out of the document an owner reads.
#[test]
fn a_generated_document_states_each_rules_computed_enforcement_label() {
    let doc = document(BASES[0]);
    for kind in RuleKind::all() {
        let prefix = format!("| `{}` |", kind.module());
        let row = doc
            .lines()
            .find(|line| line.starts_with(&prefix))
            .unwrap_or_else(|| panic!("no row for {} in:\n{doc}", kind.module()));

        let computed = kind.enforcement();
        let other = match computed {
            Enforcement::OnChain => Enforcement::PreFlight,
            Enforcement::PreFlight => Enforcement::OnChain,
        };
        assert!(
            row.contains(computed.as_str()),
            "{} is {} and its row does not say so: {row}",
            kind.module(),
            computed.as_str()
        );
        assert!(
            row.contains(computed.enforced_by()),
            "{}'s row does not say who refuses: {row}",
            kind.module()
        );
        assert!(
            !row.contains(other.as_str()),
            "{}'s row carries both layers, so the label is not the computed one: {row}",
            kind.module()
        );
    }
}

/// And the owner's actual grant: every cap carries the label the declaration producer computed for
/// it, so the document and the wallet read cannot describe the same grant differently.
#[test]
fn a_granted_wallet_is_described_with_the_producers_own_labels() {
    let manifest = every_kind_manifest();
    let doc = agent_instructions(&config(BASES[0]), None, Some(&manifest));
    let declaration = to_declaration(&manifest).expect("the manifest must validate");

    for cap in &declaration.caps {
        let row = doc
            .lines()
            .find(|line| line.starts_with(&format!("| {} |", cap.label)))
            .unwrap_or_else(|| panic!("no row for the {} grant in:\n{doc}", cap.label));
        assert!(
            row.contains(&cap.value),
            "{}'s row does not carry its value {}: {row}",
            cap.label,
            cap.value
        );
        assert!(
            row.contains(cap.enforcement.as_str()) && row.contains(cap.enforcement.enforced_by()),
            "{}'s row does not carry the computed layer: {row}",
            cap.label
        );
    }
}

/// With nothing attached, the document says nothing is attached.
///
/// The reference's no-manifest branch exists for the same reason: an agent told it is bounded when
/// it is not will act as though something will stop it.
#[test]
fn a_document_with_no_grant_says_so_rather_than_describing_limits() {
    let doc = document(BASES[0]);
    assert!(
        doc.contains("No agent wallet is bound here yet"),
        "the no-grant state must be stated:\n{doc}"
    );
    let granted = agent_instructions(&config(BASES[0]), None, Some(&every_kind_manifest()));
    assert!(
        !granted.contains("No agent wallet is bound here yet"),
        "a document with a grant must not also claim there is none:\n{granted}"
    );
}

// ── Scenario: no claim that a scoping rule is checked on chain ─────────────────────────────────

/// The words that name a scoping rule. None of these is enforced on chain, whatever a document says.
const SCOPING_WORDS: &[&str] = &["destination", "protocol", "recipient", "asset"];

/// Words that put a rule on the chain. "on chain", "on-chain" and "on the chain" are one claim.
const CHAIN_WORDS: [&str; 4] = ["on chain", "on-chain", "on the chain", "move contract"];

/// Whether a sentence hands a rule to the chain. Denying that the chain holds it is the opposite
/// claim and the whole point of the correction, so it is not a violation.
fn claims_the_chain(sentence: &str) -> bool {
    let lower = sentence.to_lowercase();
    if lower.contains("nothing on") {
        return false;
    }
    CHAIN_WORDS.iter().any(|word| lower.contains(word))
}

/// Words that mark a sentence as being about a limit, rather than using the same noun in another
/// sense. The install block talks about a file to download; "asset" there is a release asset, and
/// demanding that it name an enforcement layer would be nonsense. Claiming the chain holds it, it
/// still may not do, which is why only the attribution half of the check is narrowed.
const LIMIT_WORDS: &[&str] = &[
    "rule",
    "limit",
    "scope",
    "allowlist",
    "refus",
    "enforc",
    "grant",
];

/// Whether a sentence is making a claim about a limit at all.
fn is_about_a_limit(sentence: &str) -> bool {
    LIMIT_WORDS.iter().any(|word| sentence.contains(word))
}

/// Whether a sentence says who does hold the rule.
fn attributes_the_rule(sentence: &str) -> bool {
    let lower = sentence.to_lowercase();
    lower.contains("pre-flight") || lower.contains("signer") || lower.contains("nothing on")
}

/// Sentences are taken inside one line, never across lines: a markdown table is many claims and
/// joining its rows into one sentence would make every row read as every other row's.
fn scoping_sentences(document: &str) -> Vec<String> {
    document
        .lines()
        .flat_map(|line| line.split(". "))
        .map(str::to_lowercase)
        .filter(|sentence| SCOPING_WORDS.iter().any(|word| sentence.contains(word)))
        .collect()
}

#[test]
fn a_generated_document_never_puts_a_scoping_rule_on_chain() {
    for manifest in [None, Some(every_kind_manifest())] {
        let doc = agent_instructions(&config(BASES[0]), Some(&a_skill()), manifest.as_ref());
        let sentences = scoping_sentences(&doc);
        assert!(
            sentences.len() > 2,
            "the document must name the scoping rules, or this check checks nothing:\n{doc}"
        );
        for sentence in sentences {
            assert!(
                !claims_the_chain(&sentence),
                "a scoping rule is placed on the chain: {sentence:?}"
            );
            assert!(
                !is_about_a_limit(&sentence) || attributes_the_rule(&sentence),
                "a scoping rule is named without saying who holds it: {sentence:?}"
            );
        }
    }
}

/// The slippage floor is the one limit two layers refuse, and saying only one of them is wrong in
/// both directions: a reader who thinks only the chain holds it will ship an envelope the signer
/// rejects, and a reader who thinks only the signer holds it will believe a mismatched floor
/// reaches the chain unchecked.
#[test]
fn the_slippage_floor_is_stated_as_enforced_twice() {
    let doc = document(BASES[0]);
    let sentence = doc
        .lines()
        .find(|line| line.contains("slippage floor is pre-flight"))
        .unwrap_or_else(|| panic!("the slippage floor's layer is not stated:\n{doc}"));
    assert!(
        sentence.contains("refuses to sign") && sentence.contains("aborts"),
        "both refusals must be named: {sentence}"
    );
}

// ── Scenario: the install command names the resolved release origin ────────────────────────────

#[test]
fn the_install_command_names_the_resolved_release_origin_and_assets() {
    let doc = document(BASES[0]);
    let lines: Vec<&str> = doc.lines().map(str::trim).collect();
    let first = WALLET_ASSETS[0];

    // The whole line, not a substring: the binary's URL is a prefix of its checksum's, so a
    // substring match would let the download line name any origin at all.
    for name in [
        first.file.to_owned(),
        format!("{}{CHECKSUM_SUFFIX}", first.file),
    ] {
        let curl = format!("curl -fsSLO {LATEST_DOWNLOAD_URL}/{name}");
        assert!(
            lines.contains(&curl.as_str()),
            "the install block must carry the line `{curl}`:\n{doc}"
        );
    }
    assert!(
        doc.contains(&format!("shasum -a 256 -c {}{CHECKSUM_SUFFIX}", first.file)),
        "an unpinned download is only checkable against its checksum:\n{doc}"
    );
    assert!(
        doc.contains(RELEASE_REPO),
        "the document must say which origin publishes the binary:\n{doc}"
    );
    for asset in WALLET_ASSETS {
        assert!(
            doc.contains(asset.file),
            "{} is published but no reader is told it exists:\n{doc}",
            asset.file
        );
        assert!(
            doc.contains(asset.platform),
            "{} is offered with no platform a reader can match:\n{doc}",
            asset.file
        );
    }

    // The two origins that have carried these instructions before. Named in prose is fine; a URL a
    // reader can act on is not, because neither publishes a binary built from this source.
    for other in ["naisu-one/rill", "eseslabs/rill"] {
        for url in urls(&doc) {
            assert!(
                !url.contains(other),
                "the document sends a reader to {other}, which does not publish this binary: {url}"
            );
        }
    }
}

// ── The tools named are the tools offered ─────────────────────────────────────────────────────

/// Both directions. The reference document names four tools that exist on no surface here, which is
/// the failure mode of a document that lists names instead of reading them.
#[test]
fn every_tool_the_document_names_is_offered_and_every_offered_tool_is_named() {
    let doc = document(BASES[0]);
    let offered: Vec<String> = [rill_mcp::Surface::Actions, rill_mcp::Surface::Wallet]
        .into_iter()
        .flat_map(rill_mcp::tools)
        .map(|tool| tool.name.to_string())
        .collect();

    for name in &offered {
        assert!(
            doc.contains(&format!("`{name}`")),
            "{name} is offered and the document does not mention it:\n{doc}"
        );
    }

    for token in doc.split('`').skip(1).step_by(2) {
        if !token.starts_with("rill_") {
            continue;
        }
        assert!(
            offered.contains(&token.to_owned()),
            "the document tells an agent to call {token}, which no surface offers"
        );
    }
}

// ── The route, because a generator nobody can fetch is a file ─────────────────────────────────

async fn get(app: axum::Router, path: &str) -> (StatusCode, String, axum::http::HeaderMap) {
    let response = app
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        String::from_utf8_lossy(&bytes).into_owned(),
        headers,
    )
}

#[tokio::test]
async fn the_route_serves_the_document_the_generator_produces() {
    let config = config(BASES[0]);
    let expected = agent_instructions(&config, None, None);
    let (status, body, headers) = get(routes::router(AppState::new(config)), "/api/docs").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/markdown; charset=utf-8"),
        "a browser must render it as text rather than download it"
    );
    assert_eq!(
        body, expected,
        "the route must serve what the generator produces, not a copy"
    );
}

/// The per-action document, at the path the reference already advertises.
#[tokio::test]
async fn the_per_action_route_names_the_action_it_was_asked_for() {
    let state = AppState::new(config(BASES[0]));
    let skill = a_skill();
    state.skills.save(skill.clone()).expect("save the skill");
    let app = routes::router(state);

    let (status, body, _) = get(app.clone(), "/api/skills/skill_demo/instructions.md").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(&skill.name) && body.contains(&skill.id),
        "the document must name the action it was fetched for:\n{body}"
    );

    let (status, body, _) = get(app, "/api/skills/nope/instructions.md").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let error: Value = serde_json::from_str(&body).expect("the /api/* error shape is JSON");
    assert_eq!(error["success"], false);
}

// ── Conformance with the reference, which is still the stated specification ────────────────────

fn fixture() -> Value {
    serde_json::from_str(SECTION_FIXTURE).expect("the section fixture must be valid JSON")
}

/// Heading text, with the step number stripped: numbering shifts when a section is conditional, and
/// pinning it would make the fixture describe the order rather than the content.
fn heading_text(line: &str) -> String {
    let text = line.trim_start_matches('#').trim();
    match text.split_once(". ") {
        Some((number, rest)) if number.chars().all(|c| c.is_ascii_digit()) => rest.to_owned(),
        _ => text.to_owned(),
    }
}

fn our_headings(document: &str) -> Vec<String> {
    document
        .lines()
        .filter(|line| line.starts_with('#'))
        .map(heading_text)
        .collect()
}

/// Every section the fixture says was ported or corrected is still in the document.
///
/// This is the half of the drift guard that runs everywhere: the reference is the stated
/// specification, and a section of it quietly disappearing from what Rill actually serves is how the
/// port stops being a port.
#[test]
fn every_ported_reference_section_is_still_in_the_document() {
    let fixture = fixture();
    let doc = agent_instructions(
        &config(BASES[0]),
        Some(&a_skill()),
        Some(&every_kind_manifest()),
    );
    let headings = our_headings(&doc);
    let mut checked = 0;

    for section in fixture["sections"].as_array().expect("sections") {
        let disposition = section["disposition"].as_str().expect("disposition");
        let reference = section["reference"].as_str().expect("reference");
        if disposition == "omitted" {
            assert!(
                section["why"].as_str().is_some_and(|why| why.len() > 40),
                "{reference} was dropped without saying why"
            );
            continue;
        }
        let ours = section["ours"]
            .as_str()
            .unwrap_or_else(|| panic!("{reference} is {disposition} but names no section here"));
        assert!(
            headings.iter().any(|heading| heading == ours),
            "{reference} is recorded as {disposition} to \"{ours}\", which the document no longer \
             has. Its headings are {headings:?}"
        );
        checked += 1;
    }
    assert!(
        checked >= 8,
        "only {checked} sections were checked; the fixture has stopped describing the document"
    );
}

/// And the other direction: a section added here is recorded, or the fixture has stopped being a
/// map of the difference between the two implementations.
#[test]
fn every_section_of_the_document_is_accounted_for_in_the_fixture() {
    let fixture = fixture();
    let doc = agent_instructions(
        &config(BASES[0]),
        Some(&a_skill()),
        Some(&every_kind_manifest()),
    );
    let mut known: Vec<String> = fixture["sections"]
        .as_array()
        .expect("sections")
        .iter()
        .filter_map(|section| section["ours"].as_str())
        .map(str::to_owned)
        .collect();
    for added in fixture["added"].as_array().expect("added") {
        assert!(
            added["why"].as_str().is_some_and(|why| why.len() > 40),
            "a section was added with no reason recorded: {added}"
        );
        known.push(added["ours"].as_str().expect("ours").to_owned());
    }

    for heading in our_headings(&doc) {
        assert!(
            known.contains(&heading),
            "\"{heading}\" is in the document and in neither the fixture's ported sections nor its \
             added ones. Record what it is, or the fixture stops being the map between the two \
             implementations."
        );
    }
}

/// The headings the reference's generators actually emit, read from its source.
///
/// Taken up to the first em dash or template placeholder, and compared by prefix: two of the
/// reference's headings carry a character this repository forbids in anything it emits, so the
/// fixture cannot record them whole.
fn reference_headings(source: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_fence = false;
    for line in source.lines() {
        let trimmed = line.trim();
        let Some(quote) = trimmed.chars().next().filter(|c| *c == '\'' || *c == '`') else {
            continue;
        };
        let rest = &trimmed[quote.len_utf8()..];
        let Some(end) = rest.find(quote) else {
            continue;
        };
        let literal = &rest[..end];
        // A `#` inside a fenced block is a shell comment, not a heading.
        if literal.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence || !literal.starts_with('#') {
            continue;
        }
        let cut = literal
            .find('\u{2014}')
            .into_iter()
            .chain(literal.find("${"))
            .min()
            .unwrap_or(literal.len());
        out.push(literal[..cut].trim().to_owned());
    }
    out
}

/// The drift guard that needs the reference in reach.
///
/// The TypeScript repository remains the stated specification, and it is not edited from here, so
/// the only way its generators can be held to this port is to read them. Ignored by default because
/// they live outside this repository and a CI runner has no checkout of them: point
/// `RILL_REFERENCE_DIR` at one and this fails the moment a section is added, removed or renamed
/// there, which is the signal to revisit `fixtures/reference-doc-sections.json`.
#[test]
#[ignore = "requires a checkout of the TypeScript reference; set RILL_REFERENCE_DIR"]
fn the_reference_generators_still_emit_the_sections_the_fixture_pins() {
    let dir = std::env::var("RILL_REFERENCE_DIR").expect(
        "set RILL_REFERENCE_DIR to a checkout of the TypeScript reference, the repository this \
         generator was ported from",
    );
    let fixture = fixture();
    for key in ["skill_doc", "agent_instructions"] {
        let entry = &fixture["reference"][key];
        let path = std::path::Path::new(&dir).join(entry["path"].as_str().expect("path"));
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let pinned: Vec<String> = entry["headings"]
            .as_array()
            .expect("headings")
            .iter()
            .map(|heading| heading.as_str().expect("a heading").to_owned())
            .collect();
        assert_eq!(
            reference_headings(&source),
            pinned,
            "{key} no longer emits the sections the fixture pins. Decide what happens to the \
             difference in this repository's generator, then record it in \
             fixtures/reference-doc-sections.json"
        );
    }
}

// ── What the document claims about signing, and where it got it ──────────────────────────────────

/// The set of tools the document calls signing is exactly the set marked destructive.
///
/// The sentence this replaces said "Only this call produces a signature" of one tool, and in the
/// other branch "no other tool anywhere can produce a signature". Four tools on the signer are
/// marked destructive and four of them submit, one of which says so in its own description. An
/// agent acting on a document that denies a second signing path exists is the defect class this
/// unit was written to remove, reproduced inside the remover.
#[test]
fn the_document_names_every_signing_tool_and_claims_no_exclusivity() {
    let doc = rill_server::agent_docs::agent_instructions(&config(BASES[0]), None, None);

    let destructive: Vec<String> = rill_mcp::tools(rill_mcp::Surface::Wallet)
        .into_iter()
        .filter(|t| {
            t.annotations
                .as_ref()
                .and_then(|a| a.destructive_hint)
                .unwrap_or(false)
        })
        .map(|t| t.name.to_string())
        .collect();
    assert!(
        destructive.len() > 1,
        "this asserts nothing unless more than one tool signs; found {destructive:?}"
    );

    for name in &destructive {
        assert!(
            doc.contains(name),
            "{name} is marked destructive and the document does not name it, so an agent reading \
             this believes one fewer call can move money"
        );
    }
    for claim in [
        "Only this call produces a signature",
        "no other tool anywhere can produce a signature",
    ] {
        assert!(
            !doc.contains(claim),
            "the document claims exclusivity it does not have: {claim:?}"
        );
    }
    assert!(
        doc.contains("Every call that produces a signature is on the signer"),
        "and it must say what is true instead"
    );
}

/// Every URL in the document is this deployment's, including the ones a plain scheme check misses.
///
/// The existing source check skipped any line without "://" and the collector only gathered
/// candidates beginning with http:// or https://, so a literal `api.rill.naisu.one/mcp` in the
/// connect step passed all thirteen tests: the document then handed every reader a connect command
/// pointing at the dead host the plan's Open Questions is still about. Dotted hosts are matched now,
/// scheme or no scheme.
#[test]
fn no_dotted_host_appears_in_the_document_that_is_not_this_deployments() {
    let doc = rill_server::agent_docs::agent_instructions(&config(BASES[0]), None, None);
    let base = config(BASES[0]).base().to_string();

    // Hosts this document is allowed to name: its own, the release origin (a deployment-independent
    // fact with its own test), Sui's own endpoints, and whatever ALLOWED_FOREIGN_URLS already
    // declares. That constant exists so a reviewer has one place to ask "which other servers does
    // our documentation send people to", and this reuses it rather than starting a second list.
    let mut permitted = vec![
        base.replace("https://", "").replace("http://", ""),
        "github.com".to_string(),
        "sui.io".to_string(),
    ];
    permitted.extend(ALLOWED_FOREIGN_URLS.iter().map(|url| {
        url.trim_start_matches("https://")
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap_or(url)
            .to_string()
    }));

    for (index, line) in doc.lines().enumerate() {
        for token in line.split([' ', '"', '`', '(', ')', '<', '>', ',']) {
            let host = token
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .split('/')
                .next()
                .unwrap_or("");
            // A filename is dotted too, and the document names several. Excluded by their
            // extension, which no host ends in.
            const FILE_EXTENSIONS: [&str; 10] = [
                "json", "toml", "md", "rs", "sh", "yaml", "yml", "lock", "txt", "log",
            ];
            if host
                .rsplit('.')
                .next()
                .is_some_and(|ext| FILE_EXTENSIONS.contains(&ext))
            {
                continue;
            }
            // A dotted label with a plausible TLD, which is what a host looks like and a version
            // number does not.
            let looks_like_a_host = host.matches('.').count() >= 1
                && host.rsplit('.').next().is_some_and(|tld| {
                    tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic())
                })
                && host
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
            if !looks_like_a_host {
                continue;
            }
            assert!(
                permitted.iter().any(|p| host == p || host.ends_with(p)),
                "line {} names the host {host:?}, which is neither this deployment nor a permitted \
                 one: {line}",
                index + 1
            );
        }
    }
}

/// The redirect that makes GET /mcp useful is held by two literals with nothing coupling them.
///
/// The route is declared in one place and the Location header built in another. Renaming the route
/// leaves /mcp advertising a 404, which is exactly the R7 failure this unit closes, and nothing
/// tested it: the redirect was verified once by hand with curl. So follow it.
#[tokio::test]
async fn the_redirect_from_mcp_leads_somewhere_that_answers() {
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;

    let app = rill_server::routes::router(rill_server::state::AppState::new(config(BASES[0])));
    let redirected = app
        .clone()
        .oneshot(Request::get("/mcp").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert!(
        redirected.status().is_redirection(),
        "GET /mcp should point a browser at the instructions, got {}",
        redirected.status()
    );
    let location = redirected
        .headers()
        .get(header::LOCATION)
        .expect("a redirect names where it goes")
        .to_str()
        .unwrap()
        .to_owned();

    // The path, because the host in that header is this deployment's and the router serves paths.
    let path = location
        .split_once("://")
        .map(|(_, rest)| {
            rest.split_once('/')
                .map(|(_, p)| format!("/{p}"))
                .unwrap_or_default()
        })
        .unwrap_or(location.clone());
    assert!(
        !path.is_empty(),
        "the Location header has no path: {location}"
    );

    let followed = app
        .oneshot(Request::get(&path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(
        followed.status(),
        StatusCode::OK,
        "the redirect from /mcp leads to {path}, which answers {} rather than the instructions",
        followed.status()
    );
    let body = followed.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("# Rill"),
        "what the redirect leads to is not the instructions document: {}",
        &text[..text.len().min(120)]
    );
}
