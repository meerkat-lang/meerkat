//! Integration tests for dependency analysis and cycle checking
//!
//! This module specifies the comprehensive static analysis behavior for
//! intra- and inter-service dependency resolution, higher-order
//! functions, immediately invoked function expressions, closures as
//! arguments, action blocks, mutually recursive thunks, and
//! cross-service transitive pipelines

use meerkat_lib::runtime::{node::Node, parser::parse_string};

/// Parse and perform static analysis checks on a source input string
///
/// Args:
///     input (`&str`): Source string of the program to test
///
/// Returns:
///     `Result<(), String>`: `Ok(())` if static checks pass, or `Err` with
///     the stringified error message
fn check_program(input: &str) -> Result<(), String> {
    debug_assert!(!input.trim().is_empty(), "input string must not be empty");
    let mut node = Node::new();
    let prog = parse_string(input, &mut node.interner)
        .map_err(|e| format!("Parse error: {}", e))
        .expect("Test program input must be syntactically valid.");
    let res = node.run_static_checks(&prog).map_err(|e| e.to_string());
    debug_assert!(
        res.is_ok() || res.is_err(),
        "Result must be a valid Result variant"
    );
    res
}

/// Verify that a direct intra-service `var` cycle is rejected
#[test]
fn test_intra_var_cycle_err() {
    let input = "
        service A {
            var a = b;
            var b = a;
        }
    ";
    let res = check_program(input);
    assert!(res.is_err());
    let err = res.expect_err("expected dependency cycle error");
    println!("ERROR: {}", err);
    assert!(err.contains("dependency cycle detected"));
}

/// Verify that a direct intra-service `def` cycle is rejected
#[test]
fn test_intra_def_cycle_err() {
    let input = "
        service A {
            pub def a = b;
            pub def b = a;
        }
    ";
    let res = check_program(input);
    assert!(res.is_err());
    let err = res.expect_err("expected dependency cycle error");
    println!("ERROR: {}", err);
    assert!(err.contains("dependency cycle detected"));
}

/// Verify valid intra-service linear dependencies resolve
#[test]
fn test_intra_cycle_ok() {
    let input = "
        service A {
            var a = 1;
            pub def b = a;
            pub def c = b + 1;
        }
    ";
    assert!(check_program(input).is_ok());
}

/// Verify that a 2-service member cycle is rejected
#[test]
fn test_inter_cycle_2_service_err() {
    let input = "
        service s1 {
            pub def a = s2.b;
        }
        service s2 {
            pub def b = s1.a;
        }
    ";
    let res = check_program(input);
    assert!(res.is_err());
    let err = res.expect_err("expected dependency cycle error");
    println!("ERROR: {}", err);
    assert!(err.contains("dependency cycle detected"));
}

/// Verify that a 3-service transitive member cycle is rejected
#[test]
fn test_inter_cycle_3_service_err() {
    let input = "
        service s1 {
            pub def a = s2.b;
        }
        service s2 {
            pub def b = s3.c;
        }
        service s3 {
            pub def c = s1.a;
        }
    ";
    let res = check_program(input);
    assert!(res.is_err());
    let err = res.expect_err("expected dependency cycle error");
    println!("ERROR: {}", err);
    assert!(err.contains("dependency cycle detected"));
}

/// Verify that a 4-service cycle matching `test_cycle.mkt` is rejected
#[test]
fn test_inter_cycle_4_service_err() {
    let input = "
        service s1 {
            pub def val1 = s2.val2;
        }
        service s3 {
            pub def val3 = s4.val4;
            var a = 1;
        }
        service s2 {
            pub def val2 = s3.val3;
        }
        service s4 {
            pub def val4 = s1.val1;
        }
    ";
    let res = check_program(input);
    assert!(res.is_err());
    let err = res.expect_err("expected dependency cycle error");
    println!("ERROR: {}", err);
    assert!(err.contains("dependency cycle detected"));
}

/// Verify valid inter-service dependencies resolve successfully
#[test]
fn test_inter_cycle_ok() {
    let input = "
        service s1 {
            pub def a = 1;
        }
        service s2 {
            pub def b = s1.a;
        }
    ";
    assert!(check_program(input).is_ok());
}

