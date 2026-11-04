//! The pre-flight rules, checked against what a signing tool is about to do.
//!
//! One checker for every tool. Each tool describes its transaction as a [`Proposal`]: the facts it
//! can actually read, and `None` for the ones it cannot. Which rules a tool answers for is not
//! decided here or by the tool, but by [`RuleKind::signer_tools`], the same table the wallet read
//! and the generated instructions print. So a rule is refused exactly where it is said to be.
//!
//! A rule that claims a tool whose proposal cannot supply the fact the rule limits is a refusal,
//! not a pass. A claim backed by no data would otherwise read as enforced and check nothing.
//!
//! No I/O. Every input is something the tool already holds before it touches the network, so a
//! refusal here costs no round trip and a test needs no node.

use rill_core::manifest::{CapabilityManifest, CapabilityRule, RuleKind, SignerTool};

/// What one signing tool is about to do, as far as it can tell before signing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    pub tool: SignerTool,
    /// Every package a Move call in the transaction targets.
    pub packages: Option<Vec<String>>,
    /// Every coin type the transaction moves.
    pub coin_types: Option<Vec<String>>,
    /// Every address the transaction sends something to.
    pub recipients: Option<Vec<String>>,
    /// Each swap's output coin and the least it may return, `None` when the tool cannot say.
    pub swap_floors: Option<Vec<(String, Option<u64>)>>,
}

impl Proposal {
    /// A proposal that reads nothing yet, to be filled with what this tool knows.
    pub fn new(tool: SignerTool) -> Self {
        Self {
            tool,
            packages: None,
            coin_types: None,
            recipients: None,
            swap_floors: None,
        }
    }
}

/// A pre-flight rule this transaction breaks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreFlightRefusal {
    pub kind: RuleKind,
    pub detail: String,
}

impl std::fmt::Display for PreFlightRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} refuses {}", self.kind.module(), self.detail)
    }
}

impl std::error::Error for PreFlightRefusal {}

