//! A participant action that parks must not re-run the effects it already had.
//!
//! `WaitOn` parks the whole action and `dispatch_parked` re-dispatches it from
//! its first statement, so every statement that already ran runs again. The
//! local buffers are restored before parking, which makes a replayed local
//! write harmless -- it recomputes from the same starting state. A composed
//! action onto another node is not covered by that: it already executed on the
//! other node under the shared transaction id, its writes are buffered there,
//! and this node cannot roll them back. Re-sending it applies a second time on
//! top of the state the first attempt left.
//!
//! The child here is a real `Manager`, driven by the stand-in peer, so what it
//! holds after each attempt is what a real participant would hold.

use meerkat_lib::net::{
    codec, Address, MeerkatMessage, NetworkActor, NetworkCommand, NetworkEvent, NetworkReply,
    NodeType, ServiceNetId,
};
use meerkat_lib::runtime::ast::{ActionStmt, BinOp, Expr, Value};
use meerkat_lib::runtime::parser::parse_string;
use meerkat_lib::runtime::txn::{TxnId, VarLock};
use meerkat_lib::runtime::{Interner, Manager, Node};
use std::collections::HashMap;

/// Bring a `NetworkActor` up on an ephemeral loopback port and return it with
/// its dialable address.
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

async fn manager_for(code: &str, net: Option<NetworkActor>) -> Manager {
    let mut interner = Interner::new();
    let ast = parse_string(code, &mut interner).expect("valid syntax");
    let mut node = Node::new();
    node.interner = interner;
    node.unified_ast = ast.clone();
    node.static_checks().expect("static checks must pass");
    let local_ast = node.unified_ast.clone();
    node.on_manager_startup(true, net, HashMap::new(), &local_ast)
        .await
        .expect("service init must succeed")
}

/// The node under test. `mv` stands in for ordinary local work; `guard` is the
/// member the test contends so the action parks at a chosen point.
async fn mid_node(net: NetworkActor) -> Manager {
    manager_for(
        "
        service mid {
            var mv = 0;
            var guard = 0;
        }
        ",
        Some(net),
    )
    .await
}

/// The node the composed action runs on. `cv` records how many times it was
/// applied.
async fn child_node() -> Manager {
    manager_for(
        "
        service rc {
            var cv = 1;
            pub def gc = cv;
        }
        ",
        None,
    )
    .await
}

/// Write-lock `mid.guard` on behalf of a transaction younger than any this
/// process is about to mint. Wait-die resolves that by waiting, which is the
/// `WaitOn` that parks the action.
fn contend_guard(m: &mut Manager) {
    let mid = m.interner.insert("mid");
    let guard = m.interner.insert("guard");
    let younger = TxnId {
        timestamp: u128::MAX,
        node_id: m.node_id,
        iteration: 0,
    };
    m.services
        .get_mut(&mid)
        .unwrap()
        .vars
        .get_mut(&guard)
        .unwrap()
        .lock = VarLock::WriteLocked(younger);
}

/// Release the contended lock, as the holder's commit or abort would.
fn release_guard(m: &mut Manager) {
    let mid = m.interner.insert("mid");
    let guard = m.interner.insert("guard");
    m.services
        .get_mut(&mid)
        .unwrap()
        .vars
        .get_mut(&guard)
        .unwrap()
        .lock = VarLock::Unlocked;
}

/// Stand in for the node that owns `rc`: decode each `ActionRequest` and run it
/// against a real `Manager` under the shared transaction id, exactly as the
/// server loop does. Records the id of every request it served. Runs until
/// cancelled.
async fn child_peer(net: &mut NetworkActor, child: &mut Manager, served: &mut Vec<TxnId>) {
    loop {
        match net.event_rx.try_recv() {
            Ok(NetworkEvent::MessageReceived { msg, .. }) => {
                if let MeerkatMessage::ActionRequest {
                    request_id,
                    service,
                    stmts,
                    reply_to,
                    txn_id,
                    ..
                } = msg
                {
                    let svc = child.interner.insert(&service);
                    let local: Vec<ActionStmt> = stmts
                        .into_iter()
                        .map(|s| {
                            codec::decode_action_stmt(s, &mut child.interner).expect("decodable")
                        })
                        .collect();
                    let tid = txn_id.expect("a composed action carries the shared transaction id");
                    served.push(tid.clone());
                    let outcome = child
                        .execute_action_participant(svc, &local, &[], tid)
                        .await;
                    let _ = net
                        .handle_command(NetworkCommand::SendMessage {
                            addr: Address::new(&reply_to),
                            msg: MeerkatMessage::ActionResponse {
                                request_id,
                                success: outcome.is_ok(),
                                error: outcome.err().map(|e| e.to_string()),
                                touched_services: vec!["rc".to_string()],
                            },
                        })
                        .await;
                }
            }
            Ok(_) => {}
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(1)).await,
        }
    }
}

