//! A participant must commit the nodes it composed onto before recomputing
//! anything derived from them.
//!
//! An originator already does this (`execute_action_with_txn`): it stores its
//! own writes, commits every participant, and only then propagates. A node in
//! the middle of a chain -- one that is itself a participant and has composed a
//! further action onto a third node -- has to follow the same order. Applying
//! and propagating its own writes first recomputes its derived members while
//! the node below it is still holding the write buffered, storing a value that
//! was never true.
//!
//! The setup is the middle node of `client -> mid -> rc`: `mid` is committing
//! as a participant, it wrote `mv`, it has `rc` as a sub-participant, and
//! `def mid_view = mv + rc.gc` derives from both. The test stands in for `rc`
//! with a bare network peer that only reports the new value of `gc` once it has
//! been told to commit, which is exactly what a real participant does.

use meerkat_lib::net::ast::NetValue;
use meerkat_lib::net::{
    Address, MeerkatMessage, NetworkActor, NetworkCommand, NetworkEvent, NetworkReply, NodeType,
};
use meerkat_lib::runtime::ast::Value;
use meerkat_lib::runtime::parser::parse_string;
use meerkat_lib::runtime::txn::{Transaction, TxnId};
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

/// The middle node, with a real network layer so it can talk to the node below.
async fn middle_node(net: NetworkActor) -> Manager {
    let code = "
        service rc {
            var cv = 1;
            pub def gc = cv;
        }
        service mid {
            var mv = 0;
            pub def mid_view = mv + rc.gc;
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

#[tokio::test(flavor = "multi_thread")]
async fn test_participant_commits_sub_participants_before_propagating() {
    let (mid_net, _mid_addr) = listening_node().await;
    let (mut rc_net, rc_addr) = listening_node().await;

    let mut m = middle_node(mid_net).await;

    // `rc` now lives on the peer this test drives, so every read of `rc.gc`
    // goes over the wire and its timing relative to `Commit` is observable.
    let rc = m.interner.insert("rc");
    m.remote_services
        .insert(rc, Address::new(format!("{}/rc", rc_addr.0)));

    let mid = m.interner.insert("mid");
    let mv = m.interner.insert("mv");
    let mid_view = m.interner.insert("mid_view");
    assert_eq!(
        m.services[&mid].vars[&mid_view].value,
        Value::Int { val: 1 },
        "before the transaction, mid_view is 0 + 1"
    );

    // A transaction this node is holding as a participant: it wrote `mv`, and
    // it composed an action onto `rc`, which is therefore holding a write of
    // its own until we tell it to commit.
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

    // Stand in for `rc`: serve reads of `gc` as a real participant would --
    // the old committed value until `Commit` arrives, the new one after.
    let rc_loop = async {
        let mut committed = false;
        loop {
            match rc_net.event_rx.try_recv() {
                Ok(NetworkEvent::MessageReceived { msg, .. }) => match msg {
                    MeerkatMessage::LookupRequest {
                        request_id,
                        reply_to,
                        ..
                    } => {
                        let val = if committed { 11 } else { 1 };
                        let _ = rc_net
                            .handle_command(NetworkCommand::SendMessage {
                                addr: Address::new(&reply_to),
                                msg: MeerkatMessage::LookupResponse {
                                    request_id,
                                    value: NetValue::Int { val },
                                },
                            })
                            .await;
                    }
                    MeerkatMessage::Commit {
                        request_id,
                        reply_to,
                        ..
                    } => {
                        committed = true;
                        let _ = rc_net
                            .handle_command(NetworkCommand::SendMessage {
                                addr: Address::new(&reply_to),
                                msg: MeerkatMessage::CommitResponse {
                                    request_id,
                                    success: true,
                                    error: None,
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

    tokio::select! {
        biased;
        r = m.commit_participant(&tid) => { r.expect("commit must succeed"); }
        _ = rc_loop => unreachable!("the stand-in for rc runs until the commit is done"),
    }

    assert_eq!(
        m.services[&mid].vars[&mv].value,
        Value::Int { val: 1 },
        "the participant's own write must be committed"
    );
    assert_eq!(
        m.services[&mid].vars[&mid_view].value,
        Value::Int { val: 12 },
        "mid_view must be recomputed from rc's committed value (1 + 11), not from \
         the value rc still had while it was holding the write buffered"
    );
}
