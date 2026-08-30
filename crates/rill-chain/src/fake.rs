//! An in-memory Sui, for every test that is not specifically about the network.
//!
//! This exists so the rest of the workspace can be tested exhaustively without a fullnode. The
//! reference implementation reached for mocking libraries at each call site instead, which meant
//! each test decided independently what the chain does — and a mock that agrees with a mistaken
//! assumption is worse than no test.
//!
//! It is a fake, not a mock: it holds real state, answers consistently, and lets a test say what
//! the chain should do rather than what a specific call should return.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::{
    BalanceDelta, ChainError, ChainResult, ExecutionOutcome, ObjectSummary, SimulationOutcome,
    SuiRead, SuiWrite, Verification,
};

/// What the fake should answer for the next simulation.
#[derive(Debug, Clone)]
pub enum SimulationBehavior {
    /// Succeeds, reporting this gas cost.
    Succeeds { gas_used_mist: u64 },
    /// Fails with this error, classified the way the real classifier would classify it.
    Fails { error: String },
    /// The node could not be reached. Distinct from a failure — a caller must not read this as a
    /// verdict about the transaction.
    Unreachable,
}

impl Default for SimulationBehavior {
    fn default() -> Self {
        Self::Succeeds {
            gas_used_mist: 1_000_000,
        }
    }
}

#[derive(Default)]
struct State {
    objects: HashMap<String, ObjectSummary>,
    owned: HashMap<String, Vec<String>>,
    balances: HashMap<(String, String), u64>,
    simulation: SimulationBehavior,
    executions: Vec<String>,
    next_digest: usize,
    /// What `simulate_read` hands back, command by command.
    read_returns: Vec<Vec<Vec<u8>>>,
}

/// A configurable in-memory chain.
#[derive(Default)]
pub struct FakeSui {
    state: RefCell<State>,
}

impl FakeSui {
    pub fn new() -> Self {
        Self::default()
    }

    /// Place an object on the fake chain, optionally owned by an address.
    pub fn with_object(self, owner: Option<&str>, object: ObjectSummary) -> Self {
        {
            let mut s = self.state.borrow_mut();
            let id = object.reference.id.clone();
            s.objects.insert(id.clone(), object);
            if let Some(owner) = owner {
                s.owned.entry(owner.to_owned()).or_default().push(id);
            }
        }
        self
    }

    pub fn with_balance(self, owner: &str, coin_type: &str, amount: u64) -> Self {
        self.state
            .borrow_mut()
            .balances
            .insert((owner.to_owned(), coin_type.to_owned()), amount);
        self
    }

    /// Stage the BCS bytes a read should return — a mid price, a quote, a balance.
    pub fn with_read_return(self, bytes: Vec<u8>) -> Self {
        self.state.borrow_mut().read_returns.push(vec![bytes]);
        self
    }

    pub fn with_simulation(self, behavior: SimulationBehavior) -> Self {
        self.state.borrow_mut().simulation = behavior;
        self
    }

    /// Every transaction submitted so far, in order — so a test can assert that nothing was
    /// submitted, which is the assertion that matters most for a signer's refusal paths.
    pub fn submitted(&self) -> Vec<String> {
        self.state.borrow().executions.clone()
    }
}

impl SuiRead for FakeSui {
    async fn get_object(&self, id: &str) -> ChainResult<ObjectSummary> {
        self.state
            .borrow()
            .objects
            .get(id)
            .cloned()
            .ok_or_else(|| ChainError::NotFound(format!("object {id}")))
    }

    async fn list_owned_objects(&self, owner: &str) -> ChainResult<Vec<ObjectSummary>> {
        let s = self.state.borrow();
        Ok(s.owned
            .get(owner)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| s.objects.get(id).cloned())
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn get_balance(&self, owner: &str, coin_type: &str) -> ChainResult<u64> {
        Ok(self
            .state
            .borrow()
            .balances
            .get(&(owner.to_owned(), coin_type.to_owned()))
            .copied()
            .unwrap_or(0))
    }

    async fn simulate(&self, _unsigned_tx_b64: &str) -> ChainResult<SimulationOutcome> {
        match self.state.borrow().simulation.clone() {
            SimulationBehavior::Succeeds { gas_used_mist } => Ok(SimulationOutcome {
                ok: true,
                verification: Verification::Verified,
                error: None,
                gas_used_mist,
                balance_changes: Vec::new(),
                command_output_count: 0,
                command_returns: Vec::new(),
            }),
            SimulationBehavior::Fails { error } => Ok(SimulationOutcome {
                ok: false,
                verification: crate::classify_failure(&error),
                error: Some(error),
                gas_used_mist: 0,
                balance_changes: Vec::new(),
                command_output_count: 0,
                command_returns: Vec::new(),
            }),
            SimulationBehavior::Unreachable => {
                Err(ChainError::Transport("fake node is unreachable".into()))
            }
        }
    }

    /// A read returns whatever `command_returns` was staged with, so a caller reading a price can
    /// be tested without a node.
    async fn simulate_read(&self, _unsigned_tx_b64: &str) -> ChainResult<SimulationOutcome> {
        let returns = self.state.borrow().read_returns.clone();
        Ok(SimulationOutcome {
            ok: true,
            verification: Verification::Verified,
            error: None,
            gas_used_mist: 0,
            balance_changes: Vec::new(),
            command_output_count: returns.len(),
            command_returns: returns,
        })
    }
}

impl SuiWrite for FakeSui {
    async fn execute(&self, tx_b64: &str, signatures: &[String]) -> ChainResult<ExecutionOutcome> {
        if signatures.is_empty() {
            // The real node refuses this too. Keeping the fake strict here means a test cannot
            // accidentally prove that an unsigned submission works.
            return Err(ChainError::Rejected(
                "execute requires at least one signature".into(),
            ));
        }
        let mut s = self.state.borrow_mut();
        s.executions.push(tx_b64.to_owned());
        s.next_digest += 1;
        let digest = format!("FakeDigest{}", s.next_digest);
        Ok(ExecutionOutcome {
            digest,
            success: true,
            error: None,
            gas_used_mist: 1_000_000,
            balance_changes: Vec::<BalanceDelta>::new(),
        })
    }

    async fn wait_for(&self, digest: &str) -> ChainResult<ExecutionOutcome> {
        Ok(ExecutionOutcome {
            digest: digest.to_owned(),
            success: true,
            error: None,
            gas_used_mist: 1_000_000,
            balance_changes: Vec::new(),
        })
    }
}