/// What the child has buffered for `rc.cv` under `tid`, or its committed value
/// if the transaction buffered nothing.
fn child_cv(child: &mut Manager, tid: &TxnId) -> i32 {
    let rc = child.interner.insert("rc");
    let cv = child.interner.insert("cv");
    let sid = child.service_net_id_for_name(rc);
    let buffered = child
        .pending_txns
        .get(tid)
        .and_then(|t| t.written.get(&(sid, cv)).cloned());
    let value = buffered.unwrap_or_else(|| {
        child
            .services
            .get(&rc)
            .and_then(|s| s.vars.get(&cv))
            .map(|v| v.value.clone())
            .expect("rc.cv exists")
    });
    match value {
        Value::Int { val } => val,
        other => panic!("rc.cv should be an Int, got {:?}", other),
    }
}

/// Run one participant action to completion, with the stand-in child serving
/// whatever it composes. Mirrors what the server loop does for a parked
/// request: same manager, same statements, same transaction id.
async fn run_action(
    mid: &mut Manager,
    peer_net: &mut NetworkActor,
    child: &mut Manager,
    served: &mut Vec<TxnId>,
    service: meerkat_lib::runtime::interner::Symbol,
    stmts: &[ActionStmt],
    tid: &TxnId,
) -> Result<(), meerkat_lib::runtime::interpreter::EvalError> {
    tokio::select! {
        biased;
        r = mid.execute_action_participant(service, stmts, &[], tid.clone()) => r,
        _ = child_peer(peer_net, child, served) => {
            unreachable!("the stand-in child runs until the action under test finishes")
        }
    }
}

/// `do rc.{cv = cv + 1}`, dispatched to the node at `peer_addr`.
fn bump(cv: meerkat_lib::runtime::interner::Symbol, peer_addr: &Address) -> ActionStmt {
    ActionStmt::Do(Expr::Literal {
        val: Value::ActionClosure {
            stmts: vec![ActionStmt::Assign {
                name: cv,
                expr: Expr::Binop {
                    op: BinOp::Add,
                    expr1: Box::new(Expr::Variable { name: cv }),
                    expr2: Box::new(Expr::Literal {
                        val: Value::Int { val: 1 },
                    }),
                },
            }],
            env: Vec::new(),
            service_net_id: ServiceNetId::new(format!("{}/rc", peer_addr.0)),
        },
    })
}

/// Write `1` to `mid.guard`, which parks while the lock is contended.
fn touch_guard(guard: meerkat_lib::runtime::interner::Symbol) -> ActionStmt {
    ActionStmt::Assign {
        name: guard,
        expr: Expr::Literal {
            val: Value::Int { val: 1 },
        },
    }
}

