//! A node must commit the nodes it composed onto before recomputing anything
//! derived from them.
//!
//! Both commit paths that can have participants follow the same order: store
//! the node's own writes, commit every participant, and only then propagate.
//! Propagating first recomputes derived members while the node below is still
//! holding its write buffered, storing a value that was never true.
//!
//! The setup is `mid` in `client -> mid -> rc` (or `mid -> rc` when `mid`
//! originates): `mid` wrote `mv`, it has `rc` as a participant, and
//! `def mid_view = mv + rc.gc` derives from both. The tests stand in for `rc`
//! with a bare network peer that only reports the new value of `gc` once it has
//! been told to commit, which is exactly what a real participant does. That
//! makes the ordering directly observable instead of timing-dependent.
//!
//! `mid` reads `rc.gc` from the `Update`s it subscribed to. The commit's clock
//! shows the one it holds is behind, so it subscribes again; the stand-in
//! pushes nothing on its own, so `mid` sees the new value only through that.

use meerkat_lib::net::ast::NetValue;
use meerkat_lib::net::{
    Address, ClockEntry, MeerkatMessage, NetworkActor, NetworkCommand, NetworkEvent, NetworkReply,
    NodeType, ServiceNetId,
};
use meerkat_lib::runtime::ast::{ActionStmt, Expr, Value};
use meerkat_lib::runtime::manager::InputState;
use meerkat_lib::runtime::parser::parse_string;
use meerkat_lib::runtime::txn::{Transaction, TxnId};
use meerkat_lib::runtime::{Interner, Manager, Node};
use std::collections::{HashMap, HashSet};

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

/// The middle node, with a real network layer so it can talk to the node
/// below, and with `rc` redirected to the peer at `rc_addr` so `rc.gc` comes
/// over the wire and its timing relative to `Commit` is observable. `mid`
/// subscribes to `rc.gc` and holds its first `Update` before the test goes on.
async fn middle_node(net: NetworkActor, rc_net: &mut NetworkActor, rc_addr: &Address) -> Manager {
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
    let mut m = node
        .on_manager_startup(true, Some(net), HashMap::new(), &local_ast)
        .await
        .expect("service init must succeed");

    let rc = m.interner.insert("rc");
    m.remote_services
        .insert(rc, Address::new(format!("{}/rc", rc_addr.0)));

    let mid = m.interner.insert("mid");
    let mid_view = m.interner.insert("mid_view");
    let gc = m.interner.insert("gc");
    let cv = m.interner.insert("cv");
    let mid_id = m.services[&mid].id.clone();
    m.subscribe_remote(rc, gc, mid_id, mid_view).await;
    tokio::select! {
        biased;
        _ = await_gc_update(&mut m, 1) => {}
        _ = serve_rc(rc_net) => unreachable!("the stand-in for rc runs forever"),
    }
    assert_eq!(
        m.services[&mid].vars[&mid_view].value,
        Value::Int { val: 1 },
        "before the transaction, mid_view is 0 + 1"
    );
    assert_eq!(
        m.services[&mid].dep_cache[&mid_view][&(rc, gc)]
            .retained
            .clone()
            .unwrap(),
        (
            Value::Int { val: 1 },
            Some(HashMap::new()),
            Some(HashSet::from([(rc, cv)]))
        ),
        "the initial subscription carries gc's initial value and zero clock"
    );
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

/// Stand in for `rc`: accept a composed action, and serve reads of and
/// subscriptions to `gc` as a real participant would -- the old committed value
/// (1) until `Commit` arrives, the new one (11) after, stamped with the clock
/// the `Commit` carries. The action it accepts writes the source var `cv`,
/// whose clock was zero. Runs forever.
async fn serve_rc(rc_net: &mut NetworkActor) {
    let mut committed: Option<Vec<ClockEntry>> = None;
    // EXPERIMENT v6: the subscriber a real participant would propagate to; a
    // participant's listener table outlives each stand-in invocation
    // keyed by the originator's address (tests run in parallel, one originator each)
    static SUBSCRIBERS: std::sync::Mutex<Vec<(String, String, String)>> =
        std::sync::Mutex::new(Vec::new());
    loop {
        let value = NetValue::Int {
            val: if committed.is_some() { 11 } else { 1 },
        };
        let clock = committed.clone().unwrap_or_default();
        match rc_net.event_rx.try_recv() {
            Ok(NetworkEvent::MessageReceived { msg, .. }) => match msg {
                MeerkatMessage::LookupRequest {
                    request_id,
                    reply_to,
                    ..
                } => {
                    let msg = MeerkatMessage::LookupResponse {
                        request_id,
                        value,
                        clock: Some(clock),
                        sources: Some(vec![("rc".into(), "cv".into())]),
                    };
                    reply(rc_net, &reply_to, msg).await;
                }
                MeerkatMessage::RequestUpdates {
                    listener_service,
                    listener_def,
                    reply_to,
                    ..
                } => {
                    SUBSCRIBERS.lock().unwrap().push((
                        listener_service.clone(),
                        listener_def.clone(),
                        reply_to.clone(),
                    ));
                    let msg = MeerkatMessage::Update {
                        listener_service,
                        listener_def,
                        source_service: "rc".to_string(),
                        member: "gc".to_string(),
                        value,
                        clock,
                        sources: Some(vec![("rc".into(), "cv".into())]),
                    };
                    reply(rc_net, &reply_to, msg).await;
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
                        history: Vec::new(),
                        writes: vec![("rc".into(), "cv".into())],
                    };
                    reply(rc_net, &reply_to, msg).await;
                }
                MeerkatMessage::Commit {
                    request_id,
                    reply_to,
                    clock,
                    ..
                } => {
                    committed = Some(clock);
                    let msg = MeerkatMessage::CommitResponse {
                        request_id,
                        success: true,
                        error: None,
                    };
                    reply(rc_net, &reply_to, msg).await;
                    // EXPERIMENT v6: a real participant propagates the committed
                    // write to the defs subscribed to it (commit_participant ->
                    // propagate -> emit_update); the stand-in does the same
                    let subscriber = SUBSCRIBERS
                        .lock()
                        .unwrap()
                        .iter()
                        .rev()
                        .find(|(_, _, to)| *to == reply_to)
                        .cloned();
                    if let Some((listener_service, listener_def, to)) = subscriber {
                        let msg = MeerkatMessage::Update {
                            listener_service,
                            listener_def,
                            source_service: "rc".to_string(),
                            member: "gc".to_string(),
                            value: NetValue::Int { val: 11 },
                            clock: committed.clone().unwrap_or_default(),
                            sources: Some(vec![("rc".into(), "cv".into())]),
                        };
                        reply(rc_net, &to, msg).await;
                    }
                }
                _ => {}
            },
            Ok(_) => {}
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(1)).await,
        }
    }
}

