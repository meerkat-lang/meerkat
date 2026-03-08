use std::collections::HashSet;

use kameo::prelude::*;
use kameo::Actor;

use super::lock::LockState;
use super::pubsub::PubSub;
use super::transaction::Txn;
use crate::ast::Expr;

pub mod handler;
pub mod state;

/**
 *
 * var x := 1
 *  intialized
 *  -> receive lock request
 *  -> managing lock
 * -> grant lock
 * -> send back lock granted
 * -> receive write request
 * -> temporarily change value
 * -> send back write granted
 * -> receive transaction finished
 * -> commit change
 * var y := 2
 * ...
 */

pub struct VarActor {
    pub name: String, // this actor's var name
    pub value: state::VarValueState,

    pub pubsub: PubSub,
    pub lock_state: LockState,

    pub latest_write_txn: Option<Txn>,
}

impl VarActor {
    pub fn new(name: String, val: Expr) -> VarActor {
        VarActor {
            name,
            value: state::VarValueState::new(val),
            pubsub: PubSub::new(),
            lock_state: LockState::new(),
            latest_write_txn: None,
        }
    }
}