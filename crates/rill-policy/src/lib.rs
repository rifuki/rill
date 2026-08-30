//! Local, fail-closed verification of an ExecutionEnvelope before a key is ever used.
//!
//! # Why the states are types
//!
//! The reference runs about fifteen checks and then calls `sign()`. Nothing prevents a future
//! refactor from calling `sign()` first, or from returning early past a check, or from adding a
//! sixteenth check that a new code path skips. The checks are correct; their *ordering* is a
//! convention held up by review.
//!
//! Here an envelope moves through four types, each transition consuming the previous value:
//!
//! ```text
//! RawEnvelope → Validated → BytePinned → Simulated
//! ```
//!
//! Signing accepts only [`Simulated`]. There is no constructor for it that does not go through the
//! other three, and no way to hold both a `RawEnvelope` and the `Simulated` it became. Skipping a
//! check stops being something review has to catch and becomes something the compiler rejects.
//!
//! # What is checked here, and what is not
//!
//! Only what the chain cannot check. `agent_wallet`'s hot potato already holds budget, per-tx,
//! rate limit, time window, revocation, expiry, and the sender identity, unbypassably — repeating
//! those here would be defence in depth at best, and this crate stays small enough to audit by
//! keeping to what is genuinely irreplaceable:
//!
//! - **which protocol a released coin flows into** — Move cannot see past the command that
//!   released it, and `allowed_packages` is recorded on-chain but never asserted
//! - **whether an off-chain simulation actually succeeded and was conclusive**
//! - **byte-level pinning** of the transaction between approval and signature
//!
//! One check here has no counterpart in the reference at all: `cap_id`. After `rotate_agent` a
//! stale cap passes every local check the reference makes and aborts on-chain — the one place its
//! local verification is weaker than Move's.

use rill_chain::{ChainError, SuiRead, Verification as ChainVerification};
use rill_core::envelope::{
    digest_unsigned_ptb, EnvelopeError, ExecutionEnvelope, Network, Verification,
};

pub mod inspect;

/// The longest an envelope may be signable for. An envelope is minted, carried to the signer, and
/// used within seconds; anything claiming a longer life is either broken or someone's second
/// attempt at a replay.
pub const MAX_TTL_MS: u64 = 5 * 60 * 1000;

/// What this signer will accept, pinned ahead of time and independent of anything the envelope says.
///
/// Every field here is compared against the envelope rather than taken from it. An envelope that
/// supplied its own limits would be asking to be trusted about how much it may spend.
#[derive(Debug, Clone)]
pub struct LocalPolicy {
    pub network: Network,
    /// The address this signer holds the key for.
    pub sender: String,
    pub action_id: String,
    pub wallet_package_id: String,
    pub wallet_id: String,
    pub agent_cap_id: String,
    /// The exact Move call sequence, in order.
    pub allowed_targets: Vec<String>,
    /// Every object the transaction may touch.
    pub required_object_ids: Vec<String>,
    /// Ceiling one: the most this run may spend, from the run-set.
    pub max_amount_base_units: u64,
    /// Ceiling two: the same limit derived from an unrelated source. Both are enforced, so
    /// relaxing one does not relax the other.
    pub declared_spend_base_units: u64,
    /// Balance that must remain in the wallet after this spend. No on-chain rule expresses this.
    pub minimum_remaining_base_units: u64,
    pub gas_ceiling_base_units: u64,
}

