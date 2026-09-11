//! The real Sui client, behind the same traits the fake implements.
//!
//! Every proto type stays inside this file. A caller elsewhere in the workspace sees only this
//! crate's domain types, so swapping transports — or the proto version moving under us, which it
//! will while the Sui crates are still `0.x` — is a change here and nowhere else.

use sui_rpc::client::Client;
use sui_rpc::proto::sui::rpc::v2::{
    simulate_transaction_request::TransactionChecks, ExecuteTransactionRequest, GetObjectRequest,
    ListOwnedObjectsRequest, SimulateTransactionRequest,
};

use crate::{
    classify_failure, BalanceDelta, ChainError, ChainResult, CreatedObject, ExecutionOutcome,
    ObjectRef, ObjectSummary, SimulationOutcome, SuiRead, SuiWrite, Verification,
};

/// Fields worth asking for on an object read. Requesting a mask rather than everything keeps the
/// response small, and makes it obvious at the call site what the caller actually depends on.
const OBJECT_MASK: &[&str] = &["object_id", "version", "digest", "object_type", "owner"];

/// How many objects one `ListOwnedObjects` round trip asks for.
///
/// A round-trip size, never a cap: pages are followed until the node returns no token, so an
/// address holding more than this yields every object it has. The number used to be a cap by
/// accident, with the token never read, and an address busy enough to fill a page saw its gas
/// coins silently cut off at fifty.
pub const OWNED_OBJECTS_PAGE_SIZE: u32 = 50;

pub struct GrpcSui {
    client: Client,
}

impl GrpcSui {
    /// Connect to a fullnode. Cheap — the underlying channel connects lazily.
    pub fn new(endpoint: &str) -> ChainResult<Self> {
        Client::new(endpoint)
            .map(|client| Self { client })
            .map_err(|e| ChainError::Transport(e.to_string()))
    }

    fn mask(paths: &[&str]) -> prost_types::FieldMask {
        prost_types::FieldMask {
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
        }
    }

    /// Every object `owner` holds, walked `page_size` at a time.
    ///
    /// Public so a test can force a small page and prove the token path against a real node,
    /// because an address with fewer objects than one page never exercises it and a bug there is
    /// invisible until an address gets busy. Production goes through the trait, at
    /// [`OWNED_OBJECTS_PAGE_SIZE`].
    pub async fn list_owned_objects_paged(
        &self,
        owner: &str,
        page_size: u32,
    ) -> ChainResult<Vec<ObjectSummary>> {
        walk_pages(|token| async move {
            let mut request = ListOwnedObjectsRequest::default();
            request.owner = Some(owner.to_owned());
            request.page_size = Some(page_size);
            request.page_token = token;
            request.read_mask = Some(GrpcSui::mask(OBJECT_MASK));

            let response = self
                .client
                .clone()
                .state_client()
                .list_owned_objects(request)
                .await
                .map_err(refusal_or_transport)?
                .into_inner();

            Ok(Page {
                objects: response.objects.iter().map(to_summary).collect(),
                next: response.next_page_token,
            })
        })
        .await
    }

