//! Issue #20, option B: `do s.a` on a remote service `s` ships the single
//! statement `do a` to the owner, instead of first fetching the action's body
//! with a `LookupRequest`.
//!
//! The node under test runs `app`, which calls `rc.act` on a stand-in peer.
//! The stand-in records every message it receives, so the test can see which
//! messages were sent and in what order.

use meerkat_lib::net::ast::NetValue;
use meerkat_lib::net::codec::encode_value;
use meerkat_lib::net::{
    Address, MeerkatMessage, NetworkActor, NetworkCommand, NetworkEvent, NetworkReply, NodeType,
    ServiceNetId,
};
use meerkat_lib::runtime::ast::{ActionStmt, Expr, Value};
use meerkat_lib::runtime::parser::parse_string;
use meerkat_lib::runtime::{Interner, Node};
use std::cell::RefCell;
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

async fn reply(net: &mut NetworkActor, reply_to: &str, msg: MeerkatMessage) {
    let _ = net
        .handle_command(NetworkCommand::SendMessage {
            addr: Address::new(reply_to),
            msg,
        })
        .await;
}

/// Stand in for `rc`: answer every message, and record each one in `log`.
/// Runs forever.
async fn stand_in_rc(net: &mut NetworkActor, rc_addr: &Address, log: &RefCell<Vec<String>>) {
    loop {
        match net.event_rx.try_recv() {
            Ok(NetworkEvent::MessageReceived { msg, .. }) => match msg {
                MeerkatMessage::LookupRequest {
                    request_id,
                    reply_to,
                    member,
                    ..
                } => {
                    log.borrow_mut().push(format!("LookupRequest {member}"));
                    // The body of `rc.act`: an empty action closure owned by `rc`
                    let closure = Value::ActionClosure {
                        stmts: vec![],
                        env: vec![],
                        service_net_id: ServiceNetId::new(format!("{}/rc", rc_addr.0)),
                    };
                    let value: NetValue =
                        encode_value(&closure, &Interner::new()).expect("closure encodes");
                    let msg = MeerkatMessage::LookupResponse {
                        request_id,
                        value,
                        clock: Some(Vec::new()),
                        sources: Some(Vec::new()),
                        called: false,
                    };
                    reply(net, &reply_to, msg).await;
                }
                MeerkatMessage::ActionRequest {
                    request_id,
                    reply_to,
                    service,
                    stmts,
                    env,
                    ..
                } => {
                    log.borrow_mut().push(format!(
                        "ActionRequest service={service} stmts={stmts:?} env_len={}",
                        env.len()
                    ));
                    let msg = MeerkatMessage::ActionResponse {
                        request_id,
                        success: true,
                        error: None,
                        history: Vec::new(),
                        writes: Vec::new(),
                    };
                    reply(net, &reply_to, msg).await;
                }
                MeerkatMessage::Commit {
                    request_id,
                    reply_to,
                    ..
                } => {
                    log.borrow_mut().push("Commit".to_string());
                    let msg = MeerkatMessage::CommitResponse {
                        request_id,
                        success: true,
                        error: None,
                    };
                    reply(net, &reply_to, msg).await;
                }
                MeerkatMessage::Abort {
                    request_id,
                    reply_to,
                    ..
                } => {
                    log.borrow_mut().push("Abort".to_string());
                    let msg = MeerkatMessage::AbortResponse { request_id };
                    reply(net, &reply_to, msg).await;
                }
                _ => {}
            },
            Ok(_) => {}
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(1)).await,
        }
    }
}

