//! Integration tests for atomic update transactions (`runtime::update`)
//!
//! Validates atomic hot-reloading updates on `Manager` instances,
//! testing state separation, replacement-only bounds checks, static
//! analysis validation, and commit fidelity across all high-level
//! algorithm steps

use meerkat_lib::net::ast::NetValue;
use meerkat_lib::net::{
    Address, ClockEntry, MeerkatMessage, NetworkActor, NetworkCommand, NetworkEvent, NetworkReply,
    NodeType, ServiceNetId,
};
use meerkat_lib::runtime::{
    ast::{ActionStmt, Expr, Stmt, Value},
    interpreter::{eval, EvalContext},
    limits::MAX_PENDING_UPDATES_PER_PEER,
    parser::parse_string,
    txn::{Transaction as LockTxn, TxnId, VarLock},
    update::Transaction,
    Interner, Manager, Node,
};
use std::collections::HashMap;
use std::time::Duration;

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

/// Start a manager on `initial_code`, then apply `update_code` to it
async fn manager_after_update(
    initial_code: &str,
    update_code: &str,
) -> meerkat_lib::runtime::Manager {
    let mut interner = Interner::new();
    let initial_ast = parse_string(initial_code, &mut interner).unwrap();
    let mut node = Node::new();
    node.interner = interner;
    node.unified_ast = initial_ast;
    node.static_checks().unwrap();

    let local_ast = node.unified_ast.clone();
    let mut manager = node
        .on_manager_startup(true, None, HashMap::new(), &local_ast)
        .await
        .unwrap();

    let update_ast = parse_string(update_code, &mut manager.interner).unwrap();
    let mut txn = Transaction::new(update_ast);
    let poll_res = txn.poll(&mut manager).await;
    assert!(poll_res.is_ok(), "poll error: {:?}", poll_res);
    manager
}

/// Verify an update that turns a def into a var stops it listening to the
/// member the def read in the same service
///
/// This went wrong when the old def stayed in `service.defs` after the update
/// and the listener wiring consulted it (#210): the var stayed wired, and the
/// next change to `x` re-ran the old expression `x + 1` over the var's own
/// value
#[tokio::test]
async fn test_update_def_to_var_unwires_same_service_listener() {
    let mut manager = manager_after_update(
        "
        service s1 {
            var x = 1;
            pub def d = x + 1;
        }
        ",
        "
        update s1 {
            var d: int = x;
        }
        ",
    )
    .await;

    let s1_sym = manager.interner.insert("s1");
    let x_sym = manager.interner.insert("x");
    let d_sym = manager.interner.insert("d");

    manager
        .assign(s1_sym, d_sym, Value::Int { val: 50 }, None)
        .await
        .unwrap();
    manager
        .assign(s1_sym, x_sym, Value::Int { val: 11 }, None)
        .await
        .unwrap();

    let d_val = manager.lookup(d_sym, s1_sym, None).await.unwrap();
    assert_eq!(
        d_val,
        Value::Int { val: 50 },
        "12 means the var was recomputed from the old def expression"
    );
}

/// Verify an update that turns a def into a var stops it listening to the
/// member the def read in another service
#[tokio::test]
async fn test_update_def_to_var_unwires_cross_service_listener() {
    let mut manager = manager_after_update(
        "
        service b {
            var m = 1;
            pub def mm = m;
        }
        service a {
            pub def d = b.mm + 1;
        }
        ",
        "
        update a {
            var d: int = b.mm;
        }
        ",
    )
    .await;

    let a_sym = manager.interner.insert("a");
    let b_sym = manager.interner.insert("b");
    let m_sym = manager.interner.insert("m");
    let d_sym = manager.interner.insert("d");

    manager
        .assign(a_sym, d_sym, Value::Int { val: 50 }, None)
        .await
        .unwrap();
    manager
        .assign(b_sym, m_sym, Value::Int { val: 11 }, None)
        .await
        .unwrap();

    let d_val = manager.lookup(d_sym, a_sym, None).await.unwrap();
    assert_eq!(
        d_val,
        Value::Int { val: 50 },
        "12 means the var was recomputed from the old def expression"
    );
}

