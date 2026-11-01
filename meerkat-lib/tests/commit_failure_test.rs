//! A distributed commit that fails must be reported, not silently swallowed.
//!
//! Once a node decides to commit, it stores its own writes and asks every
//! participant to commit. A participant can refuse (a node further down the
//! chain never acknowledged) or never answer at all. When that result is
//! dropped the originator returns `Ok`, and the CLI prints `@test ... passed`
//! for a transaction only part of which is committed.
//!
//! Both tests stand in for the participant with a bare network peer that
//! refuses every `Commit`, so the outcome is chosen by the test rather than
//! being timing-dependent.

use meerkat_lib::net::ast::NetValue;
use meerkat_lib::net::{
    Address, MeerkatMessage, NetworkActor, NetworkCommand, NetworkEvent, NetworkReply, NodeType,
    ServiceNetId,
};
use meerkat_lib::runtime::ast::{ActionStmt, Expr, Value};
use meerkat_lib::runtime::manager::ParkedRequest;
use meerkat_lib::runtime::parser::parse_string;
use meerkat_lib::runtime::txn::{Transaction, TxnId, VarLock, WaitKey};
use meerkat_lib::runtime::{Interner, Manager, Node, Symbol};
use std::cell::Cell;
use std::collections::HashMap;

const REASON: &str = "node below never acknowledged";

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

/// The node under test, with `rc` redirected to the peer at `rc_addr`.
/// `mid_view` derives from both its own `mv` and `rc.gc`; `mid_own` derives
/// from `mv` alone. What each holds after a commit says which state the
/// recompute actually saw.
async fn middle_node(net: NetworkActor, rc_addr: &Address) -> Manager {
    let code = "
        service rc {
            var cv = 1;
            pub def gc = cv;
        }
        service mid {
            var mv = 0;
            pub def mid_view = mv + rc.gc;
            pub def mid_own = mv * 2;
        }
    ";
    let mut interner = Interner::new();
    let ast = parse_string(code, &mut interner).expect("valid syntax");
    let mut node = Node::new();
    node.interner = interner;
    node.unified_ast = ast.clone();
    node.static_checks().expect("static checks must pass");
    let local_ast = node.unified_ast.clone();
    let mut m = node
        .on_manager_startup(true, Some(net), HashMap::new(), &local_ast)
        .await
        .expect("service init must succeed");
    let rc = m.interner.insert("rc");
    m.remote_services
        .insert(rc, Address::new(format!("{}/rc", rc_addr.0)));
    m
}

async fn reply(net: &mut NetworkActor, reply_to: &str, msg: MeerkatMessage) {
    let _ = net
        .handle_command(NetworkCommand::SendMessage {
            addr: Address::new(reply_to),
            msg,
        })
        .await;
}

/// Stand in for `rc`: accept a composed action, refuse every `Commit` with
/// `reason` after recording in `got_commit` that it arrived, and so go on
/// serving the `gc = 1` it had committed before. Runs forever.
async fn refuse_commits(net: &mut NetworkActor, reason: &str, got_commit: &Cell<bool>) {
    loop {
        match net.event_rx.try_recv() {
            Ok(NetworkEvent::MessageReceived { msg, .. }) => match msg {
                MeerkatMessage::LookupRequest {
                    request_id,
                    reply_to,
                    ..
                } => {
                    let value = NetValue::Int { val: 1 };
                    let msg = MeerkatMessage::LookupResponse { request_id, value };
                    reply(net, &reply_to, msg).await;
                }
                MeerkatMessage::ActionRequest {
                    request_id,
                    reply_to,
                    ..
                } => {
                    let msg = MeerkatMessage::ActionResponse {
                        request_id,
                        success: true,
                        error: None,
                    };
                    reply(net, &reply_to, msg).await;
                }
                MeerkatMessage::Commit {
                    request_id,
                    reply_to,
                    ..
                } => {
                    got_commit.set(true);
                    let msg = MeerkatMessage::CommitResponse {
                        request_id,
                        success: false,
                        error: Some(reason.to_string()),
                    };
                    reply(net, &reply_to, msg).await;
                }
                _ => {}
            },
            Ok(_) => {}
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(1)).await,
        }
    }
}

/// The local half of the commit is done and cannot be taken back, and every
/// member derived from it is recomputed from what is observable.
fn assert_committed_locally(m: &mut Manager) {
    let mid = m.interner.insert("mid");
    let mv = m.interner.insert("mv");
    let mid_own = m.interner.insert("mid_own");
    let mid_view = m.interner.insert("mid_view");
    let vars = &m.services[&mid].vars;
    assert_eq!(vars[&mv].value, Value::Int { val: 1 });
    assert!(
        matches!(vars[&mv].lock, VarLock::Unlocked),
        "the write lock must be released"
    );
    assert_eq!(
        vars[&mid_own].value,
        Value::Int { val: 2 },
        "a member derived only from local state must still be refreshed: nothing \
         else will ever repair it"
    );
    assert_eq!(
        vars[&mid_view].value,
        Value::Int { val: 2 },
        "a member derived from the node below is recomputed against what that \
         node reports as committed (1 + 1)"
    );
}

