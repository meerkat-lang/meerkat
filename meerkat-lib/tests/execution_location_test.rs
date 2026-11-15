//! Issue #20, stages B and C: a call of a member of a remote service goes to its
//! owner as a `LookupRequest` that carries the arguments when the caller's copy
//! declares a special form or it has no copy, and the owner
//! decides whether to run it (design note `docs/design/execution-location.md`,
//! "Where a call runs" and "Protocol").
//!
//! The node under test (the caller) runs `app`, which calls `s1.f` on a
//! stand-in peer that records every message and answers as the test chooses.
//! Stage C adds `do s1.a(args)` of a special action, which is one
//! `ActionRequest`. Some tests use a real owner.

use meerkat_lib::net::ast::NetValue;
use meerkat_lib::net::codec::{encode_value, validate_call_args};
use meerkat_lib::net::ServiceNetId;
use meerkat_lib::net::{
    Address, MeerkatMessage, NetworkActor, NetworkCommand, NetworkEvent, NetworkReply, NodeType,
};
use meerkat_lib::runtime::ast::{ActionStmt, BinOp, Expr, Stmt, Value};
use meerkat_lib::runtime::limits::MAX_NET_CALL_ARGS;
use meerkat_lib::runtime::parser::parse_string;
use meerkat_lib::runtime::tt::{Param, Type};
use meerkat_lib::runtime::{Interner, Manager, Node};
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

/// What the stand-in received
#[derive(Debug, Clone, PartialEq)]
enum Seen {
    Lookup {
        member: String,
        args: Option<Vec<i32>>,
        in_txn: bool,
    },
    /// An `ActionRequest`: its statements as the wire shows them (`Debug`) and
    /// the integer values of its environment
    Action {
        stmts: String,
        env: Vec<(String, i32)>,
    },
    Commit,
    Abort,
}

type Log = RefCell<Vec<Seen>>;

/// What the stand-in answers to a lookup: the value and whether it is a call's
/// result
type Answer = (NetValue, bool);

fn int(val: i32) -> NetValue {
    NetValue::Int { val }
}

/// The integer arguments of a request
fn ints(args: Option<Vec<NetValue>>) -> Option<Vec<i32>> {
    args.map(|args| {
        args.into_iter()
            .map(|a| match a {
                NetValue::Int { val } => val,
                other => panic!("expected an int argument, got {other:?}"),
            })
            .collect()
    })
}

/// `fn (x: int) => x + y` of service `s1`, as an owner built before this
/// change (or one where `f` is a plain def) answers a lookup of `f`
fn f_closure() -> NetValue {
    let mut i = Interner::new();
    let [x, y, s1] = ["x", "y", "s1"].map(|n| i.insert(n));
    let closure = Value::Closure {
        params: vec![Param {
            name: x,
            ty: Some(Type::Int),
        }],
        body: Box::new(Expr::Binop {
            op: BinOp::Add,
            expr1: Box::new(Expr::Variable { name: x }),
            expr2: Box::new(Expr::Variable { name: y }),
        }),
        env: vec![],
        service_name: s1,
        return_ty: None,
    };
    encode_value(&closure, &i).expect("closure encodes")
}

