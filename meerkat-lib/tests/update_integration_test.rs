//! Integration tests for atomic update transactions (`runtime::update`)
//!
//! Validates atomic hot-reloading updates on `Manager` instances,
//! testing state separation, replacement-only bounds checks, static
//! analysis validation, and commit fidelity across all high-level
//! algorithm steps

use meerkat_lib::runtime::{
    ast::Value,
    interpreter::{eval, EvalContext},
    parser::parse_string,
    txn::{Transaction as LockTxn, TxnId, VarLock},
    update::Transaction,
    Interner, Node,
};
use std::collections::HashMap;

/// Helper function to create an initialized test `Manager` instance
///
/// Builds a default running environment containing service `s1` with
/// initial state `var x = 10` and `pub def y = (x * 2)`
///
/// Args:
///     `interner` (`&mut Interner`): String interner reference
///
/// Returns:
///     `meerkat_lib::runtime::Manager`: Ready test `Manager` instance
async fn setup_test_manager(interner: &mut Interner) -> meerkat_lib::runtime::Manager {
    let initial_code = "
        service s1 {
            var x = 10;
            pub def y = (x * 2);
        }
    ";

    let initial_ast = parse_string(initial_code, interner).unwrap();
    let mut node = Node::new();
    node.interner = interner.clone();
    node.unified_ast = initial_ast;
    node.static_checks().unwrap();

    let local_ast = node.unified_ast.clone();
    let manager = node
        .on_manager_startup(true, None, HashMap::new(), &local_ast)
        .await
        .unwrap();

    *interner = manager.interner.clone();
    manager
}

/// Verify Step 1: Malformed update syntax is rejected during parsing
///
/// Ensures syntax validation fails early before `Transaction` creation
///
/// Returns:
///     `()`
#[tokio::test]
async fn test_step1_receive_and_limits_syntax_error() {
    let mut interner = Interner::new();

    let malformed_update_code = "
        update s1 {
            var x = ;
        }
    ";
    let parse_res = parse_string(malformed_update_code, &mut interner);
    assert!(parse_res.is_err());
}

/// Verify Step 2: Lock acquisition conflict aborts update safely
///
/// Pre-locks variable `x` with `VarLock::WriteLocked` and asserts that
/// `Transaction::poll` fails while preserving the original `x = 10` state
///
/// Returns:
///     `()`
#[tokio::test]
async fn test_step2_locking_conflict() {
    let mut interner = Interner::new();
    let mut manager = setup_test_manager(&mut interner).await;

    let s1_sym = manager.interner.insert("s1");
    let x_sym = manager.interner.insert("x");

    let lock_txn_id = TxnId::new(manager.node_id);
    if let Some(service) = manager.services.get_mut(&s1_sym) {
        if let Some(var_state) = service.vars.get_mut(&x_sym) {
            var_state.lock = VarLock::WriteLocked(lock_txn_id);
        }
    }

    let update_code = "
        update s1 {
            var x = 99;
        }
    ";
    let update_ast = parse_string(update_code, &mut manager.interner).unwrap();

    let mut txn = Transaction::new(update_ast);
    let poll_res = txn.poll(&mut manager).await;
    assert!(poll_res.is_err());

    let service = manager.services.get(&s1_sym).unwrap();
    let var_state = service.vars.get(&x_sym).unwrap();
    assert_eq!(var_state.value, Value::Int { val: 10 });
}

/// Verify Step 3: Non-existent target service update rejection
///
/// Asserts that attempting to update an unknown service `nonexistent_service`
/// is cleanly rejected during transaction validation, returning `Err`
///
/// Returns:
///     `()`
#[tokio::test]
async fn test_step3_nonexistent_service_rejection() {
    let mut interner = Interner::new();
    let mut manager = setup_test_manager(&mut interner).await;

    let invalid_update_code = "
        update nonexistent_service {
            var x = 99;
        }
    ";
    let update_ast = parse_string(invalid_update_code, &mut manager.interner).unwrap();

    let mut txn = Transaction::new(update_ast);
    let poll_res = txn.poll(&mut manager).await;
    assert!(poll_res.is_err());

    let s1_sym = manager.interner.insert("s1");
    let x_sym = manager.interner.insert("x");
    let service = manager.services.get(&s1_sym).unwrap();
    assert_eq!(
        service.vars.get(&x_sym).unwrap().value,
        Value::Int { val: 10 }
    );
}