    /// The exact bytes of a transaction that already landed, base64 BCS.
    ///
    /// # Effects say a transaction worked, never what it did
    ///
    /// [`SuiWrite::wait_for`] reads a digest's effects, which report success, gas and balance
    /// changes. None of that names a single Move call, so a claim of the form "this digest placed a
    /// DeepBook order through the delegated capability" cannot be checked from effects at all. It
    /// was instead recorded in a commit message, which no test reads.
    ///
    /// These are the bytes the signature covered. Handed to the signer's own decoder they yield the
    /// Move call sequence that is on chain, so a recorded digest becomes an assertion rather than a
    /// note. It costs nothing and changes nothing: a read of a transaction that was already paid
    /// for.
    ///
    /// Inherent rather than on [`SuiRead`], because no production path needs it and every
    /// implementor of that trait would otherwise have to answer for a transaction history it does
    /// not have.
    ///
    /// # A recorded digest is not permanent evidence
    ///
    /// A public fullnode prunes. `GiL7unaYVnx7TF9QDtpUgc3nFSdWxVgkLb6sMDQfCm77`, recorded by commit
    /// `4ebe18a`, was gone from `fullnode.testnet.sui.io` eight days later, confirmed by `sui client
    /// tx-block` answering the same way. So `NotFound` here means what it says and is a normal
    /// outcome rather than a fault: the caller decides whether a digest it can no longer read is a
    /// failure. It is also why a test that only reads digests back is not enough on its own.
    pub async fn landed_transaction_base64(&self, digest: &str) -> ChainResult<String> {
        use sui_rpc::proto::sui::rpc::v2::GetTransactionRequest;
        let mut request = GetTransactionRequest::default();
        request.digest = Some(digest.to_owned());
        request.read_mask = Some(GrpcSui::mask(&["digest", "transaction.bcs"]));

        let response = self
            .client
            .clone()
            .ledger_client()
            .get_transaction(request)
            .await
            // The shared classifier sends every status that is not one of Sui's two refusal codes
            // to `Transport`, which is right for a build or a submit: there, not knowing is the
            // thing the caller must not mistake for an answer. Here an unknown digest *is* the
            // answer, and reporting it as "could not reach the Sui node" would send a reader to
            // check a network that is fine. So this one status is translated at this one call site
            // rather than changed for every path that shares the classifier.
            .map_err(|status| match status.code() {
                tonic::Code::NotFound => ChainError::NotFound(status.message().to_owned()),
                _ => refusal_or_transport(status),
            })?
            .into_inner();

        // A node that answers without the bytes is not the same thing as a node that says the
        // digest is unknown, and it used to share NotFound with it. Callers handle pruning by moving
        // on, which is right for a digest the node does not have and wrong for a response missing
        // what the read mask asked for: the second is a fault here, and a caller that moves on has
        // turned it into a test that passes by skipping. Reducing the mask to ["digest"] made a live
        // test do exactly that, which is how this was found. Decoding an empty buffer would be worse
        // again, reporting "this transaction called nothing", a false acquittal.
        let bytes = response
            .transaction
            .as_ref()
            .and_then(|t| t.transaction.as_ref())
            .and_then(|t| t.bcs.as_ref())
            .and_then(|b| b.value.as_ref())
            .ok_or_else(|| {
                ChainError::Malformed(format!(
                    "the node answered for {digest} but carried no transaction bytes, so the read \
                     mask did not ask for them"
                ))
            })?;

        use base64::Engine as _;
        Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
    }
}

/// One page of a listing, in this crate's terms.
struct Page<T> {
    objects: Vec<ObjectSummary>,
    next: Option<T>,
}

/// Follow a listing's page tokens to the end.
///
/// Separated from the transport so the walk itself can be tested without a node: the property
/// that matters is that the token is carried into the next request until the node stops sending
/// one, and that property does not need gRPC to be checked.
async fn walk_pages<T, F, Fut>(mut fetch: F) -> ChainResult<Vec<ObjectSummary>>
where
    F: FnMut(Option<T>) -> Fut,
    Fut: std::future::Future<Output = ChainResult<Page<T>>>,
{
    let mut all = Vec::new();
    let mut token = None;
    loop {
        let page = fetch(token.take()).await?;
        let empty = page.objects.is_empty();
        all.extend(page.objects);
        match page.next {
            // A token on an empty page would be a node that never finishes. Stop with what was
            // read rather than spin; a node that does this is broken in a way no loop can fix.
            Some(next) if !empty => token = Some(next),
            _ => return Ok(all),
        }
    }
}