/// Verify an update that turns a def into a var is not undone by an `Update`
/// from a subscription the def made before
///
/// An owner on another node keeps the def's subscription after the update,
/// since nothing cancels it, and keeps sending `Update`s for it. Calling
/// `handle_update` directly delivers one the way the network dispatcher would.
/// Two things each keep the var safe: the update's commit rebuilds
/// `service.defs` without the old def, and a delivery is dropped unless the
/// def's current graph reads that member (`Manager::accept_delivery`)
#[tokio::test]
async fn test_update_def_to_var_ignores_update_from_stale_subscription() {
    let mut manager = manager_after_update(
        "
        service b {
            var m = 1;
            pub def mm = m;
        }
        service a {
            pub def d = b.mm + 1;
        }
        ",
        "
        update a {
            var d: int = b.mm;
        }
        ",
    )
    .await;

    let a_sym = manager.interner.insert("a");
    let b_sym = manager.interner.insert("b");
    let mm_sym = manager.interner.insert("mm");
    let d_sym = manager.interner.insert("d");

    assert!(
        !manager.services[&a_sym].defs.contains_key(&d_sym),
        "the update must drop the old def expression for `d`"
    );

    manager
        .assign(a_sym, d_sym, Value::Int { val: 50 }, None)
        .await
        .unwrap();
    let a_id = manager.services[&a_sym].id.clone();
    let value =
        meerkat_lib::net::codec::encode_value(&Value::Int { val: 11 }, &manager.interner).unwrap();
    manager
        .handle_update(
            a_id,
            d_sym,
            (b_sym, mm_sym),
            value,
            Default::default(),
            None,
        )
        .await;

    assert!(
        !manager.services[&a_sym].dep_cache.contains_key(&d_sym),
        "an `Update` for a listener that is no longer a def must be ignored"
    );
    let d_val = manager.lookup(d_sym, a_sym, None).await.unwrap();
    assert_eq!(
        d_val,
        Value::Int { val: 50 },
        "12 means the var was recomputed from the old def expression"
    );
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

/// Verify that failing to create a service due to type mismatch cleans the unified AST
///
/// This ensures phantom `Stmt::Service` nodes are not left in the AST when initialization
/// rolls back due to a failure (e.g. type error).
#[tokio::test]
async fn test_integration_create_service_rollback_cleans_ast() {
    let mut interner = Interner::new();
    let mut manager = setup_test_manager(&mut interner).await;

    let bad_s_sym = manager.interner.insert("bad_service");
    let x_sym = manager.interner.insert("x");

    // Construct a declaration that adds an Int and a Bool, which will fail type checking/eval
    let bad_decls = vec![meerkat_lib::runtime::ast::Decl::VarDecl {
        name: x_sym,
        ty: None,
        val: meerkat_lib::runtime::ast::Expr::Binop {
            op: meerkat_lib::runtime::ast::BinOp::Add,
            expr1: Box::new(meerkat_lib::runtime::ast::Expr::Literal {
                val: Value::Int { val: 42 },
            }),
            expr2: Box::new(meerkat_lib::runtime::ast::Expr::Literal {
                val: Value::Bool { val: false },
            }),
        },
    }];

    let create_result = manager.create_service(bad_s_sym, bad_decls).await;
    assert!(create_result.is_err(), "Service creation should fail");

    assert!(
        !manager.unified_ast().iter().any(|s| {
            if let meerkat_lib::runtime::ast::Stmt::Service { name, .. } = s {
                *name == bad_s_sym
            } else {
                false
            }
        }),
        "Phantom service must not remain in unified_ast after integration rollback"
    );
}

// Hot updates publish after successful recomputation when the value/clock pair
// changes or a source-change notification is pending.
async fn listening_node() -> (NetworkActor, Address) {
    let mut network = NetworkActor::new(NodeType::Server).await.unwrap();
    let NetworkReply::ListenSuccess { addr } = network
        .handle_command(NetworkCommand::Listen {
            addr: Address::new("/ip4/127.0.0.1/tcp/0"),
        })
        .await
    else {
        panic!("network must listen");
    };
    let address = Address::new(format!("{}/p2p/{}", addr.0, network.local_peer_id()));
    (network, address)
}

async fn setup_gate_test_manager() -> (Manager, NetworkActor, Address) {
    let (network, _) = listening_node().await;
    let (peer, address) = listening_node().await;
    let mut interner = Interner::new();
    let ast = parse_string(
        "service remote {
            var input = 20;
            pub def current = input;
        }
        service local {
            var x = 1;
            var y = 1;
            pub def result = x + 1;
            pub def downstream = result * 2;
        }",
        &mut interner,
    )
    .unwrap();
    let local = interner.insert("local");
    let local_ast: Vec<_> = ast
        .iter()
        .filter(|stmt| matches!(stmt, Stmt::Service { name, .. } if *name == local))
        .cloned()
        .collect();
    let mut node = Node::new();
    node.interner = interner;
    node.unified_ast = ast;
    node.static_checks().unwrap();
    let manager = node
        .on_manager_startup(
            true,
            Some(network),
            HashMap::from([("remote".to_string(), format!("{}/remote", address.0))]),
            &local_ast,
        )
        .await
        .unwrap();
    (manager, peer, address)
}

