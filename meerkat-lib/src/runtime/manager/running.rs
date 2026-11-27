//! Registrations cover execution before commit and recursive abort cleanup.
//! Dropping a registration removes bookkeeping, not the transaction's locks.
use super::{Address, EvalError, ServiceNetId, Symbol, Transaction, TxnId};
use std::collections::{HashMap, HashSet};
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Default)]
struct State {
    count: usize,
    doomed: bool,
    aborting: bool,
    // Reentrant lock requests cannot borrow the outer transaction. Hand their
    // reservations back to its owner instead of creating a second pending txn.
    locked: HashSet<(ServiceNetId, Symbol)>,
    service_locked: HashSet<ServiceNetId>,
    participants: HashSet<Address>,
}

#[derive(Clone, Default)]
pub(super) struct RunningTransactions(Arc<Mutex<HashMap<TxnId, State>>>);

pub(super) struct Registration {
    registry: RunningTransactions,
    tid: TxnId,
}

/// Owns a checked-out transaction and its registration across awaits. Lock
/// cleanup is explicit; dropping a cancelled future only unregisters it.
pub(crate) struct RunningTransaction {
    txn: Transaction,
    registration: Option<Registration>,
}

impl Deref for RunningTransaction {
    type Target = Transaction;
    fn deref(&self) -> &Transaction {
        &self.txn
    }
}

impl DerefMut for RunningTransaction {
    fn deref_mut(&mut self) -> &mut Transaction {
        &mut self.txn
    }
}

impl RunningTransaction {
    /// Check the outcome before choosing commit, park, or abort. Once an
    /// attempt is discarded, cleanup callbacks must not change a decision to
    /// retry with a fresh ID. Parking under the same ID still needs a recheck.
    pub(crate) fn finish<T>(&mut self, result: Result<T, EvalError>) -> Result<T, EvalError> {
        self.collect_reservations();
        match &self.registration {
            Some(registration) => registration.finish(result),
            None => result,
        }
    }

    pub(crate) fn is_running(&self) -> bool {
        self.registration.is_some()
    }

    /// The caller must enter commit without awaiting after this check.
    pub(crate) fn begin_commit(&mut self) -> Result<(), EvalError> {
        self.finish(Ok(()))?;
        self.registration = None;
        Ok(())
    }

    fn collect_reservations(&mut self) {
        if let Some(registration) = &self.registration {
            let mut states = registration.registry.states();
            let state = states.get_mut(&self.txn.id).unwrap();
            self.txn.locked.extend(state.locked.drain());
            self.txn.service_locked.extend(state.service_locked.drain());
            self.txn.participants.extend(state.participants.drain());
        }
    }

    /// Only eager locking may reenter an executing transaction. A nested lock
    /// request hands its locks to the outer owner; only that owner may park.
    pub(super) fn park_locks(mut self) -> Option<Transaction> {
        self.collect_reservations();
        let registration = self.registration.as_ref().unwrap();
        let mut states = registration.registry.states();
        let state = states.get_mut(&self.txn.id).unwrap();
        if state.count > 1 {
            debug_assert!(self.txn.read_cache.is_empty() && self.txn.written.is_empty());
            state.locked.extend(self.txn.locked.drain());
            state.service_locked.extend(self.txn.service_locked.drain());
            state.participants.extend(self.txn.participants.drain());
            None
        } else {
            Some(self.txn)
        }
    }

    pub(super) fn into_inner(mut self) -> Transaction {
        self.collect_reservations();
        self.txn
    }
}

impl RunningTransactions {
    pub(super) fn checkout(&self, txn: Transaction) -> RunningTransaction {
        let registration = Some(self.enter(&txn.id));
        RunningTransaction { txn, registration }
    }
    fn states(&self) -> MutexGuard<'_, HashMap<TxnId, State>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(super) fn enter(&self, tid: &TxnId) -> Registration {
        self.states().entry(tid.clone()).or_default().count += 1;
        Registration {
            registry: self.clone(),
            tid: tid.clone(),
        }
    }

    pub(super) fn contains(&self, tid: &TxnId) -> bool {
        self.states().contains_key(tid)
    }

    pub(super) fn begin_abort(&self, tid: &TxnId) {
        self.states().get_mut(tid).unwrap().aborting = true;
    }

    pub(super) fn rejects_locks(&self, tid: &TxnId) -> bool {
        self.states()
            .get(tid)
            .is_some_and(|state| state.doomed || state.aborting)
    }

    pub(super) fn doom(&self, tid: &TxnId) -> bool {
        if let Some(state) = self.states().get_mut(tid) {
            state.doomed = true;
            true
        } else {
            false
        }
    }

    pub(super) fn check(&self, tid: &TxnId) -> Result<(), EvalError> {
        if self.states().get(tid).is_some_and(|state| state.doomed) {
            Err(EvalError::RunningTransaction(format!(
                "{tid:?} received an abort while running"
            )))
        } else {
            Ok(())
        }
    }
}

impl Registration {
    /// Preserve a terminal error's diagnostic, but never park/retry a doomed run.
    pub(super) fn finish<T>(&self, result: Result<T, EvalError>) -> Result<T, EvalError> {
        match result {
            Ok(value) => self.registry.check(&self.tid).map(|()| value),
            Err(e @ (EvalError::WaitOn(_) | EvalError::WaitDieAbort(_))) => {
                self.registry.check(&self.tid)?;
                Err(e)
            }
            Err(e) => Err(e),
        }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut states = self.registry.states();
        if let Some(state) = states.get_mut(&self.tid) {
            state.count -= 1;
            if state.count == 0 {
                states.remove(&self.tid);
            }
        }
    }
}
