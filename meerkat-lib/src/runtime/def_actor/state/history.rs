//! a finer grained history of applied changes (under development)
//!
//! This module provides a mechanism to track a collection of `PropChange`'s and
//! automatically drop any change that has been fully superseded by newer writes.
//! Whenever a new change is added, only the most recent write per variable is
//! kept; older changes whose writes are all dominated by later transactions are
//! moved into the `dropped` set and no longer considered “live.”
//!
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};

use super::{ChangeId, PropChange};
use crate::runtime::transaction::{Txn, TxnId};

/// applied changes
/// - (compared to prior implementation) now support drop unnecessary changes
pub struct AppliedChanges {
    /// changes that have not been dropped yet,
    /// key: change
    /// value: live writes that has not been dominated by writes in other changes
    pub undropped: HashMap<ChangeId, HashSet<String>>,

    /// changes that have been dropped
    /// as soon as all writes of the a change are dominated by existing writers,
    /// we don't need to keep it at all
    pub dropped: HashSet<ChangeId>,

    /// key: writed var v
    /// value: latest txn writes to v, together with the change it belongs to
    pub write_to_changes: HashMap<String, (TxnId, ChangeId)>,
}

impl AppliedChanges {
    pub fn new() -> Self {
        AppliedChanges {
            undropped: HashMap::new(),
            dropped: HashSet::new(),
            write_to_changes: HashMap::new(),
        }
    }

    pub fn add_change(&mut self, change: &PropChange) {
        let mut write_to_max_txn = HashMap::new();
        for txn in change.preds.iter() {
            for write in txn.assns.iter() {
                write_to_max_txn
                    .entry(write.dest.clone())
                    .and_modify(|old_txn: &mut Txn| {
                        if txn.id > old_txn.id {
                            *old_txn = txn.clone();
                        }
                    })
                    .or_insert(txn.clone());
            }
        }

        let mut change_live_set = HashSet::new();
        for (write, txn) in write_to_max_txn {
            if let Some((max_id, max_change)) = self.write_to_changes.get_mut(&write) {
                // if txn is a more latest txn that writes to this var,
                if *max_id < txn.id {
                    // we remove write from the change's live set,
                    // since it is now dominated by the new txn
                    let live_set = self
                        .undropped
                        .get_mut(max_change)
                        .expect("change should not be dropped already");
                    live_set.remove(&write);

                    // if the change's live set becomes empty, ok to drop
                    if live_set.is_empty() {
                        let (v, _) = self.undropped.remove_entry(max_change).unwrap();
                        self.dropped.insert(v);
                    }
                    change_live_set.insert(write.clone());
                    *max_id = txn.id.clone();
                    *max_change = change.id;
                }
            } else {
                change_live_set.insert(write.clone());
                self.write_to_changes
                    .insert(write, (txn.id.clone(), change.id));
            }
        }

        if change_live_set.len() > 0 {
            self.undropped.insert(change.id, change_live_set);
        } else {
            self.dropped.insert(change.id);
        }
    }

    pub fn has_change(&self, change: &ChangeId) -> bool {
        self.undropped.contains_key(change) || self.dropped.contains(change)
    }

    pub fn get_undropped_changes(&self) -> HashSet<ChangeId> {
        self.undropped.keys().cloned().collect()
    }

    pub fn get_all_applied_changes(&self) -> HashSet<ChangeId> {
        self.undropped
            .keys()
            .cloned()
            .chain(self.dropped.iter().cloned())
            .collect()
    }
}
