//! how we key the pending changes of a def actor F
//!
//! we maintain a dependency graph of pending changes, where applying a change
//! requires for all transactions t in change, if t writes t ancestor of F,
//! then we want to see all relevant args(F) has t in hand to be applied
//!

use std::collections::{HashMap, HashSet, VecDeque};

use crate::{
    ast::{Assn, Expr},
    runtime::transaction::{Txn, TxnId},
};

use super::{ChangeId, PropChange};

pub struct PendingChanges {
    /// relevant var maps to args of def F's expression
    /// when we see a transaction t writes to a relevant var f,
    /// then all var_to_inputs[f] should see transaction t
    expr: Expr,
    pub var_to_args: HashMap<String, HashSet<String>>,

    /// dependency graph (a hypergraph):
    ///
    /// in a way that a change depends on (arg_name, txn_id)'s
    /// # req_to_changes, include pending and applied changes
    /// key: (arg_name, txn_id)
    /// value: a set of changes whose from_name is arg_name, and preds contains txn_id
    pub req_to_changes: HashMap<(String, TxnId), HashSet<ChangeId>>,
    /// # changes_to_req:
    /// key: change
    /// value: (arg_name, txn_id) that the change depends on
    pub change_to_reqs: HashMap<ChangeId, HashSet<(String, TxnId)>>,
    // todo: there are rooms for optimization here!
    // - incrementally update some data structures to avoid full scan each time
    // - ... ?
}

impl PendingChanges {
    pub fn new(expr: Expr, var_to_args: HashMap<String, HashSet<String>>) -> Self {
        PendingChanges {
            expr,
            var_to_args,
            req_to_changes: HashMap::new(),
            change_to_reqs: HashMap::new(),
        }
    }

    pub fn add_change(&mut self, change: &PropChange) {
        // change depends on (arg, t)
        //
        // if a write (var, ...) appears in txn t in change
        // then for all arg in var_to_inputs[var] should see t,
        // namely change depends a change on arg, whose preds contains t
        // recorded as (arg, t)
        for Txn { id: txn_id, assns, inserts} in change.preds.iter() {

            for Assn { dest, .. } in assns.iter() {
                if let Some(args) = self.var_to_args.get(dest) {
                    for arg in args.iter()
                    {
                        self.change_to_reqs
                            .entry(change.id)
                            .or_insert(HashSet::new())
                            .insert((arg.clone(), txn_id.clone()));
                    }
                }
            }

            for insert in inserts.iter() {
                if let Some(args) = self.var_to_args.get(&insert.table_name) {
                    for arg in args.iter()
                    {
                        self.change_to_reqs
                            .entry(change.id)
                            .or_insert(HashSet::new())
                            .insert((arg.clone(), txn_id.clone()));
                    }
                }
            }
        }

        // change provides (arg, t)
        for Txn { id: txn_id, .. } in change.preds.iter() {
            self.req_to_changes
                .entry((change.from_name.clone(), txn_id.clone()))
                .or_insert(HashSet::new())
                .insert(change.id);
        }
    }


    /// todo(): try different search strategies
    /// - search for minimal batch of changes (find SCC's):
    ///   - generated change will have the least number of preds
    ///   - better liveness of the distributed system
    /// - search for maximal batch of changes (worklist algorithm):
    ///   - minimal number of messages to communicate
    ///
    pub fn search_smallest_batches(&self) -> Vec<HashSet<ChangeId>> {
        todo!()
    }

    /// worklist algorithm
    /// self.req_to_changes serve as history of applied changes
    pub fn search_largest_batch(&self) -> HashSet<ChangeId> {
        let mut req_to_changes = self.req_to_changes.clone();

        // worklist keep track of changes whose preds are missing
        let mut worklist = VecDeque::new();
        let mut kept_changes = HashSet::new();

        let mut empty_reqs = HashSet::new();
        for (req, changes) in req_to_changes.iter_mut() {
            if changes.is_empty() {
                empty_reqs.insert(req.clone());
            }
        }
        for (change, reqs) in self.change_to_reqs.iter() {
            if reqs.iter().any(|req| empty_reqs.contains(req)) {
                worklist.push_back(*change);
                kept_changes.insert(*change);
            }
        }

        while let Some(change) = worklist.pop_front() {
            for (req, changes) in req_to_changes.iter_mut() {
                changes.remove(&change);
                if changes.is_empty() {
                    empty_reqs.insert(req.clone());
                }
            }
            for (new_change, reqs) in self.change_to_reqs.iter() {
                if !kept_changes.contains(new_change)
                    && reqs.iter().any(|req| empty_reqs.contains(req))
                {
                    worklist.push_back(*new_change);
                    kept_changes.insert(*new_change);
                }
            }
        }

        self.change_to_reqs
            .keys()
            .cloned()
            .collect::<HashSet<_>>()
            .difference(&kept_changes)
            .cloned()
            .collect::<HashSet<_>>()
    }

    pub fn remove_batch_from_pending(&mut self, changes: &HashSet<ChangeId>) {
        for change in changes.iter() {
            self.change_to_reqs.remove(change);
        }
    }

    pub fn has_no_pending_changes(&self) -> bool {
        self.change_to_reqs.is_empty()
    }
}