/// Stand in for `s1`: answer every lookup with `answer(member, args)` and
/// every commit or abort, and record each message in `log`. Runs forever.
async fn stand_in_s1(
    net: &mut NetworkActor,
    log: &Log,
    answer: &dyn Fn(&str, &Option<Vec<NetValue>>) -> Answer,
) {
    loop {
        match net.event_rx.try_recv() {
            Ok(NetworkEvent::MessageReceived { msg, .. }) => match msg {
                MeerkatMessage::LookupRequest {
                    request_id,
                    reply_to,
                    member,
                    args,
                    txn_id,
                    ..
                } => {
                    let (value, called) = answer(&member, &args);
                    log.borrow_mut().push(Seen::Lookup {
                        member,
                        args: ints(args),
                        in_txn: txn_id.is_some(),
                    });
                    let msg = MeerkatMessage::LookupResponse {
                        request_id,
                        value,
                        clock: Some(Vec::new()),
                        sources: Some(Vec::new()),
                        called,
                    };
                    reply(net, &reply_to, msg).await;
                }
                // A subscription is answered with the member's current value, as
                // an owner does, and is not a request of the kind under test
                MeerkatMessage::RequestUpdates {
                    member,
                    listener_service,
                    listener_def,
                    reply_to,
                    ..
                } => {
                    let msg = MeerkatMessage::Update {
                        listener_service,
                        listener_def,
                        source_service: "s1".to_string(),
                        value: answer(&member, &None).0,
                        member,
                        clock: Vec::new(),
                        sources: Some(vec![("s1".into(), "y".into())]),
                    };
                    reply(net, &reply_to, msg).await;
                }
                MeerkatMessage::ActionRequest {
                    request_id,
                    reply_to,
                    stmts,
                    env,
                    ..
                } => {
                    log.borrow_mut().push(Seen::Action {
                        stmts: format!("{stmts:?}"),
                        env: env
                            .into_iter()
                            .map(|(name, v)| (name, ints(Some(vec![v])).unwrap()[0]))
                            .collect(),
                    });
                    let msg = MeerkatMessage::ActionResponse {
                        request_id,
                        success: true,
                        error: None,
                        history: Vec::new(),
                        writes: Vec::new(),
                        declined: false,
                    };
                    reply(net, &reply_to, msg).await;
                }
                MeerkatMessage::Commit {
                    request_id,
                    reply_to,
                    ..
                } => {
                    log.borrow_mut().push(Seen::Commit);
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
                    log.borrow_mut().push(Seen::Abort);
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

/// Build the node under test from `code`, with `s1` redirected to the peer at
/// `s1_addr`; also returns the program's `@test` blocks
async fn caller_node(
    code: &str,
    net: NetworkActor,
    s1_addr: &Address,
) -> (Manager, Vec<Vec<ActionStmt>>) {
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
    let s1 = m.interner.insert("s1");
    m.remote_services
        .insert(s1, Address::new(format!("{}/s1", s1_addr.0)));
    let tests = ast
        .iter()
        .filter_map(|s| match s {
            Stmt::Test { stmts, .. } => Some(stmts.clone()),
            _ => None,
        })
        .collect();
    (m, tests)
}

/// The caller's program: a copy of `s1`, as the CLI holds after an import
const CALLER: &str = "
    service s1 {
        var y = 5;
        pub def f(x: int) = x + y;
    }
    service lib {
        pub def helper = fn (n: int) => s1.f(n);
    }
    service app {
        var v = 0;
        var a = 1;
        pub def d = a + s1.f(3);
        pub def run = action { let r = s1.f(3); v = r; };
    }
    @test(app) {
        assert(s1.f(3) == 8);
    }
";

/// Run `stmts` on `app` of `m` while the stand-in serves, and return the result
async fn run_on_app(
    m: &mut Manager,
    s1_net: &mut NetworkActor,
    log: &Log,
    answer: &dyn Fn(&str, &Option<Vec<NetValue>>) -> Answer,
    stmts: &[ActionStmt],
    as_test: bool,
) -> Result<(), meerkat_lib::runtime::interpreter::EvalError> {
    run_on(m, "app", s1_net, log, answer, stmts, as_test).await
}

/// Run `stmts` on the service `service` of `m` while the stand-in serves
async fn run_on(
    m: &mut Manager,
    service: &str,
    s1_net: &mut NetworkActor,
    log: &Log,
    answer: &dyn Fn(&str, &Option<Vec<NetValue>>) -> Answer,
    stmts: &[ActionStmt],
    as_test: bool,
) -> Result<(), meerkat_lib::runtime::interpreter::EvalError> {
    let svc = m.interner.insert(service);
    tokio::select! {
        biased;
        r = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            if as_test {
                m.execute_test_block(svc, stmts).await
            } else {
                m.execute_action(svc, stmts).await
            }
        }) => r.expect("the run must not time out"),
        _ = stand_in_s1(s1_net, log, answer) => {
            unreachable!("the stand-in for s1 runs forever")
        }
    }
}

/// `do run;` on `app`
fn do_run(m: &mut Manager) -> Vec<ActionStmt> {
    let run = m.interner.insert("run");
    vec![ActionStmt::Do(Expr::Variable { name: run })]
}

fn var_of(m: &mut Manager, service: &str, name: &str) -> Value {
    let (svc, name) = (m.interner.insert(service), m.interner.insert(name));
    m.services[&svc].vars[&name].value.clone()
}

/// `let r = s1.f(3); v = r;` in an action sends exactly one request for `f`,
/// with the argument and in the transaction, and no lookup of anything else;
/// the owner's `called: true` reply is the result, and the write commits.
///
/// Without the change `LookupRequest` has no `args`: this does not compile
/// (and the old path would also send lookups of `f` and `y`).
#[tokio::test(flavor = "multi_thread")]
async fn a_call_in_an_action_is_one_request_with_the_arguments() {
    let (app_net, _) = listening_node().await;
    let (mut s1_net, s1_addr) = listening_node().await;
    let (mut m, _) = caller_node(CALLER, app_net, &s1_addr).await;
    let stmts = do_run(&mut m);
    let log = Log::default();
    let answer = |_: &str, _: &Option<Vec<NetValue>>| (int(8), true);

    let result = run_on_app(&mut m, &mut s1_net, &log, &answer, &stmts, false).await;

    assert!(result.is_ok(), "the action must succeed, got: {result:?}");
    assert_eq!(var_of(&mut m, "app", "v"), Value::Int { val: 8 });
    let log = log.borrow();
    assert_eq!(
        log.first(),
        Some(&Seen::Lookup {
            member: "f".to_string(),
            args: Some(vec![3]),
            in_txn: true,
        }),
        "the call must be sent as one lookup with its arguments, log: {log:?}"
    );
    assert_eq!(
        log.iter()
            .filter(|e| matches!(e, Seen::Lookup { .. }))
            .count(),
        1,
        "the call must send no other lookup, log: {log:?}"
    );
    assert_eq!(log.last(), Some(&Seen::Commit), "log: {log:?}");
}

/// An owner that answers with the member (`called: false`: one built before
/// this change, or one where `f` is plain) leaves the caller to apply it, so
/// the body's read of `s1.y` is a plain lookup with no arguments.
#[tokio::test(flavor = "multi_thread")]
async fn a_member_reply_is_applied_by_the_caller() {
    let (app_net, _) = listening_node().await;
    let (mut s1_net, s1_addr) = listening_node().await;
    let (mut m, _) = caller_node(CALLER, app_net, &s1_addr).await;
    let stmts = do_run(&mut m);
    let log = Log::default();
    let answer = |member: &str, _: &Option<Vec<NetValue>>| match member {
        "f" => (f_closure(), false),
        _ => (int(5), false),
    };

    let result = run_on_app(&mut m, &mut s1_net, &log, &answer, &stmts, false).await;

    assert!(result.is_ok(), "the action must succeed, got: {result:?}");
    assert_eq!(var_of(&mut m, "app", "v"), Value::Int { val: 8 });
    let lookups: Vec<_> = log
        .borrow()
        .iter()
        .filter_map(|e| match e {
            Seen::Lookup { member, args, .. } => Some((member.clone(), args.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        lookups,
        vec![("f".to_string(), Some(vec![3])), ("y".to_string(), None)],
        "the call, then the plain read of what the applied body reads"
    );
}

/// An argument with no wire form (html, typed `string`) to a special def is not
/// an error: the call falls back to a plain lookup and the caller applies the
/// member, as before calls ran on the owner.
#[tokio::test(flavor = "multi_thread")]
async fn an_argument_with_no_wire_form_falls_back_to_a_lookup() {
    const CODE: &str = "
        service s1 {
            pub def g(t: string) = 7;
        }
        service app {
            var v = 0;
            pub def run = action { let r = s1.g((<p>x</p>)); v = r; };
        }
    ";
    let (app_net, _) = listening_node().await;
    let (mut s1_net, s1_addr) = listening_node().await;
    let (mut m, _) = caller_node(CODE, app_net, &s1_addr).await;
    let stmts = do_run(&mut m);
    let log = Log::default();
    let g_closure = {
        let mut i = Interner::new();
        let [t, s1] = ["t", "s1"].map(|n| i.insert(n));
        encode_value(
            &Value::Closure {
                params: vec![Param {
                    name: t,
                    ty: Some(Type::String),
                }],
                body: Box::new(Expr::Literal {
                    val: Value::Int { val: 7 },
                }),
                env: vec![],
                service_name: s1,
                return_ty: None,
            },
            &i,
        )
        .expect("closure encodes")
    };
    let answer = move |_: &str, _: &Option<Vec<NetValue>>| (g_closure.clone(), false);

    let result = run_on_app(&mut m, &mut s1_net, &log, &answer, &stmts, false).await;

    assert!(result.is_ok(), "the action must succeed, got: {result:?}");
    assert_eq!(var_of(&mut m, "app", "v"), Value::Int { val: 7 });
    assert!(
        log.borrow().iter().any(|e| matches!(
            e,
            Seen::Lookup { member, args: None, .. } if member == "g"
        )),
        "the member must be fetched with a plain lookup, log: {:?}",
        log.borrow()
    );
}

/// A caller whose copy declares `f` plain (the `fn` spelling) sends a plain
/// lookup for `s1.f(3)`, with no arguments on the wire, and applies the
/// owner's answer itself.
///
/// Without the change the call is sent with `args: Some([3])`.
#[tokio::test(flavor = "multi_thread")]
async fn a_copy_that_declares_the_member_plain_sends_a_plain_lookup() {
    const CODE: &str = "
        service s1 {
            var y = 5;
            pub def f = fn (x: int) => x + y;
        }
        service app {
            var v = 0;
            pub def run = action { let r = s1.f(3); v = r; };
        }
    ";
    let (app_net, _) = listening_node().await;
    let (mut s1_net, s1_addr) = listening_node().await;
    let (mut m, _) = caller_node(CODE, app_net, &s1_addr).await;
    let stmts = do_run(&mut m);
    let log = Log::default();
    let answer = |member: &str, _: &Option<Vec<NetValue>>| match member {
        "f" => (f_closure(), false),
        _ => (int(5), false),
    };

    let result = run_on_app(&mut m, &mut s1_net, &log, &answer, &stmts, false).await;

    assert!(result.is_ok(), "the action must succeed, got: {result:?}");
    assert_eq!(var_of(&mut m, "app", "v"), Value::Int { val: 8 });
    assert_eq!(
        lookups_of(&log.borrow()),
        vec![("f".to_string(), None), ("y".to_string(), None)]
    );
}

/// A caller with no copy of `s1` (the REPL, the browser) cannot tell, so it
/// sends the call with its arguments and lets the owner decide.
///
/// Without the change to the gate this passes too: it pins the case the gate
/// must keep sending.
#[tokio::test(flavor = "multi_thread")]
async fn a_caller_with_no_copy_sends_the_call_with_its_arguments() {
    let (app_net, _) = listening_node().await;
    let (mut s1_net, s1_addr) = listening_node().await;
    let (mut m, _) = caller_node(CALLER, app_net, &s1_addr).await;
    let s1 = m.interner.insert("s1");
    let without_s1: Vec<Stmt> = m
        .unified_ast()
        .iter()
        .filter(|stmt| !matches!(stmt, Stmt::Service { name, .. } if *name == s1))
        .cloned()
        .collect();
    m.install_unified_ast(without_s1);
    let stmts = do_run(&mut m);
    let log = Log::default();
    let answer = |_: &str, _: &Option<Vec<NetValue>>| (int(8), true);

    let result = run_on_app(&mut m, &mut s1_net, &log, &answer, &stmts, false).await;

    assert!(result.is_ok(), "the action must succeed, got: {result:?}");
    assert_eq!(var_of(&mut m, "app", "v"), Value::Int { val: 8 });
    assert_eq!(
        lookups_of(&log.borrow()),
        vec![("f".to_string(), Some(vec![3]))]
    );
}

/// A `@test` statement is not in a transaction: the call goes out with no
/// `txn_id`.
#[tokio::test(flavor = "multi_thread")]
async fn a_call_in_a_test_statement_carries_no_transaction() {
    let (app_net, _) = listening_node().await;
    let (mut s1_net, s1_addr) = listening_node().await;
    let (mut m, tests) = caller_node(CALLER, app_net, &s1_addr).await;
    let log = Log::default();
    let answer = |_: &str, _: &Option<Vec<NetValue>>| (int(8), true);

    let result = run_on_app(&mut m, &mut s1_net, &log, &answer, &tests[0], true).await;

    assert!(result.is_ok(), "the test must pass, got: {result:?}");
    assert_eq!(
        *log.borrow(),
        vec![Seen::Lookup {
            member: "f".to_string(),
            args: Some(vec![3]),
            in_txn: false,
        }]
    );
}

/// Creating a service reads remote function code through its own reads, so
/// the call gate is closed for the whole creation transaction: an initializer
/// that calls `s1.f` directly, or through a function of another local service
/// (which switches the evaluation's service), sends only plain lookups.
///
/// Fails without the gate's `is_creation_txn` test: the calls then carry
/// arguments.
#[tokio::test(flavor = "multi_thread")]
async fn creating_a_service_sends_plain_lookups() {
    let (app_net, _) = listening_node().await;
    let (mut s1_net, s1_addr) = listening_node().await;
    let (mut m, _) = caller_node(CALLER, app_net, &s1_addr).await;
    let extra = parse_string(
        "service direct { var w = s1.f(3); }
         service indirect { var u = lib.helper(4); }",
        &mut m.interner,
    )
    .expect("valid syntax");
    let mut all = m.unified_ast().to_vec();
    all.extend(extra.clone());
    m.install_unified_ast(all);
    let log = Log::default();
    // What a call would answer if one were sent, to tell it from a plain read
    let answer = |member: &str, args: &Option<Vec<NetValue>>| match (member, args) {
        (_, Some(_)) => (int(-1), true),
        ("f", None) => (f_closure(), false),
        _ => (int(5), false),
    };

    let created = tokio::select! {
        biased;
        r = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            for stmt in &extra {
                m.run_declaration(stmt).await?;
            }
            Ok::<(), String>(())
        }) => r.expect("creation must not time out"),
        _ = stand_in_s1(&mut s1_net, &log, &answer) => {
            unreachable!("the stand-in for s1 runs forever")
        }
    };

    assert!(created.is_ok(), "creation must succeed, got: {created:?}");
    assert_eq!(var_of(&mut m, "direct", "w"), Value::Int { val: 8 });
    assert_eq!(var_of(&mut m, "indirect", "u"), Value::Int { val: 9 });
    let log = log.borrow();
    assert!(
        log.iter().any(|e| matches!(e, Seen::Lookup { .. })),
        "creation must read s1, log: {log:?}"
    );
    assert!(
        log.iter()
            .all(|e| !matches!(e, Seen::Lookup { args: Some(_), .. })),
        "creation must send no call, log: {log:?}"
    );
}

/// A def that calls `s1.f(3)` is recomputed from the values its owners
/// pushed when one of its inputs changes: no request is sent.
#[tokio::test(flavor = "multi_thread")]
async fn recomputing_a_def_sends_no_request() {
    let (app_net, _) = listening_node().await;
    let (mut s1_net, s1_addr) = listening_node().await;
    let (mut m, _) = caller_node(CALLER, app_net, &s1_addr).await;
    assert_eq!(var_of(&mut m, "app", "d"), Value::Int { val: 9 });
    let [app, a, d, s1, f, y] = ["app", "a", "d", "s1", "f", "y"].map(|n| m.interner.insert(n));
    let log = Log::default();
    let answer = |member: &str, _: &Option<Vec<NetValue>>| match member {
        "f" => (f_closure(), false),
        _ => (int(5), false),
    };

    // `d` listens to `s1.f` and to `s1.y`, which `f` reads: s1 pushes both
    let app_id = m.services[&app].id.clone();
    m.subscribe_remote(s1, f, app_id.clone(), d).await;
    m.subscribe_remote(s1, y, app_id, d).await;
    let pump = async {
        for _ in 0..100 {
            m.dispatch_network_events().await;
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    };
    tokio::select! {
        biased;
        _ = pump => {}
        _ = stand_in_s1(&mut s1_net, &log, &answer) => {
            unreachable!("the stand-in for s1 runs forever")
        }
    }

    let stmts = [ActionStmt::Assign {
        name: a,
        expr: Expr::Literal {
            val: Value::Int { val: 10 },
        },
    }];

    let result = tokio::select! {
        biased;
        r = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            m.execute_action(app, &stmts),
        ) => r.expect("the action must not time out"),
        _ = stand_in_s1(&mut s1_net, &log, &answer) => {
            unreachable!("the stand-in for s1 runs forever")
        }
    };

    assert!(result.is_ok(), "the action must succeed, got: {result:?}");
    assert_eq!(
        var_of(&mut m, "app", "d"),
        Value::Int { val: 18 },
        "d must follow a"
    );
    assert_eq!(*log.borrow(), vec![], "recomputing a def must send nothing");
}

/// A count of arguments over the limit is refused before any is decoded.
#[test]
fn too_many_arguments_are_refused() {
    assert!(validate_call_args(0).is_ok());
    assert!(validate_call_args(MAX_NET_CALL_ARGS).is_ok());
    assert!(validate_call_args(MAX_NET_CALL_ARGS + 1).is_err());
}

/// Against a real owner, `let r = s1.f(3); v = r;` gives 8 with exactly one
/// request before the commit.
#[tokio::test(flavor = "multi_thread")]
async fn a_call_against_a_real_owner_is_one_round_trip() {
    let (app_net, _) = listening_node().await;
    let (owner_net, owner_addr) = listening_node().await;
    let (mut m, _) = caller_node(CALLER, app_net, &owner_addr).await;

    // The owner: the same `s1` with its own state, on a node of its own
    let mut interner = Interner::new();
    let ast = parse_string(
        "service s1 { var y = 5; pub def f(x: int) = x + y; }",
        &mut interner,
    )
    .expect("valid syntax");
    let mut node = Node::new();
    node.interner = interner;
    node.unified_ast = ast.clone();
    node.static_checks().expect("static checks must pass");
    let mut owner = node
        .on_manager_startup(true, Some(owner_net), HashMap::new(), &ast)
        .await
        .expect("owner startup");

    // Give `y` a clock: what the call's body read must reach the caller's
    // commit stamp through the reply clock
    let [owner_s1, owner_y] = ["s1", "y"].map(|n| owner.interner.insert(n));
    owner
        .assign(owner_s1, owner_y, Value::Int { val: 5 }, None)
        .await
        .expect("owner write");

    let stmts = do_run(&mut m);
    let received = RefCell::new(Vec::new());
    let serve = async {
        loop {
            while let Some(NetworkEvent::MessageReceived { msg, .. }) = owner.try_recv_event() {
                received.borrow_mut().push(match &msg {
                    MeerkatMessage::LookupRequest { member, args, .. } => {
                        format!("Lookup {member} args={}", args.as_ref().map_or(0, Vec::len))
                    }
                    MeerkatMessage::Commit { .. } => "Commit".to_string(),
                    other => format!("{other:?}"),
                });
                owner.serve_request(msg).await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    };
    let app = m.interner.insert("app");
    let result = tokio::select! {
        biased;
        r = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            m.execute_action(app, &stmts),
        ) => r.expect("the action must not time out"),
        _ = serve => unreachable!("the owner serves forever"),
    };

    assert!(result.is_ok(), "the action must succeed, got: {result:?}");
    assert_eq!(var_of(&mut m, "app", "v"), Value::Int { val: 8 });
    let (app_sym, v_sym) = (m.interner.insert("app"), m.interner.insert("v"));
    assert!(
        !m.services[&app_sym].vars[&v_sym].vector_clock.is_empty(),
        "the clock of what the owner read must reach the caller's write"
    );
    assert_eq!(
        *received.borrow(),
        vec!["Lookup f args=1".to_string(), "Commit".to_string()],
        "one request, then the commit"
    );
}

// Stage C: `do s1.a(args)` of a special action is one `ActionRequest`.

/// The caller's copy of `s1` with special forms, and the statements under test
const SPECIAL: &str = "
    service s1 {
        var y = 5;
        pub def f(x: int) = x + y;
        pub action a(x: int) { y = y * x; }
        pub action reset() { y = 0; }
        pub def mk(x: int) = action { y = x; };
    }
    service app {
        pub def run_a = action { do s1.a(3); };
        pub def run_wiki = action { do s1.a(s1.f(3)); };
        pub def run_reset = action { do s1.reset(); };
        pub def run_mk = action { do s1.mk(3); };
    }
    @test(s1) {
        do a(3);
    }
";

/// The `fn` spelling of the same `s1`
const FN_SPELLING: &str = "
    service s1 {
        var y = 5;
        pub def f = fn (x: int) => x + y;
        pub def a = fn (x: int) => action { y = y * x; };
    }
    service app {
        pub def run_wiki = action { do s1.a(s1.f(3)); };
    }
";

/// `do NAME;` on `app`
fn do_member(m: &mut Manager, name: &str) -> Vec<ActionStmt> {
    let name = m.interner.insert(name);
    vec![ActionStmt::Do(Expr::Variable { name })]
}

fn lookups_of(log: &[Seen]) -> Vec<(String, Option<Vec<i32>>)> {
    log.iter()
        .filter_map(|e| match e {
            Seen::Lookup { member, args, .. } => Some((member.clone(), args.clone())),
            _ => None,
        })
        .collect()
}

fn action(stmts: &str, env: Vec<(&str, i32)>) -> Seen {
    Seen::Action {
        stmts: stmts.to_string(),
        env: env.into_iter().map(|(n, v)| (n.to_string(), v)).collect(),
    }
}

/// `do s1.a(__arg0)` as the wire shows it
const DO_A: &str = r#"[Do(Call { func: MemberAccess { service_name: "s1", member_name: "a" }, args: [Variable { name: "__arg0" }] })]"#;

/// `do s1.a(3);` with `action a(x: int)` in the caller's copy is exactly one
/// `ActionRequest` `[do s1.a(__arg0)]` with `__arg0 = 3`, no lookup, then the
/// commit.
///
/// Without the change the call goes through the evaluator gate: a
/// `LookupRequest a` comes first, so the log differs.
#[tokio::test(flavor = "multi_thread")]
async fn do_of_a_special_action_is_one_action_request() {
    let (app_net, _) = listening_node().await;
    let (mut s1_net, s1_addr) = listening_node().await;
    let (mut m, _) = caller_node(SPECIAL, app_net, &s1_addr).await;
    let stmts = do_member(&mut m, "run_a");
    let log = Log::default();
    let answer = |_: &str, _: &Option<Vec<NetValue>>| (int(-1), true);

    let result = run_on_app(&mut m, &mut s1_net, &log, &answer, &stmts, false).await;

    assert!(result.is_ok(), "the action must succeed, got: {result:?}");
    assert_eq!(
        *log.borrow(),
        vec![action(DO_A, vec![("__arg0", 3)]), Seen::Commit]
    );
}

/// The wiki's example against special forms: the call of `f` goes to the
/// owner and its result is the action's argument: two requests before the
/// commit.
///
/// Without the change a lookup of `a` follows the one of `f`, and the body
/// of `a` is sent instead of `do s1.a(__arg0)`.
#[tokio::test(flavor = "multi_thread")]
async fn the_wiki_example_takes_two_requests() {
    let (app_net, _) = listening_node().await;
    let (mut s1_net, s1_addr) = listening_node().await;
    let (mut m, _) = caller_node(SPECIAL, app_net, &s1_addr).await;
    let stmts = do_member(&mut m, "run_wiki");
    let log = Log::default();
    let answer = |member: &str, args: &Option<Vec<NetValue>>| match (member, args) {
        ("f", Some(_)) => (int(8), true),
        _ => (int(-1), true),
    };

    let result = run_on_app(&mut m, &mut s1_net, &log, &answer, &stmts, false).await;

    assert!(result.is_ok(), "the action must succeed, got: {result:?}");
    assert_eq!(
        *log.borrow(),
        vec![
            Seen::Lookup {
                member: "f".to_string(),
                args: Some(vec![3]),
                in_txn: true,
            },
            action(DO_A, vec![("__arg0", 8)]),
            Seen::Commit,
        ]
    );
}

/// `fn (x: int) => action { y = y * x; }` of `s1`, as a plain owner answers a
/// lookup of `a`
fn a_closure() -> NetValue {
    let mut i = Interner::new();
    let [x, y, s1] = ["x", "y", "s1"].map(|n| i.insert(n));
    let closure = Value::Closure {
        params: vec![Param {
            name: x,
            ty: Some(Type::Int),
        }],
        body: Box::new(Expr::Action(vec![ActionStmt::Assign {
            name: y,
            expr: Expr::Binop {
                op: BinOp::Mul,
                expr1: Box::new(Expr::Variable { name: y }),
                expr2: Box::new(Expr::Variable { name: x }),
            },
        }])),
        env: vec![],
        service_name: s1,
        return_ty: None,
    };
    encode_value(&closure, &i).expect("closure encodes")
}

/// The same statement against the `fn` spelling in the caller's copy: the
/// copy declares the members plain, so the caller sends plain lookups with no
/// arguments, the owner answers with the members and the caller applies them:
/// four requests before the commit, the last one carrying the action's body.
#[tokio::test(flavor = "multi_thread")]
async fn the_fn_spelling_takes_four_requests() {
    let (app_net, _) = listening_node().await;
    let (mut s1_net, s1_addr) = listening_node().await;
    let (mut m, _) = caller_node(FN_SPELLING, app_net, &s1_addr).await;
    // A caller that imported `s1` holds no running copy of it: the action
    // closure must resolve to the owner, not to a local service of that name
    let s1 = m.interner.insert("s1");
    m.services.remove(&s1);
    let stmts = do_member(&mut m, "run_wiki");
    let log = Log::default();
    let answer = |member: &str, _: &Option<Vec<NetValue>>| match member {
        "f" => (f_closure(), false),
        "a" => (a_closure(), false),
        _ => (int(5), false),
    };

    let result = run_on_app(&mut m, &mut s1_net, &log, &answer, &stmts, false).await;

    assert!(result.is_ok(), "the action must succeed, got: {result:?}");
    let log = log.borrow();
    let mut lookups = lookups_of(&log);
    assert_eq!(lookups.len(), 3, "log: {log:?}");
    lookups.sort();
    assert_eq!(
        lookups,
        vec![
            ("a".to_string(), None),
            ("f".to_string(), None),
            ("y".to_string(), None),
        ],
        "log: {log:?}"
    );
    match &log[log.len() - 2] {
        Seen::Action { stmts, env } => {
            assert!(
                stmts.contains(r#"Assign { name: "y""#),
                "the action's body must be sent, got: {stmts}"
            );
            assert!(env.iter().any(|(n, v)| n == "x" && *v == 8), "env: {env:?}");
        }
        other => panic!("expected the action request, got {other:?}, log: {log:?}"),
    }
    assert_eq!(log.len(), 5, "four requests and the commit, log: {log:?}");
    assert_eq!(log.last(), Some(&Seen::Commit));
}

/// A zero-parameter special action sends `do s1.reset()` with an empty
/// environment.
#[tokio::test(flavor = "multi_thread")]
async fn a_zero_parameter_action_sends_an_empty_environment() {
    let (app_net, _) = listening_node().await;
    let (mut s1_net, s1_addr) = listening_node().await;
    let (mut m, _) = caller_node(SPECIAL, app_net, &s1_addr).await;
    let stmts = do_member(&mut m, "run_reset");
    let log = Log::default();
    let answer = |_: &str, _: &Option<Vec<NetValue>>| (int(-1), true);

    let result = run_on_app(&mut m, &mut s1_net, &log, &answer, &stmts, false).await;

    assert!(result.is_ok(), "the action must succeed, got: {result:?}");
    assert_eq!(
        *log.borrow(),
        vec![
            action(
                r#"[Do(Call { func: MemberAccess { service_name: "s1", member_name: "reset" }, args: [] })]"#,
                vec![]
            ),
            Seen::Commit
        ]
    );
}

/// A special def that returns an action is not sent by name: the owner decides
/// which action it returns, so the call is a lookup with the arguments and the
/// returned action's body is then sent.
#[tokio::test(flavor = "multi_thread")]
async fn a_special_def_returning_an_action_goes_through_the_owner() {
    let (app_net, _) = listening_node().await;
    let (mut s1_net, s1_addr) = listening_node().await;
    let (mut m, _) = caller_node(SPECIAL, app_net, &s1_addr).await;
    let stmts = do_member(&mut m, "run_mk");
    let log = Log::default();
    let owner_id = ServiceNetId::new(format!("{}/s1", s1_addr.0));
    let answer = move |member: &str, args: &Option<Vec<NetValue>>| {
        assert_eq!((member, args.is_some()), ("mk", true));
        let mut i = Interner::new();
        let y = i.insert("y");
        let action = Value::ActionClosure {
            stmts: vec![ActionStmt::Assign {
                name: y,
                expr: Expr::Literal {
                    val: Value::Int { val: 3 },
                },
            }],
            env: vec![],
            service_net_id: owner_id.clone(),
        };
        (encode_value(&action, &i).expect("action encodes"), true)
    };

    let result = run_on_app(&mut m, &mut s1_net, &log, &answer, &stmts, false).await;

    assert!(result.is_ok(), "the action must succeed, got: {result:?}");
    let log = log.borrow();
    assert_eq!(log.len(), 3, "log: {log:?}");
    assert_eq!(
        log[0],
        Seen::Lookup {
            member: "mk".to_string(),
            args: Some(vec![3]),
            in_txn: true,
        }
    );
    assert!(
        matches!(&log[1], Seen::Action { stmts, env }
            if stmts.contains(r#"Assign { name: "y""#) && env.is_empty()),
        "the returned action's body must be sent, log: {log:?}"
    );
    assert_eq!(log[2], Seen::Commit);
}

/// In a `@test(s1)` block of the caller with `s1` remote, the bare name `a`
/// targets `s1`: `do a(3);` sends `do s1.a(__arg0)`.
#[tokio::test(flavor = "multi_thread")]
async fn the_bare_name_in_a_test_block_is_sent_by_name() {
    let (app_net, _) = listening_node().await;
    let (mut s1_net, s1_addr) = listening_node().await;
    let (mut m, tests) = caller_node(SPECIAL, app_net, &s1_addr).await;
    let log = Log::default();
    let answer = |_: &str, _: &Option<Vec<NetValue>>| (int(-1), true);

    let result = run_on(&mut m, "s1", &mut s1_net, &log, &answer, &tests[0], true).await;

    assert!(result.is_ok(), "the test must pass, got: {result:?}");
    assert_eq!(
        *log.borrow(),
        vec![action(DO_A, vec![("__arg0", 3)]), Seen::Commit]
    );
}

/// A special action named like an argument (`__arg0`) is reached by name: the
/// callee is sent as a member access, which the environment cannot shadow.
///
/// Without the change the callee is `Variable { name: "__arg0" }`, which on the
/// owner resolves to the argument value, not to the member.
#[tokio::test(flavor = "multi_thread")]
async fn a_special_action_named_like_an_argument_reaches_the_member() {
    const CODE: &str = "
        service s1 {
            var y = 5;
            pub action __arg0(x: int) { y = x; }
        }
        service app {
            pub def run = action { do s1.__arg0(3); };
        }
    ";
    let (app_net, _) = listening_node().await;
    let (mut s1_net, s1_addr) = listening_node().await;
    let (mut m, _) = caller_node(CODE, app_net, &s1_addr).await;
    let stmts = do_run(&mut m);
    let log = Log::default();
    let answer = |_: &str, _: &Option<Vec<NetValue>>| (int(-1), true);

    let result = run_on_app(&mut m, &mut s1_net, &log, &answer, &stmts, false).await;

    assert!(result.is_ok(), "the action must succeed, got: {result:?}");
    assert_eq!(
        *log.borrow(),
        vec![
            action(
                r#"[Do(Call { func: MemberAccess { service_name: "s1", member_name: "__arg0" }, args: [Variable { name: "__arg0" }] })]"#,
                vec![("__arg0", 3)]
            ),
            Seen::Commit
        ]
    );
}

/// Run `do run_wiki;` of the caller built from `SPECIAL` against a real owner
/// running `owner_code`, and return what the owner received and its `y`.
async fn wiki_against_owner(owner_code: &str) -> (Vec<String>, Value) {
    run_against_owner(SPECIAL, owner_code, "run_wiki").await
}

/// Run `do member;` of the caller built from `caller_code` against a real
/// owner running `owner_code`, and return what the owner received and its `y`.
async fn run_against_owner(
    caller_code: &str,
    owner_code: &str,
    member: &str,
) -> (Vec<String>, Value) {
    run_against_owner_with(caller_code, owner_code, member, false).await
}

/// `run_against_owner`, optionally without the caller's running copy of `s1`,
/// as for a caller that imported it: a fetched action must then resolve to the
/// owner, not to a local service of that name.
async fn run_against_owner_with(
    caller_code: &str,
    owner_code: &str,
    member: &str,
    remove_local_s1: bool,
) -> (Vec<String>, Value) {
    let (app_net, _) = listening_node().await;
    let (owner_net, owner_addr) = listening_node().await;
    let (mut m, _) = caller_node(caller_code, app_net, &owner_addr).await;
    if remove_local_s1 {
        let s1 = m.interner.insert("s1");
        m.services.remove(&s1);
    }

    let mut interner = Interner::new();
    let ast = parse_string(owner_code, &mut interner).expect("valid syntax");
    let mut node = Node::new();
    node.interner = interner;
    node.unified_ast = ast.clone();
    node.static_checks().expect("static checks must pass");
    let mut owner = node
        .on_manager_startup(true, Some(owner_net), HashMap::new(), &ast)
        .await
        .expect("owner startup");

    let stmts = do_member(&mut m, member);
    let received = RefCell::new(Vec::new());
    let serve = async {
        loop {
            while let Some(NetworkEvent::MessageReceived { msg, .. }) = owner.try_recv_event() {
                received.borrow_mut().push(match &msg {
                    MeerkatMessage::LookupRequest { member, args, .. } => {
                        format!("Lookup {member} args={}", args.as_ref().map_or(0, Vec::len))
                    }
                    MeerkatMessage::ActionRequest { .. } => "Action".to_string(),
                    MeerkatMessage::Commit { .. } => "Commit".to_string(),
                    other => format!("{other:?}"),
                });
                owner.serve_request(msg).await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    };
    let app = m.interner.insert("app");
    let result = tokio::select! {
        biased;
        r = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            m.execute_action(app, &stmts),
        ) => r.expect("the action must not time out"),
        _ = serve => unreachable!("the owner serves forever"),
    };
    assert!(result.is_ok(), "the action must succeed, got: {result:?}");
    let y = var_of(&mut owner, "s1", "y");
    (received.into_inner(), y)
}

/// The same call against a real owner: the member `__arg0` is reached, not the
/// argument value, so the owner's `y` becomes 3.
#[tokio::test(flavor = "multi_thread")]
async fn a_special_action_named_like_an_argument_runs_on_the_owner() {
    const CODE: &str = "
        service s1 {
            var y = 5;
            pub action __arg0(x: int) { y = x; }
        }
        service app {
            pub def run = action { do s1.__arg0(3); };
        }
    ";
    let (received, y) = run_against_owner(CODE, CODE, "run").await;

    assert_eq!(y, Value::Int { val: 3 });
    assert_eq!(received, vec!["Action".to_string(), "Commit".to_string()]);
}

/// Against a real owner declaring special forms, `do s1.a(s1.f(3));` leaves
/// `y` at 5 * 8 = 40 with exactly two requests before the commit.
#[tokio::test(flavor = "multi_thread")]
async fn the_wiki_example_against_a_real_owner_takes_two_requests() {
    let (received, y) = wiki_against_owner(SPECIAL).await;

    assert_eq!(y, Value::Int { val: 40 });
    assert_eq!(
        received,
        vec![
            "Lookup f args=1".to_string(),
            "Action".to_string(),
            "Commit".to_string()
        ]
    );
}

/// The caller's copy is out of date when the owner has the `fn` spelling: the
/// caller still sends `a` by name, and the owner runs it, because its `a` is
/// still a function whose body is an action written in place. The result is the
/// same, as the design requires.
#[tokio::test(flavor = "multi_thread")]
async fn an_owner_with_the_fn_spelling_gives_the_same_result() {
    const OWNER: &str = "
        service s1 {
            var y = 5;
            pub def f = fn (x: int) => x + y;
            pub def a = fn (x: int) => action { y = y * x; };
        }
    ";
    let (received, y) = wiki_against_owner(OWNER).await;

    assert_eq!(y, Value::Int { val: 40 }, "received: {received:?}");
    // The owner answers the call of `f` with the member, which the caller
    // applies, reading `y`; `a` is still sent by name, in one request
    assert_eq!(
        received,
        vec![
            "Lookup f args=1".to_string(),
            "Lookup y args=0".to_string(),
            "Action".to_string(),
            "Commit".to_string()
        ]
    );
}

const CLOSURE_S1: &str = "
    service s1 {
        var y = 0;
        pub def apply(g: int -> int) = g(1);
        pub action set(x: int) { y = x; }
    }
";

const CLOSURE_FN_S1: &str = "
    service s1 {
        var y = 0;
        pub def apply = fn (g: int -> int) => g(1);
        pub def set = fn (x: int) => action { y = x; };
    }
";

const CLOSURE_APP: &str = "
    service app {
        var v = 7;
        pub def run = action { do s1.set(s1.apply(fn (n: int) => v + n)); };
    }
";

/// `do s1.set(s1.apply(fn (n: int) => v + n))` leaves `y` at 8. A closure
/// argument carries its service only by name, so on the owner its read of `v`
/// would fail: the call of `apply` is a plain lookup and the caller applies
/// the member, reading its own `v`. `set` is still one by-name request.
#[tokio::test(flavor = "multi_thread")]
async fn a_closure_argument_is_applied_on_the_caller() {
    let caller = format!("{CLOSURE_S1}{CLOSURE_APP}");
    let (received, y) = run_against_owner(&caller, CLOSURE_S1, "run").await;

    assert_eq!(y, Value::Int { val: 8 }, "received: {received:?}");
    assert_eq!(
        received,
        vec![
            "Lookup apply args=0".to_string(),
            "Action".to_string(),
            "Commit".to_string()
        ]
    );
}

/// An owner that also runs a service named `app` with its own `v = 100` must
/// not be read: the closure belongs to the caller's `app`, so `y` is 8, not
/// 101.
#[tokio::test(flavor = "multi_thread")]
async fn a_closure_argument_does_not_read_a_service_of_the_same_name_on_the_owner() {
    let caller = format!("{CLOSURE_S1}{CLOSURE_APP}");
    let owner = format!("{CLOSURE_S1} service app {{ var v = 100; }}");
    let (received, y) = run_against_owner(&caller, &owner, "run").await;

    assert_eq!(y, Value::Int { val: 8 }, "received: {received:?}");
}

/// The `fn` spelling on both sides gives the same `y`, 8. The fetched action
/// runs on the owner, so the caller's own copy of `s1` is removed first. A
/// call that is not sent reads its callee before its arguments
/// (`evaluator.rs`), so `set` is fetched before `apply`.
#[tokio::test(flavor = "multi_thread")]
async fn a_closure_argument_gives_the_fn_spelling_result() {
    let caller = format!("{CLOSURE_FN_S1}{CLOSURE_APP}");
    let (received, y) = run_against_owner_with(&caller, CLOSURE_FN_S1, "run", true).await;

    assert_eq!(y, Value::Int { val: 8 }, "received: {received:?}");
    assert_eq!(
        received,
        vec![
            "Lookup set args=0".to_string(),
            "Lookup apply args=0".to_string(),
            "Action".to_string(),
            "Commit".to_string()
        ]
    );
}

/// An argument with no wire form (html) cannot be sent by name, but the action
/// may never read it: `do s1.show(html)` then fetches `show` and ships only
/// what the action's body captures, as before calls ran on the owner.
///
/// Without the fallback the `do` fails, because the by-name request cannot
/// encode `__arg0`.
#[tokio::test(flavor = "multi_thread")]
async fn an_html_argument_takes_the_fetch_path() {
    const CODE: &str = "
        service s1 {
            var y = 5;
            pub action show(t: string) { y = 0; }
        }
        service app {
            pub def run_show = action { do s1.show((<p>x</p>)); };
        }
    ";
    let (app_net, _) = listening_node().await;
    let (mut s1_net, s1_addr) = listening_node().await;
    let (mut m, _) = caller_node(CODE, app_net, &s1_addr).await;
    // A caller that imported `s1` holds no running copy of it: the fetched
    // action must resolve to the owner, not to a local service of that name
    let s1 = m.interner.insert("s1");
    m.services.remove(&s1);
    let stmts = do_member(&mut m, "run_show");
    let log = Log::default();
    let show_closure = {
        let mut i = Interner::new();
        let [t, y, s1] = ["t", "y", "s1"].map(|n| i.insert(n));
        encode_value(
            &Value::Closure {
                params: vec![Param {
                    name: t,
                    ty: Some(Type::String),
                }],
                body: Box::new(Expr::Action(vec![ActionStmt::Assign {
                    name: y,
                    expr: Expr::Literal {
                        val: Value::Int { val: 0 },
                    },
                }])),
                env: vec![],
                service_name: s1,
                return_ty: None,
            },
            &i,
        )
        .expect("closure encodes")
    };
    let answer = move |_: &str, _: &Option<Vec<NetValue>>| (show_closure.clone(), false);

    let result = run_on_app(&mut m, &mut s1_net, &log, &answer, &stmts, false).await;

    assert!(result.is_ok(), "the action must succeed, got: {result:?}");
    let seen = log.borrow();
    assert!(
        matches!(seen.first(), Some(Seen::Lookup { member, args: None, .. }) if member == "show"),
        "a plain lookup of show comes first: {seen:?}"
    );
    assert!(
        matches!(seen.get(1), Some(Seen::Action { stmts, env }) if stmts.contains("Assign") && env.is_empty()),
        "then the action's body, with nothing captured: {seen:?}"
    );
    assert_eq!(seen.last(), Some(&Seen::Commit), "{seen:?}");
}

/// Run `do member;` of the caller built from `caller_code` against a real owner
/// running `owner_code` that holds a forward to the CALLER's service `app`
/// (`pub def a = app.hit;`). The owner sees `app` as a remote service at the
/// caller's address, as `caller_node` does for `s1`: it keeps a copy of `app`'s
/// declaration for its static checks, but not a running instance. Returns the
/// result of the action, what the owner received and the caller's `app.hits`.
async fn run_against_forwarding_owner(
    caller_code: &str,
    owner_code: &str,
    member: &str,
) -> (
    Result<(), meerkat_lib::runtime::interpreter::EvalError>,
    Vec<String>,
    Value,
) {
    let (app_net, app_addr) = listening_node().await;
    let (owner_net, owner_addr) = listening_node().await;
    let (mut m, _) = caller_node(caller_code, app_net, &owner_addr).await;

    let mut interner = Interner::new();
    let ast = parse_string(owner_code, &mut interner).expect("valid syntax");
    let mut node = Node::new();
    node.interner = interner;
    node.unified_ast = ast.clone();
    node.static_checks().expect("static checks must pass");
    let mut owner = node
        .on_manager_startup(true, Some(owner_net), HashMap::new(), &ast)
        .await
        .expect("owner startup");
    let app = owner.interner.insert("app");
    owner.services.remove(&app);
    owner
        .remote_services
        .insert(app, Address::new(format!("{}/app", app_addr.0)));

    let stmts = do_member(&mut m, member);
    let received = RefCell::new(Vec::new());
    let serve = async {
        loop {
            while let Some(NetworkEvent::MessageReceived { msg, .. }) = owner.try_recv_event() {
                received.borrow_mut().push(match &msg {
                    MeerkatMessage::LookupRequest { member, args, .. } => {
                        format!("Lookup {member} args={}", args.as_ref().map_or(0, Vec::len))
                    }
                    MeerkatMessage::ActionRequest { by_name, .. } => {
                        format!("Action by_name={by_name}")
                    }
                    MeerkatMessage::Commit { .. } => "Commit".to_string(),
                    other => format!("{other:?}"),
                });
                owner.serve_request(msg).await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    };
    let caller_app = m.interner.insert("app");
    let result = tokio::select! {
        biased;
        r = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            m.execute_action(caller_app, &stmts),
        ) => r.expect("the action must not time out"),
        _ = serve => unreachable!("the owner serves forever"),
    };
    let hits = var_of(&mut m, "app", "hits");
    (result, received.into_inner(), hits)
}

/// The owner changed `a` into a forward to an action of the caller's own service
/// after the caller took its copy, which still shows `pub def a = action { .. }`.
/// `do s1.a;` must run `app.hit` on the caller, once, as it does when the caller
/// fetches `a`. The by-name request is declined by the owner, and the caller
/// then looks `a` up and runs the action it gets.
///
/// Without the owner's decline the owner evaluates `a`, gets the caller's
/// action and sends it back to the caller, which is waiting for the owner's
/// reply: the call times out and `hits` stays 0.
#[tokio::test(flavor = "multi_thread")]
async fn a_stale_copy_of_an_action_that_became_a_forward_is_fetched() {
    const CALLER: &str = "
        service s1 {
            var y = 0;
            pub def a = action { y = 1; };
        }
        service app {
            var hits = 0;
            pub def hit = action { hits = hits + 1; };
            pub def run = action { do s1.a; };
        }
    ";
    const OWNER: &str = "
        service app {
            var hits = 0;
            pub def hit = action { hits = hits + 1; };
        }
        service s1 {
            var y = 0;
            pub def a = app.hit;
        }
    ";
    let (result, received, hits) = run_against_forwarding_owner(CALLER, OWNER, "run").await;

    assert!(result.is_ok(), "the action must succeed, got: {result:?}");
    assert_eq!(hits, Value::Int { val: 1 }, "received: {received:?}");
    assert_eq!(
        received,
        vec![
            "Action by_name=true".to_string(),
            "Lookup a args=0".to_string(),
            "Commit".to_string()
        ]
    );
}

/// The same with arguments: the caller's copy still declares `pub action
/// a(x: int)`, the owner's `a` is now a plain def whose function returns the
/// caller's action. The by-name call is declined, the call goes through the
/// def-call path (a lookup that carries the argument; the owner's `a` is plain,
/// so it answers with the member) and the caller applies it and runs the action
/// it returns, once.
///
/// Without the owner's decline the call times out as in the zero-argument case.
#[tokio::test(flavor = "multi_thread")]
async fn a_stale_copy_of_a_special_action_that_became_a_forward_is_fetched() {
    const CALLER: &str = "
        service s1 {
            var y = 0;
            pub action a(x: int) { y = x; }
        }
        service app {
            var hits = 0;
            pub def hit = action { hits = hits + 1; };
            pub def run = action { do s1.a(5); };
        }
    ";
    const OWNER: &str = "
        service app {
            var hits = 0;
            pub def hit = action { hits = hits + 1; };
        }
        service s1 {
            var y = 0;
            pub def a = fn (x: int) => app.hit;
        }
    ";
    let (result, received, hits) = run_against_forwarding_owner(CALLER, OWNER, "run").await;

    assert!(result.is_ok(), "the action must succeed, got: {result:?}");
    assert_eq!(hits, Value::Int { val: 1 }, "received: {received:?}");
    assert_eq!(
        received,
        vec![
            "Action by_name=true".to_string(),
            "Lookup a args=1".to_string(),
            "Commit".to_string()
        ]
    );
}
