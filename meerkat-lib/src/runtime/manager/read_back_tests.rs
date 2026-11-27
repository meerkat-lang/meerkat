use super::*;
use crate::runtime::parser::parse_string;
use crate::runtime::txn::VarLock;
use crate::runtime::Node;

async fn setup() -> (Manager, Symbol, Symbol, Symbol, Symbol) {
    let mut node = Node::new();
    node.unified_ast = parse_string(
        "service a { var x = 0; var y = 0; table rows { value: int, }; }",
        &mut node.interner,
    )
    .unwrap();
    node.static_checks().unwrap();
    let ast = node.unified_ast.clone();
    let mut manager = node
        .on_manager_startup(true, None, HashMap::new(), &ast)
        .await
        .unwrap();
    let [a, x, y, rows] = ["a", "x", "y", "rows"].map(|n| manager.interner.insert(n));
    (manager, a, x, y, rows)
}

/// Model the state held by an enclosing execution at an RPC suspension point.
fn check_out(manager: &mut Manager, tid: &TxnId) -> Transaction {
    let mut txn = manager
        .pending_txns
        .remove(tid)
        .unwrap_or_else(|| Transaction::new(tid.clone()));
    manager.active_txns.enter(&mut txn);
    txn
}

fn assign_int(name: Symbol, val: i32) -> ActionStmt {
    ActionStmt::Assign {
        name,
        expr: Expr::Literal {
            val: Value::Int { val },
        },
    }
}

#[tokio::test]
async fn nested_read_back_preserves_enclosing_writes_and_checks_before_cache() {
    let (mut manager, a, x, y, _) = setup().await;
    let tid = TxnId::new(manager.node_id);
    let mut outer = check_out(&mut manager, &tid);
    // Cache x in the read-back's separate transaction before the outer write.
    manager
        .remote_read_participant(a, x, tid.clone())
        .await
        .unwrap();
    manager
        .assign(a, x, Value::Int { val: 10 }, &mut outer)
        .unwrap();
    assert_eq!(
        manager.lookup(x, a, Some(&mut outer)).await.unwrap(),
        Value::Int { val: 10 }
    );
    manager
        .remote_read_participant(a, y, tid.clone())
        .await
        .unwrap();
    // Both a successful nested exit and a failed one must preserve the guard.
    for _ in 0..2 {
        let error = manager
            .remote_read_participant(a, x, tid.clone())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("a.x"), "{error}");
        assert!(error.to_string().contains(&format!("{tid:?}")), "{error}");
        assert!(manager.active_txns.contains(&tid));
    }
    manager.active_txns.exit(&mut outer);
    assert!(!manager.active_txns.contains(&tid));
    manager.release_all_locks(&outer);
    assert_eq!(
        manager
            .remote_read_participant(a, x, tid.clone())
            .await
            .unwrap()
            .0,
        Value::Int { val: 0 }
    );
    manager.abort_participant(&tid).await;
}

#[tokio::test]
async fn read_back_seeds_existing_writes_and_checks_all_enclosing_executions() {
    let (mut manager, a, x, y, _) = setup().await;
    let tid = TxnId::new(manager.node_id);
    manager
        .execute_action_participant(a, &[assign_int(x, 10)], &[], tid.clone())
        .await
        .unwrap();
    assert!(!manager.active_txns.contains(&tid));
    let mut outer = check_out(&mut manager, &tid);
    let mut middle = check_out(&mut manager, &tid);
    // The innermost read must see both enclosing registrations, not just one.
    manager
        .assign(a, y, Value::Int { val: 20 }, &mut middle)
        .unwrap();
    for member in [x, y] {
        assert!(matches!(
            manager
                .remote_read_participant(a, member, tid.clone())
                .await,
            Err(EvalError::RuntimeError(_))
        ));
    }
    manager.active_txns.exit(&mut middle);
    manager.release_all_locks(&middle);
    manager
        .remote_read_participant(a, y, tid.clone())
        .await
        .unwrap();
    assert!(manager
        .remote_read_participant(a, x, tid.clone())
        .await
        .is_err());
    manager.active_txns.exit(&mut outer);
    manager.pending_txns.insert(tid.clone(), outer);
    assert_eq!(
        manager
            .remote_read_participant(a, x, tid.clone())
            .await
            .unwrap()
            .0,
        Value::Int { val: 10 }
    );
    manager.abort_participant(&tid).await;
}

#[tokio::test]
async fn table_read_back_rejects_buffered_appends() {
    let (mut manager, a, _, _, rows) = setup().await;
    let tid = TxnId::new(manager.node_id);
    let mut outer = check_out(&mut manager, &tid);
    let field = manager.interner.insert("value");
    let vals: Vec<_> = [1, 2]
        .into_iter()
        .map(|val| Value::Record {
            fields: vec![(field, Value::Int { val })],
        })
        .collect();
    for row in &vals {
        manager
            .append_table_row(a, rows, row.clone(), &mut outer)
            .unwrap();
    }
    assert_eq!(
        manager.lookup(rows, a, Some(&mut outer)).await.unwrap(),
        Value::List { vals }
    );
    assert!(manager
        .remote_read_participant(a, rows, tid.clone())
        .await
        .is_err());
    manager.active_txns.exit(&mut outer);
    manager.release_all_locks(&outer);
}

