use std::collections::{HashMap, HashSet};
use std::time::Duration;
use std::vec;

use kameo::mailbox::Signal;
use kameo::{error::Infallible, prelude::*};
use log::info;

use crate::ast::Expr;
use crate::runtime::message::CmdMsg;
use crate::runtime::{lock, message::Msg};

use super::DefActor;

pub const TICK_INTERVAL: Duration = Duration::from_millis(100);

impl kameo::prelude::Message<Msg> for DefActor {
    type Reply = Msg;

    async fn handle(&mut self, msg: Msg, _ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        match msg {
            Msg::Subscribe { from_addr, .. } => {
                self.pubsub.subscribe(from_addr);
                Msg::SubscribeGranted {
                    name: self.name.clone(),
                    value: self.value.clone(),
                    preds: self.state.get_all_applied_txns(), // todo we use all applied txns now
                }
            }

            Msg::SubscribeGranted {
                name,
                value,
                preds,
            } => {
                // notice this is equivalent to a change message for def actor
                self.state.receive_change(name, value, preds);
                Msg::Unit
            }

            // Msg::LockRequest {
            //     lock,
            //     from_mgr_addr,
            // } => {
            //     if !self.lock_state.add_wait(lock.clone(), from_mgr_addr) {
            //         return Msg::LockAbort {
            //             from_name: self.name.clone(),
            //             lock,
            //         };
            //     }
            //     Msg::Unit
            // }

            // Msg::LockRelease { txn, .. } => {
            //     assert!(self.lock_state.has_granted(&txn.id));

            //     let lock = self
            //         .lock_state
            //         .remove_granted_or_wait(&txn.id)
            //         .expect("lock should be granted before release");

            //     assert!(lock.is_read());
            //     Msg::Unit
            // }

            // Msg::LockAbort { lock, .. } => {
            //     self.lock_state.remove_granted_or_wait(&lock.txn_id);
            //     Msg::Unit
            // }
            Msg::UsrReadDefRequest {
                from_mgr_addr,
                txn_id,
                pred,
            } => {
                // def actor will send back result only if all pred has been applied

                // assert!(self.lock_state.has_granted(&txn));
                // // remove read lock immediately
                // self.lock_state.remove_granted_if_read(&txn);

                if pred.len() == 0 {
                    let _ = from_mgr_addr
                        .tell(Msg::UsrReadDefResult {
                            txn_id: txn_id.clone(),
                            name: self.name.clone(),
                            result: self.value.clone().into(),
                            preds: self.state.get_all_applied_txns(), // todo!("switch to undropped txns later")
                        })
                        .await;
                } else {
                    self.read_requests.insert(txn_id, (from_mgr_addr, pred));
                }

                Msg::Unit
            }

            Msg::TestReadDefRequest { 
                from_mgr_addr, 
                test_id, 
                preds 
            } => {
                self.test_read_request = Some((test_id, (from_mgr_addr, preds)));

                Msg::Unit
            }

            Msg::PropChange {
                from_name,
                val,
                preds,
            } => {
                self.state.receive_change(from_name, val, preds);
                Msg::Unit
            }

            _ => panic!("DefActor: unexpected message: {:?}", msg),
        }
    }
}

impl Actor for DefActor {
    type Error = Infallible;

    async fn next(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        mailbox_rx: &mut MailboxReceiver<Self>,
    ) -> Option<Signal<Self>> {
        let mut interval = tokio::time::interval(TICK_INTERVAL);

        loop {
            tokio::select! {
                // if a real message waiting, return immediately:
                maybe_signal = mailbox_rx.recv() => {
                    return maybe_signal;
                }

                // else, every 100 ms ticks
                _ = interval.tick() => {
                    info!("{} has value {}", self.name, self.value);
                    if let Err(e) = self.tick().await {
                        eprintln!("[{}] tick() failed: {:?}", self.name, e);
                    }
                }
            }
        }
    }
}

impl DefActor {
    async fn tick(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // // if can grant new waiting lock
        // if let Some((lock, mgr)) = self.lock_state.grant_oldest_wait() {
        //     let msg = Msg::LockGranted {
        //         from_name: self.name.clone(),
        //         lock,
        //     };

        //     mgr.tell(msg).await?;
        // }

        // if we search for new batch of changes
        let changes = self.state.search_batch();
        info!("{:?} Search batch found: {:?}", self.name, changes);
        if changes.len() > 0 {
            self.value = self.state.apply_batch(&changes);
            info!("{:?} Successfully apply batch function, got new value: {}", self.name, self.value);
            let preds = self.state.get_preds_of_changes(&changes);

            let msg = Msg::PropChange {
                from_name: self.name.clone(),
                val: self.value.clone().into(),
                preds,
            };
            self.pubsub.publish(msg).await;
        }

        // if we have read request and applied its preds
        let mut processed = vec![];
        for (txn, (from_mgr_addr, pred)) in self.read_requests.iter() {
            if self.state.has_applied_txns(pred) {
                let _ = from_mgr_addr
                    .tell(Msg::UsrReadDefResult {
                        txn_id: txn.clone(),
                        name: self.name.clone(),
                        result: self.value.clone().into(),
                        preds: self.state.get_all_applied_txns(), // todo!("switch to undropped txns later")
                    })
                    .await;
                processed.push(txn.clone());
            }
        }
        for txn in processed {
            self.read_requests.remove(&txn); // removed processed read request
        }

        // if we have test read request and applied its preds
        if let Some((test_id, (from_mgr_addr, preds))) = &self.test_read_request {
            if self.state.has_applied_txns(preds) {
                let _ = from_mgr_addr
                    .tell(Msg::TestReadDefResult {
                        test_id: test_id.clone(),
                        result: self.value.clone().into(),
                    })
                    .await;
                self.test_read_request = None;
                // todo!("at this point we should kill/stop_gracefully the actor")
            }
        }

        Ok(())
    }
}