/// Verify Step 4 & 5: Static validation rejection untaints state
///
/// Sends an update assigning a `String` to integer field `x` and asserts
/// that typechecking fails while `Manager` runtime state remains intact
///
/// Returns:
///     `()`
#[tokio::test]
async fn test_step4_and_5_static_validation_rejection() {
    let mut interner = Interner::new();
    let mut manager = setup_test_manager(&mut interner).await;

    let ill_typed_update_code = "
        update s1 {
            var x = \"string_value_instead_of_int\";
        }
    ";
    let update_ast = parse_string(ill_typed_update_code, &mut manager.interner).unwrap();

    let mut txn = Transaction::new(update_ast);
    let poll_res = txn.poll(&mut manager).await;
    assert!(poll_res.is_err());

    let s1_sym = manager.interner.insert("s1");
    let x_sym = manager.interner.insert("x");
    let service = manager.services.get(&s1_sym).unwrap();
    assert_eq!(
        service.vars.get(&x_sym).unwrap().value,
        Value::Int { val: 10 }
    );
}

/// Verify Step 6: Old-state evaluation constraint failure aborts
///
/// Sends an update containing a division-by-zero expression `(1 / 0)`
/// and verifies that evaluation against the old state fails safely
///
/// Returns:
///     `()`
#[tokio::test]
async fn test_step6_old_state_evaluation_failure() {
    let mut interner = Interner::new();
    let mut manager = setup_test_manager(&mut interner).await;

    let eval_fail_update_code = "
        update s1 {
            var x = (1 / 0);
        }
    ";
    let update_ast = parse_string(eval_fail_update_code, &mut manager.interner).unwrap();

    let mut txn = Transaction::new(update_ast);
    let poll_res = txn.poll(&mut manager).await;
    assert!(poll_res.is_err());

    let s1_sym = manager.interner.insert("s1");
    let x_sym = manager.interner.insert("x");
    let service = manager.services.get(&s1_sym).unwrap();
    assert_eq!(
        service.vars.get(&x_sym).unwrap().value,
        Value::Int { val: 10 }
    );
}

/// Verify Step 8 & 9: Successful update commit and def propagation
///
/// Applies `var x = 42` to service `s1`, asserting that `x` updates to `42`
/// and evaluating `y` (`x * 2`) yields `84`
///
/// Returns:
///     `()`
#[tokio::test]
async fn test_step8_and_9_success_and_propagation() {
    let mut interner = Interner::new();
    let mut manager = setup_test_manager(&mut interner).await;

    let update_code = "
        update s1 {
            var x = 42;
        }
    ";
    let update_ast = parse_string(update_code, &mut manager.interner).unwrap();

    let mut txn = Transaction::new(update_ast);
    let poll_res = txn.poll(&mut manager).await;
    assert!(poll_res.is_ok());

    let s1_sym = manager.interner.insert("s1");
    let x_sym = manager.interner.insert("x");
    let y_sym = manager.interner.insert("y");

    let service = manager.services.get(&s1_sym).unwrap();
    assert_eq!(
        service.vars.get(&x_sym).unwrap().value,
        Value::Int { val: 42 }
    );

    let y_expr = service.defs.get(&y_sym).cloned().unwrap();
    let env: Vec<(meerkat_lib::runtime::Symbol, Value)> = Vec::new();
    let mut eval_txn = LockTxn::new(TxnId::new(manager.node_id));
    let mut ctx = EvalContext {
        manager: &mut manager,
        service_name: s1_sym,
        txn: Some(&mut eval_txn),
    };

    let y_val = eval(&y_expr, &env, &mut ctx).await.unwrap();
    assert_eq!(y_val, Value::Int { val: 84 });
}

