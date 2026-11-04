//! How far an agent may move each runtime parameter of an owner-published action node.
//!
//! A published action may be built with a few values supplied at run time, so an agent can place a
//! smaller order or demand a better swap floor without the owner publishing again. Every one of
//! those values is bounded here, and the same table serves both sides that enforce it: the builder
//! checks each override against the node the owner published, and the signer checks it against
//! the bounds the owner signed into the grant. A key with no entry is not a runtime parameter at
//! all, and an override of it is refused.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::amounts::{parse_u64_string, Decimal};
use crate::flow::FlowNode;

/// The prefix every refusal carries, so an agent can tell a bound from any other build failure.
pub const REFUSED: &str = "runtime_override_refused";

/// The bound one runtime key is held to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// Selects what is traded or where. Only the published value is accepted.
    Fixed,
    /// An object the owner chose. Fixed like [`Rule::Fixed`], except that the published skill
    /// carries placeholders and the owner names the real objects per wallet when granting, so only
    /// the signed grant knows the value to pin.
    Object,
    /// An amount the action spends or commits. It may only be lowered.
    AtMost,
    /// A minimum the action must receive. It may only be raised.
    AtLeast,
    /// A DeepBook limit price, which may only move in the owner's favour: an ask may only sell
    /// higher and a bid may only buy lower.
    Price,
    /// Any `u64`. Used for the client order id, which only labels an order.
    AnyU64,
}

/// Every runtime parameter, by node type. A key appears under one node type only, which is what
/// lets a signer holding a grant from before this table classify a key without the node's type.
pub const TABLE: &[(&str, &str, Rule)] = &[
    ("cetus_swap", "amount_in", Rule::AtMost),
    ("cetus_swap", "min_amount_out", Rule::AtLeast),
    ("haedal_stake", "amount", Rule::AtMost),
    ("deepbook_limit_order", "poolKey", Rule::Fixed),
    ("deepbook_limit_order", "isBid", Rule::Fixed),
    ("deepbook_limit_order", "payWithDeep", Rule::Fixed),
    ("deepbook_limit_order", "balanceManagerId", Rule::Object),
    ("deepbook_limit_order", "tradeCapId", Rule::Object),
    ("deepbook_limit_order", "depositCapId", Rule::Object),
    ("deepbook_limit_order", "price", Rule::Price),
    ("deepbook_limit_order", "quantity", Rule::AtMost),
    ("deepbook_limit_order", "depositSui", Rule::AtMost),
    ("deepbook_limit_order", "clientOrderId", Rule::AnyU64),
];

/// The rule for `key` on a node of type `kind`, or `None` when it is not a runtime parameter.
pub fn rule(kind: &str, key: &str) -> Option<Rule> {
    TABLE
        .iter()
        .find(|(k, name, _)| *k == kind && *name == key)
        .map(|(_, _, rule)| *rule)
}

/// The rule for `key` on whatever node type carries it.
pub fn rule_for_key(key: &str) -> Option<Rule> {
    TABLE
        .iter()
        .find(|(_, name, _)| *name == key)
        .map(|(_, _, rule)| *rule)
}

/// The runtime keys of a node type, in table order.
pub fn keys(kind: &str) -> impl Iterator<Item = (&'static str, Rule)> + '_ {
    TABLE
        .iter()
        .filter(move |(k, _, _)| *k == kind)
        .map(|(_, key, rule)| (*key, *rule))
}

/// A bound resolved to values: what the owner signs into a grant and what both sides check.
///
/// Unknown fields are refused, so a signer never reads a bound it does not understand as no bound.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eq: Option<Value>,
    /// Inclusive lower bound, as exact decimal text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<String>,
    /// Inclusive upper bound, as exact decimal text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<String>,
    /// The value must be a `u64` in decimal text.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub u64: bool,
}

impl Limit {
    /// Refuse `value` for `key` unless every part of this bound holds. A bound with no part is
    /// refused too: an empty entry is a malformed grant, not permission.
    pub fn check(&self, key: &str, value: &Value) -> Result<(), String> {
        if self.eq.is_none() && self.min.is_none() && self.max.is_none() && !self.u64 {
            return Err(format!("{key} has no usable bound"));
        }
        if let Some(fixed) = &self.eq {
            if !same_value(value, fixed) {
                return Err(format!(
                    "{key} is fixed at {} and cannot be {}",
                    show(fixed),
                    show(value)
                ));
            }
        }
        if self.min.is_some() || self.max.is_some() {
            let given = decimal(key, value)?;
            if let Some(min) = &self.min {
                if order(key, &given, min)? == Ordering::Less {
                    return Err(format!("{key} {given} is below the bound {min}"));
                }
            }
            if let Some(max) = &self.max {
                if order(key, &given, max)? == Ordering::Greater {
                    return Err(format!("{key} {given} exceeds the bound {max}"));
                }
            }
        }
        if self.u64 {
            value
                .as_str()
                .ok_or(())
                .and_then(|s| parse_u64_string(s).map_err(|_| ()))
                .map_err(|_| format!("{key} must be a u64 in decimal text, not {}", show(value)))?;
        }
        Ok(())
    }
}