/// Handle incoming messages until `mid` holds an `Update` of `rc.gc` with
/// value `val`. The one it held before stays until then
async fn await_gc_update(m: &mut Manager, val: i32) {
    let [mid, mid_view, rc, gc] = ["mid", "mid_view", "rc", "gc"].map(|n| m.interner.insert(n));
    let cached = |m: &Manager| {
        let deps = m.services[&mid].dep_cache.get(&mid_view);
        deps.and_then(|deps| deps.get(&(rc, gc))?.newest())
            .is_some_and(|entry| entry.0 == Value::Int { val } && entry.1.is_some())
    };
    let pump = async {
        while !cached(m) {
            m.dispatch_network_events().await;
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), pump)
        .await
        .expect("rc must answer mid's subscription");
}

fn assert_committed_and_recomputed(m: &mut Manager) {
    let mid = m.interner.insert("mid");
    let mv = m.interner.insert("mv");
    let mid_view = m.interner.insert("mid_view");
    let rc = m.interner.insert("rc");
    let cv = m.interner.insert("cv");
    let gc = m.interner.insert("gc");
    assert_eq!(
        m.services[&mid].vars[&mv].value,
        Value::Int { val: 1 },
        "mid's own write must be committed"
    );
    let commit_clock = HashMap::from([((mid, mv), 1), ((rc, cv), 1)]);
    assert_eq!(
        m.services[&mid].vars[&mv].vector_clock, commit_clock,
        "mid's own write is stamped with the transaction's one clock, which \
         advances the source clock of both writes (section 5.2)"
    );
    assert_eq!(
        m.services[&mid].dep_cache[&mid_view][&(rc, gc)]
            .retained
            .clone()
            .unwrap(),
        (
            Value::Int { val: 11 },
            Some(commit_clock),
            Some(HashSet::from([(rc, cv)]))
        ),
        "the refreshed cache preserves rc.gc's committed value and the clock \
         rc's write was stamped with"
    );
    assert_eq!(
        m.services[&mid].vars[&mid_view].value,
        Value::Int { val: 12 },
        "mid_view must be recomputed from rc's committed value (1 + 11), not from \
         the value rc still had while it was holding the write buffered"
    );
    assert_eq!(
        m.services[&mid].vars[&mid_view].vector_clock,
        HashMap::from([((mid, mv), 1), ((rc, cv), 1)]),
        "mid_view merges the local write clock and the remote source clock"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_participant_commits_sub_participants_before_propagating() {
    let (mid_net, _mid_addr) = listening_node().await;
    let (mut rc_net, rc_addr) = listening_node().await;
    let mut m = middle_node(mid_net, &mut rc_net, &rc_addr).await;

    // A transaction this node is holding as a participant: it wrote `mv`, and
    // it composed an action onto `rc`, which is therefore holding a write of
    // its own until we tell it to commit.
    commit_as_participant(&mut m, &mut rc_net, &rc_addr).await;
    assert_committed_and_recomputed(&mut m);
}

/// Commit, as a participant, a transaction that wrote `mid.mv` and has `rc` as
/// a sub-participant
async fn commit_as_participant(m: &mut Manager, rc_net: &mut NetworkActor, rc_addr: &Address) {
    let mid = m.interner.insert("mid");
    let mv = m.interner.insert("mv");
    let rc = m.interner.insert("rc");
    let cv = m.interner.insert("cv");
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
    // What the originator commits with: both writes, neither with any history
    let clock = HashMap::from([((mid, mv), 1), ((rc, cv), 1)]);

    let committed = tokio::select! {
        biased;
        c = async {
            let c = m.commit_participant(&tid, &clock).await;
            await_gc_update(m, 11).await;
            c
        } => c,
        _ = serve_rc(rc_net) => unreachable!("the stand-in for rc runs forever"),
    };
    assert!(
        committed.is_ok(),
        "forwarding the commit to rc must succeed: {:?}",
        committed
    );
}

/// In steady state `mid_view`'s value of `rc.gc` comes from `dep_cache`,
/// filled by the `Update`s `rc` pushes, not from a lookup. `rc`'s `Update` for
/// this commit can arrive after its `CommitResponse`, so the cache can still
/// hold the old value when `mid` propagates. The stand-in never pushes an
/// `Update`, only answers subscriptions, which pins exactly that ordering.
#[tokio::test(flavor = "multi_thread")]
async fn test_propagation_does_not_use_cached_participant_values() {
    let (mid_net, _mid_addr) = listening_node().await;
    let (mut rc_net, rc_addr) = listening_node().await;
    let mut m = middle_node(mid_net, &mut rc_net, &rc_addr).await;

    let mid = m.interner.insert("mid");
    let mid_view = m.interner.insert("mid_view");
    let rc = m.interner.insert("rc");
    let gc = m.interner.insert("gc");
    let cv = m.interner.insert("cv");
    m.services
        .get_mut(&mid)
        .unwrap()
        .dep_cache
        .entry(mid_view)
        .or_default()
        .insert(
            (rc, gc),
            InputState::seeded((
                Value::Int { val: 1 },
                Some(HashMap::new()),
                Some(HashSet::from([(rc, cv)])),
            )),
        );

    commit_as_participant(&mut m, &mut rc_net, &rc_addr).await;
    assert_committed_and_recomputed(&mut m);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_originator_commits_participants_before_propagating() {
    let (mid_net, _mid_addr) = listening_node().await;
    let (mut rc_net, rc_addr) = listening_node().await;
    let mut m = middle_node(mid_net, &mut rc_net, &rc_addr).await;

    // `mid` originates `mv = 1; do <action on rc>`, so `rc` joins as a
    // participant holding its write until the originator commits.
    let mid = m.interner.insert("mid");
    let mv = m.interner.insert("mv");
    let cv = m.interner.insert("cv");
    let on_rc = Value::ActionClosure {
        stmts: vec![ActionStmt::Assign {
            name: cv,
            expr: Expr::Literal {
                val: Value::Int { val: 11 },
            },
        }],
        env: vec![],
        service_net_id: ServiceNetId(format!("{}/rc", rc_addr.0)),
    };
    let stmts = vec![
        ActionStmt::Assign {
            name: mv,
            expr: Expr::Literal {
                val: Value::Int { val: 1 },
            },
        },
        ActionStmt::Do(Expr::Literal { val: on_rc }),
    ];

    let result = tokio::select! {
        biased;
        r = async {
            let r = m.execute_action(mid, &stmts).await;
            await_gc_update(&mut m, 11).await;
            r
        } => r,
        _ = serve_rc(&mut rc_net) => unreachable!("the stand-in for rc runs forever"),
    };
    assert!(result.is_ok(), "the action must commit: {:?}", result);
    assert_committed_and_recomputed(&mut m);
}