async fn update_service_code(manager: &mut Manager, code: &str) {
    let ast = parse_string(code, &mut manager.interner).unwrap();
    Transaction::new(ast).poll(manager).await.unwrap();
}

async fn subscribe(manager: &mut Manager, address: &Address) {
    let local = manager.interner.insert("local");
    let result = manager.interner.insert("result");
    let sink = manager.interner.insert("sink");
    manager
        .handle_request_updates(
            local,
            result,
            ServiceNetId("observer".to_string()),
            sink,
            address.0.clone(),
        )
        .await;
}

async fn next_message(peer: &mut NetworkActor) -> MeerkatMessage {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match peer.event_rx.recv().await.expect("network remains open") {
                NetworkEvent::MessageReceived { msg, .. } => return msg,
                NetworkEvent::SendFailed { error, .. } => panic!("send failed: {error:?}"),
                _ => {}
            }
        }
    })
    .await
    .expect("expected message must arrive")
}

async fn assert_update(
    peer: &mut NetworkActor,
    expected_value: i32,
    expected_clock: &[(&str, &str, u64)],
) {
    loop {
        match next_message(peer).await {
            MeerkatMessage::Ping { .. } => continue,
            MeerkatMessage::Update {
                listener_service,
                listener_def,
                source_service,
                member,
                value,
                clock,
                ..
            } => {
                assert_eq!(listener_service, "observer");
                assert_eq!(listener_def, "sink");
                assert_eq!(source_service, "local");
                assert_eq!(member, "result");
                assert!(matches!(value, NetValue::Int { val } if val == expected_value));
                let actual: HashMap<_, _> = clock
                    .into_iter()
                    .map(|entry| ((entry.service, entry.member), entry.counter))
                    .collect();
                let expected = expected_clock
                    .iter()
                    .map(|&(service, member, counter)| {
                        ((service.to_string(), member.to_string()), counter)
                    })
                    .collect();
                assert_eq!(actual, expected);
                return;
            }
            other => panic!("expected result Update, got {other:?}"),
        }
    }
}

