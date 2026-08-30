//! The `ExecutionEnvelope` — what the keyless server hands the local signer.
//!
//! One definition, strict at every level. An envelope carrying any field beyond what is declared
//! here fails to deserialize rather than passing through, so a field cannot be smuggled in by a
//! future change and quietly relied on by one side only.
//!
//! ## On `price` and `quantity`
//!
//! The reference declares both as JSON numbers, which is where its float first enters the money
//! path — by the time DeepBook's scalar conversion runs, the value has already been through an
//! IEEE-754 double. Here they are [`Amount`]s: a string on the wire, and an exact decimal in
//! memory that never becomes a float.
//!
//! For the migration window, deserialization also accepts a JSON number, reading its decimal text
//! rather than its binary value. That tolerance exists so an envelope built by the TypeScript
//! server can still be validated by the Rust signer while the two are cut over separately, and it
//! should be removed once neither side emits numbers. Serialization always writes a string.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest as _, Sha256};

use crate::amounts::{AmountError, Decimal};

/// The envelope format version. A signer refuses anything it does not recognise.
pub const EXECUTION_ENVELOPE_VERSION: &str = "1";

/// A token amount on the wire: always written as a string, never as a float.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Amount(String);

impl Amount {
    /// Wrap a decimal string, validating that it parses exactly.
    pub fn parse(value: &str) -> Result<Self, AmountError> {
        Decimal::parse(value)?;
        Ok(Self(value.to_owned()))
    }

    /// The exact decimal. Infallible — the value was validated on construction.
    pub fn decimal(&self) -> Decimal {
        Decimal::parse(&self.0).expect("an Amount is validated when it is built")
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for Amount {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Amount {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        // A JSON number is accepted only through its decimal text — see the module note. Taking
        // the textual form is what keeps the binary double out of the arithmetic that follows.
        let raw = match serde_json::Value::deserialize(d)? {
            serde_json::Value::String(s) => s,
            serde_json::Value::Number(n) => n.to_string(),
            other => {
                return Err(D::Error::custom(format!(
                    "an amount must be a decimal string, got {other}"
                )))
            }
        };
        Amount::parse(&raw).map_err(D::Error::custom)
    }
}

/// Simulation verification. Two states only — a failure is the boolean `ok`, not a third variant,
/// so there is no way to express "failed but verified" or to widen this into an escape hatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verification {
    Verified,
    Unverified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    Testnet,
    Mainnet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ObjectChangeKind {
    Mutated,
    Created,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BalanceChange {
    pub owner: String,
    pub coin_type: String,
    /// Base units, as a string — a balance delta can exceed what a double holds exactly.
    pub amount: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ObjectChange {
    #[serde(rename = "type")]
    pub kind: ObjectChangeKind,
    pub object_id: String,
    pub object_type: String,
}

/// The result of the server's strict simulation. The signer accepts an envelope only when this
/// says `ok` **and** `verified`; anything else is refused before a key is touched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StrictSimulationResult {
    pub ok: bool,
    pub verification: Verification,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Gas in base units, as a string for the same reason balances are.
    pub gas_estimate: String,
    pub balance_changes: Vec<BalanceChange>,
    pub object_changes: Vec<ObjectChange>,
}

/// DeepBook order parameters, resolved by the server and pinned into the envelope so the signer
/// can check that the PTB it received actually encodes the order it was told about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeepBookResolvedParams {
    pub pool_key: String,
    pub pool_id: String,
    pub client_order_id: String,
    pub spend_amount_mist: String,
    pub price: Amount,
    pub quantity: Amount,
    pub deposit_sui: Amount,
    pub is_bid: bool,
    pub pay_with_deep: bool,
}

/// One step of a non-DeepBook flow (a swap, a stake), for the generic build path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnvelopeStep {
    pub node_id: String,
    pub kind: String,
    pub targets: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spend_amount_mist: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub object_ids: Vec<String>,
}

/// The unsigned build product. It carries no signature and no key material, and the signer
/// re-derives everything in it rather than trusting any of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutionEnvelope {
    pub version: String,
    pub action_id: String,
    /// SHA-256 over the base64 transaction string. Both sides derive it the same way, so byte
    /// drift between build and sign shows up as a mismatch rather than as a signed surprise.
    pub action_digest: String,
    pub network: Network,
    pub sender: String,
    pub wallet_package_id: String,
    pub wallet_id: String,
    pub agent_cap_id: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub balance_manager_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trade_cap_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_params: Option<DeepBookResolvedParams>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<EnvelopeStep>,

    pub allowed_targets: Vec<String>,
    pub required_object_ids: Vec<String>,
    pub required_guards: Vec<String>,

    pub unsigned_ptb: String,
    pub preview: String,
    pub simulation: StrictSimulationResult,
    /// RFC 3339. Short-lived by construction; the signer additionally caps how far ahead it may be.
    pub expires_at: String,
}

/// Why an envelope was refused at the schema level. Semantic checks — freshness, digest equality,
/// target scope — belong to the signer's policy, not here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvelopeError {
    UnsupportedVersion(String),
    /// Neither a DeepBook order nor a set of generic steps — the envelope describes nothing.
    NoActionShape,
    /// A DeepBook envelope missing part of the trio the signer's inspector requires.
    IncompleteDeepBookBinding,
}

impl std::fmt::Display for EnvelopeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedVersion(v) => {
                write!(f, "unsupported envelope version \"{v}\"; this signer speaks \"{EXECUTION_ENVELOPE_VERSION}\"")
            }
            Self::NoActionShape => write!(
                f,
                "envelope carries neither DeepBook resolved parameters nor any steps, so it \
                 describes no action to perform"
            ),
            Self::IncompleteDeepBookBinding => write!(
                f,
                "a DeepBook envelope must carry balanceManagerId, tradeCapId, and resolvedParams \
                 together"
            ),
        }
    }
}

impl std::error::Error for EnvelopeError {}

impl ExecutionEnvelope {
    /// Structural checks the shape alone cannot express.
    ///
    /// Deliberately narrow: this answers "is this a well-formed envelope", not "is it safe to
    /// sign". The second question is the signer's, and keeping it there is what stops a caller
    /// from mistaking a parsed envelope for an approved one.
    pub fn validate_shape(&self) -> Result<(), EnvelopeError> {
        if self.version != EXECUTION_ENVELOPE_VERSION {
            return Err(EnvelopeError::UnsupportedVersion(self.version.clone()));
        }
        let deepbook_parts = [
            self.balance_manager_id.is_some(),
            self.trade_cap_id.is_some(),
            self.resolved_params.is_some(),
        ];
        let deepbook_count = deepbook_parts.iter().filter(|p| **p).count();
        if deepbook_count > 0 && deepbook_count < deepbook_parts.len() {
            return Err(EnvelopeError::IncompleteDeepBookBinding);
        }
        if deepbook_count == 0 && self.steps.is_empty() {
            return Err(EnvelopeError::NoActionShape);
        }
        Ok(())
    }
}

/// SHA-256 over the **UTF-8 bytes of the base64 string**, hex-encoded lowercase.
///
/// Hashing the base64 text rather than the decoded bytes is what the reference does, and both
/// sides must agree on it exactly — so this is deliberately the same choice, not an improvement.
/// It is a hash, not a signature: it detects drift, it does not authorise anything.
pub fn digest_unsigned_ptb(unsigned_ptb: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(unsigned_ptb.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
