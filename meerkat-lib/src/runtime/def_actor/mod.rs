use futures::future::Either;
use kameo::actor::ActorRef;
use std::collections::HashMap;
use std::collections::HashSet;

use super::pubsub::PubSub;
use crate::ast::Expr;
use crate::runtime::manager::Manager;
use crate::runtime::transaction::{TxnId, TxnPred};
use crate::runtime::TestId;
use state::ChangeState;

pub mod handler;
pub mod state;

// pub type TickFunc = Box<
//     dyn for<'a> FnMut(
//             &'a mut DefActor,
//         ) -> Pin<Box
//                 <dyn Future<Output = Result<(), Box<dyn Error + Send>>> + Send + 'a>
//             >
//             + Send + 'static,
// >;

pub struct DefActor {
    pub name: String,
    pub value: Expr, // expr of def
    // pub is_assert_actor_of: Option<(TestId, ActorRef<Manager>)>,

    pub pubsub: PubSub,
    // pub lock_state: LockState,

    // read request is for transactions reading this def
    pub read_requests: HashMap<TxnId, (ActorRef<Manager>, Vec<TxnId>)>,
    // test read request is for assertion def actor, one shot request
    pub test_read_request: Option<(TestId, (ActorRef<Manager>, Vec<TxnId>))>,

    pub state: ChangeState,
}

impl DefActor {
    pub fn new(
        name: String,
        expr: Expr,                                    // def's expr
        value: Expr,                                   // def's initialized value
        arg_to_values: HashMap<String, Expr>,          // def's args to their initialized values
        arg_to_vars: HashMap<String, HashSet<String>>, // args to their transitively dependent vars
        // if arg itself is var, then arg_to_vars[arg] = {arg}
    ) -> DefActor {
        DefActor {
            name,
            value,
            // is_assert_actor_of: testid_and_manager,
            pubsub: PubSub::new(),
            // lock_state: LockState::new(),
            read_requests: HashMap::new(),
            test_read_request: None,
            state: ChangeState::new(expr, arg_to_values, arg_to_vars),
        }
    }
}