/// A participant action that composed onto another node and then parked must
/// apply that composed action exactly once.
///
/// The action is `do rc.{cv = cv + 1}` followed by a write to a contended
/// member. The first attempt runs the composed action (the child buffers
/// `cv = 2`) and then parks. When the lock frees the action is re-dispatched
/// from its first statement, which sends the same composed action again under
/// the same transaction id; the child resumes the same `pending_txns` entry,
/// reads its own buffered `2` and buffers `3`. Nothing reports an error, and
/// the transaction goes on to commit a value the program never asked for.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_parked_action_applies_its_composed_child_action_exactly_once() {
    let (mid_net, _mid_addr) = listening_node().await;
    let (mut peer_net, peer_addr) = listening_node().await;

    let mut mid = mid_node(mid_net).await;
    let mut child = child_node().await;
    let mut served: Vec<TxnId> = Vec::new();

    let mid_sym = mid.interner.insert("mid");
    let guard = mid.interner.insert("guard");
    // Built in `mid`'s interner: the closure is encoded here and decoded into
    // the child's interner on arrival.
    let cv = mid.interner.insert("cv");

    let stmts = vec![
        // The composed action: `rc.cv = rc.cv + 1`, on the node the peer owns.
        ActionStmt::Do(Expr::Literal {
            val: Value::ActionClosure {
                stmts: vec![ActionStmt::Assign {
                    name: cv,
                    expr: Expr::Binop {
                        op: BinOp::Add,
                        expr1: Box::new(Expr::Variable { name: cv }),
                        expr2: Box::new(Expr::Literal {
                            val: Value::Int { val: 1 },
                        }),
                    },
                }],
                env: Vec::new(),
                service_net_id: ServiceNetId::new(format!("{}/rc", peer_addr.0)),
            },
        }),
        // Parks: the lock on `guard` is held by a younger transaction.
        ActionStmt::Assign {
            name: guard,
            expr: Expr::Literal {
                val: Value::Int { val: 1 },
            },
        },
    ];

    contend_guard(&mut mid);
    let tid = TxnId::new(mid.node_id);

    // First attempt: the composed action lands, then the action parks.
    let first = tokio::select! {
        biased;
        r = mid.execute_action_participant(mid_sym, &stmts, &[], tid.clone()) => r,
        _ = child_peer(&mut peer_net, &mut child, &mut served) => {
            unreachable!("the stand-in child runs until the action under test finishes")
        }
    };
    assert!(
        matches!(
            first,
            Err(meerkat_lib::runtime::interpreter::EvalError::WaitOn(_))
        ),
        "the contended write must park the action, got: {:?}",
        first
    );
    assert_eq!(served.len(), 1, "the composed action ran once so far");
    assert_eq!(
        child_cv(&mut child, &tid),
        2,
        "the child buffered one increment"
    );

    // The holder releases, and `wake_ready` re-dispatches the parked action --
    // which is this exact call, from the first statement.
    release_guard(&mut mid);
    let second = tokio::select! {
        biased;
        r = mid.execute_action_participant(mid_sym, &stmts, &[], tid.clone()) => r,
        _ = child_peer(&mut peer_net, &mut child, &mut served) => {
            unreachable!("the stand-in child runs until the action under test finishes")
        }
    };
    second.expect("the re-dispatched action must now succeed");

    assert!(
        served.iter().all(|t| *t == tid),
        "every composed request belongs to the same transaction"
    );
    assert_eq!(
        served.len(),
        1,
        "the composed action must be sent once, not re-sent on replay: the child \
         already executed it under this transaction id and holds its writes buffered"
    );
    assert_eq!(
        child_cv(&mut child, &tid),
        2,
        "`do rc.{{cv = cv + 1}}` appears once in the program, so `cv` must end at 2"
    );
}

/// Two composed actions in one parked action are both replayed as completed,
/// and neither is re-sent.
///
/// Guards the position bookkeeping: the records are matched by dispatch order,
/// so a rewind that lands off by one would skip the wrong call or re-send it.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_parked_action_replays_every_composed_action_it_completed() {
    let (mid_net, _mid_addr) = listening_node().await;
    let (mut peer_net, peer_addr) = listening_node().await;

    let mut mid = mid_node(mid_net).await;
    let mut child = child_node().await;
    let mut served: Vec<TxnId> = Vec::new();

    let mid_sym = mid.interner.insert("mid");
    let guard = mid.interner.insert("guard");
    let cv = mid.interner.insert("cv");

    let stmts = vec![
        bump(cv, &peer_addr),
        bump(cv, &peer_addr),
        touch_guard(guard),
    ];

    contend_guard(&mut mid);
    let tid = TxnId::new(mid.node_id);

    let first = run_action(
        &mut mid,
        &mut peer_net,
        &mut child,
        &mut served,
        mid_sym,
        &stmts,
        &tid,
    )
    .await;
    assert!(
        matches!(
            first,
            Err(meerkat_lib::runtime::interpreter::EvalError::WaitOn(_))
        ),
        "the contended write must park the action, got: {:?}",
        first
    );
    assert_eq!(served.len(), 2, "both composed actions ran before the park");
    assert_eq!(child_cv(&mut child, &tid), 3, "1 + two increments");

    release_guard(&mut mid);
    run_action(
        &mut mid,
        &mut peer_net,
        &mut child,
        &mut served,
        mid_sym,
        &stmts,
        &tid,
    )
    .await
    .expect("the re-dispatched action must now succeed");

    assert_eq!(
        served.len(),
        2,
        "neither composed action may be sent a second time"
    );
    assert_eq!(
        child_cv(&mut child, &tid),
        3,
        "two increments appear in the program, so `cv` must end at 3"
    );
}

