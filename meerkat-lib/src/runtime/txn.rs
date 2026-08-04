//! Transaction ID and per-variable lock state for the `Manager`

use crate::net::{Address, ServiceNetId};
use crate::runtime::ast::Value;
use crate::runtime::interner::Symbol;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use web_time::{SystemTime, UNIX_EPOCH};

/// Key identifying a resource in the lock wait queue (service or member)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum WaitKey {
    /// Service-level lock resource key
    Service(ServiceNetId),
    /// Member-level variable lock resource key
    Member(ServiceNetId, Symbol),
}

/// A globally unique transaction identifier represented by `TxnId`
///
/// Per the `Historiographer` design, a `tid` is a (unique node
/// identifier, timestamp) pair. The timestamp is nanoseconds since
/// the Unix epoch; the `node_id` makes IDs from different nodes
/// distinct and serves as the age tiebreaker. Serializable so it
/// can travel in cross-node messages
///
/// Age (for wait-die): older = smaller timestamp, with `node_id` as
/// tiebreaker. Higher `iteration` = higher priority among retries
/// of the same transaction
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TxnId {
    /// Creation time as nanoseconds since the Unix epoch, used as a
    /// logical clock: monotonically increasing per node so two
    /// transactions minted in the same wall-clock tick still get
    /// distinct IDs
    pub timestamp: u128,
    /// (Probabilistically) unique identifier of the originating node
    pub node_id: u64,
    /// Incremented on retry so retried transactions gain priority
    pub iteration: u32,
}

/// Last nanosecond timestamp handed out as a `TxnId` on this process
///
/// Acts as a logical clock: if the wall clock has not advanced past
/// it, we bump by one so two transactions in the same tick still
/// get distinct (and ordered) IDs
/// This guarantees uniqueness on a node, which matters because
/// `TxnId` aliases lock owners and keys participant-held transaction
/// state
static LAST_TXN_NANOS: Mutex<u128> = Mutex::new(0);

impl TxnId {
    /// Create a new, node-unique transaction ID originating from the given node
    pub fn new(node_id: u64) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        // Claim a timestamp strictly greater than any previously issued
        // one, even if the clock repeated or went backwards. A clock
        // error (`now == 0`) can no longer cause a collision because we
        // still take `last + 1`
        let timestamp = {
            let mut last = LAST_TXN_NANOS.lock().unwrap_or_else(|e| e.into_inner());
            let ts = now.max(*last + 1);
            *last = ts;
            ts
        };
        TxnId {
            timestamp,
            node_id,
            iteration: 0,
        }
    }

    /// Return a new `TxnId` with the same timestamp/node but higher
    /// iteration, for use when retrying an aborted transaction
    pub fn retry(&self) -> Self {
        TxnId {
            timestamp: self.timestamp,
            node_id: self.node_id,
            iteration: self.iteration + 1,
        }
    }
}

impl PartialOrd for TxnId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TxnId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Age ordering: older `timestamp` = smaller = higher priority, with
        // `node_id` as the tiebreaker. Higher `iteration` = higher priority among
        // retries of the same transaction
        self.timestamp
            .cmp(&other.timestamp)
            .then(self.node_id.cmp(&other.node_id))
            .then(other.iteration.cmp(&self.iteration))
    }
}

/// Per-variable lock state used by the `Manager`
///
/// Multiple readers are allowed simultaneously; writers are exclusive
#[derive(Debug, Clone)]
pub enum VarLock {
    Unlocked,
    ReadLocked(HashSet<TxnId>),
    WriteLocked(TxnId),
}

impl Default for VarLock {
    fn default() -> Self {
        Self::new()
    }
}

impl VarLock {
    pub fn new() -> Self {
        VarLock::Unlocked
    }

    /// Try to acquire a read lock, succeeding unless write-locked
    pub fn try_read(&mut self, txn_id: &TxnId) -> bool {
        match self {
            VarLock::Unlocked => {
                let mut set = HashSet::new();
                set.insert(txn_id.clone());
                *self = VarLock::ReadLocked(set);
                true
            }
            VarLock::ReadLocked(set) => {
                set.insert(txn_id.clone());
                true
            }
            VarLock::WriteLocked(_) => false,
        }
    }

