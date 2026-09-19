//! Wait-die's "wait" half has to reach the node that started the transaction.
//!
//! A participant that cannot take a lock because the holder is younger returns
//! `EvalError::WaitOn`, and the server loop parks the request on the contended
//! key and re-runs it when the lock frees. An originator has no such queue: it
//! drives its own transaction, so the only way it can wait is to abort and run
//! the transaction again.
//!
//! This matters more since defs are recomputed inside the transaction.
//! `recompute_def_in_txn` evaluates under the transaction id, so a def's
//! cross-service dependency that nothing has cached is read through `lookup`
//! and takes a read lock -- which means an ordinary local write, in a program
//! with no remote services at all, can now come back `WaitOn`.

use meerkat_lib::net::{
    Address, MeerkatMessage, NetworkActor, NetworkCommand, NetworkEvent, NetworkReply, NodeType,
    ServiceNetId,
};
use meerkat_lib::runtime::ast::{ActionStmt, Expr, Value};
use meerkat_lib::runtime::parser::parse_string;
use meerkat_lib::runtime::txn::{TxnId, VarLock};
use meerkat_lib::runtime::{Interner, Manager, Node};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

/// `q.mm` derives from `q`'s own `w` and from `p.get_n`, so writing `w` forces
/// a recompute that has to read across the service boundary.
const PROGRAM: &str = "
    service p {
        var n = 1;
        pub def get_n = n;
    }
    service q {
        var w = 0;
        pub def mm = w + p.get_n;
    }
";

async fn setup(net: Option<NetworkActor>) -> Manager {
    let mut interner = Interner::new();
    let ast = parse_string(PROGRAM, &mut interner).expect("valid syntax");
    let mut node = Node::new();
    node.interner = interner;
    node.unified_ast = ast.clone();
    node.static_checks().expect("static checks must pass");
    let local_ast = node.unified_ast.clone();
    node.on_manager_startup(true, net, HashMap::new(), &local_ast)
        .await
        .expect("service init must succeed")
}

/// Write-lock `p.get_n` on behalf of a transaction younger than any this
/// process is about to mint, which is the case wait-die resolves by waiting.
fn hold_get_n_by_a_younger_transaction(m: &mut Manager) {
    let p = m.interner.insert("p");
    let get_n = m.interner.insert("get_n");
    let younger = TxnId {
        timestamp: u128::MAX,
        node_id: m.node_id,
        iteration: 0,
    };
    m.services
        .get_mut(&p)
        .unwrap()
        .vars
        .get_mut(&get_n)
        .unwrap()
        .lock = VarLock::WriteLocked(younger);
}

/// A wait that never resolves must still be reported in terms of the program,
/// not as the runtime's own control signal.
#[tokio::test]
async fn test_originator_reports_a_contended_member_not_a_wait_key() {
    let mut m = setup(None).await;
    hold_get_n_by_a_younger_transaction(&mut m);

    let q = m.interner.insert("q");
    let w = m.interner.insert("w");
    let stmts = vec![ActionStmt::Assign {
        name: w,
        expr: Expr::Literal {
            val: Value::Int { val: 5 },
        },
    }];

    let err = m
        .execute_action(q, &stmts)
        .await
        .expect_err("the contended read cannot succeed while the lock is held");
    let msg = err.to_string();
    assert!(
        msg.contains("p.get_n"),
        "the message must name what was contended, got: {msg}"
    );
    assert!(
        !msg.contains("Symbol(") && !msg.contains("ServiceNetId("),
        "`WaitOn` carries interned ids that mean nothing outside this node and \
         must not reach the caller, got: {msg}"
    );
}

/// Bring a `NetworkActor` up on an ephemeral loopback port.
async fn listening_node() -> (NetworkActor, Address) {
    let mut net = NetworkActor::new(NodeType::Server)
        .await
        .expect("network actor");
    let reply = net
        .handle_command(NetworkCommand::Listen {
            addr: Address::new("/ip4/127.0.0.1/tcp/0"),
        })
        .await;
    let addr = match reply {
        NetworkReply::ListenSuccess { addr } => addr,
        other => panic!("expected ListenSuccess, got {:?}", other),
    };
    let full = Address::new(format!("{}/p2p/{}", addr.0, net.local_peer_id()));
    (net, full)
}

/// The transaction has to be run again, not abandoned at the first contention.
///
/// Each attempt aborts its participants before retrying, so a participant that
/// counts the `Abort`s it is sent counts the attempts -- and the originator
/// awaits each acknowledgement, so the count is not timing-dependent.
#[tokio::test(flavor = "multi_thread")]
async fn test_originator_retries_the_transaction_on_a_wait() {
    let (origin_net, _origin_addr) = listening_node().await;
    let (mut peer_net, peer_addr) = listening_node().await;

    let mut m = setup(Some(origin_net)).await;
    hold_get_n_by_a_younger_transaction(&mut m);

    let q = m.interner.insert("q");
    let w = m.interner.insert("w");

    // The `do` runs first so the peer is a registered participant by the time
    // the write below contends; the write is what forces `mm` to be recomputed.
    let remote = ServiceNetId::new(format!("{}/rem", peer_addr.0));
    let stmts = vec![
        ActionStmt::Do(Expr::Literal {
            val: Value::ActionClosure {
                stmts: Vec::new(),
                env: Vec::new(),
                service_net_id: remote,
            },
        }),
        ActionStmt::Assign {
            name: w,
            expr: Expr::Literal {
                val: Value::Int { val: 5 },
            },
        },
    ];

    let aborts = AtomicUsize::new(0);
    let peer = async {
        loop {
            match peer_net.event_rx.try_recv() {
                Ok(NetworkEvent::MessageReceived { msg, .. }) => match msg {
                    MeerkatMessage::ActionRequest {
                        request_id,
                        reply_to,
                        ..
                    } => {
                        let _ = peer_net
                            .handle_command(NetworkCommand::SendMessage {
                                addr: Address::new(&reply_to),
                                msg: MeerkatMessage::ActionResponse {
                                    request_id,
                                    success: true,
                                    error: None,
                                    touched_services: Vec::new(),
                                },
                            })
                            .await;
                    }
                    MeerkatMessage::Abort {
                        request_id,
                        reply_to,
                        ..
                    } => {
                        aborts.fetch_add(1, Ordering::SeqCst);
                        let _ = peer_net
                            .handle_command(NetworkCommand::SendMessage {
                                addr: Address::new(&reply_to),
                                msg: MeerkatMessage::AbortResponse { request_id },
                            })
                            .await;
                    }
                    _ => {}
                },
                Ok(_) => {}
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(1)).await,
            }
        }
    };

    let result = tokio::select! {
        biased;
        r = m.execute_action(q, &stmts) => r,
        _ = peer => unreachable!("the stand-in participant runs until the transaction is done"),
    };
    result.expect_err("the contended read cannot succeed while the lock is held");

    let attempts = aborts.load(Ordering::SeqCst);
    assert_eq!(
        attempts,
        meerkat_lib::runtime::manager::MAX_WAIT_DIE_RETRIES as usize + 1,
        "the transaction must be retried through the wait-die budget, not \
         abandoned after the first contention"
    );
}
