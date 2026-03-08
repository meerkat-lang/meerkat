use core::panic;
use std::collections::{BTreeMap, HashMap};

use kameo::actor::ActorRef;

use super::{manager::Manager, transaction::TxnId};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LockKind {
    Read,
    Write,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Lock {
    pub lock_kind: LockKind,
    pub txn_id: TxnId,
}

/// TODO: think more carefully about order between Read and Write??
impl PartialOrd for Lock {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.txn_id.cmp(&other.txn_id))
    }
}

impl Ord for Lock {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.txn_id.cmp(&other.txn_id)
    }
}

impl Lock {
    pub fn new_read(txn_id: TxnId) -> Self {
        Lock {
            lock_kind: LockKind::Read,
            txn_id,
        }
    }

    pub fn new_write(txn_id: TxnId) -> Self {
        Lock {
            lock_kind: LockKind::Write,
            txn_id,
        }
    }

    pub fn is_read(&self) -> bool {
        self.lock_kind == LockKind::Read
    }

    pub fn is_write(&self) -> bool {
        self.lock_kind == LockKind::Write
    }
}

/// lock state for an actor
/// where support:
/// 1. peek min granted lock (for wait-die checking)
/// 2. peek and pop min, delete arbitrary waiting lock
pub struct LockState {
    pub granted_locks: BTreeMap<TxnId, Lock>, // current lock granted
    pub waiting_locks: BTreeMap<TxnId, (Lock, ActorRef<Manager>)>, // locks waiting to be granted
}

impl LockState {
    pub fn new() -> Self {
        LockState {
            granted_locks: BTreeMap::new(),
            waiting_locks: BTreeMap::new(),
        }
    }

    /// only allow lock older than all granted locks to wait
    /// return true if lock added to waiting list
    pub fn add_wait(&mut self, lock: Lock, who_request: ActorRef<Manager>) -> bool {
        if let Some((oldest, _)) = self.granted_locks.first_key_value() {
            // if receive lock younger than oldest granted lock, ignore
            if lock.txn_id > *oldest {
                return false;
            }
        }

        self.waiting_locks
            .insert(lock.txn_id.clone(), (lock, who_request));

        return true;
    }

    fn pop_oldest_wait(&mut self) -> Option<(Lock, ActorRef<Manager>)> {
        self.waiting_locks.pop_first().map(|(_, res)| res)
    }

    pub fn grant_oldest_wait(&mut self) -> Option<(Lock, ActorRef<Manager>)> {
        assert!(self.check_granted_isvalid());
        if let Some((_, lock)) = self.granted_locks.first_key_value() {
            if lock.is_write() {
                // if current granted lock is write lock
                return None; // cannot grant another lock
            }
        }

        // if current granted lock are read locks
        if let Some((lock, mgr)) = self.pop_oldest_wait() {
            self.granted_locks.insert(lock.txn_id.clone(), lock.clone());

            assert!(self.check_granted_isvalid());
            return Some((lock, mgr));
        }
        None
    }

    pub fn remove_granted(&mut self, txn_id: &TxnId) -> Option<Lock> {
        self.granted_locks.remove(txn_id)
    }

    pub fn remove_granted_if_read(&mut self, txn_id: &TxnId) {
        if let Some(Lock {
            lock_kind: LockKind::Read,
            ..
        }) = self.granted_locks.get(txn_id)
        {
            self.remove_granted(txn_id);
        }
    }

    pub fn remove_wait(&mut self, txn_id: &TxnId) {
        self.waiting_locks.remove(txn_id);
    }

    pub fn remove_granted_or_wait(&mut self, txn_id: &TxnId) -> Option<Lock> {
        let mut res = None;
        if let Some(lock) = self.remove_granted(txn_id) {
            res = Some(lock);
        }
        if let Some((lock, _)) = self.waiting_locks.remove(txn_id) {
            if res.is_none() {
                res = Some(lock);
            } else {
                panic!("Txn {:?} has both granted and waiting lock", txn_id);
            }
        }
        res
    }

    pub fn clear_granted(&mut self) {
        self.granted_locks.clear();
    }

    pub fn has_granted(&self, txn_id: &TxnId) -> bool {
        self.granted_locks.contains_key(txn_id)
    }

    pub fn has_granted_write(&self, txn_id: &TxnId) -> bool {
        self.granted_locks.contains_key(txn_id)
            && self.granted_locks.get(txn_id).unwrap().lock_kind == LockKind::Write
    }

    pub fn check_granted_isvalid(&self) -> bool {
        //either all Read or unique Write lock
        self.granted_locks
            .iter()
            .all(|(_, lock)| lock.lock_kind == LockKind::Read)
            || self.granted_locks.len() == 1
    }
}
