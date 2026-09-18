//! Transaction-local reactivity: a transaction must see its own writes
//! reflected in the derived members (`def`s) computed from them, locally and
//! across nodes, while `var`s stay non-reactive.
//!
//! A `def` is an eagerly evaluated cached value rather than a thunk, so its
//! stored value is only refreshed by propagation. These tests pin down when
//! that refresh happens relative to a transaction's own writes.

use meerkat_lib::runtime::ast::{ActionStmt, Stmt, Value};
use meerkat_lib::runtime::interpreter::EvalError;
use meerkat_lib::runtime::parser::parse_string;
use meerkat_lib::runtime::txn::TxnId;
use meerkat_lib::runtime::{Interner, Manager, Node};
use std::collections::HashMap;

/// Build a `Manager` from Meerkat source, running the same static checks the
/// CLI runs. Returns the manager and the program's `@test` blocks.
async fn setup(code: &str) -> (Manager, Vec<(String, Vec<ActionStmt>)>) {
    let mut interner = Interner::new();
    let ast = parse_string(code, &mut interner).expect("valid syntax");
    let mut node = Node::new();
    node.interner = interner;
    node.unified_ast = ast.clone();
    node.static_checks().expect("static checks must pass");

    let local_ast = node.unified_ast.clone();
    let manager = node
        .on_manager_startup(true, None, HashMap::new(), &local_ast)
        .await
        .expect("service init must succeed");

    let tests = ast
        .iter()
        .filter_map(|s| match s {
            Stmt::Test {
                service_name,
                stmts,
            } => Some((
                manager.interner.get(*service_name).to_string(),
                stmts.clone(),
            )),
            _ => None,
        })
        .collect();
    (manager, tests)
}

/// Run one `@test` block the way the CLI does: as a single transaction.
async fn run_test_block(
    manager: &mut Manager,
    service: &str,
    stmts: &[ActionStmt],
) -> Result<(), EvalError> {
    let svc = manager.interner.insert(service);
    manager.execute_action(svc, stmts).await
}

/// A transaction that writes `x` must see `def y = x + 1` refreshed, including
/// through a chain of defs, and the committed values must agree afterwards.
#[tokio::test]
async fn test_local_def_chain_refreshed_within_transaction() {
    let (mut m, tests) = setup(
        "
        service s {
            var x = 0;
            pub def y = x + 1;
            pub def z = y * 2;
        }
        @test(s) {
            assert(y == 1);
            assert(z == 2);
            x = 4;
            assert(y == 5);
            assert(z == 10);
        }
        @test(s) {
            assert(x == 4);
            assert(y == 5);
            assert(z == 10);
        }
        ",
    )
    .await;
    for (svc, stmts) in &tests {
        run_test_block(&mut m, svc, stmts).await.unwrap();
    }
}

/// A `var` is a leaf, not a reactive member: per `docs/internals/statics.md` its
/// initializer may read other members but it never propagates changes. This
/// holds for a cross-service initializer too, which is the case that used to
/// register a listener no recompute could ever satisfy.
#[tokio::test]
async fn test_var_initialized_from_another_service_is_not_reactive() {
    let (mut m, tests) = setup(
        "
        service p {
            var n = 1;
            pub def get_n = n;
            pub def set_n = action { n = 5; };
        }
        service q {
            var w = p.get_n - 2;
            pub def mm = p.get_n + 10;
        }
        @test(q) {
            assert(w == -1);
            assert(mm == 11);
            do p.set_n;
            assert(mm == 15);
            assert(w == -1);
        }
        @test(q) {
            assert(mm == 15);
            assert(w == -1);
        }
        ",
    )
    .await;
    for (svc, stmts) in &tests {
        run_test_block(&mut m, svc, stmts).await.unwrap();
    }

    // The var must not be wired into the listener graph at all.
    let p = m.interner.insert("p");
    let get_n = m.interner.insert("get_n");
    let w = m.interner.insert("w");
    let listeners = m.services.get(&p).unwrap().listeners.get(&get_n).cloned();
    let names: Vec<_> = listeners
        .unwrap_or_default()
        .iter()
        .map(|(_, def)| *def)
        .collect();
    assert!(
        !names.contains(&w),
        "a `var` must not be registered as a reactive listener"
    );
}

/// Participant side of a distributed transaction. The owning node executes a
/// composed action under the originator's transaction id and holds the write
/// buffered. A transactional remote read of a derived member must then see that
/// buffered write -- this is what lets the originator recompute its own defs
/// over this service and get read-your-own-writes across nodes.
#[tokio::test]
async fn test_participant_transactional_read_sees_buffered_def() {
    let (mut m, _) = setup(
        "
        service s {
            var x = 0;
            pub def y = x + 1;
        }
        ",
    )
    .await;
    let svc = m.interner.insert("s");
    let x = m.interner.insert("x");
    let y = m.interner.insert("y");

    let tid = TxnId {
        timestamp: 1,
        node_id: 42,
        iteration: 0,
    };

    // `x = x + 1`, executed as a participant: locks taken, write buffered,
    // nothing committed.
    let stmts = parse_string(
        "service tmp { pub def a = action { x = x + 1; }; }",
        &mut m.interner,
    )
    .map(|ast| match &ast[0] {
        Stmt::Service { decls, .. } => match &decls[0] {
            meerkat_lib::runtime::ast::Decl::DefDecl {
                val: meerkat_lib::runtime::ast::Expr::Action(stmts),
                ..
            } => stmts.clone(),
            _ => panic!("expected an action"),
        },
        _ => panic!("expected a service"),
    })
    .unwrap();

    m.execute_action_participant(svc, &stmts, &[], tid.clone())
        .await
        .unwrap();

    // The derived member, read under the shared transaction id, reflects the
    // participant's own uncommitted write.
    let seen = m
        .remote_read_participant(svc, y, tid.clone())
        .await
        .unwrap();
    assert_eq!(
        seen,
        Value::Int { val: 2 },
        "a transactional remote read must see the participant's buffered write through a def"
    );

    // Still uncommitted: a plain read outside the transaction sees the old values.
    assert_eq!(m.lookup(x, svc, None).await.unwrap(), Value::Int { val: 0 });
    assert_eq!(m.lookup(y, svc, None).await.unwrap(), Value::Int { val: 1 });
}