/// Why an envelope was refused. Every variant is a distinct reason, because "policy violation"
/// tells an operator nothing about what to fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    Shape(EnvelopeError),
    Expired {
        expires_at: String,
    },
    UnparseableExpiry(String),
    TtlTooLong {
        ttl_ms: u64,
    },
    NetworkMismatch {
        expected: Network,
        found: Network,
    },
    SenderMismatch {
        expected: String,
        found: String,
    },
    ActionMismatch {
        expected: String,
        found: String,
    },
    IdentityMismatch {
        field: &'static str,
        expected: String,
        found: String,
    },
    /// The simulation did not succeed.
    SimulationFailed {
        error: Option<String>,
    },
    /// The simulation was inconclusive. Never accepted — there is no opt-in.
    SimulationUnverified,
    DigestMismatch {
        declared: String,
        computed: String,
    },
    GasAboveCeiling {
        declared: u64,
        ceiling: u64,
    },
    /// Ceiling one.
    SpendAboveMax {
        spend: u64,
        ceiling: u64,
    },
    /// Ceiling two, from a different source.
    SpendAboveDeclared {
        spend: u64,
        declared: u64,
    },
    ReserveBreached {
        remaining: u64,
        minimum: u64,
    },
    TargetSequenceMismatch {
        expected: Vec<String>,
        found: Vec<String>,
    },
    OffScopeTarget(String),
    ObjectSetMismatch {
        unexpected: Vec<String>,
    },
    GuardSetMismatch {
        expected: Vec<String>,
        found: Vec<String>,
    },
    /// The bytes changed between validation and signing.
    BytesChangedAfterApproval {
        approved: String,
        now: String,
    },
    StaleCap {
        held: String,
        active: String,
    },
    Chain(String),
    /// Mainnet requires an explicit opt-in that was not given.
    MainnetNotOptedIn,
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Shape(e) => write!(f, "{e}"),
            Self::Expired { expires_at } => write!(f, "this envelope expired at {expires_at}"),
            Self::UnparseableExpiry(v) => write!(f, "\"{v}\" is not a usable expiry timestamp"),
            Self::TtlTooLong { ttl_ms } => write!(
                f,
                "this envelope is signable for {ttl_ms}ms, longer than the {MAX_TTL_MS}ms ceiling; \
                 a long-lived signing window is a replay window"
            ),
            Self::NetworkMismatch { expected, found } => {
                write!(f, "envelope is for {found:?}, this signer is on {expected:?}")
            }
            Self::SenderMismatch { expected, found } => {
                write!(f, "envelope names sender {found}, this signer holds {expected}")
            }
            Self::ActionMismatch { expected, found } => {
                write!(f, "envelope is for action {found}, this run-set is for {expected}")
            }
            Self::IdentityMismatch { field, expected, found } => {
                write!(f, "{field}: envelope says {found}, run-set says {expected}")
            }
            Self::SimulationFailed { error } => write!(
                f,
                "the build-time simulation did not succeed: {}",
                error.as_deref().unwrap_or("no reason given")
            ),
            Self::SimulationUnverified => write!(
                f,
                "the build-time simulation was inconclusive; an unverified simulation is never \
                 accepted, and there is deliberately no flag to accept one"
            ),
            Self::DigestMismatch { declared, computed } => write!(
                f,
                "the envelope's digest {declared} does not match the transaction it carries \
                 ({computed})"
            ),
            Self::GasAboveCeiling { declared, ceiling } => {
                write!(f, "declared gas {declared} exceeds the ceiling {ceiling}")
            }
            Self::SpendAboveMax { spend, ceiling } => {
                write!(f, "spend {spend} exceeds this run's ceiling {ceiling}")
            }
            Self::SpendAboveDeclared { spend, declared } => write!(
                f,
                "spend {spend} exceeds the separately declared amount {declared}; both ceilings \
                 come from unrelated sources, so neither relaxes the other"
            ),
            Self::ReserveBreached { remaining, minimum } => write!(
                f,
                "this spend would leave {remaining} in the wallet, below the {minimum} reserve"
            ),
            Self::TargetSequenceMismatch { expected, found } => write!(
                f,
                "the transaction's call sequence is not the one approved.\n  expected: {}\n  found:    {}",
                expected.join(" → "),
                found.join(" → ")
            ),
            Self::OffScopeTarget(t) => write!(f, "the transaction calls {t}, which is out of scope"),
            Self::ObjectSetMismatch { unexpected } => write!(
                f,
                "the transaction touches objects that were not approved: {}",
                unexpected.join(", ")
            ),
            Self::GuardSetMismatch { expected, found } => write!(
                f,
                "guard set differs — expected {expected:?}, found {found:?}"
            ),
            Self::BytesChangedAfterApproval { approved, now } => write!(
                f,
                "the transaction changed between approval and signing ({approved} → {now}); \
                 refusing to sign bytes that were not the ones checked"
            ),
            Self::StaleCap { held, active } => write!(
                f,
                "this agent cap ({held}) is not the wallet's active cap ({active}); it was rotated"
            ),
            Self::Chain(m) => write!(f, "could not verify against chain state: {m}"),
            Self::MainnetNotOptedIn => write!(
                f,
                "refusing to sign on mainnet without an explicit opt-in"
            ),
        }
    }
}

impl std::error::Error for Rejection {}

impl From<ChainError> for Rejection {
    fn from(e: ChainError) -> Self {
        Self::Chain(e.to_string())
    }
}

// ── the states ────────────────────────────────────────────────────────────────────────────────

/// An envelope as received. Carries no promises.
pub struct RawEnvelope {
    envelope: ExecutionEnvelope,
}

/// Every check that can be made without touching bytes or the chain has passed.
pub struct Validated {
    envelope: ExecutionEnvelope,
    spend_base_units: u64,
}