/// Verify type change rejection when dependent expression breaks
///
/// Updating `x` from `Int` to `String` while `y = (x * 2)` exists must fail
/// static typechecking and preserve the original `x = 10` state
///
/// Returns:
///     `()`
#[tokio::test]
async fn test_type_change_incompatible_rejection() {
    let mut interner = Interner::new();
    let mut manager = setup_test_manager(&mut interner).await;

    let incompatible_update_code = "
        update s1 {
            var x = \"hello\";
        }
    ";
    let update_ast = parse_string(incompatible_update_code, &mut manager.interner).unwrap();

    let mut txn = Transaction::new(update_ast);
    let poll_res = txn.poll(&mut manager).await;
    assert!(poll_res.is_err());

    let s1_sym = manager.interner.insert("s1");
    let x_sym = manager.interner.insert("x");
    let service = manager.services.get(&s1_sym).unwrap();
    assert_eq!(
        service.vars.get(&x_sym).unwrap().value,
        Value::Int { val: 10 }
    );
}

/// Verify valid type shift when dependent expressions are compatible
///
/// Updates `x` from `Int` to `String` and updates `label` to return `x`,
/// asserting that the type change commits and evaluates correctly
///
/// Returns:
///     `()`
#[tokio::test]
async fn test_type_change_valid_shift() {
    let mut interner = Interner::new();
    let initial_code = "
        service s1 {
            var x = 10;
            pub def label = \"initial\";
        }
    ";

    let initial_ast = parse_string(initial_code, &mut interner).unwrap();
    let mut node = Node::new();
    node.interner = interner.clone();
    node.unified_ast = initial_ast;
    node.static_checks().unwrap();

    let local_ast = node.unified_ast.clone();
    let mut manager = node
        .on_manager_startup(true, None, HashMap::new(), &local_ast)
        .await
        .unwrap();

    let update_code = "
        update s1 {
            var x = \"migrated\";
            pub def label = x;
        }
    ";
    let update_ast = parse_string(update_code, &mut manager.interner).unwrap();

    let mut txn = Transaction::new(update_ast);
    let poll_res = txn.poll(&mut manager).await;
    assert!(poll_res.is_ok());

    let s1_sym = manager.interner.insert("s1");
    let x_sym = manager.interner.insert("x");
    let label_sym = manager.interner.insert("label");

    let service = manager.services.get(&s1_sym).unwrap();
    assert_eq!(
        service.vars.get(&x_sym).unwrap().value,
        Value::String {
            val: "migrated".to_string()
        }
    );

    let label_expr = service.defs.get(&label_sym).cloned().unwrap();
    let env: Vec<(meerkat_lib::runtime::Symbol, Value)> = Vec::new();
    let mut eval_txn = LockTxn::new(TxnId::new(manager.node_id));
    let mut ctx = EvalContext {
        manager: &mut manager,
        service_name: s1_sym,
        txn: Some(&mut eval_txn),
    };

    let label_val = eval(&label_expr, &env, &mut ctx).await.unwrap();
    assert_eq!(
        label_val,
        Value::String {
            val: "migrated".to_string()
        }
    );
}

/// Verify that introducing a new field during an update is supported
///
/// Introducing `is_celsius` during an update on `s1` appends the declaration
/// to `existing_decls` and populates `service.vars` upon commit
///
/// Returns:
///     `()`
#[tokio::test]
async fn test_update_supports_introducing_new_field() {
    let mut interner = Interner::new();
    let mut manager = setup_test_manager(&mut interner).await;

    let add_field_update_code = "
        update s1 {
            var is_celsius = true;
        }
    ";
    let update_ast = parse_string(add_field_update_code, &mut manager.interner).unwrap();

    let mut txn = Transaction::new(update_ast);
    let poll_res = txn.poll(&mut manager).await;
    assert!(poll_res.is_ok(), "poll error: {:?}", poll_res);

    let s1_sym = manager.interner.insert("s1");
    let is_celsius_sym = manager.interner.insert("is_celsius");
    let service = manager.services.get(&s1_sym).unwrap();
    assert_eq!(
        service.vars.get(&is_celsius_sym).unwrap().value,
        Value::Bool { val: true }
    );
}

