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
    BalanceDelta, ChainError, ChainResult, CreatedObject, ExecutionOutcome, ObjectSummary,
    SimulationOutcome, SuiRead, SuiWrite, Verification,
};

/// What the fake should answer for the next simulation.
#[derive(Debug, Clone)]
pub enum SimulationBehavior {
    /// Succeeds, reporting this gas cost.
    Succeeds { gas_used_mist: u64 },
    /// Fails with this error, classified the way the real classifier would classify it.
    Fails { error: String },
    /// The node read the transaction and refused to run it: a gas coin at a version that has
    /// moved, a price below the reference. Distinct from `Fails`, where it ran and did not
    /// succeed, and from `Unreachable`, where nothing was learned.
    Rejected { message: String },
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

struct State {
    objects: HashMap<String, ObjectSummary>,
    owned: HashMap<String, Vec<String>>,
    balances: HashMap<(String, String), u64>,
    simulation: SimulationBehavior,
    executions: Vec<String>,
    next_digest: usize,
    /// What `simulate_read` hands back, command by command.
    read_returns: Vec<Vec<Vec<u8>>>,
    /// What successive reads hand back, one per read, the last repeating. Empty means the fake
    /// answers every read with `read_returns`.
    read_sequence: std::collections::VecDeque<Vec<u8>>,
    /// What an execution reports as brought into existence.
    created: Vec<CreatedObject>,
    /// What the fake network answers when asked its price, or why it cannot.
    reference_gas_price: ChainResult<u64>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            objects: HashMap::new(),
            owned: HashMap::new(),
            balances: HashMap::new(),
            simulation: SimulationBehavior::default(),
            executions: Vec::new(),
            next_digest: 0,
            read_returns: Vec::new(),
            read_sequence: std::collections::VecDeque::new(),
            created: Vec::new(),
            // Testnet's, at the time of writing. Real, and different from mainnet's 100. This is
            // the network's answer, which a caller reads; it is not a price a caller assumed.
            reference_gas_price: Ok(1_000),
        }
    }
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

    /// Stage what successive reads should answer, one entry per read, the last one repeating.
    ///
    /// A read asks about live state, and a transaction in between changes the answer: a wallet
    /// with no rules, then the same wallet with two. [`FakeSui::with_read_return`] can only give
    /// one answer however many times it is asked, so a path that reads, writes, and reads again to
    /// confirm what it wrote could not be driven offline at all.
    pub fn with_read_sequence(self, answers: Vec<Vec<u8>>) -> Self {
        self.state.borrow_mut().read_sequence = answers.into();
        self
    }

    /// Stage the objects an execution should report as created.
    ///
    /// A multi-step flow cannot continue without them: `create_wallet` shares a wallet and mints a
    /// capability, and both ids are knowable only from the transaction's effects. A fake that
    /// always answered "nothing was created" could not exercise the step that reads them out,
    /// which is the one step a person was doing by hand between the two commands.
    pub fn with_created(self, created: Vec<CreatedObject>) -> Self {
        self.state.borrow_mut().created = created;
        self
    }

    pub fn with_simulation(self, behavior: SimulationBehavior) -> Self {
        self.state.borrow_mut().simulation = behavior;
        self
    }

    /// Make the fake network answer a different price. A test that builds against a value the
    /// default does not use is the one that proves the price was read rather than remembered.
    pub fn with_reference_gas_price(self, price: u64) -> Self {
        self.state.borrow_mut().reference_gas_price = Ok(price);
        self
    }

    /// Make the price unreadable, so a build path can be shown to refuse rather than guess.
    pub fn with_reference_gas_price_unavailable(self) -> Self {
        self.state.borrow_mut().reference_gas_price =
            Err(ChainError::Transport("fake node is unreachable".into()));
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
            SimulationBehavior::Rejected { message } => Err(ChainError::Rejected(message)),
            SimulationBehavior::Unreachable => {
                Err(ChainError::Transport("fake node is unreachable".into()))
            }
        }
    }

    /// Testnet's value unless a test staged another, so a test that forgets is not silently
    /// building at mainnet's.
    async fn reference_gas_price(&self) -> ChainResult<u64> {
        self.state.borrow().reference_gas_price.clone()
    }

    /// A read returns whatever `command_returns` was staged with, so a caller reading a price can
    /// be tested without a node.
    async fn simulate_read(&self, _unsigned_tx_b64: &str) -> ChainResult<SimulationOutcome> {
        // A staged sequence answers one read at a time, and its last entry keeps answering. That
        // is what lets a test pose the same question before and after a transaction changed it.
        let mut state = self.state.borrow_mut();
        let returns = if state.read_sequence.is_empty() {
            state.read_returns.clone()
        } else {
            let answer = if state.read_sequence.len() == 1 {
                state.read_sequence[0].clone()
            } else {
                state.read_sequence.pop_front().expect("a staged answer")
            };
            vec![vec![answer]]
        };
        drop(state);
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
            created: s.created.clone(),
        })
    }

    async fn wait_for(&self, digest: &str) -> ChainResult<ExecutionOutcome> {
        Ok(ExecutionOutcome {
            digest: digest.to_owned(),
            success: true,
            error: None,
            gas_used_mist: 1_000_000,
            balance_changes: Vec::new(),
            created: Vec::new(),
        })
    }
}