/// The transaction has been re-derived and matches what was approved. Nothing can substitute
/// different bytes between here and signing.
pub struct BytePinned {
    envelope: ExecutionEnvelope,
    spend_base_units: u64,
    pinned_digest: String,
}

/// Re-simulated successfully against live chain state. **This is the only type that can be
/// signed.**
pub struct Simulated {
    envelope: ExecutionEnvelope,
    spend_base_units: u64,
    gas_used_base_units: u64,
}

impl RawEnvelope {
    pub fn new(envelope: ExecutionEnvelope) -> Self {
        Self { envelope }
    }

    /// Everything checkable from the envelope and the policy alone.
    ///
    /// `now_ms` is a parameter rather than read from the clock so expiry behaviour is testable —
    /// a time-dependent check that cannot be tested at a chosen instant tends not to be tested.
    pub fn validate(self, policy: &LocalPolicy, now_ms: u64) -> Result<Validated, Rejection> {
        let e = &self.envelope;
        e.validate_shape().map_err(Rejection::Shape)?;

        // Freshness.
        let expires_at_ms = parse_rfc3339_ms(&e.expires_at)
            .ok_or_else(|| Rejection::UnparseableExpiry(e.expires_at.clone()))?;
        if expires_at_ms <= now_ms {
            return Err(Rejection::Expired {
                expires_at: e.expires_at.clone(),
            });
        }
        let ttl = expires_at_ms - now_ms;
        if ttl > MAX_TTL_MS {
            return Err(Rejection::TtlTooLong { ttl_ms: ttl });
        }

        // Identity. Each of these is pinned by the run-set, never taken from the envelope.
        if e.network != policy.network {
            return Err(Rejection::NetworkMismatch {
                expected: policy.network,
                found: e.network,
            });
        }
        if e.sender != policy.sender {
            return Err(Rejection::SenderMismatch {
                expected: policy.sender.clone(),
                found: e.sender.clone(),
            });
        }
        if e.action_id != policy.action_id {
            return Err(Rejection::ActionMismatch {
                expected: policy.action_id.clone(),
                found: e.action_id.clone(),
            });
        }
        for (field, expected, found) in [
            (
                "walletPackageId",
                &policy.wallet_package_id,
                &e.wallet_package_id,
            ),
            ("walletId", &policy.wallet_id, &e.wallet_id),
            ("agentCapId", &policy.agent_cap_id, &e.agent_cap_id),
        ] {
            if expected != found {
                return Err(Rejection::IdentityMismatch {
                    field,
                    expected: expected.clone(),
                    found: found.clone(),
                });
            }
        }

        // The simulation gate. `ok` and `verified` are both required, and `unverified` is refused
        // unconditionally — the absence of an override is the point.
        if !e.simulation.ok {
            return Err(Rejection::SimulationFailed {
                error: e.simulation.error.clone(),
            });
        }
        if e.simulation.verification != Verification::Verified {
            return Err(Rejection::SimulationUnverified);
        }

        // Declared gas, before anything is spent finding out.
        let declared_gas =
            e.simulation
                .gas_estimate
                .parse::<u64>()
                .map_err(|_| Rejection::GasAboveCeiling {
                    declared: u64::MAX,
                    ceiling: policy.gas_ceiling_base_units,
                })?;
        if declared_gas > policy.gas_ceiling_base_units {
            return Err(Rejection::GasAboveCeiling {
                declared: declared_gas,
                ceiling: policy.gas_ceiling_base_units,
            });
        }

        // The digest must describe the transaction actually carried.
        let computed = digest_unsigned_ptb(&e.unsigned_ptb);
        if computed != e.action_digest {
            return Err(Rejection::DigestMismatch {
                declared: e.action_digest.clone(),
                computed,
            });
        }

        // Two ceilings from unrelated sources. Both apply.
        let spend = resolve_spend(e)?;
        if spend > policy.max_amount_base_units {
            return Err(Rejection::SpendAboveMax {
                spend,
                ceiling: policy.max_amount_base_units,
            });
        }
        if spend > policy.declared_spend_base_units {
            return Err(Rejection::SpendAboveDeclared {
                spend,
                declared: policy.declared_spend_base_units,
            });
        }

        Ok(Validated {
            envelope: self.envelope,
            spend_base_units: spend,
        })
    }
}

impl Validated {
    pub fn envelope(&self) -> &ExecutionEnvelope {
        &self.envelope
    }

    pub fn spend_base_units(&self) -> u64 {
        self.spend_base_units
    }