// Message IDs are assigned synchronously when a send is submitted. Markers
// count sends without depending on packet order or a no-message timeout.
async fn send_marker(manager: &mut Manager, address: &Address) -> u64 {
    match manager
        .network
        .as_mut()
        .unwrap()
        .handle_command(NetworkCommand::SendMessage {
            addr: address.clone(),
            msg: MeerkatMessage::Ping {
                content: "send marker".to_string(),
            },
        })
        .await
    {
        NetworkReply::MessageSent { msg_id } => msg_id.0,
        other => panic!("marker send failed: {other:?}"),
    }
}

#[tokio::test]
async fn test_update_waits_for_missing_input_without_publishing() {
    let (mut manager, mut peer, address) = setup_gate_test_manager().await;
    subscribe(&mut manager, &address).await;
    assert_update(&mut peer, 2, &[]).await;
    let [local, result, downstream, remote, current, input] = [
        "local",
        "result",
        "downstream",
        "remote",
        "current",
        "input",
    ]
    .map(|name| manager.interner.insert(name));
    let before = send_marker(&mut manager, &address).await;

    update_service_code(
        &mut manager,
        "update local { pub def result = remote.current + 1; }",
    )
    .await;

    let after = send_marker(&mut manager, &address).await;
    // The only operation send is the subscription to the new remote input.
    // The second marker itself consumes one additional ID.
    assert_eq!(
        after - before,
        2,
        "blocked hot update must not publish its old result"
    );
    assert_eq!(
        manager.services[&local].vars[&result].value,
        Value::Int { val: 2 }
    );
    assert!(manager.services[&local].vars[&result]
        .vector_clock
        .is_empty());
    assert_eq!(
        manager.services[&local].vars[&downstream].value,
        Value::Int { val: 4 }
    );
    assert!(manager.services[&local].vars[&downstream]
        .vector_clock
        .is_empty());
    assert!(!manager.services[&local]
        .dep_cache
        .get(&result)
        .and_then(|cache| cache.get(&(remote, current))?.newest())
        .is_some_and(|entry| entry.1.is_some()));

    let (listener_service, listener_def, reply_to) = loop {
        match next_message(&mut peer).await {
            MeerkatMessage::Ping { .. } => continue,
            MeerkatMessage::RequestUpdates {
                service,
                member,
                listener_service,
                listener_def,
                reply_to,
                ..
            } => {
                assert_eq!(service, "remote");
                assert_eq!(member, "current");
                break (listener_service, listener_def, reply_to);
            }
            other => panic!("expected subscription, got {other:?}"),
        }
    };
    let reply = peer
        .handle_command(NetworkCommand::SendMessage {
            addr: Address::new(reply_to),
            msg: MeerkatMessage::Update {
                listener_service,
                listener_def,
                source_service: "remote".to_string(),
                member: "current".to_string(),
                value: NetValue::Int { val: 20 },
                clock: vec![ClockEntry {
                    service: "remote".to_string(),
                    member: "input".to_string(),
                    counter: 1,
                }],
                sources: Some(vec![("remote".into(), "input".into())]),
            },
        })
        .await;
    assert!(matches!(reply, NetworkReply::MessageSent { .. }));
    tokio::time::timeout(Duration::from_secs(10), async {
        while !manager.services[&local]
            .dep_cache
            .get(&result)
            .and_then(|cache| cache.get(&(remote, current))?.newest())
            .is_some_and(|entry| entry.1.is_some())
        {
            manager.dispatch_network_events().await;
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("initial input must resume recomputation");

    let clock = HashMap::from([((remote, input), 1)]);
    assert_eq!(
        manager.services[&local].dep_cache[&result][&(remote, current)]
            .retained
            .clone()
            .unwrap(),
        (
            Value::Int { val: 20 },
            Some(clock.clone()),
            Some(std::collections::HashSet::from([(remote, input)]))
        )
    );
    assert_eq!(
        manager.services[&local].vars[&result].value,
        Value::Int { val: 21 }
    );
    assert_eq!(manager.services[&local].vars[&result].vector_clock, clock);
    assert_eq!(
        manager.services[&local].vars[&downstream].value,
        Value::Int { val: 42 }
    );
    assert_eq!(
        manager.services[&local].vars[&downstream].vector_clock,
        clock
    );
    assert_update(&mut peer, 21, &[("remote", "input", 1)]).await;
}

/// Send `msg` from `from` to the node at `to`
async fn send(from: &mut NetworkActor, to: &Address, msg: MeerkatMessage) {
    let sent = from
        .handle_command(NetworkCommand::SendMessage {
            addr: to.clone(),
            msg,
        })
        .await;
    assert!(matches!(sent, NetworkReply::MessageSent { .. }));
}

/// A request for the code update `decls` of `service`, answered at `reply_to`
fn update_request(
    request_id: u64,
    service: &str,
    decls: &str,
    reply_to: &Address,
) -> MeerkatMessage {
    MeerkatMessage::UpdateServiceRequest {
        request_id,
        txn_id: None,
        service_name: service.to_string(),
        source: format!("update {service} {{ {decls} }}"),
        reply_to: reply_to.0.clone(),
    }
}

/// Send `node` a code update that adds `def` to `local`. The def reads
/// `remote.current`, so it has no first value, and the update no reply, until
/// the owner of `remote` answers the def's subscription
async fn request_update(
    from: &mut NetworkActor,
    reply_to: &Address,
    node: &Address,
    request_id: u64,
    def: &str,
) {
    let decls = format!("pub def {def} = remote.current;");
    send(
        from,
        node,
        update_request(request_id, "local", &decls, reply_to),
    )
    .await;
}

/// Serve what `manager` receives until `peer` gets its next message
async fn serve_until_received(manager: &mut Manager, peer: &mut NetworkActor) -> MeerkatMessage {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            manager.dispatch_network_events().await;
            match peer.event_rx.try_recv() {
                Ok(NetworkEvent::MessageReceived { msg, .. }) => return msg,
                Ok(NetworkEvent::SendFailed { error, .. }) => panic!("send failed: {error:?}"),
                _ => tokio::task::yield_now().await,
            }
        }
    })
    .await
    .expect("expected message must arrive")
}

