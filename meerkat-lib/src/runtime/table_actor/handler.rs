use std::collections::HashSet;
use std::time::Duration;
use log::info;

use super::TableActor;
use crate::runtime::message::Msg;

pub const TICK_INTERVAL: Duration = Duration::from_millis(100);

impl kameo::prelude::Message<Msg> for TableActor {
    type Reply = Msg;

    async fn handle(
        &mut self,
        msg: Msg,
        _ctx: &mut kameo::prelude::Context<Self, Self::Reply>
    ) -> Self::Reply {
        info!("Table Actor {} Receive: ", self.name);

        match msg {
            Msg::Subscribe { from_name: _, from_addr } => {
                info!("Subsribe from {:?}", from_addr);
                self.pubsub.subscribe(from_addr);
                Msg::SubscribeGranted {
                    name: self.name.clone(),
                    value: self.value.clone().into(),
                    preds: HashSet::new(),
                }
            }

            Msg::UserReadTableRequest {
                from_mgr_addr, txn, table_name, ..
            } => {
                // we assume tables are insert only
                // thus no causal consistency is guaranteed
                // therefore no bookkeeping of last applied txn is needed or returned
                let _ = from_mgr_addr.tell(Msg::UserReadTableResult {
                    txn,
                    name: table_name,
                    result: self.value.clone().into(),
                }).await;

                Msg::Unit
            },

            Msg::UserWriteTableRequest { from_mgr_addr, txn} => {
                info!("Table Actor {} inserting row {:?}", self.name, txn.inserts);

                from_mgr_addr
                    .tell(Msg::UserWriteTableFinish { txn: txn.id.clone(), name: self.name.clone() }).await
                    .unwrap();
                info!("Sent UserWriteTableFinish to manager, now propagate changes to subscribers");

                self.latest_write_txn = Some(txn.clone());

                for insert in &txn.inserts {
                    self.value.update(insert); 
                    self.pubsub
                    .publish(Msg::PropChange {
                        from_name: self.name.clone(), 
                        val: insert.row.clone(), // only send new record
                        preds: HashSet::from([txn.clone()]), // the only pred is reflexively itself
                    })
                    .await;
                info!("Prop change message sent to subscribers");
                }
    
                Msg::Unit
            }

            Msg::TestRequestPred { from_mgr_addr, test_id } => {
                info!("Only for asserts: Pred Request from {:?}", from_mgr_addr);

                // will immediately send back latest pred id
                let _ = from_mgr_addr.tell(
                    Msg::TestRequestPredGranted { 
                        from_name: self.name.clone(),
                        test_id,
                        pred_id: self.latest_write_txn.clone().map(|txn| txn.id),
                }).await;

                Msg::Unit
            }

            #[allow(unreachable_patterns)]
            _ => panic!("VarActor should not receive message {:?}", msg),
        }
    }
}
