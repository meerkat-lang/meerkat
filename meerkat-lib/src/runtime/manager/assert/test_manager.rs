use core::panic;
use std::collections::{HashMap, HashSet};

use kameo::actor::ActorRef;
use tokio::sync::mpsc::Sender;

use crate::{
    ast::Expr,
    runtime::{
        def_actor::DefActor,
        manager::{
            assert::{TestManager, TestTransReadState},
            Manager,
        },
        message::CmdMsg,
        transaction::TxnId,
        TestId,
    },
};

impl TestManager {
    pub fn new(
        test_id: TestId,
        assert_actor: ActorRef<DefActor>,
        direct_reads: &HashSet<String>,
        dep_tran_vars: &HashMap<String, HashSet<String>>,
    ) -> Self {
        let mut trans_read_states = HashMap::new();

        for name in direct_reads.iter() {
            let name_trans_read = dep_tran_vars
                .get(name)
                .expect(&format!("dep vars not found"))
                .clone();

            for name in name_trans_read.iter() {
                trans_read_states.insert(name.clone(), TestTransReadState::Requested);
            }
        }

        TestManager {
            test_id,
            assert_actor,
            trans_reads: trans_read_states,
        }
    }

    pub fn add_grant_pred(&mut self, name: String, pred: Option<TxnId>) {
        self.trans_reads
            .insert(name, TestTransReadState::Depend(pred));
    }

    pub fn all_pred_granted(&self) -> bool {
        self.trans_reads
            .iter()
            .all(|(_, state)| matches!(state, TestTransReadState::Depend(_)))
    }
}

impl Manager {
    pub async fn add_new_test(&mut self, test_id: TestId, test_name: String, bool_expr: Expr) {
        // allocate def actor for assert
        let actor_ref = self
            .alloc_def_actor(
                &format!(
                    "{}_assert_{}_${}-{}",
                    test_name, bool_expr, test_id.0, test_id.1
                ),
                bool_expr.clone(),
            )
            .await
            .expect(&format!("alloc def actor failed for test {:?}", test_id));

        // direct reads are used for acquiring pred of assert
        let direct_reads = bool_expr.free_var(&self.evaluator.reactive_names, &HashSet::new());

        let test_mgr = TestManager::new(test_id, actor_ref, &direct_reads, &self.dep_tran_vars);

        self.test_mgrs.insert(test_id, test_mgr);
    }

    pub async fn on_test_finish(&mut self, test_id: TestId, test_result: Expr) {
        // unwrap test result to bool value
        let result = match test_result {
            Expr::Bool { val } => val,
            _ => panic!("test result should be bool"),
        };

        // send AssertSucceeded back to developer channel
        self.from_developer
            .send(CmdMsg::AssertCompleted {
                test_id,
                result: result,
            })
            .await
            .unwrap();

        // deallocate actor
        let _ = self
            .test_mgrs
            .remove(&test_id)
            .expect(&format!("Test {:?} not found", test_id));

        // todo!(): remove subscriptions
        // let _ = actor_ref.stop_gracefully().await;
    }
}
