//! The largest spend a wallet would allow right now, and which rule holds that number.
//!
//! # Why this exists
//!
//! `rill_wallet` used to report which rules were attached and not one of their values. An agent
//! reading it learned that a `budget` rule and a `per_tx` rule were in force, and had no way to know
//! whether the spend it was about to attempt would be allowed. The only way to find out was to try
//! it and read the refusal, which is a round trip, a gas estimate, and a refusal an operator then
//! has to interpret.
//!
//! So the numbers are read from the chain that holds them, and one derived number is computed from
//! them: the largest single spend that would pass at this moment, with the name of the rule that
//! bounds it. The name is the useful half. "0.05 SUI, bounded by per_tx" tells an agent what to ask
//! the owner for; "0.05 SUI" alone does not.
//!
//! # Why it can be wrong, and says so
//!
//! It is an upper bound, not a promise. A rule this binary does not recognise could bound a spend
//! further, and nothing here can guess by how much, so [`Headroom::certain`] is false whenever the
//! wallet carries one. A caller that reported the number without that flag would be telling an agent
//! a limit it had not actually established, which is the failure this whole module is correcting.

use serde_json::Value;

/// The wallet's own fields, as the chain reports them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalletFacts {
    /// What the wallet still holds. Named `budget` on chain, where it is a `Balance<T>`.
    pub balance: u64,
    /// Lifetime total released from this wallet.
    pub spent: u64,
    pub expires_at_ms: u64,
    pub revoked: bool,
}

/// One rule's configured ceiling, read from the dynamic field that holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleConfig {
    Budget {
        total_mist: u64,
        spent: u64,
    },
    PerTx {
        max_mist: u64,
    },
    RateLimit {
        window_ms: u64,
        window_max: u64,
        window_start_ms: u64,
        spent_in_window: u64,
    },
    TimeWindow {
        not_before_ms: u64,
        not_after_ms: u64,
    },
}

impl RuleConfig {
    pub fn module(&self) -> &'static str {
        match self {
            Self::Budget { .. } => "budget",
            Self::PerTx { .. } => "per_tx",
            Self::RateLimit { .. } => "rate_limit",
            Self::TimeWindow { .. } => "time_window",
        }
    }
}

/// The answer, with the rule that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Headroom {
    pub base_units: u64,
    /// Which limit is the binding one: a rule module, `balance`, `expiry`, or `revoked`.
    pub bound_by: String,
    /// False when an unrecognised rule is attached, so a tighter limit may exist that this did not
    /// account for.
    pub certain: bool,
}

/// The largest single spend that would pass right now.
///
/// `now_ms` is passed rather than read so the whole computation is pure and a test can sit on either
/// side of a window boundary without sleeping.
pub fn largest_spend_now(
    wallet: &WalletFacts,
    configs: &[RuleConfig],
    unrecognised_rules: usize,
    now_ms: u64,
) -> Headroom {
    let certain = unrecognised_rules == 0;

    if wallet.revoked {
        return Headroom {
            base_units: 0,
            bound_by: "revoked".into(),
            certain,
        };
    }
    // The contract compares with `<`, so a wallet is dead at its expiry rather than at the tick
    // after it. Matching that here keeps the reported number from being one millisecond optimistic.
    if now_ms >= wallet.expires_at_ms {
        return Headroom {
            base_units: 0,
            bound_by: "expiry".into(),
            certain,
        };
    }

    // The balance is the floor under every other limit: a rule that permits more than the wallet
    // holds does not make the coins appear. `request_spend` asserts this one directly.
    let mut best = wallet.balance;
    let mut bound_by = "balance".to_string();
    let narrow = |limit: u64, name: &str, best: &mut u64, bound_by: &mut String| {
        if limit < *best {
            *best = limit;
            *bound_by = name.to_string();
        }
    };

    for config in configs {
        match *config {
            RuleConfig::Budget { total_mist, spent } => {
                narrow(
                    total_mist.saturating_sub(spent),
                    "budget",
                    &mut best,
                    &mut bound_by,
                );
            }
            RuleConfig::PerTx { max_mist } => {
                narrow(max_mist, "per_tx", &mut best, &mut bound_by);
            }
            RuleConfig::RateLimit {
                window_ms,
                window_max,
                window_start_ms,
                spent_in_window,
            } => {
                // A window that has already elapsed is a window that resets on the next spend, so
                // the whole allowance is available again. Reporting the spent-down remainder of an
                // expired window would understate what the contract will actually allow.
                let elapsed = now_ms >= window_start_ms.saturating_add(window_ms);
                let available = if elapsed {
                    window_max
                } else {
                    window_max.saturating_sub(spent_in_window)
                };
                narrow(available, "rate_limit", &mut best, &mut bound_by);
            }
            RuleConfig::TimeWindow {
                not_before_ms,
                not_after_ms,
            } => {
                if now_ms < not_before_ms || now_ms >= not_after_ms {
                    return Headroom {
                        base_units: 0,
                        bound_by: "time_window".into(),
                        certain,
                    };
                }
            }
        }
    }

    Headroom {
        base_units: best,
        bound_by,
        certain,
    }
}

/// The wallet's own numbers out of the object fields the node returned.
///
/// Every u64 arrives as a string: protobuf carries numbers as doubles, so the node sends a `u64` as
/// text rather than lose the high bits. Parsing it as a number here would reintroduce exactly the
/// rounding the node avoided.
pub fn wallet_facts(fields: &Value) -> Result<WalletFacts, String> {
    let u64_at = |key: &str| -> Result<u64, String> {
        let raw = fields
            .get(key)
            .ok_or_else(|| format!("the wallet object has no `{key}` field"))?;
        match raw {
            Value::String(s) => rill_core::amounts::parse_u64_string(s)
                .map_err(|e| format!("the wallet's `{key}`: {e}")),
            Value::Number(n) => n
                .as_u64()
                .ok_or_else(|| format!("the wallet's `{key}` is not a whole number: {n}")),
            other => Err(format!("the wallet's `{key}` is not a number: {other}")),
        }
    };
    Ok(WalletFacts {
        balance: u64_at("budget")?,
        spent: u64_at("spent")?,
        expires_at_ms: u64_at("expires_at_ms")?,
        revoked: fields
            .get("revoked")
            .and_then(Value::as_bool)
            .ok_or("the wallet object has no `revoked` field")?,
    })
}

/// One rule's config out of the dynamic field that holds it.
///
/// `value_type` names the `Config` type, so the module is the segment before `::Config`. The numbers
/// are under `value`, because the field object wraps them in the dynamic-field envelope.
pub fn rule_config(value_type: &str, field: &Value) -> Option<RuleConfig> {
    let module = value_type.strip_suffix("::Config")?.rsplit("::").next()?;
    let value = field.get("value")?;
    let at = |key: &str| -> Option<u64> {
        match value.get(key)? {
            Value::String(s) => s.parse().ok(),
            Value::Number(n) => n.as_u64(),
            _ => None,
        }
    };
    Some(match module {
        "budget" => RuleConfig::Budget {
            total_mist: at("total_mist")?,
            spent: at("spent")?,
        },
        "per_tx" => RuleConfig::PerTx {
            max_mist: at("max_mist")?,
        },
        "rate_limit" => RuleConfig::RateLimit {
            window_ms: at("window_ms")?,
            window_max: at("window_max")?,
            window_start_ms: at("window_start_ms")?,
            spent_in_window: at("spent_in_window")?,
        },
        "time_window" => RuleConfig::TimeWindow {
            not_before_ms: at("not_before_ms")?,
            not_after_ms: at("not_after_ms")?,
        },
        _ => return None,
    })
}