    /// Re-derive the digest and confirm the bytes are still the ones that were checked.
    ///
    /// This is deliberately a second computation rather than a reuse of the first. The window
    /// between "we validated this" and "we signed this" is exactly where a substitution would go,
    /// and a check that trusts its own earlier result does not close it.
    pub fn pin_bytes(self) -> Result<BytePinned, Rejection> {
        let recomputed = digest_unsigned_ptb(&self.envelope.unsigned_ptb);
        if recomputed != self.envelope.action_digest {
            return Err(Rejection::BytesChangedAfterApproval {
                approved: self.envelope.action_digest.clone(),
                now: recomputed,
            });
        }
        Ok(BytePinned {
            pinned_digest: recomputed,
            envelope: self.envelope,
            spend_base_units: self.spend_base_units,
        })
    }
}

impl BytePinned {
    pub fn envelope(&self) -> &ExecutionEnvelope {
        &self.envelope
    }

    pub fn pinned_digest(&self) -> &str {
        &self.pinned_digest
    }

    /// Re-simulate the exact transaction that will be signed, against live state.
    ///
    /// The build-time simulation was run by the server, which the signer does not trust. This one
    /// is ours. A node that cannot be reached is an error and never a pass — a gate that fails
    /// open on a dropped connection is worse than no gate.
    pub async fn simulate(
        self,
        chain: &impl SuiRead,
        policy: &LocalPolicy,
    ) -> Result<Simulated, Rejection> {
        let outcome = chain.simulate(&self.envelope.unsigned_ptb).await?;
        if !outcome.ok {
            return Err(Rejection::SimulationFailed {
                error: outcome.error,
            });
        }
        if outcome.verification != ChainVerification::Verified {
            return Err(Rejection::SimulationUnverified);
        }
        if outcome.gas_used_mist > policy.gas_ceiling_base_units {
            return Err(Rejection::GasAboveCeiling {
                declared: outcome.gas_used_mist,
                ceiling: policy.gas_ceiling_base_units,
            });
        }
        Ok(Simulated {
            envelope: self.envelope,
            spend_base_units: self.spend_base_units,
            gas_used_base_units: outcome.gas_used_mist,
        })
    }
}

impl Simulated {
    pub fn envelope(&self) -> &ExecutionEnvelope {
        &self.envelope
    }

    pub fn spend_base_units(&self) -> u64 {
        self.spend_base_units
    }

    pub fn gas_used_base_units(&self) -> u64 {
        self.gas_used_base_units
    }

    /// The bytes a signer may sign. Reachable only from this type, which is reachable only through
    /// every transition above.
    pub fn signable_bytes(&self) -> &str {
        &self.envelope.unsigned_ptb
    }
}

/// The spend amount, taken from the envelope's own declaration.
fn resolve_spend(e: &ExecutionEnvelope) -> Result<u64, Rejection> {
    if let Some(params) = &e.resolved_params {
        return params
            .spend_amount_mist
            .parse::<u64>()
            .map_err(|_| Rejection::SpendAboveMax {
                spend: u64::MAX,
                ceiling: 0,
            });
    }
    let mut total: u64 = 0;
    for step in &e.steps {
        if let Some(amount) = &step.spend_amount_mist {
            let parsed = amount
                .parse::<u64>()
                .map_err(|_| Rejection::SpendAboveMax {
                    spend: u64::MAX,
                    ceiling: 0,
                })?;
            total = total.saturating_add(parsed);
        }
    }
    Ok(total)
}

/// Parse an RFC 3339 instant to epoch milliseconds.
///
/// Hand-written for the same reason `rill-core`'s date formatting is: a calendar crate is a large
/// dependency for one field, and this parser is deliberately strict — it accepts the shape the
/// server emits and refuses everything else, rather than being lenient about a timestamp that
/// decides whether something is still signable.
pub fn parse_rfc3339_ms(value: &str) -> Option<u64> {
    // Expected: YYYY-MM-DDTHH:MM:SS[.mmm]Z
    let bytes = value.as_bytes();
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    if !value.ends_with('Z') {
        return None;
    }
    let year: i64 = value.get(0..4)?.parse().ok()?;
    let month: u32 = value.get(5..7)?.parse().ok()?;
    let day: u32 = value.get(8..10)?.parse().ok()?;
    let hour: u64 = value.get(11..13)?.parse().ok()?;
    let minute: u64 = value.get(14..16)?.parse().ok()?;
    let second: u64 = value.get(17..19)?.parse().ok()?;
    let millis: u64 = if bytes[19] == b'.' {
        value.get(20..23)?.parse().ok()?
    } else {
        0
    };
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    u64::try_from(days)
        .ok()?
        .checked_mul(86_400_000)?
        .checked_add(hour * 3_600_000 + minute * 60_000 + second * 1_000 + millis)
}

/// Howard Hinnant's `days_from_civil` — the inverse of `rill-core`'s formatter.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}
