use std::collections::{HashMap, HashSet};

use tokio::sync::mpsc::Sender;

use crate::{
    ast::Expr,
    runtime::{
        lock::LockKind,
        manager::{
            action::{DirectReadState, TransReadState, TxnManager, WriteState},
            Manager,
        },
        message::CmdMsg,
        transaction::{Txn, TxnId},
    },
};

impl TxnManager {
    /// when receive a granted lock from name,
    /// update transaction manager's read/write state
    pub fn add_grant_lock(&mut self, name: String, kind: LockKind, pred_id: Option<TxnId>) {
        if kind == LockKind::Read {
            assert!(self.trans_reads.get(&name) == Some(TransReadState::Requested).as_ref());
            self.trans_reads
                .insert(name, TransReadState::Granted(pred_id));
        } else {
            // notice in the case the transaction requires both read and write
            // lock on the name, we only send and receive the write lock request
            // and grant, but need additionally update the read lock also granted
            if self.trans_reads.contains_key(&name) {
                self.trans_reads
                    .insert(name.clone(), TransReadState::Granted(pred_id));
            }
            self.writes.insert(name, WriteState::Granted);
        }
    }

    /// when receive a finished read from name ..
    pub fn add_finished_read(&mut self, name: String, result: Expr, pred: HashSet<Txn>) {
        assert!(
            matches!(
                self.direct_reads.get(&name),
                Some(DirectReadState::RequestedAndDepend(_))
            ),
            "assertion fail on {:?}",
            self.direct_reads.get(&name)
        );
        self.direct_reads
            .insert(name, DirectReadState::Read(result));

        self.preds.extend(pred);
    }

    /// when receive a finished write from name ..
    pub fn add_finished_write(&mut self, name: String) {
        assert!(self.writes.get(&name) == Some(WriteState::Granted).as_ref());
        self.writes.insert(name, WriteState::Writed);
    }

    /// check if all locks are granted
    pub fn all_lock_granted(&self) -> bool {
        self.trans_reads
            .iter()
            .all(|(_, v)| matches!(v, TransReadState::Granted(_)))
            && self.writes.iter().all(|(_, v)| *v == WriteState::Granted)
    }

    /// check if all reads are finished
    pub fn all_read_finished(&self) -> bool {
        self.direct_reads
            .iter()
            .all(|(_, v)| matches!(v, DirectReadState::Read(_)))
    }

    /// get all read results
    pub fn get_read_results(&self) -> HashMap<String, Expr> {
        self.direct_reads
            .iter()
            .filter_map(|(name, state)| match state {
                DirectReadState::Read(result) => Some((name.clone(), result.clone())),
                _ => panic!("read not finished"),
            })
            .collect()
    }

    /// check if all writes are finished
    pub fn all_write_finished(&self) -> bool {
        self.writes.iter().all(|(_, v)| *v == WriteState::Writed)
    }

    /// record that the transaction is aborted due to lock aborted
    pub fn abort_lock(&mut self) {
        for (_, state) in self.trans_reads.iter_mut() {
            *state = TransReadState::Aborted;
        }
        for (_, state) in self.writes.iter_mut() {
            *state = WriteState::Aborted;
        }
    }

    /// check if any lock of thetransaction is aborted
    pub fn is_aborted(&self) -> bool {
        self.trans_reads
            .iter()
            .any(|(_, v)| *v == TransReadState::Aborted)
            || self.writes.iter().any(|(_, v)| *v == WriteState::Aborted)
    }

    pub fn get_client_sender(&self) -> Sender<CmdMsg> {
        self.from_client.clone()
    }
}

/// derived methods on service manager, wrt txn id
macro_rules! delegate_to_txn {
    // Mutable delegates take (&mut self, &TxnId, ...) ->  call &mut TxnManager
    (mut $fn_name:ident ( $($arg:ident : $arg_ty:ty),* ) ) => {
        pub fn $fn_name(&mut self, txn_id: &TxnId, $($arg : $arg_ty),* ) {
            let mgr = self.txn_mgrs
                .get_mut(txn_id)
                .expect("txn manager not found");
            mgr.$fn_name($($arg),*);
        }
    };
    // Immutable delegates take (&self, &TxnId) -> call &TxnManager
    (imm $fn_name:ident () -> $ret:ty) => {
        pub fn $fn_name(&self, txn_id: &TxnId) -> $ret {
            let mgr = self
                .txn_mgrs
                .get(txn_id)
                .expect("txn manager not found");
            mgr.$fn_name()
        }
    };
}

impl Manager {
    // pub fn new_txn_mgr(
    //     &mut self,
    //     txn: &Txn,
    //     from_client: Sender<CmdMsg>,
    //     read_set: HashSet<String>,
    //     write_set: HashSet<String>,
    // ) {
    //     let new_mgr = TxnManager::new(txn.clone(), from_client, read_set, write_set);
    //     self.txn_mgrs.insert(txn.id.clone(), new_mgr);
    // }

    // pub fn get_mut_txn_mgr(&mut self, txn_id: &TxnId) -> &mut TxnManager {
    //     self.txn_mgrs
    //         .get_mut(txn_id)
    //         .expect("txn manager not found")
    // }

    pub fn get_read_results(&self, txn_id: &TxnId) -> HashMap<String, Expr> {
        self.txn_mgrs
            .get(txn_id)
            .expect(&format!("txn manager not found"))
            .direct_reads
            .iter()
            .filter_map(|(name, state)| match state {
                DirectReadState::Read(result) => Some((name.clone(), result.clone())),
                _ => None,
            })
            .collect()
    }

    pub fn get_preds(&self, txn_id: &TxnId) -> HashSet<Txn> {
        self.txn_mgrs
            .get(txn_id)
            .expect(&format!("txn manager not found"))
            .preds
            .clone()
    }

    // invoke the macro to generate one‐line wrappers:
    delegate_to_txn!(mut add_grant_lock(name: String, kind: LockKind, pred_id: Option<TxnId>));
    delegate_to_txn!(mut add_finished_read(name: String, result: Expr, pred: HashSet<Txn>));
    delegate_to_txn!(mut add_finished_write(name: String));
    delegate_to_txn!(mut abort_lock());
    delegate_to_txn!(imm all_lock_granted() -> bool);
    delegate_to_txn!(imm all_read_finished() -> bool);
    delegate_to_txn!(imm all_write_finished() -> bool);
    delegate_to_txn!(imm is_aborted() -> bool);
    delegate_to_txn!(imm get_client_sender() -> Sender<CmdMsg>);
}