#[tokio::test]
async fn parked_read_back_leaves_the_enclosing_registration() {
    let (mut manager, a, x, y, _) = setup().await;
    let tid = TxnId::new(manager.node_id);
    let mut outer = check_out(&mut manager, &tid);
    manager
        .assign(a, x, Value::Int { val: 10 }, &mut outer)
        .unwrap();
    manager
        .services
        .get_mut(&a)
        .unwrap()
        .vars
        .get_mut(&y)
        .unwrap()
        .lock = VarLock::WriteLocked(TxnId::new(manager.node_id));
    assert!(matches!(
        manager.remote_read_participant(a, y, tid.clone()).await,
        Err(EvalError::WaitOn(_))
    ));
    manager
        .services
        .get_mut(&a)
        .unwrap()
        .vars
        .get_mut(&y)
        .unwrap()
        .lock = VarLock::Unlocked;
    manager
        .remote_read_participant(a, y, tid.clone())
        .await
        .unwrap();
    assert!(manager
        .remote_read_participant(a, x, tid.clone())
        .await
        .is_err());
    manager.active_txns.exit(&mut outer);
    manager.release_all_locks(&outer);
    assert!(!manager.active_txns.contains(&tid));
}

#[tokio::test]
async fn parked_action_removes_registration_and_restores_prior_writes() {
    let (mut manager, a, x, y, _) = setup().await;
    let tid = TxnId::new(manager.node_id);
    manager
        .execute_action_participant(a, &[assign_int(x, 10)], &[], tid.clone())
        .await
        .unwrap();
    manager
        .services
        .get_mut(&a)
        .unwrap()
        .vars
        .get_mut(&y)
        .unwrap()
        .lock = VarLock::WriteLocked(TxnId::new(manager.node_id));
    let stmts = [assign_int(x, 20), assign_int(y, 1)];
    let error = manager
        .execute_action_participant(a, &stmts, &[], tid.clone())
        .await
        .unwrap_err();
    assert!(matches!(error, EvalError::WaitOn(_)), "{error:?}");
    assert!(manager.active_txns.is_empty());
    assert_eq!(
        manager
            .remote_read_participant(a, x, tid.clone())
            .await
            .unwrap()
            .0,
        Value::Int { val: 10 }
    );
    manager
        .services
        .get_mut(&a)
        .unwrap()
        .vars
        .get_mut(&y)
        .unwrap()
        .lock = VarLock::Unlocked;
    manager
        .execute_action_participant(a, &stmts, &[], tid.clone())
        .await
        .unwrap();
    assert!(manager.active_txns.is_empty());
    assert_eq!(
        manager
            .remote_read_participant(a, x, tid.clone())
            .await
            .unwrap()
            .0,
        Value::Int { val: 20 }
    );
    manager.abort_participant(&tid).await;
}

#[tokio::test(start_paused = true)]
async fn execution_exit_cleans_registration_after_success_failure_and_retry() {
    let (mut manager, a, x, y, _) = setup().await;
    manager
        .execute_action(a, &[assign_int(x, 10)])
        .await
        .unwrap();
    assert!(manager.active_txns.is_empty());
    let fail = [
        assign_int(x, 20),
        ActionStmt::Assert(
            Expr::Literal {
                val: Value::Bool { val: false },
            },
            "false".into(),
        ),
    ];
    assert!(matches!(
        manager.execute_action(a, &fail).await,
        Err(EvalError::AssertionError(_))
    ));
    assert!(manager.active_txns.is_empty());
    let tid = TxnId::new(manager.node_id);
    assert!(matches!(
        manager.execute_action_participant(a, &fail, &[], tid).await,
        Err(EvalError::AssertionError(_))
    ));
    assert!(manager.active_txns.is_empty());
    // Every originator attempt buffers x before encountering y's lock.
    for timestamp in [0, u128::MAX] {
        manager
            .services
            .get_mut(&a)
            .unwrap()
            .vars
            .get_mut(&y)
            .unwrap()
            .lock = VarLock::WriteLocked(TxnId {
            timestamp,
            node_id: 1,
            iteration: 0,
        });
        assert!(matches!(
            manager
                .execute_action(a, &[assign_int(x, 30), assign_int(y, 1)])
                .await,
            Err(EvalError::WaitDieAbort(_))
        ));
        assert!(manager.active_txns.is_empty());
        assert_eq!(manager.services[&a].vars[&x].value, Value::Int { val: 10 });
        assert!(matches!(
            manager.services[&a].vars[&x].lock,
            VarLock::Unlocked
        ));
    }
}
