//! Issue #20, stage B: a call of a member of a remote service goes to its
//! owner as a `LookupRequest` that carries the arguments, and the owner
//! decides whether to run it (design note `docs/design/execution-location.md`,
//! "Where a call runs" and "Protocol").
//!
//! The node under test (the caller) runs `app`, which calls `s1.f` on a
//! stand-in peer that records every message and answers as the test chooses.
//! The last test uses a real owner.

use meerkat_lib::net::ast::NetValue;
use meerkat_lib::net::codec::{encode_value, validate_call_args};
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
    let app = m.interner.insert("app");
    tokio::select! {
        biased;
        r = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            if as_test {
                m.execute_test_block(app, stmts).await
            } else {
                m.execute_action(app, stmts).await
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

/// An argument with no wire form (html, typed `string`) to a plain function
/// member is not an error: the call falls back to a plain lookup and the
/// caller applies the member, as before calls ran on the owner.
#[tokio::test(flavor = "multi_thread")]
async fn an_argument_with_no_wire_form_falls_back_to_a_lookup() {
    const CODE: &str = "
        service s1 {
            pub def g = fn (t: string) => 7;
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
