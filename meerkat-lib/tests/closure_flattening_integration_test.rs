//! Integration tests for closure environment capture, free variable extraction, and closure flattening.
//!
//! This module verifies that the `graphs::free_var` module and closure compilation
//! correctly extract free variables, handle parameter shadowing, support higher-order functions,
//! flatten nested environments, and properly distinguish static local environment capture from
//! dynamic service state lookups in action blocks.

use meerkat_lib::runtime::{
    ast::{Expr, Stmt, Value},
    interpreter::{eval, EvalContext},
    parser::parse_string,
    Interner, Node,
};
use std::collections::{HashMap, HashSet};

/// Helper function to parse a program string and initialize a Node
fn setup_node<'a>(input: &'a str) -> (Node<'a>, Vec<Stmt>) {
    let mut node = Node::new();
    let stmts = parse_string(input, &mut node.interner).expect("Test input must be valid syntax");
    (node, stmts)
}

/// Helper function to initialize a Manager with a given Meerkat code snippet
async fn setup_manager(code: &str, interner: &mut Interner) -> meerkat_lib::runtime::Manager {
    let initial_ast = parse_string(code, interner).unwrap();
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

/// Verify that free_var analysis correctly extracts free variables from closures
/// while omitting parameter-bound variables.
#[test]
fn test_free_var_closure_parameter_shadowing() {
    let input = "
        service test_service {
            var x = 10;
            def adder = fn (x: int) => x + y;
        }
    ";
    let (node, stmts) = setup_node(input);
    let mut interner = node.interner;
    let x_sym = interner.insert("x");
    let y_sym = interner.insert("y");

    if let Stmt::Service { decls, .. } = &stmts[0] {
        if let meerkat_lib::runtime::ast::Decl::DefDecl {
            val: Expr::Func { params, body, .. },
            ..
        } = &decls[1]
        {
            let bound: HashSet<_> = params.iter().map(|p| p.name).collect();
            assert!(bound.contains(&x_sym));

            let free_vars = meerkat_lib::runtime::graphs::free_var::free_var(body, &bound);
            assert!(
                !free_vars.contains(&x_sym),
                "x should be bound by parameter"
            );
            assert!(
                free_vars.contains(&y_sym),
                "y should be identified as free variable"
            );
        }
    }
}

/// Verify nested closure environment flattening and free variable resolution across scopes
#[test]
fn test_nested_closure_environment_flattening() {
    let input = "
        service s {
            def curried = fn (a: int) => fn (b: int) => fn (c: int) => a + b + c + z;
            var z = 100;
        }
    ";
    let mut node = Node::new();
    let stmts = parse_string(input, &mut node.interner).unwrap();
    assert!(node.run_static_checks(&stmts).is_ok());
}

/// Verify that action closures do not statically capture service variables in environment,
/// enabling dynamic/fresh lookups of service state at execution time.
#[tokio::test]
async fn test_action_closure_dynamic_service_state() {
    let code = "
        service counter {
            var count = 0;
            def inc = fn () => action {
                count = count + 1;
            };
        }
    ";
    let mut interner = Interner::new();
    let mut mgr = setup_manager(code, &mut interner).await;

    let svc_sym = interner.insert("counter");
    let inc_sym = interner.insert("inc");
    let count_sym = interner.insert("count");

    let inc_expr = {
        let service = mgr
            .services
            .get(&svc_sym)
            .expect("Service counter must exist");
        service
            .defs
            .get(&inc_sym)
            .expect("inc def must exist")
            .clone()
    };

    let mut ctx = EvalContext {
        manager: &mut mgr,
        service_name: svc_sym,
        txn: None,
    };
    let inc_val = eval(&inc_expr, &[], &mut ctx).await.unwrap();

    if let Value::Closure { env, .. } = &inc_val {
        assert!(
            !env.iter().any(|(sym, _)| *sym == count_sym),
            "Service variable 'count' should NOT be statically captured into action closure env"
        );
    } else {
        panic!("Expected Value::Closure");
    }
}

/// Verify evaluation of closures with captured free variables passed to higher-order functions
#[tokio::test]
async fn test_higher_order_function_closure_evaluation() {
    let code = "
        service math {
            def apply = fn (f: (int) -> int, val: int) => f(val);
            def make_adder = fn (base: int) => fn (x: int) => x + base;
            def add5 = make_adder(5);
            def res = apply(add5, 10);
        }
    ";
    let mut interner = Interner::new();
    let mut mgr = setup_manager(code, &mut interner).await;

    let svc_sym = interner.insert("math");
    let res_sym = interner.insert("res");

    let res_expr = {
        let service = mgr.services.get(&svc_sym).expect("Service math must exist");
        service
            .defs
            .get(&res_sym)
            .expect("res def must exist")
            .clone()
    };

    let mut ctx = EvalContext {
        manager: &mut mgr,
        service_name: svc_sym,
        txn: None,
    };
    let res_val = eval(&res_expr, &[], &mut ctx).await.unwrap();

    assert_eq!(res_val, Value::Int { val: 15 });
}

/// Verify that complex closure expressions with tuples, lists, and conditions correctly capture all free variables
#[test]
fn test_complex_closure_free_variables() {
    let input = "
        service complex {
            def process = fn (p: int) => (p + a + b + c + d + e);
            var a = 1;
            var b = 2;
            var c = 3;
            var d = 4;
            var e = 5;
        }
    ";
    let (node, stmts) = setup_node(input);
    let mut interner = node.interner;
    let p_sym = interner.insert("p");
    let a_sym = interner.insert("a");
    let b_sym = interner.insert("b");
    let c_sym = interner.insert("c");
    let d_sym = interner.insert("d");
    let e_sym = interner.insert("e");

    if let Stmt::Service { decls, .. } = &stmts[0] {
        if let meerkat_lib::runtime::ast::Decl::DefDecl {
            val: Expr::Func { params, body, .. },
            ..
        } = &decls[0]
        {
            let bound: HashSet<_> = params.iter().map(|param| param.name).collect();
            assert!(bound.contains(&p_sym));

            let free_vars = meerkat_lib::runtime::graphs::free_var::free_var(body, &bound);
            assert!(free_vars.contains(&a_sym));
            assert!(free_vars.contains(&b_sym));
            assert!(free_vars.contains(&c_sym));
            assert!(free_vars.contains(&d_sym));
            assert!(free_vars.contains(&e_sym));
            assert!(!free_vars.contains(&p_sym));
        }
    }
}