/// Suppression is scoped to the replay, not to the transaction.
///
/// An originator can compose two actions onto the same participant under one
/// transaction id. The second is a new dispatch, not a replay of the first, and
/// must go out: treating "already composed onto this node" as the condition
/// would silently drop it.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_second_composed_action_under_the_same_transaction_still_dispatches() {
    let (mid_net, _mid_addr) = listening_node().await;
    let (mut peer_net, peer_addr) = listening_node().await;

    let mut mid = mid_node(mid_net).await;
    let mut child = child_node().await;
    let mut served: Vec<TxnId> = Vec::new();

    let mid_sym = mid.interner.insert("mid");
    let cv = mid.interner.insert("cv");
    let stmts = vec![bump(cv, &peer_addr)];

    // Nothing is contended here: both actions run straight through.
    let tid = TxnId::new(mid.node_id);
    for _ in 0..2 {
        run_action(
            &mut mid,
            &mut peer_net,
            &mut child,
            &mut served,
            mid_sym,
            &stmts,
            &tid,
        )
        .await
        .expect("an uncontended composed action must succeed");
    }

    assert_eq!(
        served.len(),
        2,
        "two separate actions under one transaction are two dispatches"
    );
    assert_eq!(
        child_cv(&mut child, &tid),
        3,
        "each action increments once, so `cv` must end at 3"
    );
}

/// A node whose `mid_view` derives from both its own `mv` and the composed
/// node's `rc.gc`, so a completed composed action forces a refresh that reads.
async fn mid_node_deriving_from_rc(net: NetworkActor) -> Manager {
    manager_for(
        "
        service rc {
            var cv = 1;
            pub def gc = cv;
        }
        service mid {
            var mv = 0;
            var guard = 0;
            pub def mid_view = mv + rc.gc;
        }
        ",
        Some(net),
    )
    .await
}

/// Write-lock `mid.mv` on behalf of a younger transaction, so the refresh that
/// follows a composed action parks when it reads `mv`.
fn contend_mv(m: &mut Manager) -> TxnId {
    let mid = m.interner.insert("mid");
    let mv = m.interner.insert("mv");
    let younger = TxnId {
        timestamp: u128::MAX,
        node_id: m.node_id,
        iteration: 0,
    };
    m.services
        .get_mut(&mid)
        .unwrap()
        .vars
        .get_mut(&mv)
        .unwrap()
        .lock = VarLock::WriteLocked(younger.clone());
    younger
}

/// The park can happen *inside* the `do`, after the child has already applied.
///
/// `remote_action` refreshes the local defs over the services the composed
/// action touched as soon as it returns, and that refresh reads under the
/// transaction -- so it can raise `WaitOn` with the child's write already
/// buffered on the other node. This is the park point the in-transaction
/// refresh introduced, and it is the reason the completed dispatch has to be
/// recorded before the refresh runs rather than after it.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_park_inside_the_do_statement_does_not_re_send_the_child_action() {
    let (mid_net, _mid_addr) = listening_node().await;
    let (mut peer_net, peer_addr) = listening_node().await;

    let mut mid = mid_node_deriving_from_rc(mid_net).await;
    let mut child = child_node().await;
    let mut served: Vec<TxnId> = Vec::new();

    let mid_sym = mid.interner.insert("mid");
    let mv = mid.interner.insert("mv");
    let cv = mid.interner.insert("cv");
    let stmts = vec![bump(cv, &peer_addr)];

    let holder = contend_mv(&mut mid);
    let tid = TxnId::new(mid.node_id);

    let first = run_action(
        &mut mid,
        &mut peer_net,
        &mut child,
        &mut served,
        mid_sym,
        &stmts,
        &tid,
    )
    .await;
    assert!(
        matches!(
            first,
            Err(meerkat_lib::runtime::interpreter::EvalError::WaitOn(_))
        ),
        "refreshing `mid_view` over the contended `mv` must park the action, got: {:?}",
        first
    );
    assert_eq!(
        served.len(),
        1,
        "the child ran before the refresh parked the action"
    );
    assert_eq!(
        child_cv(&mut child, &tid),
        2,
        "the child buffered one increment"
    );

    // The holder releases; the parked action is re-dispatched from the top,
    // which re-enters the same `do`.
    mid.services
        .get_mut(&mid_sym)
        .unwrap()
        .vars
        .get_mut(&mv)
        .unwrap()
        .lock
        .release(&holder);
    run_action(
        &mut mid,
        &mut peer_net,
        &mut child,
        &mut served,
        mid_sym,
        &stmts,
        &tid,
    )
    .await
    .expect("the re-dispatched action must now succeed");

    assert_eq!(
        served.len(),
        1,
        "a `do` that completed before the refresh parked must not be sent again"
    );
    assert_eq!(
        child_cv(&mut child, &tid),
        2,
        "`do rc.{{cv = cv + 1}}` appears once, so `cv` must end at 2"
    );
}