/// Whether `msg` subscribes `def` to its input, as applying the update that
/// adds the def does
fn subscribes(msg: &MeerkatMessage, def: &str) -> bool {
    matches!(msg, MeerkatMessage::RequestUpdates { listener_def, .. } if listener_def == def)
}

/// Whether `msg` refuses the update request `id` because of its peer's limit
fn refuses(msg: &MeerkatMessage, id: u64) -> bool {
    matches!(
        msg,
        MeerkatMessage::UpdateServiceResponse { request_id, error: Some(error) }
            if *request_id == id && error == "Too many pending updates from this peer"
    )
}

/// A node holds a code update until its new defs have first values, and only
/// `MAX_PENDING_UPDATES_PER_PEER` of them for one peer: the peer's next update
/// is refused, another peer's is not, and one answered makes room for another
#[tokio::test]
async fn test_update_requests_held_for_first_values_are_limited_per_peer() {
    // `peer` owns `remote`: an applied update sends it the subscription of
    // the def it adds, which stays unanswered
    let (mut manager, mut peer, address) = setup_gate_test_manager().await;
    let (mut other, other_address) = listening_node().await;
    let node = Address::new(manager.local_reply_addr().await);

    request_update(&mut peer, &address, &node, 0, "held0").await;
    let MeerkatMessage::RequestUpdates {
        listener_service,
        listener_def,
        reply_to,
        ..
    } = serve_until_received(&mut manager, &mut peer).await
    else {
        panic!("an applied update subscribes its new def");
    };
    for id in 1..MAX_PENDING_UPDATES_PER_PEER as u64 {
        let def = format!("held{id}");
        request_update(&mut peer, &address, &node, id, &def).await;
        let applied = serve_until_received(&mut manager, &mut peer).await;
        assert!(subscribes(&applied, &def), "{applied:?}");
    }

    request_update(&mut peer, &address, &node, 100, "refused").await;
    let refused = serve_until_received(&mut manager, &mut peer).await;
    assert!(
        refuses(&refused, 100),
        "the peer has as many updates held as a node holds for one: {refused:?}"
    );

    request_update(&mut other, &other_address, &node, 101, "others").await;
    let applied = serve_until_received(&mut manager, &mut peer).await;
    assert!(
        subscribes(&applied, "others"),
        "another peer's update is applied all the same: {applied:?}"
    );

    // The first def gets its input, so its update is answered
    let input = MeerkatMessage::Update {
        listener_service,
        listener_def,
        source_service: "remote".to_string(),
        member: "current".to_string(),
        value: NetValue::Int { val: 20 },
        clock: vec![ClockEntry {
            service: "remote".to_string(),
            member: "input".to_string(),
            counter: 1,
        }],
        sources: Some(vec![("remote".into(), "input".into())]),
    };
    send(&mut peer, &Address::new(reply_to), input).await;
    let answered = serve_until_received(&mut manager, &mut peer).await;
    assert!(
        matches!(
            answered,
            MeerkatMessage::UpdateServiceResponse {
                request_id: 0,
                error: None
            }
        ),
        "an update is answered once its def has a value: {answered:?}"
    );
    request_update(&mut peer, &address, &node, 102, "again").await;
    let applied = serve_until_received(&mut manager, &mut peer).await;
    assert!(
        subscribes(&applied, "again"),
        "with one answered, the peer's next update is applied: {applied:?}"
    );
}

