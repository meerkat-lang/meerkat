//！ runtime and test infrastructure
//!
//!  # How to use
//!  triggered when developer run @test(service_name) {
//!     do(action);
//!     assert(boolean_expr);
//!     assert(boolean_expr); // block next do(action) until evaled(true)
//!     ...
//!     do(action);
//!     do(action);
//!     ...
//! }
//!
//!  # implementation Idea
//!  1. each service initialize its own manager when declared
//!  2. test instantiated by service_name, do actions on service_name's manager
//!  3. test treat assert(boolean_expr) by internally allocate a def actor
//!     of boolean_expr
//!  4. test_manager will wait for bool_expr to be true before processing next
//!     action, on the other hand, timeout means assertion failed
use core::panic;
use std::collections::HashMap;

use crate::{
    ast::{Prog, ReplCmd, Service, Test},
    runtime::{
        message::CmdMsg,
        transaction::{TxnId, TxnPred},
    },
};
use futures::future::join_all;
use kameo::{actor::ActorRef, spawn};
use log::info;
use super::manager::Manager;
use tokio::{
    sync::mpsc::{self, Receiver, Sender},
    task::JoinSet,
};

// pub mod instr;


pub type TestId = (usize, usize);
const MPSC_CHANNEL_SIZE: usize = 100;
const TIMEOUT_INTERVAL: u64 = 5000;

pub async fn run(prog: &Prog) -> Result<(), Box<dyn std::error::Error>> {
    let (dev_tx, mut dev_rx) = mpsc::channel::<CmdMsg>(MPSC_CHANNEL_SIZE);

    assert!(
        prog.services.len() > 0 && prog.tests.len() > 0,
        "There must be at least one service and one test"
    );

    let mut services = HashMap::new();
    for srv in &prog.services {
        let srv_actor_ref: ActorRef<Manager> = run_srv(srv, dev_tx.clone()).await?;
        services.insert(srv.name.clone(), srv_actor_ref);
    }

    let mut test_channels = HashMap::new();
    let test_completions = join_all(prog.tests.iter().enumerate().map(|(idx, test)| {
        let (tst_tx, tst_rx) = mpsc::channel::<CmdMsg>(MPSC_CHANNEL_SIZE);
        let (cli_tx, cli_rx) = mpsc::channel::<CmdMsg>(MPSC_CHANNEL_SIZE);
        test_channels.insert(idx, tst_tx);
        let srv_actor_ref = services.get(&test.name).unwrap();
        run_test(
            idx,
            test,
            srv_actor_ref.clone(),
            cli_tx.clone(),
            cli_rx,
            tst_rx,
        )
    }));

    // Message dispatcher to route AssertCompleted messages to appropriate test channels
    tokio::spawn(async move {
        while let Some(msg) = dev_rx.recv().await {
            match msg {
                CmdMsg::AssertCompleted { test_id, result } => {
                    let test_idx = test_id.0;
                    test_channels
                        .get(&test_idx)
                        .unwrap()
                        .send(CmdMsg::AssertCompleted { test_id, result })
                        .await
                        .unwrap();
                }
                _ => panic!("Unexpected message on dev_rx: {:?}", msg),
            }
        }
    });

    for result in test_completions.await {
        result?;
    }

    Ok(())
}

pub async fn run_srv(
    srv: &Service,
    dev_tx: Sender<CmdMsg>,
) -> Result<ActorRef<Manager>, Box<dyn std::error::Error>> {
    // initialize the service's manager
    let srv_manager = Manager::new(srv.name.clone(), dev_tx);
    let srv_actor_ref = spawn(srv_manager);

    // synchronously wait for manager to be initialized
    if let Some(CmdMsg::CodeUpdateGranted { .. }) = srv_actor_ref
        .ask(CmdMsg::CodeUpdate { srv: srv.clone() })
        .await?
    {
        println!("Service {} initialized", srv.name);
    } else {
        panic!("Service {} initialization failed", srv.name);
    }

    Ok(srv_actor_ref)
}

