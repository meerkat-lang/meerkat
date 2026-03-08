//! assertion implementation
//! Motivation:
//! - provide assert for developer testing the meerkat source code
//!
//! Design:
//! - each assertion spawns a def actor, subscribing to all free reactive names in the assertion
//! - once all transactions required by the assertion are committed and applied by the actor
//!   we decide if the assertions succeeds or not
//! - (todo) if some required transactions never arrives, assertion timeout
//!
//! We treat each assertion as a weaker form of transaction + 2 phase process
//! - no explicit transaction id
//! - since assertion is read only, (Phase 1)
//!   trans_read requests are sended to relevant var actors
//!   who send back pred txn immediately (no acquisition for read lock)
//! - (Phase 2) send UsrReadDefRequest to assertion def actor
//!   when hearing back from the def actor, send back AssertComplete
//!
use std::collections::HashMap;

use kameo::actor::ActorRef;
use tokio::sync::mpsc::Sender;

use crate::runtime::{def_actor::DefActor, message::CmdMsg, transaction::TxnId, TestId};

mod do_test;
mod test_manager;

#[derive(Debug)]
pub struct TestManager {
    pub test_id: TestId,
    pub assert_actor: ActorRef<DefActor>,
    pub trans_reads: HashMap<String, TestTransReadState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestTransReadState {
    Requested,
    Depend(Option<TxnId>),
}
