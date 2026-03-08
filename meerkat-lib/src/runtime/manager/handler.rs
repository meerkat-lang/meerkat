use core::panic;

use log::info;
use std::time::Duration;
use std::{collections::HashSet, error::Error};

use crate::runtime::message::{CmdMsg, Msg};
use crate::runtime::transaction::Txn;
use kameo::mailbox::Signal;
use kameo::{error::Infallible, prelude::*};

pub const TICK_INTERVAL: Duration = Duration::from_millis(100);

use super::Manager;

/// message between manager and REPL
impl kameo::prelude::Message<CmdMsg> for Manager {
    type Reply = Option<CmdMsg>;

    async fn handle(&mut self, msg: CmdMsg, _ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        use CmdMsg::*;
        info!("MANAGER {} RECEIVE form Command Line: ", self.name);
        match msg {
            TryAssert { name, test: bool_expr, test_id } => {
                info!("Try Test {}: expr {}", name, bool_expr);
                self.add_new_test(test_id, name, bool_expr).await;

                let _ = self.request_assertion_preds(test_id).await;
                None
            }

            DoAction { from_client_addr, txn_id, action } => {
                info!("Do Action");
                // todo better modularity here
                if let Ok((assns, inserts)) = self.eval_action(action.clone()) {
                    // let txn_mgr = self.new_txn(
                    //     txn_id.clone(),
                    //     assns.clone(),
                    //     inserts.clone(),
                    //     from_client_addr
                    // );
                    // self.txn_mgrs.insert(txn_id.clone(), txn_mgr);
                    self.add_new_txn(txn_id.clone(), assns.clone(), inserts.clone(),from_client_addr);

                    if inserts.is_empty() {
                        info!("No inserts to process");
                    }
                    let curr_txn = Txn {
                        id: txn_id.clone(),
                        assns: assns,
                        inserts: inserts.clone()
                    };
                    for insert in inserts {
                        if let Ok(true) = self.eval_insert(&insert) {
                            info!("Sending user write request to table actor ");
                            
                            if let Some(actor) = self.tablename_to_actors.get(&insert.table_name) {
                                let mgr_addr = self.address
                                    .as_ref()
                                    .expect("Manager address not set");
                                actor
                                    .tell(Msg::UserWriteTableRequest {
                                        from_mgr_addr: mgr_addr.clone(),
                                        txn: curr_txn.clone(),
                                    }).await
                                    .unwrap();
                            } else {
                                info!("No table actor for table: {}", insert.table_name);
                            }
                        }
                    }
                } else {
                    info!("Expr:: Action not found, it is {:?}", &action);
                }

                // request locks
                let _ = self.request_locks(&txn_id).await;

                None
            }

            TransactionAborted { txn_id } => {
                info!("Transaction Aborted");
                let client_sender = self.get_client_sender(&txn_id);
                client_sender.send(CmdMsg::TransactionAborted { txn_id }).await.unwrap();

                None
            }

            CodeUpdate { srv } => {
                info!("Code Update");
                self.alloc_service(&srv).await;
                // todo(): handle alloc_service asynchronously
                // to do so, exploit the developer sender
                // for delayed response
                // and change logic in runtime.mod
                Some(CodeUpdateGranted { srv_name: srv.name })
            }
            _ => {
                panic!("Manager should not receive message from REPL");
            }
        }
    }
}

/// message between manager and var/def/table actors
impl kameo::prelude::Message<Msg> for Manager {
    type Reply = Msg;