/// Hold a transaction on `m` as a participant: it wrote `mid.mv` under a write
/// lock, and has `participants` as sub-participants. Returns its id and the
/// locked key.
fn hold_participant_txn(
    m: &mut Manager,
    participants: &[&Address],
) -> (TxnId, (ServiceNetId, Symbol)) {
    let mid = m.interner.insert("mid");
    let mv = m.interner.insert("mv");
    let tid = TxnId {
        timestamp: 1,
        node_id: 7,
        iteration: 0,
    };
    let key = (m.service_net_id_for_name(mid), mv);
    m.services
        .get_mut(&mid)
        .unwrap()
        .vars
        .get_mut(&mv)
        .unwrap()
        .lock = VarLock::WriteLocked(tid.clone());
    let mut txn = Transaction::new(tid.clone());
    txn.locked.insert(key.clone());
    txn.written.insert(key.clone(), Value::Int { val: 1 });
    txn.participants
        .extend(participants.iter().map(|a| (*a).clone()));
    m.pending_txns.insert(tid.clone(), txn);
    (tid, key)
}

/// The originator must not report success when a participant refused to commit.
#[tokio::test(flavor = "multi_thread")]
async fn test_originator_surfaces_a_participant_commit_failure() {
    let (mid_net, _mid_addr) = listening_node().await;
    let (mut rc_net, rc_addr) = listening_node().await;
    let mut m = middle_node(mid_net, &rc_addr).await;

    // `mid` originates `mv = 1; do <action on rc>`, so `rc` joins as a
    // participant.
    let mid = m.interner.insert("mid");
    let mv = m.interner.insert("mv");
    let stmts = vec![
        ActionStmt::Assign {
            name: mv,
            expr: Expr::Literal {
                val: Value::Int { val: 1 },
            },
        },
        ActionStmt::Do(Expr::Literal {
            val: Value::ActionClosure {
                stmts: Vec::new(),
                env: Vec::new(),
                service_net_id: ServiceNetId(format!("{}/rc", rc_addr.0)),
            },
        }),
    ];

    let got_commit = Cell::new(false);
    let result = tokio::select! {
        biased;
        r = m.execute_action(mid, &stmts) => r,
        _ = refuse_commits(&mut rc_net, REASON, &got_commit) => {
            unreachable!("the stand-in for rc runs forever")
        }
    };

    let err = result.expect_err(
        "a transaction whose participant refused to commit must not report success: the \
         CLI prints `@test ... passed` on Ok, for a transaction only part of which committed",
    );
    assert!(
        err.to_string().contains(REASON),
        "the participant's reason must reach the caller, got: {err}"
    );
    assert_committed_locally(&mut m);
}

/// A participant whose forward of the commit failed reports the failure, and
/// still propagates and still frees its locks.
///
/// The two outcomes are independent: the caller must report the error *and*
/// wake whatever was parked on the freed locks, which it finds in the deferred
/// queue rather than in the error path's return value.
#[tokio::test(flavor = "multi_thread")]
async fn test_participant_reports_a_failed_commit_forward_and_still_propagates() {
    let (mid_net, _mid_addr) = listening_node().await;
    let (mut rc_net, rc_addr) = listening_node().await;
    let mut m = middle_node(mid_net, &rc_addr).await;

    let (tid, (sid, mv)) = hold_participant_txn(&mut m, &[&rc_addr]);
    let key = WaitKey::Member(sid, mv);
    let mid = m.interner.insert("mid");
    let waiter = ParkedRequest::Lookup {
        request_id: 1,
        reply_to: String::new(),
        service: mid,
        member: mv,
        tid: TxnId::new(m.node_id),
    };
    m.park_request_key(key.clone(), waiter);

    let got_commit = Cell::new(false);
    let committed = tokio::select! {
        biased;
        c = m.commit_participant(&tid) => c,
        _ = refuse_commits(&mut rc_net, REASON, &got_commit) => {
            unreachable!("the stand-in for rc runs forever")
        }
    };

    let err = committed.expect_err("a refused commit below must be reported upward");
    assert!(
        err.to_string().contains(REASON),
        "the reason must survive the hop, got: {err}"
    );
    assert!(
        m.take_freed_awaiting_wake().contains(&key),
        "the freed lock must be reported so what is parked on it gets woken"
    );
    assert_committed_locally(&mut m);
}

/// A refused commit must not stop the commit loop: every later participant
/// still needs its `Commit`, or it is left prepared and holding locks.
#[tokio::test(flavor = "multi_thread")]
async fn test_every_participant_is_committed_after_one_refuses() {
    let (mid_net, _mid_addr) = listening_node().await;
    let (mut rc_net, rc_addr) = listening_node().await;
    let (mut other_net, other_addr) = listening_node().await;
    let mut m = middle_node(mid_net, &rc_addr).await;
    let (tid, _) = hold_participant_txn(&mut m, &[&rc_addr, &other_addr]);

    // Both refuse, so whichever is sent `Commit` first, the other is only
    // reached if the loop keeps going after a failure.
    let (rc_got, other_got) = (Cell::new(false), Cell::new(false));
    let committed = tokio::select! {
        biased;
        c = m.commit_participant(&tid) => c,
        _ = refuse_commits(&mut rc_net, "rc refused", &rc_got) => {
            unreachable!("the stand-in for rc runs forever")
        }
        _ = refuse_commits(&mut other_net, "other refused", &other_got) => {
            unreachable!("the other stand-in runs forever")
        }
    };

    assert!(
        rc_got.get() && other_got.get(),
        "every participant must be sent `Commit` (rc: {}, other: {})",
        rc_got.get(),
        other_got.get()
    );
    // Which failure is reported depends on the participant set's iteration
    // order, which means nothing to a caller, so either is accepted.
    let err = committed
        .expect_err("a refused commit must be reported")
        .to_string();
    assert!(
        err.contains("rc refused") || err.contains("other refused"),
        "the error must be one of the participants' reasons, got: {err}"
    );
    assert_committed_locally(&mut m);
}
