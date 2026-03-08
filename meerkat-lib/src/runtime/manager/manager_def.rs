use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Display;

use kameo::prelude::*;
use tokio::sync::mpsc::Sender;

use crate::runtime::manager::action::TxnManager;
use crate::runtime::manager::assert::TestManager;
use crate::runtime::message::CmdMsg;
use crate::runtime::TestId;

use crate::runtime::def_actor::DefActor;
use crate::runtime::evaluator::Evaluator;
use crate::runtime::transaction::TxnId;
use crate::runtime::var_actor::VarActor;
use crate::runtime::table_actor::TableActor;


#[derive(Debug)]
pub struct Manager {
    /// basic info of the manager
    pub name: String,
    pub address: Option<ActorRef<Manager>>,
    pub from_developer: Sender<CmdMsg>, // sender to developer side

    pub varname_to_actors: HashMap<String, ActorRef<VarActor>>,
    pub defname_to_actors: HashMap<String, ActorRef<DefActor>>,
    pub tablename_to_actors: HashMap<String, ActorRef<TableActor>>,

    /// analysis and initial evaluation of program stored at manager
    /// todo!("probably can use for later eval of program")
    /// then manager need regularly fetch values from var/def actors
    /// might be benefitial
    pub evaluator: Evaluator,

    /// dependency graph
    pub dep_graph: HashMap<String, HashSet<String>>, // name to all its deps
    pub dep_tran_vars: HashMap<String, HashSet<String>>, // name to transitively dep `var`

    /// manager transactions and tests submitted to manager from client/developer
    pub txn_mgrs: HashMap<TxnId, TxnManager>,
    pub test_mgrs: HashMap<TestId, TestManager>,
}

impl Manager {
    /// to spawn a manager:
    /// let mgr = Manager::new(); spawn(mgr);
    pub fn new(name: String, from_developer: Sender<CmdMsg>) -> Self {
        Manager {
            name,
            address: None,
            from_developer,

            varname_to_actors: HashMap::new(),
            defname_to_actors: HashMap::new(),
            tablename_to_actors: HashMap::new(),

            evaluator: Evaluator::new(HashMap::new()),
            dep_graph: HashMap::new(),
            dep_tran_vars: HashMap::new(),

            txn_mgrs: HashMap::new(),
            test_mgrs: HashMap::new(),
        }
    }
}

impl Display for Manager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} and actor ref: {:?}\n", self.name, self.address)?;
        write!(
            f,
            "varname_to_actors: {:?}\n defname_to_actors: {:?}\n tablename_to_actors: {:?}\n",
            self.varname_to_actors, self.defname_to_actors, self.tablename_to_actors
        )?;
        // write!(f, "txn_mgrs: {:?}\n", self.txn_mgrs)?;
        Ok(())
    }
}
