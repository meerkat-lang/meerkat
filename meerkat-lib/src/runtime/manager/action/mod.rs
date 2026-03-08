//! when transaction is received by manager
//! we allocate a new transaction manager on it to monitor the transaction
use std::collections::{HashMap, HashSet};

use serde::de;
use tokio::sync::mpsc::Sender;

use crate::{
    ast::Expr,
    runtime::{
        message::CmdMsg,
        transaction::{Txn, TxnId},
    },
};

pub mod do_action;
pub mod txn_manager;

#[derive(Clone, Debug)]
pub struct TxnManager {
    pub txn: Txn,
    /// channel to client who submitted the txn
    pub from_client: Sender<CmdMsg>,

    /// map of each read to the state
    pub direct_reads: HashMap<String, DirectReadState>, // direct read
    pub trans_reads: HashMap<String, TransReadState>, // transitive read (var only)
    /// .. to write state
    pub writes: HashMap<String, WriteState>,
    /// preds to apply this transaction
    pub preds: HashSet<Txn>,
}

/// states of transitive read (we need request lock for these names)
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TransReadState {
    Requested,              // default
    Granted(Option<TxnId>), // lock granted
    Aborted,                // lock aborted
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DirectReadState {
    RequestedAndDepend(HashSet<String>), // requested read to name, depend on
    Read(Expr),                          // read result received
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum WriteState {
    Requested, // default
    Granted,   // lock granted
    Aborted,   // lock aborted
    Writed,    // successfully write to name
}

impl TxnManager {
    pub fn new(
        txn: Txn,
        from_client: Sender<CmdMsg>,
        direct_reads: HashSet<String>,
        dep_tran_vars: &HashMap<String, HashSet<String>>,
        writes: HashSet<String>,
    ) -> Self {
        let mut direct_read_states = HashMap::new();
        let mut trans_read_states = HashMap::new();

        for name in direct_reads.iter() {
            let name_trans_read = dep_tran_vars
                .get(name)
                .expect(&format!("dep vars not found"))
                .clone();

            for name in name_trans_read.iter() {
                trans_read_states.insert(name.clone(), TransReadState::Requested);
            }
            direct_read_states.insert(
                name.clone(),
                DirectReadState::RequestedAndDepend(name_trans_read),
            );
        }

        let write_states = HashMap::from_iter(
            writes
                .iter()
                .map(|name| (name.clone(), WriteState::Requested)),
        );

        TxnManager {
            txn,
            from_client,
            direct_reads: direct_read_states,
            trans_reads: trans_read_states,
            writes: write_states,
            preds: HashSet::new(),
        }
    }
}