/// Verify that updating a service cannot remove existing fields
///
/// Ensures updating `x` in `s1` retains all existing fields (`x` and `y`)
/// in the runtime `Manager` service environment without dropping members
///
/// Returns:
///     `()`
#[tokio::test]
async fn test_update_rejects_removing_existing_field() {
    let mut interner = Interner::new();
    let initial_code = "
        service s1 {
            var x = 10;
            var y = 20;
        }
    ";

    let initial_ast = parse_string(initial_code, &mut interner).unwrap();
    let mut node = Node::new();
    node.interner = interner.clone();
    node.unified_ast = initial_ast;
    node.static_checks().unwrap();

    let local_ast = node.unified_ast.clone();
    let mut manager = node
        .on_manager_startup(true, None, HashMap::new(), &local_ast)
        .await
        .unwrap();

    let update_code = "
        update s1 {
            var x = 99;
        }
    ";
    let update_ast = parse_string(update_code, &mut manager.interner).unwrap();

    let mut txn = Transaction::new(update_ast);
    let poll_res = txn.poll(&mut manager).await;
    assert!(poll_res.is_ok());

    let s1_sym = manager.interner.insert("s1");
    let x_sym = manager.interner.insert("x");
    let y_sym = manager.interner.insert("y");

    let service = manager.services.get(&s1_sym).unwrap();
    assert_eq!(
        service.vars.get(&x_sym).unwrap().value,
        Value::Int { val: 99 }
    );
    assert_eq!(
        service.vars.get(&y_sym).unwrap().value,
        Value::Int { val: 20 }
    );
}

/// Verify introducing a new field fails when whole-service lock is held
///
/// Attempting to introduce `new_field` while another transaction holds a
/// whole-service lock on `s1` returns `Err` under wait-die deadlock prevention
///
/// Returns:
///     `()`
#[tokio::test]
async fn test_update_new_field_conflict_service_lock_rejection() {
    let mut interner = Interner::new();
    let mut manager = setup_test_manager(&mut interner).await;

    let s1_sym = manager.interner.insert("s1");
    let holder_txn = TxnId::new(999);
    manager.acquire_service_lock(s1_sym, &holder_txn).unwrap();

    let update_code = "
        update s1 {
            var new_field = 42;
        }
    ";
    let update_ast = parse_string(update_code, &mut manager.interner).unwrap();

    let mut txn = Transaction::new(update_ast);
    let poll_res = txn.poll(&mut manager).await;
    assert!(poll_res.is_err());

    let new_field_sym = manager.interner.insert("new_field");
    let service = manager.services.get(&s1_sym).unwrap();
    assert!(!service.vars.contains_key(&new_field_sym));
}

/// Verify introducing multiple new variables and reactive defs in update
///
/// Ensures an update introducing `new_a`, `new_b`, and reactive `sum`
/// succeeds and correctly populates service state upon commit
///
/// Returns:
///     `()`
#[tokio::test]
async fn test_update_multiple_new_fields_and_reactive_defs_success() {
    let mut interner = Interner::new();
    let mut manager = setup_test_manager(&mut interner).await;

    let update_code = "
        update s1 {
            var new_a = 10;
            var new_b = 20;
            pub def sum = new_a + new_b;
        }
    ";
    let update_ast = parse_string(update_code, &mut manager.interner).unwrap();

    let mut txn = Transaction::new(update_ast);
    let poll_res = txn.poll(&mut manager).await;
    assert!(poll_res.is_ok(), "poll error: {:?}", poll_res);

    let s1_sym = manager.interner.insert("s1");
    let new_a_sym = manager.interner.insert("new_a");
    let new_b_sym = manager.interner.insert("new_b");
    let sum_sym = manager.interner.insert("sum");

    let service = manager.services.get(&s1_sym).unwrap();
    assert_eq!(
        service.vars.get(&new_a_sym).unwrap().value,
        Value::Int { val: 10 }
    );
    assert_eq!(
        service.vars.get(&new_b_sym).unwrap().value,
        Value::Int { val: 20 }
    );

    let sum_val = manager.lookup(sum_sym, s1_sym, None).await.unwrap();
    assert_eq!(sum_val, Value::Int { val: 30 });
}

