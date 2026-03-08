//! state of value maintained by var actor

use log::info;

use crate::{ast::Expr, runtime::transaction::TxnId};

#[derive(Debug, Clone)]
pub enum VarValueState {
    Uninit,    // uninitialized
    Val(Expr), // stable state of a var actor value
    Trans(Option<Expr>, (Expr, TxnId)), // when receive write request, var actor is in transition
               // first slot for old value, second slot for new value
}

impl VarValueState {
    pub fn new(val: Expr) -> VarValueState {
        VarValueState::Val(val)
    }

    /// when receive write (value update) request,
    /// var actor turns into transition state
    pub fn update(&mut self, new_val: Expr, txn_id: TxnId) {
        use self::VarValueState::*;
        match self {
            VarValueState::Uninit => *self = Trans(None, (new_val, txn_id)),
            VarValueState::Val(expr) => *self = Trans(Some(expr.clone()), (new_val, txn_id)),
            VarValueState::Trans(_, _) => panic!("unrosolved transition state"),
        }
    }

    /// when receive lock release, any transition state should be confirmed
    /// and if value is updated, return the new value
    pub fn confirm_update(&mut self) -> Option<(Expr, TxnId)> {
        if let VarValueState::Trans(_, (new_val, txn_id)) = self.clone() {
            *self = VarValueState::Val(new_val.clone());
            return Some((new_val, txn_id));
        }
        None
    }

    /// when receive lock abort, transition state should be rolled back
    pub fn roll_back_if_relevant(&mut self, txn: &TxnId) {
        if let VarValueState::Trans(old_val, (_, write_txn)) = self {
            if txn != write_txn {
                return;
            }
            if let Some(val) = old_val {
                *self = VarValueState::Val(val.clone());
            } else {
                *self = VarValueState::Uninit;
            }
        }
    }
}

impl Into<Expr> for VarValueState {
    fn into(self) -> Expr {
        match self {
            VarValueState::Uninit => panic!("uninit state should not be converted to value"),
            VarValueState::Val(val) => val,
            VarValueState::Trans(old_val, _) => {
                if let Some(val) = old_val {
                    info!(
                        "var is requested for subscription when in a value transition state,
                           send subscriber with old value in transition state"
                    );
                    val
                } else {
                    panic!("var is not initialized")
                }
            }
        }
    }
}