/// What `select` makes of the first message `peer` gets that it takes
async fn received<T>(peer: &mut NetworkActor, select: impl Fn(MeerkatMessage) -> Option<T>) -> T {
    loop {
        if let Some(selected) = select(next_message(peer).await) {
            return selected;
        }
    }
}

/// An update a node is still applying counts toward its peer's limit. While
/// the node waits for the update's remote participant to commit it serves
/// other requests, and the peer's next update is refused before it has any
/// effect
#[tokio::test]
async fn test_update_being_applied_counts_toward_the_peer_limit() {
    let (mut manager, mut peer, address) = setup_gate_test_manager().await;
    let node = Address::new(manager.local_reply_addr().await);
    // A second service, which the locks of an update to `local` do not cover
    let spare = parse_string("service spare { var z = 0; }", &mut manager.interner).unwrap();
    manager.run_declaration(&spare[0]).await.unwrap();
    let [local, spare, held, late] =
        ["local", "spare", "held", "late"].map(|name| manager.interner.insert(name));

    // The peer is one update short of the limit
    for id in 1..MAX_PENDING_UPDATES_PER_PEER as u64 {
        let def = format!("held{id}");
        request_update(&mut peer, &address, &node, id, &def).await;
        let applied = serve_until_received(&mut manager, &mut peer).await;
        assert!(subscribes(&applied, &def), "{applied:?}");
    }

    // One more reads `remote.current` in an initializer, under the update's
    // transaction. That makes `peer` a participant: once the update is
    // installed the node asks it to commit, and serves other requests until
    // it answers. The peer uses that wait to ask for another update
    let decls = "var seed = remote.current; pub def held = remote.current;";
    let update = update_request(100, "local", decls, &address);
    send(&mut peer, &node, update).await;
    let participant = async {
        let (request_id, reply_to) = received(&mut peer, |msg| match msg {
            MeerkatMessage::LookupRequest {
                request_id,
                reply_to,
                ..
            } => Some((request_id, reply_to)),
            _ => None,
        })
        .await;
        let value = MeerkatMessage::LookupResponse {
            request_id,
            value: NetValue::Int { val: 20 },
            clock: Some(Vec::new()),
            sources: Some(vec![("remote".into(), "input".into())]),
        };
        send(&mut peer, &Address::new(reply_to), value).await;
        let (request_id, reply_to) = received(&mut peer, |msg| match msg {
            MeerkatMessage::Commit {
                request_id,
                reply_to,
                ..
            } => Some((request_id, reply_to)),
            _ => None,
        })
        .await;
        let another = update_request(101, "spare", "pub def late = remote.current;", &address);
        send(&mut peer, &node, another).await;
        // Applied, it subscribes its def; refused, it is answered
        let outcome = received(&mut peer, |msg| {
            let answered = matches!(
                msg,
                MeerkatMessage::UpdateServiceResponse {
                    request_id: 101,
                    ..
                }
            );
            (answered || subscribes(&msg, "late")).then_some(msg)
        })
        .await;
        let committed = MeerkatMessage::CommitResponse {
            request_id,
            success: true,
            error: None,
        };
        send(&mut peer, &Address::new(reply_to), committed).await;
        outcome
    };
    let serve = async {
        while !manager.services[&local].defs.contains_key(&held) {
            manager.dispatch_network_events().await;
            tokio::task::yield_now().await;
        }
    };
    let exchange = async { tokio::join!(participant, serve) };
    let (outcome, ()) = tokio::time::timeout(Duration::from_secs(10), exchange)
        .await
        .expect("the exchange must finish");

    assert!(
        refuses(&outcome, 101),
        "with the update being applied, the peer is at the limit: {outcome:?}"
    );
    assert!(!manager.services[&spare].defs.contains_key(&late));
}