    /// Try to acquire an exclusive write lock, failing if any lock is held
    pub fn try_write(&mut self, txn_id: &TxnId) -> bool {
        match self {
            VarLock::Unlocked => {
                *self = VarLock::WriteLocked(txn_id.clone());
                true
            }
            VarLock::ReadLocked(_) | VarLock::WriteLocked(_) => false,
        }
    }

    /// Release a read lock held by `txn_id`
    pub fn release_read(&mut self, txn_id: &TxnId) {
        if let VarLock::ReadLocked(set) = self {
            set.remove(txn_id);
            if set.is_empty() {
                *self = VarLock::Unlocked;
            }
        }
    }

    /// Release the write lock if currently held by `txn_id`
    pub fn release_write(&mut self, txn_id: &TxnId) {
        if matches!(self, VarLock::WriteLocked(tid) if tid == txn_id) {
            *self = VarLock::Unlocked;
        }
    }

    /// Upgrade a read lock held solely by `txn_id` to a write lock
    ///
    /// Needed for read-then-write patterns (e.g., `x = x + 1`)
    /// Returns `true` if upgrade succeeded or variable is already
    /// write-locked by `txn_id`
    pub fn upgrade_to_write(&mut self, txn_id: &TxnId) -> bool {
        match self {
            VarLock::ReadLocked(set) if set.len() == 1 && set.contains(txn_id) => {
                *self = VarLock::WriteLocked(txn_id.clone());
                true
            }
            VarLock::WriteLocked(tid) if tid == txn_id => true,
            VarLock::Unlocked => false,
            VarLock::ReadLocked(_) | VarLock::WriteLocked(_) => false,
        }
    }

    /// Release any lock (read or write) held by `txn_id`
    pub fn release(&mut self, txn_id: &TxnId) {
        match self {
            VarLock::ReadLocked(_) => self.release_read(txn_id),
            VarLock::WriteLocked(_) => self.release_write(txn_id),
            VarLock::Unlocked => {}
        }
    }

    /// Return the oldest transaction holding this lock (if any) that is not `ignore_tid`
    pub fn oldest_other_holder(&self, ignore_tid: &TxnId) -> Option<TxnId> {
        match self {
            VarLock::Unlocked => None,
            VarLock::ReadLocked(set) => set.iter().filter(|tid| *tid != ignore_tid).min().cloned(),
            VarLock::WriteLocked(tid) => {
                if tid != ignore_tid {
                    Some(tid.clone())
                } else {
                    None
                }
            }
        }
    }
}

/// Composite state for a single variable represented by `VarState`
///
/// Consolidates value, lock, and transaction history into one
/// structure instead of three separate maps
#[derive(Debug, Clone)]
pub struct VarState {
    /// Current value of the variable
    pub value: crate::runtime::ast::Value,
    /// Lock state for two-phase locking
    pub lock: VarLock,
    /// Most recent transaction to write this variable
    pub latest_write_txn: Option<TxnId>,
}

impl VarState {
    /// Create a new `VarState` instance
    pub fn new(value: crate::runtime::ast::Value) -> Self {
        VarState {
            value,
            lock: VarLock::new(),
            latest_write_txn: None,
        }
    }
}

/// Per-transaction state represented by `Transaction`
///
/// Owned by the code executing a transaction and passed around
/// during execution rather than stored on the `Manager`
///
/// A single `Manager` may eventually run multiple transactions
/// concurrently, so transaction state must not live on the `Manager`
#[derive(Debug)]
pub struct Transaction {
    /// Globally unique transaction identifier
    pub id: TxnId,
    /// Pairs of `(ServiceNetId, Symbol)` this transaction currently holds a lock on
    pub locked: HashSet<(ServiceNetId, Symbol)>,
    /// Services this transaction currently holds a whole-service lock on
    pub service_locked: HashSet<ServiceNetId>,
    /// Values already read in this transaction, keyed by (service, variable)
    /// (avoids re-fetching, including redundant network round-trips)
    pub read_cache: HashMap<(ServiceNetId, Symbol), Value>,
    /// Values written by this transaction, keyed by (service, variable),
    /// buffered and applied only on successful commit (so a failed
    /// transaction leaves no partial writes)
    pub written: HashMap<(ServiceNetId, Symbol), Value>,
    /// Remote nodes that joined this transaction (executed a composed action
    /// under this `id` and are holding locks or buffered writes until commit or abort)
    pub participants: HashSet<Address>,
}