/// What a gRPC status means at this boundary.
///
/// `InvalidArgument` and `FailedPrecondition` are answers: the node read the request and refused
/// it. There are no effects to carry an answer of that kind, so it arrives as a status rather than
/// as a failed outcome, and it is the earliest verdict Sui gives. Three refusals reach here and
/// all three are verdicts. A stale gas reference (`Transaction needs to be rebuilt because object
/// ... is unavailable for consumption`, verbatim from testnet). A price below the reference. And
/// an owner-signed spend presenting the agent's `AgentCap`, refused while the node checks input
/// objects, before any Move code runs, which is the object model doing its job and the first of
/// the two lines this product is built on.
///
/// Reporting any of them as "could not reach the node" sends whoever reads it to check a network
/// that is fine, while the transaction they hold will never work. Everything else is transport:
/// nothing was learned, and the caller must not treat it as a verdict.
fn refusal_or_transport(status: tonic::Status) -> ChainError {
    match status.code() {
        tonic::Code::InvalidArgument | tonic::Code::FailedPrecondition => {
            ChainError::Rejected(status.message().to_owned())
        }
        _ => ChainError::Transport(status.message().to_owned()),
    }
}

fn to_summary(o: &sui_rpc::proto::sui::rpc::v2::Object) -> ObjectSummary {
    ObjectSummary {
        reference: ObjectRef {
            id: o.object_id().to_owned(),
            version: o.version(),
            digest: o.digest().to_owned(),
        },
        object_type: o.object_type.clone(),
        fields: None,
        // `Owner.version` carries the initial shared version when the object is shared, so it is
        // read only for the SHARED kind — for an owned object the same field means nothing.
        shared_initial_version: o.owner.as_ref().and_then(|owner| {
            matches!(
                owner.kind(),
                sui_rpc::proto::sui::rpc::v2::owner::OwnerKind::Shared
            )
            .then(|| owner.version)
            .flatten()
        }),
    }
}

/// Objects created by a transaction, read from its effects.
///
/// `input_state == DOES_NOT_EXIST` is what marks a creation: the object had no prior version, so
/// this transaction is where it began. Reading `output_owner` at the same time answers the question
/// the caller actually has next — whether it is shared (and at which version) or owned, and by whom.
fn created_objects(
    effects: Option<&sui_rpc::proto::sui::rpc::v2::TransactionEffects>,
) -> Vec<CreatedObject> {
    use sui_rpc::proto::sui::rpc::v2::changed_object::InputObjectState;
    use sui_rpc::proto::sui::rpc::v2::owner::OwnerKind;

    let Some(effects) = effects else {
        return Vec::new();
    };
    effects
        .changed_objects
        .iter()
        .filter(|c| c.input_state() == InputObjectState::DoesNotExist)
        .map(|c| {
            let owner = c.output_owner.as_ref();
            CreatedObject {
                object_id: c.object_id().to_owned(),
                object_type: c.object_type.clone(),
                shared_initial_version: owner.and_then(|o| {
                    matches!(o.kind(), OwnerKind::Shared)
                        .then(|| o.version)
                        .flatten()
                }),
                owner: owner.and_then(|o| o.address.clone()),
            }
        })
        .collect()
}

fn balance_deltas(changes: &[sui_rpc::proto::sui::rpc::v2::BalanceChange]) -> Vec<BalanceDelta> {
    changes
        .iter()
        .map(|c| BalanceDelta {
            address: c.address().to_owned(),
            coin_type: c.coin_type().to_owned(),
            amount: c.amount().to_owned(),
        })
        .collect()
}

/// Net gas: computation plus storage, less the rebate. Saturating rather than wrapping — a rebate
/// larger than the cost is not a reason to report a gigantic number.
fn net_gas(effects: Option<&sui_rpc::proto::sui::rpc::v2::TransactionEffects>) -> u64 {
    effects
        .and_then(|e| e.gas_used.as_ref())
        .map(|g| {
            g.computation_cost()
                .saturating_add(g.storage_cost())
                .saturating_sub(g.storage_rebate())
        })
        .unwrap_or(0)
}

impl SuiRead for GrpcSui {
    async fn get_object(&self, id: &str) -> ChainResult<ObjectSummary> {
        let mut request = GetObjectRequest::default();
        request.object_id = Some(id.to_owned());
        request.read_mask = Some(GrpcSui::mask(OBJECT_MASK));

        let response = self
            .client
            .clone()
            .ledger_client()
            .get_object(request)
            .await
            .map_err(|s| match s.code() {
                tonic::Code::NotFound => ChainError::NotFound(format!("object {id}")),
                _ => refusal_or_transport(s),
            })?
            .into_inner();

        response
            .object
            .as_ref()
            .map(to_summary)
            .ok_or_else(|| ChainError::NotFound(format!("object {id}")))
    }