#[tokio::test]
async fn test_update_propagates_unchanged_value_with_new_clock() {
    let (mut manager, mut peer, address) = setup_gate_test_manager().await;
    subscribe(&mut manager, &address).await;
    assert_update(&mut peer, 2, &[]).await;
    let [local, result, downstream, y] =
        ["local", "result", "downstream", "y"].map(|name| manager.interner.insert(name));
    manager
        .execute_action(
            local,
            &[ActionStmt::Assign {
                name: y,
                expr: Expr::Literal {
                    val: Value::Int { val: 1 },
                },
            }],
        )
        .await
        .unwrap();
    let clock = HashMap::from([((local, y), 1)]);
    assert_eq!(manager.services[&local].vars[&y].vector_clock, clock);
    assert!(manager.services[&local].vars[&result]
        .vector_clock
        .is_empty());
    let before = send_marker(&mut manager, &address).await;

    update_service_code(&mut manager, "update local { pub def result = y + 1; }").await;

    let after = send_marker(&mut manager, &address).await;
    assert_eq!(
        after - before,
        2,
        "clock-only change must publish one result Update"
    );
    assert_eq!(
        manager.services[&local].vars[&result].value,
        Value::Int { val: 2 }
    );
    assert_eq!(manager.services[&local].vars[&result].vector_clock, clock);
    assert_eq!(
        manager.services[&local].vars[&downstream].value,
        Value::Int { val: 4 }
    );
    assert_eq!(
        manager.services[&local].vars[&downstream].vector_clock,
        clock
    );
    assert_update(&mut peer, 2, &[("local", "y", 1)]).await;
}

#[tokio::test]
async fn test_update_unchanged_result_still_initializes_subscription() {
    let (mut manager, mut peer, address) = setup_gate_test_manager().await;
    let local = manager.interner.insert("local");
    let result = manager.interner.insert("result");

    update_service_code(&mut manager, "update local { pub def result = x + 1; }").await;

    assert_eq!(
        manager.services[&local].vars[&result].value,
        Value::Int { val: 2 }
    );
    assert!(manager.services[&local].vars[&result]
        .vector_clock
        .is_empty());
    let before = send_marker(&mut manager, &address).await;
    subscribe(&mut manager, &address).await;
    let after = send_marker(&mut manager, &address).await;
    assert_eq!(
        after - before,
        2,
        "new subscriber must receive the unchanged current pair"
    );
    assert_update(&mut peer, 2, &[]).await;
}