fn show(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn decimal(key: &str, value: &Value) -> Result<Decimal, String> {
    let text = value
        .as_str()
        .ok_or_else(|| format!("{key} must be exact decimal text, not {}", show(value)))?;
    Decimal::parse(text).map_err(|e| format!("{key}: {e}"))
}

fn order(key: &str, given: &Decimal, bound: &str) -> Result<Ordering, String> {
    let bound = Decimal::parse(bound).map_err(|e| format!("{key} bound: {e}"))?;
    given.cmp_exact(&bound).map_err(|e| format!("{key}: {e}"))
}

/// One comparable form for a fixed value: booleans accept the strings the compiler accepts, and
/// object ids compare as addresses, so `0x21` and its 64-digit form are the same object.
fn canonical(value: &Value) -> Option<String> {
    match value {
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => n.as_u64().map(|n| n.to_string()),
        Value::String(s) if s == "true" || s == "false" => Some(s.clone()),
        Value::String(s) => match s.strip_prefix("0x") {
            Some(hex)
                if !hex.is_empty()
                    && hex.len() <= 64
                    && hex.bytes().all(|b| b.is_ascii_hexdigit()) =>
            {
                Some(format!("0x{:0>64}", hex.to_ascii_lowercase()))
            }
            _ => Some(s.clone()),
        },
        _ => None,
    }
}

/// Whether two fixed values name the same thing.
pub fn same_value(a: &Value, b: &Value) -> bool {
    matches!((canonical(a), canonical(b)), (Some(a), Some(b)) if a == b)
}

/// The value a node publishes for `key`: inputs first, then config, as the compiler reads it.
pub fn published<'a>(node: &'a FlowNode, key: &str) -> Option<&'a Value> {
    node.inputs
        .as_ref()
        .and_then(|v| v.get(key))
        .or_else(|| node.config.as_ref().and_then(|v| v.get(key)))
        .filter(|v| !v.is_null())
}

/// A published boolean, read as the compiler reads it, with absent meaning `false`.
fn flag(node: &FlowNode, key: &str) -> Result<bool, String> {
    match published(node, key) {
        None => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(Value::String(s)) if s == "true" => Ok(true),
        Some(Value::String(s)) if s == "false" => Ok(false),
        Some(other) => Err(format!("{key} is published as {other}, not a boolean")),
    }
}

/// A published value that must be nonempty text. Anything else has no bound to compare against,
/// and a bounded key without one is refused rather than left open.
fn text<'a>(node: &'a FlowNode, key: &str) -> Result<&'a str, String> {
    match published(node, key) {
        Some(Value::String(s)) if !s.is_empty() => Ok(s),
        None => Err(format!("{key} has no published value to bound it")),
        Some(other) => Err(format!(
            "{key} is published as {other}, which is not text and cannot bound it"
        )),
    }
}

/// Whether `key` is counted in base units, so a fractional value is malformed rather than small.
pub fn base_units(key: &str) -> bool {
    matches!(key, "amount_in" | "amount" | "min_amount_out")
}

/// The bound on `key` for this node, from the node's own published values.
pub fn limit(node: &FlowNode, key: &str) -> Result<Limit, String> {
    let rule = rule(&node.kind, key)
        .ok_or_else(|| format!("{key} is not a runtime parameter of {}", node.kind))?;
    Ok(match rule {
        Rule::Fixed if matches!(key, "isBid" | "payWithDeep") => Limit {
            eq: Some(Value::Bool(flag(node, key)?)),
            ..Limit::default()
        },
        Rule::Fixed | Rule::Object => Limit {
            eq: Some(Value::String(text(node, key)?.to_owned())),
            ..Limit::default()
        },
        Rule::AtMost => Limit {
            max: Some(text(node, key)?.to_owned()),
            u64: base_units(key),
            ..Limit::default()
        },
        // No published floor means any floor tightens the action, so the bound starts at zero.
        Rule::AtLeast => Limit {
            min: Some(match published(node, key) {
                None => "0".to_owned(),
                Some(_) => {
                    let floor = text(node, key)?;
                    parse_u64_string(floor).map_err(|e| format!("{key}: {e}"))?;
                    floor.to_owned()
                }
            }),
            u64: true,
            ..Limit::default()
        },
        Rule::Price => {
            let price = text(node, key)?.to_owned();
            if flag(node, "isBid")? {
                Limit {
                    max: Some(price),
                    ..Limit::default()
                }
            } else {
                Limit {
                    min: Some(price),
                    ..Limit::default()
                }
            }
        }
        Rule::AnyU64 => Limit {
            u64: true,
            ..Limit::default()
        },
    })
}