/// The node under test runs `app`, with `rc` redirected to the stand-in peer
/// at `rc_addr`.
async fn app_node(net: NetworkActor, rc_addr: &Address) -> meerkat_lib::runtime::Manager {
    let code = "
        service rc {
            var cv = 1;
            pub def act = action { cv = cv + 1; };
            pub def fwd = act;
        }
        service app {
            pub def direct = action { do rc.act; };
            pub def forwarded = action { do rc.fwd; };
            pub def indirect = action { let f = rc.act; do f; };
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

/// `do s.a` on a remote service ships `do a` and sends no lookup.
#[tokio::test(flavor = "multi_thread")]
async fn do_on_a_remote_member_sends_no_lookup() {
    let (app_net, _app_addr) = listening_node().await;
    let (mut rc_net, rc_addr) = listening_node().await;
    let mut m = app_node(app_net, &rc_addr).await;

    let app = m.interner.insert("app");
    let direct = m.interner.insert("direct");
    let stmts = [ActionStmt::Do(Expr::Variable { name: direct })];
    let log = RefCell::new(Vec::new());
    let result = tokio::select! {
        biased;
        r = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            m.execute_action(app, &stmts),
        ) => r.expect("the action must not time out"),
        _ = stand_in_rc(&mut rc_net, &rc_addr, &log) => {
            unreachable!("the stand-in for rc runs forever")
        }
    };

    assert!(result.is_ok(), "the action must succeed, got: {result:?}");
    let log = log.borrow();
    assert!(
        !log.iter().any(|e| e.starts_with("LookupRequest")),
        "a `do s.a` on a remote service must not send a lookup, log: {log:?}"
    );
    let action_requests: Vec<&String> = log
        .iter()
        .filter(|e| e.starts_with("ActionRequest service=rc"))
        .collect();
    assert_eq!(
        action_requests.len(),
        1,
        "exactly one action must be shipped to rc, log: {log:?}"
    );
    let shipped = action_requests[0];
    assert!(
        shipped.contains("Do(") && shipped.contains("\"act\""),
        "the shipped statement must be `do act`, got: {shipped}"
    );
    assert!(
        shipped.ends_with("env_len=0"),
        "the shipped action must carry no environment, got: {shipped}"
    );
    assert_eq!(
        log.last().map(String::as_str),
        Some("Commit"),
        "the transaction must end by committing, log: {log:?}"
    );
}

/// A `do` of a bound remote action (`let f = s.a; do f;`) still fetches the
/// body with a lookup first.
#[tokio::test(flavor = "multi_thread")]
async fn do_on_a_bound_remote_member_keeps_the_old_path() {
    let (app_net, _app_addr) = listening_node().await;
    let (mut rc_net, rc_addr) = listening_node().await;
    let mut m = app_node(app_net, &rc_addr).await;

    let app = m.interner.insert("app");
    let indirect = m.interner.insert("indirect");
    let stmts = [ActionStmt::Do(Expr::Variable { name: indirect })];
    let log = RefCell::new(Vec::new());
    let _result = tokio::select! {
        biased;
        r = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            m.execute_action(app, &stmts),
        ) => r.expect("the action must not time out"),
        _ = stand_in_rc(&mut rc_net, &rc_addr, &log) => {
            unreachable!("the stand-in for rc runs forever")
        }
    };

    let log = log.borrow();
    assert!(
        log.first().is_some_and(|e| e.starts_with("LookupRequest")),
        "the bound form must still fetch the body with a lookup first, log: {log:?}"
    );
}

/// A member of `s` whose initializer is not an action written in place (here
/// `pub def fwd = act;`, standing for a forward to another service's action)
/// may hold an action that does not belong to `s`, so `do s.fwd` keeps the old
/// path: the first message is a lookup, not a shipped `do fwd` (#213).
#[tokio::test(flavor = "multi_thread")]
async fn do_on_a_forwarding_member_keeps_the_old_path() {
    let (app_net, _app_addr) = listening_node().await;
    let (mut rc_net, rc_addr) = listening_node().await;
    let mut m = app_node(app_net, &rc_addr).await;

    let app = m.interner.insert("app");
    let forwarded = m.interner.insert("forwarded");
    let stmts = [ActionStmt::Do(Expr::Variable { name: forwarded })];
    let log = RefCell::new(Vec::new());
    let result = tokio::select! {
        biased;
        r = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            m.execute_action(app, &stmts),
        ) => r.expect("the action must not time out"),
        _ = stand_in_rc(&mut rc_net, &rc_addr, &log) => {
            unreachable!("the stand-in for rc runs forever")
        }
    };

    assert!(result.is_ok(), "the old path must finish, got: {result:?}");
    let log = log.borrow();
    assert_eq!(
        log.first().map(String::as_str),
        Some("LookupRequest fwd"),
        "a member that is not an action literal must be looked up first, log: {log:?}"
    );
    assert!(
        !log.iter().any(|e| e.contains("\"fwd\"")),
        "`do fwd` must not be shipped to rc, log: {log:?}"
    );
    assert_eq!(
        log.last().map(String::as_str),
        Some("Commit"),
        "the old path must end by committing, log: {log:?}"
    );
}
