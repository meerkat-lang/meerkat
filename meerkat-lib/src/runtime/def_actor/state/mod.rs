use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
};

use crate::{
    ast::Expr,
    runtime::{
        evaluator::eval_def_expr,
        transaction::{Txn, TxnId},
    },
};

use history::AppliedChanges;
use log::info;
use pending::PendingChanges;

pub mod history;
pub mod pending;

pub struct ChangeState {
    pub id_cnt: ChangeId,
    pub id_to_change: HashMap<ChangeId, PropChange>,

    pub expr: Expr,                           // current value of def
    pub arg_to_values: HashMap<String, Expr>, // args of expr

    pub pending_changes: PendingChanges,
    pub applied_changes: AppliedChanges,
}

impl ChangeState {
    pub fn new(
        expr: Expr,
        arg_to_values: HashMap<String, Expr>,
        arg_to_vars: HashMap<String, HashSet<String>>, // args to their transitively dependent vars
    ) -> Self {
        let mut var_to_args = HashMap::new();

        for (arg, vars) in arg_to_vars {
            for var in vars {
                var_to_args
                    .entry(var)
                    .or_insert(HashSet::new())
                    .insert(arg.clone());
            }
        }

        ChangeState {
            id_cnt: 0,
            id_to_change: HashMap::new(),
            expr: expr.clone(),
            arg_to_values,
            pending_changes: PendingChanges::new(expr, var_to_args),
            applied_changes: AppliedChanges::new(),
        }
    }

    pub fn receive_change(
        &mut self,
        from_name: String,
        new_val: Expr,
        preds: HashSet<Txn>,
    ) {
        // info!("received change: ({}, {:?}, {:#?})", from_name, new_val, preds);
        let change = PropChange {
            id: self.id_cnt,
            from_name,
            new_val,
            preds,
        };

        self.pending_changes.add_change(&change);
        //info!("Adding change {:?}", &change);

        self.id_to_change.insert(self.id_cnt, change);
        self.id_cnt += 1;
    }

    pub fn search_batch(&mut self) -> HashSet<ChangeId> {
        self.pending_changes.search_largest_batch()
    }

    pub fn apply_batch(&mut self, changes: &HashSet<ChangeId>) -> Expr {
        self.pending_changes.remove_batch_from_pending(changes);

        for change_id in changes.iter() {
            let change = &self.id_to_change[change_id];
            info!("change being applied: {}", &change.from_name);

            if let Some(Expr::Table { records, .. }) = self.arg_to_values.get_mut(&change.from_name) {
                if let Expr::Vector { .. } = &change.new_val {
                    records.push(change.new_val.clone());                // update table val if record is inserted
                } else {
                    self.arg_to_values.insert(change.from_name.clone(), change.new_val.clone()); 
                }
            } else {
                self.arg_to_values
                .insert(change.from_name.clone(), change.new_val.clone());
            }

            self.applied_changes.add_change(change);
        }
        info!("{:?}'s env before re-evaluating: {:#?}", self.expr, self.arg_to_values);

        eval_def_expr(&self.expr, &self.arg_to_values)
    }

    pub fn get_preds_of_changes(&self, changes: &HashSet<ChangeId>) -> HashSet<Txn> {
        let mut preds = HashSet::new();
        for change_id in changes.iter() {
            let change = &self.id_to_change[change_id];
            preds.extend(change.preds.clone());
        }
        preds
    }

    pub fn has_applied_txns(&self, txns: &Vec<TxnId>) -> bool {
        txns.iter()
            .all(|id| self.get_all_applied_txns().iter().any(|t| t.id == *id))
    }

    pub fn get_all_applied_txns(&self) -> HashSet<Txn> {
        let changes = self.applied_changes.get_all_applied_changes();
        self.get_preds_of_changes(&changes)
    }

    pub fn get_all_undropped_txns(&self) -> HashSet<Txn> {
        let changes = self.applied_changes.get_undropped_changes();
        self.get_preds_of_changes(&changes)
    }
}

/// internal representation of a prop change received by a def actor
pub type ChangeId = i64;
#[derive(Eq, Clone, Debug)]
pub struct PropChange {
    pub id: ChangeId,
    pub from_name: String,
    pub new_val: Expr,
    pub preds: HashSet<Txn>,
}

impl PartialEq for PropChange {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl PartialOrd for PropChange {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.id.partial_cmp(&other.id)
    }
}

impl Ord for PropChange {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.id.cmp(&other.id)
    }
}

impl Hash for PropChange {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}
