//! A distributed commit that fails must be reported, not silently swallowed.
//!
//! `execute_action_with_txn` decides to commit, stores its own writes, and then
//! asks every participant to commit. A participant can refuse (a node further
//! down the chain never acknowledged) or never answer at all. When that result
//! is dropped the originator returns `Ok`, and the CLI prints `@test ... passed`
//! for a transaction only part of which is on disk.
//!
//! Both tests stand in for the participant with a bare network peer, so the
//! commit outcome is chosen by the test rather than being timing-dependent.

use meerkat_lib::net::{
    Address, MeerkatMessage, NetworkActor, NetworkCommand, NetworkEvent, NetworkReply, NodeType,
    ServiceNetId,
};
use meerkat_lib::runtime::ast::{ActionStmt, Expr, Value};
use meerkat_lib::runtime::parser::parse_string;
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

/// The originating node: one local service whose action writes a `var` and
/// composes an action onto the node the test stands in for.
async fn origin_node(net: NetworkActor) -> Manager {
    let code = "
        service app {
            var a = 0;
            pub def av = a + 1;
        }
    ";
    let mut interner = Interner::new();
    let ast = parse_string(code, &mut interner).expect("valid syntax");
    let mut node = Node::new();
    node.interner = interner;
    node.unified_ast = ast.clone();
    node.static_checks().expect("static checks must pass");
    let local_ast = node.unified_ast.clone();
    node.on_manager_startup(true, Some(net), HashMap::new(), &local_ast)
        .await
        .expect("service init must succeed")
}

/// Stand in for a participant: accept the composed action, then answer the
/// `Commit` with the outcome the test asked for. Runs until cancelled.
async fn participant_that_refuses_to_commit(net: &mut NetworkActor) {
    loop {
        match net.event_rx.try_recv() {
            Ok(NetworkEvent::MessageReceived { msg, .. }) => match msg {
                MeerkatMessage::ActionRequest {
                    request_id,
                    reply_to,
                    ..
                } => {
                    let _ = net
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
                MeerkatMessage::Commit {
                    request_id,
                    reply_to,
                    ..
                } => {
                    let _ = net
                        .handle_command(NetworkCommand::SendMessage {
                            addr: Address::new(&reply_to),
                            msg: MeerkatMessage::CommitResponse {
                                request_id,
                                success: false,
                                error: Some("participant could not commit".to_string()),
                            },
                        })
                        .await;
                }
                _ => {}
            },
            Ok(_) => {}
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(1)).await,
        }
    }
}

/// The originator must not report success when a participant refused to commit.
#[tokio::test(flavor = "multi_thread")]
async fn test_originator_surfaces_a_participant_commit_failure() {
    let (origin_net, _origin_addr) = listening_node().await;
    let (mut peer_net, peer_addr) = listening_node().await;

    let mut m = origin_node(origin_net).await;
    let app = m.interner.insert("app");
    let a = m.interner.insert("a");

    // `do` on a closure owned by the peer: `service_name_for_net_id` returns
    // `None` for it, so the executor ships it as a remote action, which is what
    // registers the peer as a participant of this transaction.
    let remote = ServiceNetId::new(format!("{}/rem", peer_addr.0));
    let stmts = vec![
        ActionStmt::Assign {
            name: a,
            expr: Expr::Literal {
                val: Value::Int { val: 1 },
            },
        },
        ActionStmt::Do(Expr::Literal {
            val: Value::ActionClosure {
                stmts: Vec::new(),
                env: Vec::new(),
                service_net_id: remote,
            },
        }),
    ];

    let result = tokio::select! {
        biased;
        r = m.execute_action_with_txn(app, &stmts, &[]) => r,
        _ = participant_that_refuses_to_commit(&mut peer_net) => {
            unreachable!("the stand-in participant runs until the transaction is done")
        }
    };

    let err = result.expect_err(
        "a transaction whose participant refused to commit must not report success: the \
         CLI prints `@test ... passed` on Ok, for a transaction only part of which committed",
    );
    assert!(
        err.to_string().contains("participant could not commit"),
        "the participant's reason must reach the caller, got: {err}"
    );
}