/// Verify `DAG` dependencies across 4 services in diamond topology
#[test]
fn test_inter_diamond_dag_ok() {
    let input = "
        service s1 {
            pub def root = 100;
        }
        service s2 {
            pub def branch_a = s1.root + 1;
        }
        service s3 {
            pub def branch_b = s1.root + 2;
        }
        service s4 {
            pub def result = s2.branch_a + s3.branch_b;
        }
    ";
    assert!(check_program(input).is_ok());
}

/// Verify suspended forward references in closures
#[test]
fn test_delayed_fwd_closure_ok() {
    let input = "
        service s {
            def f = fn () => a;
            var a = 0;
        }
    ";
    assert!(check_program(input).is_ok());
}

/// Verify nested suspended closures referencing uninitialized variables
#[test]
fn test_nested_lazy_closure_ok() {
    let input = "
        service s {
            def f = fn () => fn () => x + 1;
            var x = 5;
        }
    ";
    assert!(check_program(input).is_ok());
}

/// Verify closure invoked after variable declaration resolves
#[test]
fn test_closure_invoked_after_decl_ok() {
    let input = "
        service s {
            def f = fn () => x + 1;
            var x = 5;
            def y = f();
        }
    ";
    assert!(check_program(input).is_ok());
}

/// Verify that closure invoked before variable declaration is rejected
#[test]
fn test_closure_invoked_before_decl_rejected() {
    let input = "
        service s {
            def f = fn () => x + 1;
            def y = f();
            var x = 5;
        }
    ";
    let res = check_program(input);
    assert!(
        res.is_err(),
        "Static analysis MUST reject closure invoked before declaration"
    );
}

/// Verify that `IIFE` with forward reference is rejected
#[test]
fn test_iife_eager_forward_ref_rejected() {
    let input = "
        service s {
            def y = (fn () => x + 1) ();
            var x = 5;
        }
    ";
    let res = check_program(input);
    assert!(
        res.is_err(),
        "Static analysis MUST reject IIFE eager evaluation"
    );
}

/// Verify nested `IIFE` eagerly evaluating forward ref is rejected
#[test]
fn test_nested_iife_eager_forward_ref_rejected() {
    let input = "
        service s {
            def y = (fn () => (fn () => x + 1) ()) ();
            var x = 5;
        }
    ";
    let res = check_program(input);
    assert!(
        res.is_err(),
        "Static analysis MUST reject nested IIFE eager evaluation"
    );
}

/// Verify suspended forward references in `HOF` definitions
#[test]
fn test_delayed_fwd_hof_ok() {
    let input = "
        service s {
            var b = 0;
            var c = 0;
            def app: ((int) -> int) -> (int) -> int =
                fn (f: (int) -> int) => fn (x: int) => f(x + b);
            def d = app(fn (x: int) => x + a);
            var a = 0;
        }
    ";
    assert!(check_program(input).is_ok());
}

/// Verify eager read of uninitialized var in `HOF` is rejected
#[test]
fn test_hof_boundary_forward_ref_rejected() {
    let input = "
        service s {
            var b = 0;
            var c = 0;
            def app: ((int) -> int) -> (int) -> int =
                fn (f: (int) -> int) => fn (x: int) => f(x + b);
            def d = app(fn (x: int) => x + a)(c);
            var a = 0;
        }
    ";
    let res = check_program(input);
    assert!(
        res.is_err(),
        "Static analysis MUST reject eager read through HOF parameter"
    );
}

/// Verify invoking closure returning forward ref is rejected
#[test]
fn test_hof_returning_closure_eager_forward_ref_rejected() {
    let input = "
        service s {
            def make_adder = fn (x: int) => fn (y: int) => x + z;
            def add = make_adder(1)(2);
            var z = 10;
        }
    ";
    let res = check_program(input);
    assert!(
        res.is_err(),
        "Static analysis MUST reject eager invocation of closure"
    );
}

/// Verify invoking closure returning forward ref after decl resolves
#[test]
fn test_hof_returning_closure_after_decl_ok() {
    let input = "
        service s {
            def make_adder = fn (x: int) => fn (y: int) => x + z;
            var z = 10;
            def add = make_adder(1)(2);
        }
    ";
    assert!(check_program(input).is_ok());
}