    async fn list_owned_objects(&self, owner: &str) -> ChainResult<Vec<ObjectSummary>> {
        self.list_owned_objects_paged(owner, OWNED_OBJECTS_PAGE_SIZE)
            .await
    }

    async fn get_balance(&self, owner: &str, coin_type: &str) -> ChainResult<u64> {
        use sui_rpc::proto::sui::rpc::v2::GetBalanceRequest;
        let mut request = GetBalanceRequest::default();
        request.owner = Some(owner.to_owned());
        request.coin_type = Some(coin_type.to_owned());

        let response = self
            .client
            .clone()
            .state_client()
            .get_balance(request)
            .await
            .map_err(refusal_or_transport)?
            .into_inner();

        Ok(response
            .balance
            .as_ref()
            .and_then(|b| b.balance)
            .unwrap_or(0))
    }

    async fn simulate(&self, unsigned_tx_b64: &str) -> ChainResult<SimulationOutcome> {
        let transaction = decode_transaction(unsigned_tx_b64)?;

        let mut request = SimulateTransactionRequest::default();
        request.transaction = Some(transaction);
        // Checks stay ENABLED. A simulation with checks off answers a different question than the
        // one the gate is asking.
        request.checks = Some(TransactionChecks::Enabled as i32);

        // A transport failure is NOT a verdict. It is returned as an error rather than as a failed
        // simulation, so a dropped connection can never read as "the transaction would fail" —
        // or, worse, be smoothed into something a caller treats as a checked result. A refusal
        // before execution IS a verdict, and is told apart from it: see `refusal_or_transport`.
        let response = self
            .client
            .clone()
            .execution_client()
            .simulate_transaction(request)
            .await
            .map_err(refusal_or_transport)?
            .into_inner();

        let executed = response.transaction.as_ref();
        let effects = executed.and_then(|t| t.effects.as_ref());
        let status = effects.and_then(|e| e.status.as_ref());
        let ok = status.and_then(|s| s.success).unwrap_or(false);
        let error = status
            .and_then(|s| s.error.as_ref())
            .and_then(|e| e.description.clone());

        let verification = if ok {
            Verification::Verified
        } else {
            classify_failure(error.as_deref().unwrap_or(""))
        };

        Ok(SimulationOutcome {
            ok,
            verification,
            error,
            gas_used_mist: net_gas(effects),
            balance_changes: executed
                .map(|t| balance_deltas(&t.balance_changes))
                .unwrap_or_default(),
            command_output_count: response.command_outputs.len(),
            command_returns: response
                .command_outputs
                .iter()
                .map(|c| {
                    c.return_values
                        .iter()
                        .filter_map(|v| v.value.as_ref())
                        .map(|b| b.value.clone().unwrap_or_default().to_vec())
                        .collect()
                })
                .collect(),
        })
    }

    async fn reference_gas_price(&self) -> ChainResult<u64> {
        use sui_rpc::proto::sui::rpc::v2::GetEpochRequest;

        let mut request = GetEpochRequest::default();
        // No epoch named means the current one.
        request.read_mask = Some(GrpcSui::mask(&["epoch", "reference_gas_price"]));

        let response = self
            .client
            .clone()
            .ledger_client()
            .get_epoch(request)
            .await
            .map_err(refusal_or_transport)?
            .into_inner();

        response
            .epoch
            .and_then(|e| e.reference_gas_price)
            .ok_or_else(|| {
                ChainError::NotFound(
                    "the node did not report a reference gas price; building against a guess \
                     would produce a transaction it may reject"
                        .into(),
                )
            })
    }

