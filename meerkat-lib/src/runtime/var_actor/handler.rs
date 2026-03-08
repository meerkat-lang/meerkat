//! Logic for Var Actor
//!

use std::collections::HashSet;
use std::time::Duration;

use kameo::mailbox::Signal;
use kameo::{error::Infallible, prelude::*};
use log::info;

use super::VarActor;
use crate::runtime::{lock::Lock, message::Msg};

pub const TICK_INTERVAL: Duration = Duration::from_millis(100);

impl kameo::prelude::Message<Msg> for VarActor {
    type Reply = Msg;

    async fn handle(
        &mut self,
        msg: Msg,
        _ctx: &mut kameo::prelude::Context<Self, Self::Reply>,
    ) -> Self::Reply {
        info!("VAR ACTOR {} RECEIVE: ", self.name);
        match msg {
            Msg::Subscribe {
                from_name: _,
                from_addr,
            } => {
                info!("Subscribe from {:?}", from_addr);
                self.pubsub.subscribe(from_addr);

                Msg::SubscribeGranted {
                    name: self.name.clone(),
                    value: self.value.clone().into(),
                    preds: self
                        .latest_write_txn
                        .clone()
                        .map_or_else(|| HashSet::new(), |txn| HashSet::from([txn])),
                }
            }

            Msg::LockRequest {
                lock,
                from_mgr_addr: from_name,
            } => {
                info!("Lock Request from {:?} {:?}", from_name, lock);
                if !self.lock_state.add_wait(lock.clone(), from_name.clone()) {
                    info!("Aborted {:?}", lock);

                    let _ = from_name
                        .tell(Msg::LockAbort {
                            from_name: self.name.clone(),
                            lock,
                        })
                        .await;
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
                        pred_id: self.latest_write_txn.clone().map(|t| t.id) 
                }).await;

                Msg::Unit
            }

            Msg::LockAbort { lock, .. } => {
                info!("Lock Aborted for {:?}", lock.txn_id);
                self.lock_state.remove_granted_or_wait(&lock.txn_id);

                // roll back to previous stable state of value
                // unconfirmed write has same txn as aborted
                self.value.roll_back_if_relevant(&lock.txn_id);

                Msg::Unit
            }

            Msg::LockRelease { txn, mut preds } => {
                info!("Lock Release for txn {:?}", txn.id);
                assert!(
                    self.lock_state.has_granted(&txn.id),
                    "lock for txn {:?} should be granted before release",
                    txn.id
                );
                let lock = self
                    .lock_state
                    .remove_granted_or_wait(&txn.id)
                    .expect("lock should be granted before release");

                // if lock is read then nothing else to do
                // else if lock is write:
                if lock.is_write() {
                    let (new_value, unconfirmed_txn) = self
                        .value
                        .confirm_update()
                        .expect("should have unconfirmed value update");
                    assert!(unconfirmed_txn == txn.id);

                    self.latest_write_txn = Some(txn.clone());

                    // except for preds calculated by manager
                    // the txn itself should also have been applied when
                    // value is updated
                    preds.insert(txn.clone());

                    self.pubsub
                        .publish(Msg::PropChange {
                            from_name: self.name.clone(),
                            val: new_value,
                            preds: preds.clone(),
                        })
                        .await;
                }

                Msg::Unit
            }

            Msg::UsrReadVarRequest { txn, from_mgr_addr } => {
                info!("UsrReadVarRequest");
                assert!(self.lock_state.has_granted(&txn));

                // // remove read lock immediately
                // self.lock_state.remove_granted_if_read(&txn);

                info!("sending UsrReadVarResult to {:?}", from_mgr_addr);
                let _ = from_mgr_addr
                    .tell(Msg::UsrReadVarResult {
                        txn,
                        name: self.name.clone(),
                        result: self.value.clone().into(),
                        pred: self.latest_write_txn.clone(),
                    })
                    .await;

                Msg::Unit
            }

            Msg::UsrWriteVarRequest {
                from_mgr_addr,
                txn,
                write_val,
            } => {
                info!("UsrWriteVarRequest");
                assert!(self.lock_state.has_granted_write(&txn));

                self.value.update(write_val, txn.clone());

                info!("send UsrWriteVarFinish to manager");
                let _ = from_mgr_addr
                    .tell(Msg::UsrWriteVarFinish {
                        name: self.name.clone(),
                        txn,
                    })
                    .await;

                Msg::Unit
            }

            Msg::TestRequestPred { from_mgr_addr, test_id } => {
                info!("Only for asserts: Pred Request from {:?}", from_mgr_addr);

                // will immediately send back latest pred id
                let _ = from_mgr_addr.tell(
                    Msg::TestRequestPredGranted { 
                        from_name: self.name.clone(),
                        test_id,
                        pred_id: self.latest_write_txn.clone().map(|t| t.id) 
                }).await;

                Msg::Unit
            }

            #[allow(unreachable_patterns)]
            _ => panic!("VarActor should not receive message {:?}", msg),
        }
    }
}

impl Actor for VarActor {
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
                    info!("{} has value {:?}", self.name, self.value);
                    let _ = self.tick().await;
                }
            }
            // println!("[{}] ticked, now value is {:?}", self.name, self.value);
        }
    }
}

impl VarActor {
    async fn tick(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // if can grant new waiting lock
        if let Some((lock, mgr)) = self.lock_state.grant_oldest_wait() {
            assert!(self.lock_state.has_granted(&lock.txn_id));
            info!("{:?} grant {:?} to manager {}", self.name, lock, mgr.id());

            let msg = Msg::LockGranted {
                from_name: self.name.clone(),
                lock,
                pred_id: self.latest_write_txn.clone().map(|t| t.id),
            };

            let _ = mgr.tell(msg).await?;
        }
        Ok(())
    }
}