    async fn handle(&mut self, msg: Msg, ctx: &mut Context<Self, Self::Reply>) -> Self::Reply {
        info!("MANAGER {} RECEIVE: ", self.name);
        match msg {
            Msg::LockGranted {
                from_name,
                lock,
                pred_id,
            } => {
                info!("Lock Granted");
                self.add_grant_lock(&lock.txn_id, from_name, lock.lock_kind, pred_id);
                if self.all_lock_granted(&lock.txn_id) {
                    info!("all lock granted");
                    let _ = self.request_reads(&lock.txn_id).await;
                    info!("all read requested");
                }

                Msg::Unit
            }

            Msg::TestRequestPredGranted {
                from_name,
                test_id,
                pred_id,
            } => {
                self.add_grant_pred(test_id, from_name, pred_id);

                if self.all_pred_granted(test_id) {
                    let _ = self.request_assertion_result(test_id).await;
                }

                Msg::Unit
            }

            Msg::UsrReadVarResult {
                txn: txn_id,
                name,
                result,
                pred,
            } => {
                info!("UsrReadVarResult");
                if self.is_aborted(&txn_id) {
                    return Msg::Unit;
                }

                let pred = pred.map_or_else(
                    || HashSet::new(),
                    |p| HashSet::from([p])
                );

                self.add_finished_read(&txn_id, name, result, pred);
                info!("add finished read");
                if self.all_read_finished(&txn_id) {
                    print!("{:?}", self.txn_mgrs[&txn_id].direct_reads);
                    let _ = self.reeval_and_request_writes(&txn_id).await;
                    // todo!() current impl isn't optimized for best concurrency
                    // if re-eval block for too long
                    // feel free to spawn a new thread
                    // or put it in tick()
                }
                info!("reeval_and_request_writes");
                Msg::Unit
            }

            Msg::UsrReadDefResult {
                txn_id,
                name,
                result,
                preds,
            } => {
                info!("UsrReadDefResult");
                if self.is_aborted(&txn_id) {
                    return Msg::Unit;
                }

                self.add_finished_read(&txn_id, name, result, preds);

                if self.all_read_finished(&txn_id) {
                    let _ = self.reeval_and_request_writes(&txn_id).await;
                    // todo!() same above
                }
                Msg::Unit
            }

            Msg::TestReadDefResult { test_id, result } => {
                let _ = self.on_test_finish(test_id, result).await;
                Msg::Unit
            }

            Msg::UsrWriteVarFinish { txn: txn_id, name } => {
                info!("UsrWriteVarFinish");
                self.add_finished_write(&txn_id, name);

                if self.all_write_finished(&txn_id) {
                    let _ = self.release_locks(&txn_id).await;

                    info!("release all locks, send commit transaction");
                    let client_sender = self.get_client_sender(&txn_id);
                    client_sender
                        .send(CmdMsg::TransactionCommitted {
                            txn_id: txn_id.clone(),
                            writes: self
                                .txn_mgrs
                                .get(&txn_id)
                                .unwrap()
                                .writes
                                .keys()
                                .cloned()
                                .collect(),
                        })
                        .await
                        .unwrap();
                }
                Msg::Unit
            }
        
            Msg::UserReadTableResult { txn, name, result } => {
                info!("UserReadTableResult");

                self.add_finished_read(&txn, name, result, HashSet::new());

                Msg::Unit
            }

            Msg::UserWriteTableFinish {txn: txn_id, ..} => {
                info!("UserWriteTableFinish");

                let client_sender = self.get_client_sender(&txn_id);
                client_sender
                    .send(CmdMsg::TransactionCommitted { 
                        txn_id: txn_id.clone(),
                        writes: self
                            .txn_mgrs
                            .get(&txn_id)
                            .unwrap()
                            .writes
                            .keys()
                            .cloned()
                            .collect(),
                     })
                    .await.unwrap();

                Msg::Unit
            }

            Msg::LockAbort { from_name: _, lock } => {
                info!("Lock Abort");
                let _ = self.request_abort_locks(&lock.txn_id).await;
                self.abort_lock(&lock.txn_id); // turn all txn's lock state to aborted

                // notify another mailbox of manager myself
                let _ = ctx.actor_ref().tell(CmdMsg::TransactionAborted {
                    txn_id: lock.txn_id,
                }).await;

                Msg::Unit
            }
            _ => panic!("Manager should not receive message from var/def actors"),
        }
    }
}

impl Actor for Manager {
    type Error = Infallible;

    /// customized on_start impl of Actor trait
    /// to allow manager self reference to its addr
    async fn on_start(&mut self, actor_ref: ActorRef<Self>) -> Result<(), Self::Error> {
        info!("MANAGER on_start got ActorRef with id {}", actor_ref.id());
        self.address = Some(actor_ref.clone());
        Ok(())
    }
    async fn next(
        &mut self,
        _actor_ref: WeakActorRef<Self>,
        mailbox_rx: &mut MailboxReceiver<Self>
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
                    info!("tick");
                }
            }
            // println!("[{}] ticked, now value is {:?}", self.name, self.value);
        }
    }
}

impl Manager {
    pub async fn tell_to_name(&self, name: &String, msg: Msg) -> Result<(), Box<dyn Error>> {
        if let Some(actor) = self.varname_to_actors.get(name) {
            actor.tell(msg).await?;
        } else if let Some(actor) = self.defname_to_actors.get(name) {
            actor.tell(msg).await?;
        } else if let Some(actor) = self.tablename_to_actors.get(name) {
            actor.tell(msg).await?;
        } else {
            panic!("Service alloc: no such var or def with name: {}", name);
        }

        Ok(())
    }

    pub async fn ask_to_name(&self, name: &String, msg: Msg) -> Result<Msg, Box<dyn Error>> {
        let back_msg = if let Some(actor) = self.varname_to_actors.get(name) {
            actor.ask(msg).await?
        } else if let Some(actor) = self.defname_to_actors.get(name) {
            actor.ask(msg).await?
        } else if let Some(actor) = self.tablename_to_actors.get(name) {
            actor.ask(msg).await?
        } else {
            panic!("Service alloc: no such var or def with name: {}", name);
        };

        Ok(back_msg)
    }

    /// tell to var / defs 
    /// message 1 for names without dependent (leaf node in dependency graph), e.g. var/table_var node
    /// message 2 for names with dependent e.g. def node
    pub async fn tell_to_name2(
        &self,
        name: &String,
        msg_var: Msg,
        msg_def: Msg,
    ) -> Result<(), Box<dyn Error>> {
        if let Some(actor) = self.varname_to_actors.get(name) {
            actor.tell(msg_var).await?;
        } else if let Some(actor) = self.defname_to_actors.get(name) {
            actor.tell(msg_def).await?;
        } else if let Some(actor) = self.tablename_to_actors.get(name) {
            actor.tell(msg_var).await?;
        } else {
            panic!("Service alloc: no such var or def with name: {}", name);
        }

        Ok(())
    }
}