    async fn simulate_read(&self, unsigned_tx_b64: &str) -> ChainResult<SimulationOutcome> {
        let transaction = decode_transaction(unsigned_tx_b64)?;

        let mut request = SimulateTransactionRequest::default();
        request.transaction = Some(transaction);
        // Checks stay ON — a public fullnode applies them regardless of what this field asks for,
        // so turning them off buys nothing and would only misdescribe what ran. What makes a
        // keyless read work is the empty gas payment: the node selects and charges nothing against
        // a transaction it is only evaluating.
        request.checks = Some(TransactionChecks::Enabled as i32);
        request.do_gas_selection = Some(true);

        // A transport failure is NOT a verdict. It is returned as an error rather than as a failed
        // simulation, so a dropped connection can never read as "the transaction would fail" —
        // or, worse, be smoothed into something a caller treats as a checked result. A refusal
        // before execution IS a verdict, and is told apart from it: see `refusal_or_transport`.
        let response = self
            .client
            .clone()
            .execution_client()
            .simulate_transaction(request)
            .await
            .map_err(refusal_or_transport)?
            .into_inner();

        let executed = response.transaction.as_ref();
        let effects = executed.and_then(|t| t.effects.as_ref());
        let status = effects.and_then(|e| e.status.as_ref());
        let ok = status.and_then(|s| s.success).unwrap_or(false);
        let error = status
            .and_then(|s| s.error.as_ref())
            .and_then(|e| e.description.clone());

        let verification = if ok {
            Verification::Verified
        } else {
            classify_failure(error.as_deref().unwrap_or(""))
        };

        Ok(SimulationOutcome {
            ok,
            verification,
            error,
            gas_used_mist: net_gas(effects),
            balance_changes: executed
                .map(|t| balance_deltas(&t.balance_changes))
                .unwrap_or_default(),
            command_output_count: response.command_outputs.len(),
            command_returns: response
                .command_outputs
                .iter()
                .map(|c| {
                    c.return_values
                        .iter()
                        .filter_map(|v| v.value.as_ref())
                        .map(|b| b.value.clone().unwrap_or_default().to_vec())
                        .collect()
                })
                .collect(),
        })
    }
}

impl SuiWrite for GrpcSui {
    async fn execute(&self, tx_b64: &str, signatures: &[String]) -> ChainResult<ExecutionOutcome> {
        if signatures.is_empty() {
            return Err(ChainError::Rejected(
                "refusing to submit a transaction with no signature".into(),
            ));
        }
        let transaction = decode_transaction(tx_b64)?;

        let mut request = ExecuteTransactionRequest::default();
        request.transaction = Some(transaction);
        // Without a mask the response carries nothing, and a submitted transaction whose digest
        // comes back empty cannot be looked up afterwards — which is the one thing a caller
        // certainly wants after sending one.
        request.read_mask = Some(GrpcSui::mask(&["digest", "effects", "balance_changes"]));
        // The `bcs` field takes the signature's *bytes* — flag byte, signature, public key. The
        // base64 text must be decoded first: putting the text in directly makes the node read its
        // first character as the scheme flag, and it reports `invalid signature scheme: 4f`, which
        // is ASCII 'O' — the first character of a base64 string, not a scheme at all.
        //
        // Nothing short of a real submission finds this. A simulation carries no signature, so the
        // whole path was green up to the moment it mattered.
        let mut encoded = Vec::with_capacity(signatures.len());
        for signature in signatures {
            use base64::Engine as _;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(signature.trim())
                .map_err(|_| {
                    ChainError::Rejected(
                        "a signature that is not base64 cannot be submitted; it is not echoed here"
                            .into(),
                    )
                })?;
            let mut sig = sui_rpc::proto::sui::rpc::v2::UserSignature::default();
            sig.bcs = Some(bytes.into());
            encoded.push(sig);
        }
        request.signatures = encoded;

        let response = self
            .client
            .clone()
            .execution_client()
            .execute_transaction(request)
            .await
            // The same classifier as every read, and it matters more here than anywhere. Mapping
            // every status to a refusal told the caller the node had rejected a submission when
            // the connection had in fact dropped, and those call for opposite actions: a refusal
            // means this transaction will never land, while an unreachable node means the
            // outcome is unknown and the transaction may already be on chain. "Refused" invites
            // a retry, and retrying a submission whose fate you do not know is how one intended
            // spend becomes two attempts.
            .map_err(refusal_or_transport)?
            .into_inner();

        let executed = response.transaction.as_ref();
        let effects = executed.and_then(|t| t.effects.as_ref());
        let status = effects.and_then(|e| e.status.as_ref());

        Ok(ExecutionOutcome {
            digest: executed.and_then(|t| t.digest.clone()).unwrap_or_default(),
            success: status.and_then(|s| s.success).unwrap_or(false),
            error: status
                .and_then(|s| s.error.as_ref())
                .and_then(|e| e.description.clone()),
            gas_used_mist: net_gas(effects),
            balance_changes: executed
                .map(|t| balance_deltas(&t.balance_changes))
                .unwrap_or_default(),
            created: created_objects(effects),
        })
    }