/// Every bound this node can state, keyed by runtime key. A key whose published value cannot
/// bound it is left out, so an override of it is refused as unbounded.
pub fn limits(node: &FlowNode) -> serde_json::Map<String, Value> {
    keys(&node.kind)
        .filter_map(|(key, _)| {
            let bound = limit(node, key).ok()?;
            Some((key.to_owned(), serde_json::to_value(bound).ok()?))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow::FlowNode;
    use serde_json::json;

    fn node(kind: &str, config: Value) -> FlowNode {
        FlowNode {
            id: "n".into(),
            kind: kind.into(),
            config: Some(config),
            inputs: None,
        }
    }

    #[test]
    fn every_runtime_key_has_one_rule_and_keys_do_not_collide_across_kinds() {
        assert_eq!(rule("cetus_swap", "amount_in"), Some(Rule::AtMost));
        assert_eq!(rule("cetus_swap", "min_amount_out"), Some(Rule::AtLeast));
        assert_eq!(rule("haedal_stake", "amount"), Some(Rule::AtMost));
        assert_eq!(rule("deepbook_limit_order", "poolKey"), Some(Rule::Fixed));
        assert_eq!(rule("deepbook_limit_order", "isBid"), Some(Rule::Fixed));
        assert_eq!(
            rule("deepbook_limit_order", "tradeCapId"),
            Some(Rule::Object)
        );
        assert_eq!(rule("deepbook_limit_order", "price"), Some(Rule::Price));
        assert_eq!(
            rule("deepbook_limit_order", "clientOrderId"),
            Some(Rule::AnyU64)
        );
        assert_eq!(rule("cetus_swap", "pool"), None);
        assert_eq!(rule("guardrail", "minValue"), None);
        for (_, key, rule) in TABLE {
            assert_eq!(rule_for_key(key), Some(*rule), "{key}");
        }
    }

    #[test]
    fn limits_follow_the_published_node() {
        let ask = node(
            "deepbook_limit_order",
            json!({"poolKey":"SUI_USDC","price":"2","quantity":"1","isBid":"false"}),
        );
        assert_eq!(limit(&ask, "price").unwrap().min.as_deref(), Some("2"));
        assert_eq!(limit(&ask, "quantity").unwrap().max.as_deref(), Some("1"));
        assert_eq!(limit(&ask, "isBid").unwrap().eq, Some(json!(false)));
        assert_eq!(limit(&ask, "payWithDeep").unwrap().eq, Some(json!(false)));
        assert!(limit(&ask, "depositSui").is_err());
        let bid = node("deepbook_limit_order", json!({"price":"2","isBid":true}));
        assert_eq!(limit(&bid, "price").unwrap().max.as_deref(), Some("2"));
        let swap = node("cetus_swap", json!({"amount_in":"5"}));
        assert_eq!(
            limit(&swap, "min_amount_out").unwrap().min.as_deref(),
            Some("0")
        );
        let numeric = node("cetus_swap", json!({"min_amount_out":100}));
        assert!(limit(&numeric, "min_amount_out").is_err());
    }

    #[test]
    fn a_limit_checks_values_exactly() {
        let max = Limit {
            max: Some("1".into()),
            ..Limit::default()
        };
        assert!(max.check("quantity", &json!("1.0")).is_ok());
        assert!(max.check("quantity", &json!("1.01")).is_err());
        assert!(max.check("quantity", &json!(1)).is_err());
        let id = Limit {
            eq: Some(json!(format!("0x{}21", "0".repeat(62)))),
            ..Limit::default()
        };
        assert!(id.check("tradeCapId", &json!("0x21")).is_ok());
        assert!(id.check("tradeCapId", &json!("0x22")).is_err());
        let side = Limit {
            eq: Some(json!(false)),
            ..Limit::default()
        };
        assert!(side.check("isBid", &json!("false")).is_ok());
        assert!(side.check("isBid", &json!(true)).is_err());
        let order_id = Limit {
            u64: true,
            ..Limit::default()
        };
        assert!(order_id.check("clientOrderId", &json!("7")).is_ok());
        assert!(order_id.check("clientOrderId", &json!("1.5")).is_err());
        assert!(Limit::default().check("anything", &json!("1")).is_err());
    }
}
