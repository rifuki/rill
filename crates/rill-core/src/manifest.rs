//! The capability manifest — what an owner grants an agent, and the single place that grant is
//! turned into anything else.
//!
//! Three projections come out of one validated manifest: the on-chain `add_rule` arguments, the
//! flat policy the signer pre-checks against, and the human-readable declaration. In the reference
//! the third of those had two independent renderers — the frontend computed it locally *and* the
//! backend served it — which had to agree exactly with nothing making them. Here there is one
//! producer, and the server is the only thing that renders it.
//!
//! ## Not every rule is enforced on-chain, and the declaration says which
//!
//! `budget`, `per_tx`, `rate_limit` and `time_window` are proved against the real transaction by
//! Move and cannot be bypassed. The other four are enforced pre-flight, by the compiler and the
//! signer, because an on-chain rule can only compare against metadata the transaction asserts
//! about itself — which is decoration, not a guarantee. `slippage_floor` has a second reason: at
//! rule-prove time the swap's output does not exist yet, since it is the value being computed.
//!
//! That distinction is carried on every cap rather than smoothed over, because an owner deciding
//! how much to grant should know which limits the chain holds and which the software does.

use serde::{Deserialize, Serialize};

use crate::amounts::{AmountError, U64_MAX};
use crate::tokens::find_token;

/// A rule attached to an agent wallet. The tag is `kind` on the wire, matching the reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CapabilityRule {
    /// Lifetime spend ceiling.
    #[serde(rename_all = "camelCase")]
    Budget { total_mist: String },
    /// Per-transaction spend ceiling.
    #[serde(rename_all = "camelCase")]
    PerTx { max_mist: String },
    /// A rolling window quota.
    #[serde(rename_all = "camelCase")]
    RateLimit { window_ms: String, max_mist: String },
    /// Only these packages may be called.
    #[serde(rename_all = "camelCase")]
    ProtocolScope { allowed_packages: Vec<String> },
    /// Absolute minimum swap output, in base units — never basis points, mirroring the on-chain
    /// guard, which only ever compares against an absolute floor.
    #[serde(rename_all = "camelCase")]
    SlippageFloor { min_out_mist: String },
    /// Only these coin types may move.
    #[serde(rename_all = "camelCase")]
    AssetScope { allowed_coin_types: Vec<String> },
    /// Only these addresses may receive.
    #[serde(rename_all = "camelCase")]
    RecipientAllowlist { addresses: Vec<String> },
    /// Spends allowed at or after `not_before_ms` and strictly before `not_after_ms`.
    #[serde(rename_all = "camelCase")]
    TimeWindow {
        not_before_ms: String,
        not_after_ms: String,
    },
}