/// Check every pre-flight rule in `manifest` that claims `proposal.tool`.
///
/// `infrastructure` is the packages a protocol scope never has to list: the framework and the
/// wallet's own packages, which every gated transaction calls.
pub fn check(
    manifest: &CapabilityManifest,
    infrastructure: &[String],
    proposal: &Proposal,
) -> Result<(), PreFlightRefusal> {
    for rule in &manifest.rules {
        let kind = rule.kind();
        if !kind.signer_tools().contains(&proposal.tool) {
            continue;
        }
        let refuse = |detail: String| Err(PreFlightRefusal { kind, detail });
        // Exhaustive, so a new kind is a compile error here rather than a rule that passes.
        match rule {
            CapabilityRule::Budget { .. }
            | CapabilityRule::PerTx { .. }
            | CapabilityRule::RateLimit { .. }
            | CapabilityRule::TimeWindow { .. }
            | CapabilityRule::StepUp { .. } => {}
            CapabilityRule::ProtocolScope { allowed_packages } => {
                let Some(packages) = &proposal.packages else {
                    return refuse(UNREADABLE.into());
                };
                let allowed: Vec<String> = allowed_packages
                    .iter()
                    .chain(infrastructure)
                    .map(|p| address(p))
                    .collect();
                if let Some(off) = packages.iter().find(|p| !allowed.contains(&address(p))) {
                    return refuse(format!("a call into {off}, a package outside the scope"));
                }
            }
            CapabilityRule::AssetScope { allowed_coin_types } => {
                let Some(coins) = &proposal.coin_types else {
                    return refuse(UNREADABLE.into());
                };
                let allowed: Vec<String> = allowed_coin_types.iter().map(|t| coin(t)).collect();
                if let Some(off) = coins.iter().find(|c| !allowed.contains(&coin(c))) {
                    return refuse(format!("moving {off}, a coin outside the scope"));
                }
            }
            CapabilityRule::RecipientAllowlist { addresses } => {
                let Some(recipients) = &proposal.recipients else {
                    return refuse(UNREADABLE.into());
                };
                let allowed: Vec<String> = addresses.iter().map(|a| address(a)).collect();
                if let Some(off) = recipients.iter().find(|r| !allowed.contains(&address(r))) {
                    return refuse(format!("sending to {off}, an address not on the allowlist"));
                }
            }
            CapabilityRule::SlippageFloor {
                min_out_mist,
                coin_type,
            } => {
                let Some(floors) = &proposal.swap_floors else {
                    return refuse(UNREADABLE.into());
                };
                let Ok(least) = rill_core::amounts::parse_u64_string(min_out_mist) else {
                    return refuse(format!("a floor of {min_out_mist:?}, which is not a u64"));
                };
                let floored = coin_type.as_deref().map(coin);
                for (output, min) in floors {
                    // A floor that names a coin binds only the swaps that output it: the two
                    // numbers share units only then.
                    if floored.as_ref().is_some_and(|c| *c != coin(output)) {
                        continue;
                    }
                    match min {
                        Some(min) if *min >= least => {}
                        Some(min) => {
                            return refuse(format!(
                                "a swap to {output} that accepts {min}, below the floor of {least}"
                            ))
                        }
                        None => {
                            return refuse(format!(
                                "a swap to {output} whose least output cannot be read"
                            ))
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Said when a rule claims a tool that cannot supply the fact the rule limits.
const UNREADABLE: &str = "this transaction: the tool cannot read what the rule limits";

/// An address in the one spelling the chain uses, or the text as written when it is not one, so
/// it can only ever equal itself.
fn address(value: &str) -> String {
    value
        .trim()
        .parse::<sui_sdk_types::Address>()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| value.to_owned())
}

/// A coin type in the one spelling the chain uses, the same normalisation the builder applies.
fn coin(value: &str) -> String {
    value
        .trim()
        .parse::<sui_sdk_types::TypeTag>()
        .map(|t| t.to_string())
        .unwrap_or_else(|_| value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALLOWED: &str = "0x00000000000000000000000000000000000000000000000000000000000000be";
    const OTHER: &str = "0x00000000000000000000000000000000000000000000000000000000000000de";
    const USDC: &str =
        "0x00000000000000000000000000000000000000000000000000000000000000c1::usdc::USDC";

    fn manifest(rule: CapabilityRule) -> CapabilityManifest {
        CapabilityManifest {
            wallet_coin_type: "0x2::sui::SUI".into(),
            rules: vec![rule],
        }
    }

    fn infrastructure() -> Vec<String> {
        vec!["0x1".into(), "0x2".into(), "0x3".into(), "0xa11e7".into()]
    }

    fn refused(rule: CapabilityRule, proposal: &Proposal) -> Option<RuleKind> {
        check(&manifest(rule), &infrastructure(), proposal)
            .err()
            .map(|refusal| refusal.kind)
    }

    /// A rule that claims a tool must have the fact it limits. Without it the claim would be a
    /// pass that checked nothing, which reads exactly like enforcement.
    #[test]
    fn a_claimed_rule_with_no_data_refuses() {
        let proposal = Proposal::new(SignerTool::Spend);
        assert_eq!(
            refused(
                CapabilityRule::RecipientAllowlist {
                    addresses: vec![ALLOWED.into()],
                },
                &proposal
            ),
            Some(RuleKind::RecipientAllowlist)
        );
    }

    /// A tool the rule does not claim is not checked by it, which is the stated scope of the
    /// guarantee rather than a gap in it: the wallet read says which tools refuse.
    #[test]
    fn a_rule_does_not_check_a_tool_it_does_not_claim() {
        let mut proposal = Proposal::new(SignerTool::Stake);
        proposal.recipients = Some(vec![OTHER.into()]);
        proposal.coin_types = Some(vec!["0x2::sui::SUI".into()]);
        assert_eq!(
            refused(
                CapabilityRule::RecipientAllowlist {
                    addresses: vec![ALLOWED.into()],
                },
                &proposal
            ),
            None
        );
    }

    #[test]
    fn a_recipient_is_compared_as_an_address() {
        let mut proposal = Proposal::new(SignerTool::Spend);
        proposal.recipients = Some(vec!["0xbe".into()]);
        let rule = || CapabilityRule::RecipientAllowlist {
            addresses: vec![ALLOWED.into()],
        };
        assert_eq!(
            refused(rule(), &proposal),
            None,
            "0xbe is the allowed address"
        );
        proposal.recipients = Some(vec![OTHER.into()]);
        assert_eq!(
            refused(rule(), &proposal),
            Some(RuleKind::RecipientAllowlist)
        );
    }

    #[test]
    fn a_coin_is_compared_as_a_type() {
        let mut proposal = Proposal::new(SignerTool::Spend);
        proposal.coin_types = Some(vec![
            "0x0000000000000000000000000000000000000000000000000000000000000002::sui::SUI".into(),
        ]);
        let rule = |allowed: &str| CapabilityRule::AssetScope {
            allowed_coin_types: vec![allowed.into()],
        };
        assert_eq!(refused(rule("0x2::sui::SUI"), &proposal), None);
        assert_eq!(refused(rule(USDC), &proposal), Some(RuleKind::AssetScope));
    }

    #[test]
    fn a_package_outside_the_scope_refuses_and_infrastructure_never_does() {
        let mut proposal = Proposal::new(SignerTool::Execute);
        let rule = || CapabilityRule::ProtocolScope {
            allowed_packages: vec![ALLOWED.into()],
        };
        proposal.packages = Some(vec!["0x2".into(), "0xa11e7".into(), "0xbe".into()]);
        assert_eq!(refused(rule(), &proposal), None);
        proposal.packages = Some(vec![OTHER.into()]);
        assert_eq!(refused(rule(), &proposal), Some(RuleKind::ProtocolScope));
    }

    #[test]
    fn a_floor_binds_only_the_coin_it_names_and_refuses_an_unreadable_one() {
        let mut proposal = Proposal::new(SignerTool::Swap);
        let named = || CapabilityRule::SlippageFloor {
            min_out_mist: "1000".into(),
            coin_type: Some(USDC.into()),
        };
        proposal.swap_floors = Some(vec![("0x2::sui::SUI".into(), Some(1))]);
        assert_eq!(
            refused(named(), &proposal),
            None,
            "a SUI output is not floored in USDC"
        );
        proposal.swap_floors = Some(vec![(USDC.into(), Some(999))]);
        assert_eq!(refused(named(), &proposal), Some(RuleKind::SlippageFloor));
        proposal.swap_floors = Some(vec![(USDC.into(), Some(1000))]);
        assert_eq!(
            refused(named(), &proposal),
            None,
            "the floor itself is allowed"
        );
        proposal.swap_floors = Some(vec![(USDC.into(), None)]);
        assert_eq!(refused(named(), &proposal), Some(RuleKind::SlippageFloor));

        let every = CapabilityRule::SlippageFloor {
            min_out_mist: "1000".into(),
            coin_type: None,
        };
        proposal.swap_floors = Some(vec![("0x2::sui::SUI".into(), Some(1))]);
        assert_eq!(refused(every, &proposal), Some(RuleKind::SlippageFloor));
    }

    /// The chain holds these, on every path. The signer does not repeat them.
    #[test]
    fn an_on_chain_rule_is_never_a_pre_flight_refusal() {
        let proposal = Proposal::new(SignerTool::Spend);
        for rule in [
            CapabilityRule::Budget {
                total_mist: "1".into(),
            },
            CapabilityRule::PerTx {
                max_mist: "1".into(),
            },
            CapabilityRule::RateLimit {
                window_ms: "1".into(),
                max_mist: "1".into(),
            },
            CapabilityRule::TimeWindow {
                not_before_ms: "1".into(),
                not_after_ms: "2".into(),
            },
        ] {
            assert_eq!(refused(rule, &proposal), None);
        }
    }
}
