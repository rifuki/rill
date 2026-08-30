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
    classify_failure, BalanceDelta, ChainError, ChainResult, ExecutionOutcome, ObjectRef,
    ObjectSummary, SimulationOutcome, SuiRead, SuiWrite, Verification,
};

/// Fields worth asking for on an object read. Requesting a mask rather than everything keeps the
/// response small, and makes it obvious at the call site what the caller actually depends on.
const OBJECT_MASK: &[&str] = &["object_id", "version", "digest", "object_type", "owner"];

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
                _ => ChainError::Transport(s.message().to_owned()),
            })?
            .into_inner();

        response
            .object
            .as_ref()
            .map(to_summary)
            .ok_or_else(|| ChainError::NotFound(format!("object {id}")))
    }

    async fn list_owned_objects(&self, owner: &str) -> ChainResult<Vec<ObjectSummary>> {
        let mut request = ListOwnedObjectsRequest::default();
        request.owner = Some(owner.to_owned());
        request.page_size = Some(50);
        request.read_mask = Some(GrpcSui::mask(OBJECT_MASK));

        let response = self
            .client
            .clone()
            .state_client()
            .list_owned_objects(request)
            .await
            .map_err(|s| ChainError::Transport(s.message().to_owned()))?
            .into_inner();

        Ok(response.objects.iter().map(to_summary).collect())
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
            .map_err(|s| ChainError::Transport(s.message().to_owned()))?
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
        // or, worse, be smoothed into something a caller treats as a checked result.
        let response = self
            .client
            .clone()
            .execution_client()
            .simulate_transaction(request)
            .await
            .map_err(|s| ChainError::Transport(s.message().to_owned()))?
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
        // or, worse, be smoothed into something a caller treats as a checked result.
        let response = self
            .client
            .clone()
            .execution_client()
            .simulate_transaction(request)
            .await
            .map_err(|s| ChainError::Transport(s.message().to_owned()))?
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
        request.signatures = signatures
            .iter()
            .map(|s| {
                let mut sig = sui_rpc::proto::sui::rpc::v2::UserSignature::default();
                sig.bcs = Some(s.as_bytes().to_vec().into());
                sig
            })
            .collect();

        let response = self
            .client
            .clone()
            .execution_client()
            .execute_transaction(request)
            .await
            .map_err(|s| ChainError::Rejected(s.message().to_owned()))?
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
            .map_err(|s| ChainError::Transport(s.message().to_owned()))?
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