impl CapabilityRule {
    /// The stable discriminant, used for duplicate detection and module naming.
    pub fn kind(&self) -> RuleKind {
        match self {
            Self::Budget { .. } => RuleKind::Budget,
            Self::PerTx { .. } => RuleKind::PerTx,
            Self::RateLimit { .. } => RuleKind::RateLimit,
            Self::ProtocolScope { .. } => RuleKind::ProtocolScope,
            Self::SlippageFloor { .. } => RuleKind::SlippageFloor,
            Self::AssetScope { .. } => RuleKind::AssetScope,
            Self::RecipientAllowlist { .. } => RuleKind::RecipientAllowlist,
            Self::TimeWindow { .. } => RuleKind::TimeWindow,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuleKind {
    Budget,
    PerTx,
    RateLimit,
    ProtocolScope,
    SlippageFloor,
    AssetScope,
    RecipientAllowlist,
    TimeWindow,
}

impl RuleKind {
    /// The Move module this rule's `add_rule` / `prove` functions live in.
    ///
    /// There is deliberately no per-kind witness type name: every rule module's witness struct is
    /// literally `Rule`, disambiguated by module path. An earlier version of the reference invented
    /// names like `BudgetRule` that matched nothing on-chain.
    pub fn module(self) -> &'static str {
        match self {
            Self::Budget => "budget",
            Self::PerTx => "per_tx",
            Self::RateLimit => "rate_limit",
            Self::ProtocolScope => "protocol_scope",
            Self::SlippageFloor => "slippage_floor",
            Self::AssetScope => "asset_scope",
            Self::RecipientAllowlist => "recipient_allowlist",
            Self::TimeWindow => "time_window",
        }
    }

    /// Which layer actually holds this limit.
    pub fn enforcement(self) -> Enforcement {
        match self {
            Self::Budget | Self::PerTx | Self::RateLimit | Self::TimeWindow => Enforcement::OnChain,
            Self::ProtocolScope
            | Self::SlippageFloor
            | Self::AssetScope
            | Self::RecipientAllowlist => Enforcement::PreFlight,
        }
    }

    pub fn as_str(self) -> &'static str {
        self.module()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Enforcement {
    OnChain,
    PreFlight,
}

impl Enforcement {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OnChain => "on-chain",
            Self::PreFlight => "pre-flight",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CapabilityManifest {
    /// The coin this wallet spends. Used to format amounts for display.
    pub wallet_coin_type: String,
    pub rules: Vec<CapabilityRule>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    /// A manifest with no rules would grant unlimited, unconditional spend.
    NoRules,
    DuplicateKind {
        kind: RuleKind,
        index: usize,
    },
    /// A zero-width or inverted window can never be satisfied.
    EmptyTimeWindow {
        index: usize,
    },
    BadAmount {
        field: &'static str,
        source: AmountError,
    },
    EmptyScope {
        kind: RuleKind,
    },
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoRules => write!(
                f,
                "a capability manifest must carry at least one rule: a manifest with no \
                 restrictions grants the agent unlimited, unconditional spend, and there is no \
                 honest \"no restrictions\" default"
            ),
            Self::DuplicateKind { kind, index } => write!(
                f,
                "duplicate rule kind \"{}\" at rules[{index}]: each kind may appear at most once, \
                 so two limits of the same kind must be folded into one",
                kind.as_str()
            ),
            Self::EmptyTimeWindow { index } => write!(
                f,
                "rules[{index}] time_window: notBeforeMs must be strictly less than notAfterMs; a \
                 zero-width or inverted window can never be satisfied"
            ),
            Self::BadAmount { field, source } => write!(f, "{field}: {source}"),
            Self::EmptyScope { kind } => write!(
                f,
                "rules[{}] declares an empty scope, which makes nothing reachable — list at least \
                 one entry, or omit the rule entirely",
                kind.as_str()
            ),
        }
    }
}

impl std::error::Error for ManifestError {}

fn u64_field(value: &str, field: &'static str) -> Result<u64, ManifestError> {
    crate::amounts::parse_u64_string(value)
        .map_err(|source| ManifestError::BadAmount { field, source })
}

impl CapabilityManifest {
    /// Validate everything the shape alone cannot express.
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.rules.is_empty() {
            return Err(ManifestError::NoRules);
        }
        let mut seen: Vec<RuleKind> = Vec::with_capacity(self.rules.len());
        for (index, rule) in self.rules.iter().enumerate() {
            let kind = rule.kind();
            if seen.contains(&kind) {
                return Err(ManifestError::DuplicateKind { kind, index });
            }
            seen.push(kind);

            match rule {
                CapabilityRule::Budget { total_mist } => {
                    u64_field(total_mist, "rules[budget].totalMist")?;
                }
                CapabilityRule::PerTx { max_mist } => {
                    u64_field(max_mist, "rules[per_tx].maxMist")?;
                }
                CapabilityRule::RateLimit {
                    window_ms,
                    max_mist,
                } => {
                    u64_field(window_ms, "rules[rate_limit].windowMs")?;
                    u64_field(max_mist, "rules[rate_limit].maxMist")?;
                }
                CapabilityRule::SlippageFloor { min_out_mist } => {
                    u64_field(min_out_mist, "rules[slippage_floor].minOutMist")?;
                }
                CapabilityRule::TimeWindow {
                    not_before_ms,
                    not_after_ms,
                } => {
                    let before = u64_field(not_before_ms, "rules[time_window].notBeforeMs")?;
                    let after = u64_field(not_after_ms, "rules[time_window].notAfterMs")?;
                    if before >= after {
                        return Err(ManifestError::EmptyTimeWindow { index });
                    }
                }
                CapabilityRule::ProtocolScope { allowed_packages } => {
                    if allowed_packages.is_empty() {
                        return Err(ManifestError::EmptyScope { kind });
                    }
                }
                CapabilityRule::AssetScope { allowed_coin_types } => {
                    if allowed_coin_types.is_empty() {
                        return Err(ManifestError::EmptyScope { kind });
                    }
                }
                CapabilityRule::RecipientAllowlist { addresses } => {
                    if addresses.is_empty() {
                        return Err(ManifestError::EmptyScope { kind });
                    }
                }
            }
        }
        Ok(())
    }
}

// ── Projection 1: on-chain rule parameters ────────────────────────────────────────────────────

/// One rule's `add_rule` arguments. Only the four on-chain kinds appear; the pre-flight kinds are
/// deliberately absent rather than projected as no-ops, so a caller cannot mistake a decorative
/// on-chain rule for an enforced one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnChainRuleParams {
    pub module: &'static str,
    /// Field name to value, in the order Move's constructor takes them.
    pub config: Vec<(&'static str, u64)>,
}

pub fn to_on_chain_rule_params(
    manifest: &CapabilityManifest,
) -> Result<Vec<OnChainRuleParams>, ManifestError> {
    manifest.validate()?;
    let mut out = Vec::new();
    for rule in &manifest.rules {
        let module = rule.kind().module();
        match rule {
            CapabilityRule::Budget { total_mist } => out.push(OnChainRuleParams {
                module,
                config: vec![("totalMist", u64_field(total_mist, "totalMist")?)],
            }),
            CapabilityRule::PerTx { max_mist } => out.push(OnChainRuleParams {
                module,
                config: vec![("maxMist", u64_field(max_mist, "maxMist")?)],
            }),
            CapabilityRule::RateLimit {
                window_ms,
                max_mist,
            } => out.push(OnChainRuleParams {
                module,
                config: vec![
                    ("windowMs", u64_field(window_ms, "windowMs")?),
                    ("maxMist", u64_field(max_mist, "maxMist")?),
                ],
            }),
            CapabilityRule::TimeWindow {
                not_before_ms,
                not_after_ms,
            } => out.push(OnChainRuleParams {
                module,
                config: vec![
                    ("notBeforeMs", u64_field(not_before_ms, "notBeforeMs")?),
                    ("notAfterMs", u64_field(not_after_ms, "notAfterMs")?),
                ],
            }),
            // Pre-flight kinds project nothing. See the module note.
            CapabilityRule::ProtocolScope { .. }
            | CapabilityRule::SlippageFloor { .. }
            | CapabilityRule::AssetScope { .. }
            | CapabilityRule::RecipientAllowlist { .. } => {}
        }
    }
    Ok(out)
}

// ── Projection 2: the flat policy the signer pre-checks against ───────────────────────────────

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignerPolicy {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_amount_mist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub per_tx_max_mist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<SignerWindow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_packages: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_slippage_out_mist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_coin_types: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_recipients: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_window: Option<SignerTimeWindow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignerWindow {
    pub window_ms: String,
    pub max_mist: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignerTimeWindow {
    pub not_before_ms: String,
    pub not_after_ms: String,
}

pub fn to_signer_policy(manifest: &CapabilityManifest) -> Result<SignerPolicy, ManifestError> {
    manifest.validate()?;
    let mut policy = SignerPolicy::default();
    for rule in &manifest.rules {
        match rule {
            CapabilityRule::Budget { total_mist } => {
                policy.max_amount_mist = Some(total_mist.clone())
            }
            CapabilityRule::PerTx { max_mist } => policy.per_tx_max_mist = Some(max_mist.clone()),
            CapabilityRule::RateLimit {
                window_ms,
                max_mist,
            } => {
                policy.window = Some(SignerWindow {
                    window_ms: window_ms.clone(),
                    max_mist: max_mist.clone(),
                })
            }
            CapabilityRule::ProtocolScope { allowed_packages } => {
                policy.allowed_packages = Some(allowed_packages.clone())
            }
            CapabilityRule::SlippageFloor { min_out_mist } => {
                policy.min_slippage_out_mist = Some(min_out_mist.clone())
            }
            CapabilityRule::AssetScope { allowed_coin_types } => {
                policy.allowed_coin_types = Some(allowed_coin_types.clone())
            }
            CapabilityRule::RecipientAllowlist { addresses } => {
                policy.allowed_recipients = Some(addresses.clone())
            }
            CapabilityRule::TimeWindow {
                not_before_ms,
                not_after_ms,
            } => {
                policy.time_window = Some(SignerTimeWindow {
                    not_before_ms: not_before_ms.clone(),
                    not_after_ms: not_after_ms.clone(),
                })
            }
        }
    }
    Ok(policy)
}

// ── Projection 3: the human-readable declaration ──────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeclarationCap {
    pub label: String,
    pub value: String,
    pub enforcement: Enforcement,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityDeclaration {
    /// One plain sentence per rule, in manifest order.
    pub summary_lines: Vec<String>,
    /// One label/value pair per rule, in manifest order.
    pub caps: Vec<DeclarationCap>,
}

/// Render a base-unit amount as `"<amount> <SYMBOL>"`, or as raw base units for a coin the token
/// registry does not know. Guessing decimals would misstate the amount; saying so does not.
fn format_amount(mist: &str, coin_type: &str) -> String {
    let Some(token) = find_token(coin_type) else {
        return format!("{mist} base units of {coin_type}");
    };
    let Ok(raw) = mist.parse::<u128>() else {
        return format!("{mist} base units of {coin_type}");
    };
    let divisor = 10u128.pow(token.decimals);
    let whole = raw / divisor;
    let remainder = raw % divisor;
    if remainder == 0 {
        return format!("{whole} {}", token.symbol);
    }
    let fraction = format!("{:0width$}", remainder, width = token.decimals as usize);
    let trimmed = fraction.trim_end_matches('0');
    format!("{whole}.{trimmed} {}", token.symbol)
}

/// Render a duration in the coarsest unit that divides it evenly.
fn format_window(window_ms: &str) -> String {
    let Ok(ms) = window_ms.parse::<u128>() else {
        return format!("{window_ms}ms");
    };
    if ms > 0 && ms % 3_600_000 == 0 {
        return format!("{}h", ms / 3_600_000);
    }
    if ms > 0 && ms % 60_000 == 0 {
        return format!("{}m", ms / 60_000);
    }
    if ms > 0 && ms % 1_000 == 0 {
        return format!("{}s", ms / 1_000);
    }
    format!("{window_ms}ms")
}

/// The largest offset a JavaScript `Date` can represent. A schema-valid u64 millisecond value can
/// exceed it by a wide margin, and the reference degrades rather than throwing — matched here so
/// the two render the same text.
const MAX_SAFE_DATE_MS: u64 = 8_640_000_000_000_000;

/// Render a millisecond timestamp as an ISO-8601 instant in UTC.
///
/// Implemented directly rather than pulled from a date crate: the arithmetic is a dozen lines, and
/// a calendar library is the kind of dependency that quietly reaches for the system timezone and
/// would put I/O into a crate whose whole point is not having any.
fn format_date_ms(ms: &str) -> String {
    let Ok(value) = ms.parse::<u64>() else {
        return format!("{ms} ms (beyond representable date)");
    };
    if value > MAX_SAFE_DATE_MS {
        return format!("{ms} ms (beyond representable date)");
    }
    let days = (value / 86_400_000) as i64;
    let rem = value % 86_400_000;
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss, mmm) = (
        rem / 3_600_000,
        (rem / 60_000) % 60,
        (rem / 1_000) % 60,
        rem % 1_000,
    );
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{mmm:03}Z")
}

/// Howard Hinnant's `civil_from_days`: proleptic Gregorian date from a day count since the epoch.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn describe(rule: &CapabilityRule, coin_type: &str) -> (String, DeclarationCap) {
    let enforcement = rule.kind().enforcement();
    match rule {
        CapabilityRule::Budget { total_mist } => {
            let value = format_amount(total_mist, coin_type);
            (
                format!("Budget ≤ {value} total"),
                DeclarationCap {
                    label: "Budget".into(),
                    value,
                    enforcement,
                },
            )
        }
        CapabilityRule::PerTx { max_mist } => {
            let value = format_amount(max_mist, coin_type);
            (
                format!("Per-transaction ≤ {value}"),
                DeclarationCap {
                    label: "Per-tx max".into(),
                    value,
                    enforcement,
                },
            )
        }
        CapabilityRule::RateLimit {
            window_ms,
            max_mist,
        } => {
            let amount = format_amount(max_mist, coin_type);
            let window = format_window(window_ms);
            (
                format!("≤ {amount} per {window} window"),
                DeclarationCap {
                    label: "Rate limit".into(),
                    value: format!("{amount} / {window}"),
                    enforcement,
                },
            )
        }
        CapabilityRule::TimeWindow {
            not_before_ms,
            not_after_ms,
        } => {
            let value = format!(
                "not before {}; before {} (exclusive)",
                format_date_ms(not_before_ms),
                format_date_ms(not_after_ms)
            );
            (
                format!("Time window: {value}"),
                DeclarationCap {
                    label: "Time window".into(),
                    value,
                    enforcement,
                },
            )
        }
        CapabilityRule::ProtocolScope { allowed_packages } => {
            let value = allowed_packages.join(", ");
            (
                format!("Only protocols: {value}"),
                DeclarationCap {
                    label: "Allowed protocols".into(),
                    value,
                    enforcement,
                },
            )
        }
        CapabilityRule::SlippageFloor { min_out_mist } => {
            let value = format_amount(min_out_mist, coin_type);
            (
                format!("Min swap output ≥ {value}"),
                DeclarationCap {
                    label: "Min swap output".into(),
                    value,
                    enforcement,
                },
            )
        }
        CapabilityRule::AssetScope { allowed_coin_types } => {
            let value = allowed_coin_types.join(", ");
            (
                format!("Only coins: {value}"),
                DeclarationCap {
                    label: "Allowed coins".into(),
                    value,
                    enforcement,
                },
            )
        }
        CapabilityRule::RecipientAllowlist { addresses } => {
            let value = addresses.join(", ");
            (
                format!("Only recipients: {value}"),
                DeclarationCap {
                    label: "Allowed recipients".into(),
                    value,
                    enforcement,
                },
            )
        }
    }
}

/// The one producer of declaration text. The server exposes this over HTTP; nothing else computes
/// it, so there is no second implementation to keep in step.
pub fn to_declaration(
    manifest: &CapabilityManifest,
) -> Result<CapabilityDeclaration, ManifestError> {
    manifest.validate()?;
    let rendered: Vec<(String, DeclarationCap)> = manifest
        .rules
        .iter()
        .map(|r| describe(r, &manifest.wallet_coin_type))
        .collect();
    Ok(CapabilityDeclaration {
        summary_lines: rendered.iter().map(|(s, _)| s.clone()).collect(),
        caps: rendered.into_iter().map(|(_, c)| c).collect(),
    })
}

/// Exposed so a caller can bound-check an amount against the same ceiling the manifest uses.
pub const MAX_MANIFEST_AMOUNT: u128 = U64_MAX;