impl Transaction {
    /// Create a new `Transaction` instance
    pub fn new(id: TxnId) -> Self {
        Transaction {
            id,
            locked: HashSet::new(),
            service_locked: HashSet::new(),
            read_cache: HashMap::new(),
            written: HashMap::new(),
            participants: HashSet::new(),
        }
    }
}

/// Outcome of a wait-die conflict check represented by `WaitDie`
///
/// The requesting transaction either waits for the lock (it is older /
/// higher priority than every conflicting holder) or dies and retries
/// (some current holder is older)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitDie {
    Wait,
    Die,
}

impl VarLock {
    /// Wait-die decision for a `requester` that conflicts with this lock
    ///
    /// Returns `Die` if any current holder is strictly older (smaller
    /// `TxnId`, higher priority) than the `requester`; otherwise `Wait`
    /// A holder equal to the `requester` is ignored, since a transaction
    /// never conflicts with itself
    pub fn wait_die(&self, requester: &TxnId) -> WaitDie {
        let holder_older = match self {
            VarLock::Unlocked => false,
            VarLock::WriteLocked(owner) => owner != requester && owner < requester,
            VarLock::ReadLocked(readers) => readers.iter().any(|h| h != requester && h < requester),
        };
        if holder_older {
            WaitDie::Die
        } else {
            WaitDie::Wait
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn txn(n: u128) -> TxnId {
        TxnId {
            timestamp: n,
            node_id: 1,
            iteration: 0,
        }
    }

    fn assert_unlocked(lock: &VarLock) {
        assert!(matches!(lock, VarLock::Unlocked));
    }

    fn assert_readers(lock: &VarLock, expected: &[TxnId]) {
        match lock {
            VarLock::ReadLocked(readers) => {
                assert_eq!(readers.len(), expected.len());
                for txn_id in expected {
                    assert!(readers.contains(txn_id));
                }
            }
            other => panic!("expected read lock, got {:?}", other),
        }
    }

    fn assert_writer(lock: &VarLock, expected: &TxnId) {
        assert!(matches!(lock, VarLock::WriteLocked(owner) if owner == expected));
    }

    #[test]
    fn test_unlocked_accepts_read_lock() {
        let txn_id = txn(1);
        let mut lock = VarLock::new();

        assert!(lock.try_read(&txn_id));
        assert_readers(&lock, &[txn_id]);
    }

    #[test]
    fn test_unlocked_accepts_write_lock() {
        let txn_id = txn(1);
        let mut lock = VarLock::new();

        assert!(lock.try_write(&txn_id));
        assert_writer(&lock, &txn_id);
    }

    #[test]
    fn test_multiple_read_locks_can_coexist() {
        let txn1 = txn(1);
        let txn2 = txn(2);
        let mut lock = VarLock::new();

        assert!(lock.try_read(&txn1));
        assert!(lock.try_read(&txn2));
        assert_readers(&lock, &[txn1, txn2]);
    }

    #[test]
    fn test_write_lock_blocks_read_lock_from_another_transaction() {
        let writer = txn(1);
        let reader = txn(2);
        let mut lock = VarLock::new();

        assert!(lock.try_write(&writer));
        assert!(!lock.try_read(&reader));
        assert_writer(&lock, &writer);
    }

    #[test]
    fn test_read_lock_blocks_write_lock_from_another_transaction() {
        let reader = txn(1);
        let writer = txn(2);
        let mut lock = VarLock::new();

        assert!(lock.try_read(&reader));
        assert!(!lock.try_write(&writer));
        assert_readers(&lock, &[reader]);
    }

    #[test]
    fn test_releasing_one_of_multiple_read_locks_keeps_remaining_read_lock() {
        let txn1 = txn(1);
        let txn2 = txn(2);
        let mut lock = VarLock::new();

        assert!(lock.try_read(&txn1));
        assert!(lock.try_read(&txn2));
        lock.release(&txn1);

        assert_readers(&lock, &[txn2]);
    }

    #[test]
    fn test_releasing_last_read_lock_unlocks() {
        let txn_id = txn(1);
        let mut lock = VarLock::new();

        assert!(lock.try_read(&txn_id));
        lock.release(&txn_id);

        assert_unlocked(&lock);
    }

    #[test]
    fn test_releasing_write_lock_unlocks() {
        let txn_id = txn(1);
        let mut lock = VarLock::new();

        assert!(lock.try_write(&txn_id));
        lock.release(&txn_id);

        assert_unlocked(&lock);
    }

    #[test]
    fn test_releasing_read_lock_with_wrong_transaction_id_does_not_unlock() {
        let owner = txn(1);
        let wrong_txn = txn(2);
        let mut lock = VarLock::new();

        assert!(lock.try_read(&owner));
        lock.release(&wrong_txn);

        assert_readers(&lock, &[owner]);
    }

    #[test]
    fn test_releasing_write_lock_with_wrong_transaction_id_does_not_unlock() {
        let owner = txn(1);
        let wrong_txn = txn(2);
        let mut lock = VarLock::new();

        assert!(lock.try_write(&owner));
        lock.release(&wrong_txn);

        assert_writer(&lock, &owner);
    }

    #[test]
    fn test_sole_reader_can_upgrade_to_write_lock() {
        let txn_id = txn(1);
        let mut lock = VarLock::new();

        assert!(lock.try_read(&txn_id));
        assert!(lock.upgrade_to_write(&txn_id));

        assert_writer(&lock, &txn_id);
    }

    #[test]
    fn test_reader_cannot_upgrade_when_other_readers_exist() {
        let txn1 = txn(1);
        let txn2 = txn(2);
        let mut lock = VarLock::new();

        assert!(lock.try_read(&txn1));
        assert!(lock.try_read(&txn2));
        assert!(!lock.upgrade_to_write(&txn1));

        assert_readers(&lock, &[txn1, txn2]);
    }

    #[test]
    fn test_write_lock_upgrade_is_idempotent_for_owner() {
        let txn_id = txn(1);
        let mut lock = VarLock::new();

        assert!(lock.try_write(&txn_id));
        assert!(lock.upgrade_to_write(&txn_id));

        assert_writer(&lock, &txn_id);
    }

    #[test]
    fn test_wrong_transaction_cannot_upgrade_write_lock() {
        let owner = txn(1);
        let wrong_txn = txn(2);
        let mut lock = VarLock::new();

        assert!(lock.try_write(&owner));
        assert!(!lock.upgrade_to_write(&wrong_txn));

        assert_writer(&lock, &owner);
    }
}

#[cfg(test)]
mod wait_die_tests {
    use super::*;

    fn tid(ts: u128, node: u64) -> TxnId {
        TxnId {
            timestamp: ts,
            node_id: node,
            iteration: 0,
        }
    }

    #[test]
    fn older_requester_waits_on_write_lock() {
        let lock = VarLock::WriteLocked(tid(20, 1)); // younger holder
        assert_eq!(lock.wait_die(&tid(10, 1)), WaitDie::Wait);
    }

    #[test]
    fn younger_requester_dies_on_write_lock() {
        let lock = VarLock::WriteLocked(tid(10, 1)); // older holder
        assert_eq!(lock.wait_die(&tid(20, 1)), WaitDie::Die);
    }

    #[test]
    fn older_than_all_readers_waits() {
        let mut readers = std::collections::HashSet::new();
        readers.insert(tid(20, 1));
        readers.insert(tid(30, 1));
        let lock = VarLock::ReadLocked(readers);
        assert_eq!(lock.wait_die(&tid(10, 1)), WaitDie::Wait);
    }

    #[test]
    fn younger_than_some_reader_dies() {
        let mut readers = std::collections::HashSet::new();
        readers.insert(tid(10, 1)); // older than requester
        readers.insert(tid(30, 1));
        let lock = VarLock::ReadLocked(readers);
        assert_eq!(lock.wait_die(&tid(20, 1)), WaitDie::Die);
    }

    #[test]
    fn node_id_breaks_ties_smaller_is_older() {
        // same timestamp: smaller node_id is older (higher priority)
        assert_eq!(
            VarLock::WriteLocked(tid(10, 1)).wait_die(&tid(10, 2)),
            WaitDie::Die
        );
        assert_eq!(
            VarLock::WriteLocked(tid(10, 2)).wait_die(&tid(10, 1)),
            WaitDie::Wait
        );
    }

    #[test]
    fn requester_in_reader_set_is_not_a_self_conflict() {
        let me = tid(10, 1);
        let mut readers = std::collections::HashSet::new();
        readers.insert(me.clone());
        readers.insert(tid(20, 1)); // younger other reader
        let lock = VarLock::ReadLocked(readers);
        assert_eq!(lock.wait_die(&me), WaitDie::Wait);
    }
}