/// Verify transitive `HOF` pipelines with forward refs are rejected
#[test]
fn test_hof_pipeline_transitive_eager_rejected() {
    let input = "
        service s {
            def pipe =
                fn (f: (int) -> int) =>
                fn (g: (int) -> int) =>
                fn (x: int) => g(f(x));
            def inc = fn (x: int) => x + a;
            def d = pipe(inc)(fn (y: int) => y * 2)(5);
            var a = 0;
        }
    ";
    let res = check_program(input);
    assert!(
        res.is_err(),
        "Static analysis MUST reject transitive HOF pipeline"
    );
}

/// Verify partial application of `HOF` without forward refs resolves
#[test]
fn test_hof_curried_partial_application_ok() {
    let input = "
        service s {
            def add = fn (x: int) => fn (y: int) => x + y;
            def add5 = add(5);
            def z = add5(10);
        }
    ";
    assert!(check_program(input).is_ok());
}

/// Verify valid self-recursive function in closure thunk is allowed
#[test]
fn test_self_recursive_function_allowed() {
    let input = "
        service s {
            def fact: (int) -> int =
                fn (n: int) => if n == 0 then 1 else n * fact(n - 1);
        }
    ";
    let res = check_program(input);
    assert!(
        res.is_ok(),
        "Static analysis MUST allow self-recursive function 'fact'"
    );
}

/// Verify valid mutually recursive function definitions are allowed
#[test]
fn test_mutually_recursive_functions_allowed() {
    let input = "
        service s {
            def even: (int) -> bool =
                fn (n: int) => if n == 0 then true else odd(n - 1);
            def odd: (int) -> bool =
                fn (n: int) => if n == 0 then false else even(n - 1);
        }
    ";
    let res = check_program(input);
    assert!(
        res.is_ok(),
        "Static analysis MUST allow mutually recursive functions"
    );
}

/// Verify suspended action closure with forward reference resolves
#[test]
fn test_action_closure_forward_ref_ok() {
    let input = "
        service s {
            def act_closure = fn () => action { x = x + 1; };
            def y = act_closure();
            var x = 5;
        }
    ";
    assert!(check_program(input).is_ok());
}

/// Verify cross-service member access in suspended closure resolves
#[test]
fn test_cross_service_member_closure_access_ok() {
    let input = "
        service s1 {
            pub def get_remote = fn () => s2.val;
        }
        service s2 {
            pub def val = 42;
        }
    ";
    assert!(check_program(input).is_ok());
}

/// Verify cross-service `HOF` eager evaluation of uninit field rejected
#[test]
fn test_cross_service_hof_forward_ref_rejected() {
    let input = "
        service s1 {
            def app = fn (f: (int) -> int) => f(10);
            def res = app(s2.get_uninit);
        }
        service s2 {
            pub def get_uninit = fn (x: int) => x + uninit_var;
            var uninit_var = 100;
        }
    ";
    let res = check_program(input);
    assert!(
        res.is_err(),
        "Static analysis MUST reject cross-service HOF eager execution"
    );
}

/// Verify cross-service pipeline across 3 services resolves
#[test]
fn test_cross_service_transitive_pipeline_ok() {
    let input = "
        service s1 {
            pub def base = 10;
        }
        service s2 {
            pub def step1 = s1.base * 2;
        }
        service s3 {
            pub def step2 = s2.step1 + 5;
        }
    ";
    assert!(check_program(input).is_ok());
}

/// Verify eager conditional evaluation with forward ref is rejected
#[test]
fn test_conditional_if_eager_branch_forward_ref_rejected() {
    let input = "
        service s {
            def x = if true then y else 0;
            var y = 5;
        }
    ";
    let res = check_program(input);
    assert!(
        res.is_err(),
        "Static analysis MUST reject eager conditional evaluation"
    );
}

/// Verify that mutual aliases passed as HOF arguments are properly rejected
/// as eager forward references, preventing infinite analysis loops.
#[test]
fn test_mutual_aliases_hof_argument() {
    let input = "
        service S {
            def hof: (int) -> int = fn(x: int) => x;
            def c: int = hof(a);
            def a: int = b;
            def b: int = a;
        }
    ";

    let (tx, rx) = std::sync::mpsc::channel();
    let input_owned = input.to_string();

    std::thread::spawn(move || {
        let res = check_program(&input_owned);
        let _ = tx.send(res);
    });

    let res = rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("Static analysis infinite loop or stack overflow detected!");

    assert!(res.is_err());
    let err = res.expect_err("expected dependency cycle error");
    assert!(
        err.contains("Invalid forward reference to uninitialized value")
            || err.contains("dependency cycle detected")
    );
}
