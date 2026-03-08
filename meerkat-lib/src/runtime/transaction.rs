use std::hash::{Hash, Hasher};
use tokio::time::Instant;

use crate::ast::{Assn, Insert, Expr};

#[derive(PartialEq, Eq, Ord, Clone, Debug, Hash)]
pub struct TxnId {
    pub time: Instant,
    // TODO: add address or uuid to break ties
    pub iteration: u32,
}

impl TxnId {
    pub fn new() -> TxnId {
        TxnId {
            time: Instant::now(),
            iteration: 0,
        }
    }

    pub fn retry_id(&self) -> TxnId {
        TxnId {
            time: self.time,
            iteration: self.iteration + 1,
        }
    }
}

impl PartialOrd for TxnId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(
            self.time
                .cmp(&other.time)
                // NOTE: the order is flipped here, because we want higher iterations to have higher
                // priority, which means they must compare as Ordering::Less, opposite of usual ordering.
                .then(other.iteration.cmp(&self.iteration)),
        )
    }
}

// a single update to state var
#[derive(Clone, Debug)]
pub struct WriteToName {
    pub name: String,
    pub expr: Expr,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct TxnPred {
    pub id: TxnId,
    pub writes: Vec<String>,
}

// (txid, writes)
// writes := a list of updates to state vars
// Clone, PartialEq, Eq, Hash, Debug
#[derive(Clone, Debug)]
pub struct Txn {
    pub id: TxnId,
    pub assns: Vec<Assn>,
    pub inserts: Vec<Insert>,
}

impl PartialEq for Txn {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl Eq for Txn {}

impl Hash for Txn {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state)
    }
}

impl Txn {
    pub fn new(id: TxnId, assns: Vec<Assn>, inserts: Vec<Insert>) -> Txn {
        Txn { id, assns, inserts }
    }

    pub fn new_without_id(assns: Vec<Assn>) -> Txn {
        Txn {
            id: TxnId::new(),
            assns,
            inserts: Vec::new(),
        }
    }
}