/// Executes a test on a specified service by processing a sequence of commands.
///
/// # Arguments
///
/// * `test` - A reference to the `Test` structure containing the test commands.
/// * `srv_actor_ref` - An `ActorRef` to the service manager responsible for handling the test.
/// * `cli_tx` - A sender channel for sending `CmdMsg` commands to the client.
/// * `cli_rx` - A receiver channel for receiving `CmdMsg` responses from the client.
/// * `dev_rx` - A receiver channel for receiving `CmdMsg` responses from the manager.
///
/// # Returns
///
/// A `Result` that is `Ok` if the test completes successfully, or an error if something goes wrong.
///
/// # Description
///
/// The function iterates over a list of commands in the test. It handles `Do` commands by
/// initiating a transaction and waiting for commit or abort messages. It handles `Assert`
/// commands by sending assertions to the manager and waiting for a success message or a timeout.
/// The test proceeds to the next command only if assertions are successful or transactions
/// are committed. The function concludes when all commands have been processed.

pub async fn run_test(
    idx: usize,
    test: &Test,
    srv_actor_ref: ActorRef<Manager>,
    cli_tx: Sender<CmdMsg>,
    mut cli_rx: Receiver<CmdMsg>,
    mut tst_rx: Receiver<CmdMsg>,
) -> Result<(), Box<dyn std::error::Error>> {
    // start testing on the service
    println!("testing {}", test.name);
    let mut test_id = 0usize;
    let mut received_passed_tests = HashMap::<TestId, bool>::new();

    // one handler loop keep listening to incoming messages from manager
    // the main thread keep processing actions and asserts
    // if handler loop hear TransactionAbort, roll back to that transaction
    // if handler loop hear all assert true, roll forward to next action
    let mut txn_to_cmd_idx = HashMap::new();
    let mut process_cmd_idx = 0;

    let mut retry_txid: Option<TxnId> = None;
    while process_cmd_idx < test.commands.len() {
        let cmd = &test.commands[process_cmd_idx];

        match cmd {
            ReplCmd::Do(action) => {
                let txn_id = retry_txid.take().unwrap_or_else(TxnId::new);
                txn_to_cmd_idx.insert(txn_id.clone(), process_cmd_idx);
                srv_actor_ref
                    .tell(CmdMsg::DoAction {
                        txn_id,
                        action: action.clone(),
                        from_client_addr: cli_tx.clone(),
                    })
                    .await?;

                loop {
                    tokio::select! {
                        maybe_msg = cli_rx.recv() => {
                            if let Some(msg) = maybe_msg {
                                match msg {
                                    CmdMsg::TransactionAborted { txn_id } => {
                                        info!("Transaction {txn_id:?} aborted. Retrying");
                                        retry_txid = Some(txn_id.retry_id());
                                        break;
                                    }
                                    CmdMsg::TransactionCommitted { txn_id, writes } => {
                                        info!("Transaction {:?} committed", txn_id);
                                        process_cmd_idx += 1;
                                        break;
                                    }
                                    _ => panic!("unexpected message")
                                }
                            }
                        }
                    }
                }
            }
            ReplCmd::Assert(expr) => {
                test_id += 1;

                srv_actor_ref
                    .tell(CmdMsg::TryAssert {
                        name: test.name.clone(),
                        test: expr.clone(),
                        test_id: (idx, test_id),
                    })
                    .await?;

                loop {
                    if let Some(result) = received_passed_tests.get(&(idx, test_id)) {
                        if *result {
                            println!("pass test {}", expr);
                        } else {
                            println!("fail test {}", expr);
                        }
                        process_cmd_idx += 1;
                        break;
                    }

                    let maybe_msg = tst_rx.recv().await;
                    if let Some(CmdMsg::AssertCompleted {
                        test_id: recv_id,
                        result: test_result,
                    }) = maybe_msg
                    {
                        info!(
                            "Manager received Assertion {:?} {}",
                            recv_id,
                            if test_result { "passed" } else { "failed" }
                        );
                        received_passed_tests.insert(recv_id, test_result);

                        if let Some(result) = received_passed_tests.get(&(idx, test_id)) {
                            if *result {
                                println!("pass test {}", expr);
                            } else {
                                println!("fail test {}", expr);
                            }

                            process_cmd_idx += 1;
                            break;
                        }
                    }
                }
            }
        }
    }
    println!("testing {} finished", test.name);
    Ok(())
}
