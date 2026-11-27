//! Registrations cover execution before commit and recursive abort cleanup.
//! Dropping a registration removes bookkeeping, not the transaction's locks.
use super::{EvalError, Transaction, TxnId};
use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Default)]
struct State {
    count: usize,
    doomed: bool,
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
    pub(crate) fn finish<T>(&self, result: Result<T, EvalError>) -> Result<T, EvalError> {
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

    pub(super) fn into_inner(self) -> Transaction {
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