    async fn wait_for(&self, digest: &str) -> ChainResult<ExecutionOutcome> {
        use sui_rpc::proto::sui::rpc::v2::GetTransactionRequest;
        let mut request = GetTransactionRequest::default();
        request.digest = Some(digest.to_owned());
        request.read_mask = Some(GrpcSui::mask(&["digest", "effects", "balance_changes"]));

        let response = self
            .client
            .clone()
            .ledger_client()
            .get_transaction(request)
            .await
            .map_err(refusal_or_transport)?
            .into_inner();

        let executed = response.transaction.as_ref();
        let effects = executed.and_then(|t| t.effects.as_ref());
        let status = effects.and_then(|e| e.status.as_ref());

        Ok(ExecutionOutcome {
            digest: digest.to_owned(),
            success: status.and_then(|s| s.success).unwrap_or(false),
            error: status
                .and_then(|s| s.error.as_ref())
                .and_then(|e| e.description.clone()),
            gas_used_mist: net_gas(effects),
            balance_changes: executed
                .map(|t| balance_deltas(&t.balance_changes))
                .unwrap_or_default(),
            created: created_objects(effects),
        })
    }
}

/// Decode a base64 BCS transaction into the proto wrapper the RPC expects.
fn decode_transaction(b64: &str) -> ChainResult<sui_rpc::proto::sui::rpc::v2::Transaction> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| ChainError::Rejected(format!("transaction is not valid base64: {e}")))?;
    let mut transaction = sui_rpc::proto::sui::rpc::v2::Transaction::default();
    transaction.bcs = Some(bytes.into());
    Ok(transaction)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ObjectRef;

    fn object(n: usize) -> ObjectSummary {
        ObjectSummary {
            reference: ObjectRef {
                id: format!("0x{n:064x}"),
                version: 1,
                digest: String::new(),
            },
            object_type: None,
            fields: None,
            shared_initial_version: None,
        }
    }

    /// A node holding `total` objects that hands out `page_size` per request and a token for the
    /// rest. The token is the index of the next object, which is all a token is.
    fn node(total: usize, page_size: usize) -> impl FnMut(Option<usize>) -> PageFuture {
        move |token| {
            let start = token.unwrap_or(0);
            let end = (start + page_size).min(total);
            let objects = (start..end).map(object).collect();
            let next = (end < total).then_some(end);
            Box::pin(async move { Ok(Page { objects, next }) })
        }
    }

    type PageFuture =
        std::pin::Pin<Box<dyn std::future::Future<Output = ChainResult<Page<usize>>>>>;

    /// The bug this guards: fifty-one objects on a fifty-object page, and the fifty-first is the
    /// gas coin.
    #[tokio::test]
    async fn every_page_is_followed_until_the_node_stops_sending_a_token() {
        let all = walk_pages(node(123, 50)).await.expect("walk");
        assert_eq!(all.len(), 123, "three pages, the last one short");
        let ids: Vec<&str> = all.iter().map(|o| o.reference.id.as_str()).collect();
        assert_eq!(ids[0], object(0).reference.id, "order is the node's order");
        assert_eq!(ids[122], object(122).reference.id);
        assert_eq!(
            ids.iter().collect::<std::collections::BTreeSet<_>>().len(),
            123,
            "no object is read twice"
        );
    }

    #[tokio::test]
    async fn a_single_short_page_needs_no_second_request() {
        let mut requests = 0;
        let mut fetch = node(7, 50);
        let all = walk_pages(|token| {
            requests += 1;
            fetch(token)
        })
        .await
        .expect("walk");
        assert_eq!(all.len(), 7);
        assert_eq!(requests, 1);
    }

    /// A node that keeps sending a token with nothing behind it would otherwise be followed
    /// forever.
    #[tokio::test]
    async fn an_empty_page_that_still_carries_a_token_ends_the_walk() {
        let mut requests = 0;
        let all = walk_pages(|_token: Option<usize>| {
            requests += 1;
            Box::pin(async {
                Ok(Page {
                    objects: Vec::new(),
                    next: Some(1usize),
                })
            }) as PageFuture
        })
        .await
        .expect("walk");
        assert!(all.is_empty());
        assert_eq!(requests, 1, "stopped rather than spun");
    }

    /// A failure on the third page must not come back as the first two pages' worth of objects,
    /// because a caller would read that as the whole set.
    #[tokio::test]
    async fn a_transport_failure_mid_walk_is_an_error_not_a_partial_set() {
        let mut requests = 0;
        let result = walk_pages(|token: Option<usize>| {
            requests += 1;
            let page = token.unwrap_or(0);
            Box::pin(async move {
                if page >= 2 {
                    Err(ChainError::Transport("dropped".into()))
                } else {
                    Ok(Page {
                        objects: vec![object(page)],
                        next: Some(page + 1),
                    })
                }
            }) as PageFuture
        })
        .await;
        assert_eq!(result, Err(ChainError::Transport("dropped".into())));
        assert_eq!(requests, 3);
    }

    /// The node's own words for a stale gas coin, and what this boundary makes of them.
    #[test]
    fn a_refused_input_is_a_rejection_and_a_dropped_connection_is_transport() {
        let stale = tonic::Status::invalid_argument(
            "Error checking transaction input objects: Transaction needs to be rebuilt because \
             object 0xe863 version 0x3bb012c3 (8GLu) is unavailable for consumption, current \
             version: 0x3bb012c4",
        );
        assert!(matches!(
            refusal_or_transport(stale),
            ChainError::Rejected(m) if m.contains("unavailable for consumption")
        ));
        assert!(matches!(
            refusal_or_transport(tonic::Status::invalid_argument(
                "Gas price 999 under reference gas price (RGP) 1000"
            )),
            ChainError::Rejected(_)
        ));
        assert!(matches!(
            refusal_or_transport(tonic::Status::unavailable("connection reset")),
            ChainError::Transport(_)
        ));
        assert!(matches!(
            refusal_or_transport(tonic::Status::deadline_exceeded("timed out")),
            ChainError::Transport(_)
        ));
    }

    /// The text a testnet node returned for an owner-signed spend presenting the agent's cap, with
    /// `InvalidArgument`. It is the earliest refusal Sui gives, and it is a refusal.
    const OWNERSHIP: &str = "Error checking transaction input objects: Transaction was not signed \
         by the correct sender: Object 0xea3f is owned by account address 0xb93c, but given \
         owner/signer address is 0xb649";

    #[test]
    fn a_refusal_before_execution_is_a_verdict_not_an_outage() {
        assert_eq!(
            refusal_or_transport(tonic::Status::invalid_argument(OWNERSHIP)),
            ChainError::Rejected(OWNERSHIP.to_owned()),
            "the node read the transaction and said no; that is an answer"
        );
    }

    /// The gate must fail closed on a node that did not answer, and it must say so: a dropped
    /// connection dressed up as a refusal sends someone to fix a transaction that was fine.
    #[test]
    fn an_unreachable_node_is_still_an_outage() {
        for status in [
            tonic::Status::unavailable("connection refused"),
            tonic::Status::deadline_exceeded("timed out"),
            tonic::Status::internal("node error"),
        ] {
            assert!(
                matches!(refusal_or_transport(status), ChainError::Transport(_)),
                "only a refusal the node actually made may be reported as one"
            );
        }
    }
}