/// Verify that an atomic block containing updates for multiple services
/// applies all updates atomically
#[tokio::test]
async fn test_atomic_block_multiple_services_update() {
    let mut interner = Interner::new();
    let initial_code = "
        service s1 {
            var x = 10;
        }
        service s2 {
            var y = 20;
        }
    ";
    let initial_ast = parse_string(initial_code, &mut interner).expect("initial parse failed");
    let mut node = Node::new();
    node.interner = interner.clone();
    node.unified_ast = initial_ast;
    node.static_checks().expect("static checks failed");

    let local_ast = node.unified_ast.clone();
    let mut manager = node
        .on_manager_startup(true, None, HashMap::new(), &local_ast)
        .await
        .expect("manager startup failed");

    let atomic_code = "
        atomic {
            update s1 {
                var x = 100;
            }
            update s2 {
                var y = 200;
            }
        }
    ";
    let ast = parse_string(atomic_code, &mut manager.interner).expect("atomic parse failed");
    assert_eq!(ast.len(), 1);

    if let meerkat_lib::runtime::ast::Stmt::Atomic { updates } = &ast[0] {
        let mut txn = Transaction::new(updates.clone());
        let poll_res = txn.poll(&mut manager).await;
        assert!(poll_res.is_ok(), "poll error: {:?}", poll_res);
    } else {
        panic!("expected Stmt::Atomic");
    }

    let s1_sym = manager.interner.insert("s1");
    let s2_sym = manager.interner.insert("s2");
    let x_sym = manager.interner.insert("x");
    let y_sym = manager.interner.insert("y");

    let s1 = manager.services.get(&s1_sym).expect("service s1 missing");
    assert_eq!(
        s1.vars.get(&x_sym).expect("var x missing").value,
        Value::Int { val: 100 }
    );

    let s2 = manager.services.get(&s2_sym).expect("service s2 missing");
    assert_eq!(
        s2.vars.get(&y_sym).expect("var y missing").value,
        Value::Int { val: 200 }
    );
}

/// Verify that an update introducing a new def correctly re-wires listeners
/// so subsequent var assignments trigger reactive propagation
///
/// Returns:
///     `()`
#[tokio::test]
async fn test_update_listener_rewiring_propagation() {
    let mut interner = Interner::new();
    let initial_code = "
        service s1 {
            var x = 10;
        }
    ";
    let initial_ast = parse_string(initial_code, &mut interner).unwrap();
    let mut node = Node::new();
    node.interner = interner.clone();
    node.unified_ast = initial_ast;
    node.static_checks().unwrap();

    let local_ast = node.unified_ast.clone();
    let mut manager = node
        .on_manager_startup(true, None, HashMap::new(), &local_ast)
        .await
        .unwrap();

    let update_code = "
        update s1 {
            var x = 10;
            pub def y = (x + 1);
        }
    ";
    let update_ast = parse_string(update_code, &mut manager.interner).unwrap();
    let mut txn = Transaction::new(update_ast);
    let poll_res = txn.poll(&mut manager).await;
    assert!(poll_res.is_ok(), "poll error: {:?}", poll_res);

    let s1_sym = manager.interner.insert("s1");
    let x_sym = manager.interner.insert("x");
    let y_sym = manager.interner.insert("y");

    // Assign x = 20 to trigger reactive propagation over newly re-wired listener
    manager
        .assign(s1_sym, x_sym, Value::Int { val: 20 }, None)
        .await
        .unwrap();

    let y_val = manager.lookup(y_sym, s1_sym, None).await.unwrap();
    assert_eq!(y_val, Value::Int { val: 21 });
}