/// The middle node of `client -> mid -> rc`, with a real network layer so it
/// can talk to the node below. `mid_view` derives from both its own `mv` and
/// `rc.gc`; `mid_own` derives from `mv` alone. What each holds after a commit
/// says which state the recompute actually saw.
async fn middle_node(net: NetworkActor) -> Manager {
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
    node.on_manager_startup(true, Some(net), HashMap::new(), &local_ast)
        .await
        .expect("service init must succeed")
}

/// A participant whose forward of the commit failed reports the failure and
/// still recomputes what it derives.
///
/// The local commit is done either way -- 2PC has no way back once the decision
/// is taken -- so the two outcomes are independent and `commit_participant`
/// returns them separately. Skipping propagation on this path would leave every
/// derived member of the write stale with nothing to repair it: `mid_own` reads
/// only local state, so no `Update` will ever arrive to correct it. Recomputing
/// instead gives each member the value it derives from the committed state that
/// is observable at that moment, which is also what repairs itself if the node
/// below did commit and only the acknowledgement was lost.
#[tokio::test(flavor = "multi_thread")]
async fn test_participant_reports_a_failed_commit_forward_and_still_propagates() {
    use meerkat_lib::net::ast::NetValue;
    use meerkat_lib::runtime::txn::{Transaction, TxnId};

    let (mid_net, _mid_addr) = listening_node().await;
    let (mut rc_net, rc_addr) = listening_node().await;

    let mut m = middle_node(mid_net).await;
    let rc = m.interner.insert("rc");
    m.remote_services
        .insert(rc, Address::new(format!("{}/rc", rc_addr.0)));

    let mid = m.interner.insert("mid");
    let mv = m.interner.insert("mv");
    let mid_view = m.interner.insert("mid_view");
    let mid_own = m.interner.insert("mid_own");

    let tid = TxnId {
        timestamp: 1,
        node_id: 7,
        iteration: 0,
    };
    let mut txn = Transaction::new(tid.clone());
    txn.written
        .insert((m.service_net_id_for_name(mid), mv), Value::Int { val: 1 });
    txn.participants.insert(rc_addr.clone());
    m.pending_txns.insert(tid.clone(), txn);

    // Stand in for `rc`: it refuses the commit, so it goes on serving the `gc`
    // it had committed before the transaction.
    let rc_loop = async {
        loop {
            match rc_net.event_rx.try_recv() {
                Ok(NetworkEvent::MessageReceived { msg, .. }) => match msg {
                    MeerkatMessage::LookupRequest {
                        request_id,
                        reply_to,
                        ..
                    } => {
                        let _ = rc_net
                            .handle_command(NetworkCommand::SendMessage {
                                addr: Address::new(&reply_to),
                                msg: MeerkatMessage::LookupResponse {
                                    request_id,
                                    value: NetValue::Int { val: 1 },
                                },
                            })
                            .await;
                    }
                    MeerkatMessage::Commit {
                        request_id,
                        reply_to,
                        ..
                    } => {
                        let _ = rc_net
                            .handle_command(NetworkCommand::SendMessage {
                                addr: Address::new(&reply_to),
                                msg: MeerkatMessage::CommitResponse {
                                    request_id,
                                    success: false,
                                    error: Some("node below never acknowledged".to_string()),
                                },
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

    let committed = tokio::select! {
        biased;
        c = m.commit_participant(&tid) => c,
        _ = rc_loop => unreachable!("the stand-in for rc runs until the commit is done"),
    };

    let err = committed
        .forward_error
        .expect("a refused commit below must be reported upward");
    assert!(
        err.to_string().contains("node below never acknowledged"),
        "the reason must survive the hop, got: {err}"
    );
    assert_eq!(
        m.services[&mid].vars[&mv].value,
        Value::Int { val: 1 },
        "the local half of the commit is done and cannot be taken back"
    );
    assert_eq!(
        m.services[&mid].vars[&mid_own].value,
        Value::Int { val: 2 },
        "a member derived only from local state must still be refreshed: nothing \
         else will ever repair it"
    );
    assert_eq!(
        m.services[&mid].vars[&mid_view].value,
        Value::Int { val: 2 },
        "a member derived from the node below is recomputed against what that \
         node reports as committed (1 + 1), so it agrees with a fresh read"
    );
}