/// Verify cross-service dependency listener re-wiring during update
///
/// Returns:
///     `()`
#[tokio::test]
async fn test_update_cross_service_listener_rewiring() {
    let mut interner = Interner::new();
    let initial_code = "
        service s1 {
            var x = 10;
        }
        service s2 {
            pub def y = s1.x;
        }
    ";
    let initial_ast = parse_string(initial_code, &mut interner).unwrap();
    let mut node = Node::new();
    node.interner = interner.clone();
    node.unified_ast = initial_ast;
    node.static_checks().unwrap();

    let local_ast = node.unified_ast.clone();
    let mut manager = node
        .on_manager_startup(true, None, HashMap::new(), &local_ast)
        .await
        .unwrap();

    let update_code = "
        update s2 {
            pub def y = (s1.x * 3);
        }
    ";
    let update_ast = parse_string(update_code, &mut manager.interner).unwrap();
    let mut txn = Transaction::new(update_ast);
    let poll_res = txn.poll(&mut manager).await;
    assert!(poll_res.is_ok(), "poll error: {:?}", poll_res);

    let s1_sym = manager.interner.insert("s1");
    let s2_sym = manager.interner.insert("s2");
    let x_sym = manager.interner.insert("x");
    let y_sym = manager.interner.insert("y");

    // Assign s1.x = 5 to trigger propagation across services
    manager
        .assign(s1_sym, x_sym, Value::Int { val: 5 }, None)
        .await
        .unwrap();

    let y_val = manager.lookup(y_sym, s2_sym, None).await.unwrap();
    assert_eq!(y_val, Value::Int { val: 15 });
}

/// Verify that adding a new cross-service dependency in an update
/// block correctly rewires listener edges
///
/// Ensures listener edges are dynamically established when a definition
/// is updated to reference a cross-service variable
///
/// Returns:
///     `()`
#[tokio::test]
async fn test_update_adds_new_cross_service_listener_edge() {
    let mut interner = Interner::new();
    let initial_code = "
        service s1 {
            var x = 100;
        }
        service s2 {
            pub def y = 1;
        }
    ";
    let initial_ast = parse_string(initial_code, &mut interner).unwrap();
    let mut node = Node::new();
    node.interner = interner.clone();
    node.unified_ast = initial_ast;
    node.static_checks().unwrap();

    let local_ast = node.unified_ast.clone();
    let mut manager = node
        .on_manager_startup(true, None, HashMap::new(), &local_ast)
        .await
        .unwrap();

    let update_code = "
        update s2 {
            pub def y = s1.x;
        }
    ";
    let update_ast = parse_string(update_code, &mut manager.interner).unwrap();
    let mut txn = Transaction::new(update_ast);
    let poll_res = txn.poll(&mut manager).await;
    assert!(poll_res.is_ok(), "poll error: {:?}", poll_res);

    let s1_sym = manager.interner.insert("s1");
    let s2_sym = manager.interner.insert("s2");
    let x_sym = manager.interner.insert("x");
    let y_sym = manager.interner.insert("y");

    // Assign s1.x = 200 to verify the newly wired cross-service listener triggers
    manager
        .assign(s1_sym, x_sym, Value::Int { val: 200 }, None)
        .await
        .unwrap();

    let y_val = manager.lookup(y_sym, s2_sym, None).await.unwrap();
    assert_eq!(y_val, Value::Int { val: 200 });
}

/// Verify empty statement lists do not panic and complete cleanly
#[tokio::test]
async fn test_update_empty_statements_no_panic() {
    let interner = Interner::new();
    let mut manager = meerkat_lib::runtime::Manager::new(interner);
    let mut txn = Transaction::new(Vec::new());
    let poll_res = txn.poll(&mut manager).await;
    assert!(
        poll_res.is_ok(),
        "Empty transaction poll should succeed as no-op"
    );
}
