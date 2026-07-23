//! Static type checking integration tests

use meerkat_lib::runtime::{node::Node, parser::parse_string};

/// Helper to parse and type check a string program
fn check_program(input: &str) -> Result<(), String> {
    let mut node = Node::new();
    let prog = parse_string(input, &mut node.interner)
        .map_err(|e| format!("Parse error: {}", e))
        .expect("Test program input must be syntactically valid.");
    node.run_static_checks(&prog).map_err(|e| e.to_string())
}

/// Verify that basic primitive types check correctly
#[test]
fn test_integration_primitive_types() {
    let input = "
        service employee_db {
            def base: int = 5000;
            def name: string = \"Alice Smith\";
            def active: bool = true;
            def get_salary: (int, bool) -> int =
                fn (bonus: int, manager: bool) =>
                    if active && manager then base + bonus * 2 else base;
        }
    ";
    let res = check_program(input);
    assert!(res.is_ok())
}

/// Verify that type mismatches on primitives are rejected
#[test]
fn test_integration_primitive_mismatch() {
    let input = "
        service employee_db {
            def base: int = 5000;
            def name: string = \"Alice Smith\";
            def bad_salary: (string) -> int =
                fn (bonus: string) => base + bonus;
        }
    ";
    let res = check_program(input);
    assert!(res.is_err());
    let err = res.expect_err("Type checking must fail.");
    assert!(err.contains("type check error"));
}

/// Verify annotated function parameters and return types
#[test]
fn test_integration_function_checking() {
    let input = "
        service calc_pipeline {
            def pipeline: ((int) -> int, (int) -> bool) -> (int) -> bool =
                fn (f: (int) -> int, g: (int) -> bool) =>
                    fn (x: int) => g(f(x));
        }
    ";
    let res = check_program(input);
    assert!(res.is_ok())
}

/// Verify incorrect function call argument types are rejected
#[test]
fn test_integration_function_call_mismatch() {
    let input = "
        service calc_pipeline {
            def f: (int) -> bool = fn (x: int) => x > 0;
            def bad_call: bool = f(\"not_an_int\");
        }
    ";
    let res = check_program(input);
    assert!(res.is_err())
}

/// Verify unannotated closures fail if params cannot be inferred
#[test]
fn test_integration_unannotated_closure_fails() {
    let input = "
        service inference_fail_service {
            def run = fn x => fn y => x + y;
        }
    ";
    let res = check_program(input);
    assert!(res.is_err())
}

/// Verify unannotated closures succeed with annotated bindings
#[test]
fn test_integration_unannotated_closure_with_annotated_binding() {
    let input = "
        service inference_success_service {
            def run: (int) -> (int) -> int =
                fn x => fn y => x + y;
        }
    ";
    let res = check_program(input);
    assert!(res.is_ok())
}

/// Verify list constructors check elements against inner type
#[test]
fn test_integration_list_checking() {
    let input = "
        service list_service {
            def matrix: int list list = [
                [1, 2, 3],
                [4 + 5, 6 / 2],
                [10 * 3]
            ];
        }
    ";
    let res = check_program(input);
    assert!(res.is_ok())
}

/// Verify list elements violating expected type are rejected
#[test]
fn test_integration_list_element_mismatch() {
    let input = "
        service list_service {
            def matrix: int list list = [
                [1, 2],
                [\"bad\", 4]
            ];
        }
    ";
    let res = check_program(input);
    assert!(res.is_err())
}

/// Verify that tuples are checked and element arity is validated
#[test]
fn test_integration_tuple_checking() {
    let input = "
        service tuple_service {
            def data: ((int, bool), string, unit) =
                {{42, true}, \"ok\", {}};
        }
    ";
    let res = check_program(input);
    assert!(res.is_ok())
}

/// Verify that tuple element type mismatch is rejected
#[test]
fn test_integration_tuple_element_mismatch() {
    let input = "
        service tuple_service {
            def data: ((int, bool), string, unit) =
                {{42, \"bad_bool\"}, \"ok\", {}};
        }
    ";
    let res = check_program(input);
    assert!(res.is_err())
}

/// Verify compound arithmetic binary operations
#[test]
fn test_integration_arithmetic_operators() {
    let input = "
        service calc {
            def complex_calc: (int, int) -> int =
                fn (x: int, y: int) =>
                    (x + y) * (x - y) / (x + 10);
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify addition with type mismatch is rejected
#[test]
fn test_integration_binop_add_mismatch() {
    let input = "
        service calc {
            def complex_add: (int) -> int =
                fn (x: int) => x + \"invalid\";
        }
    ";
    assert!(check_program(input).is_err())
}

/// Verify comparison operators produce boolean output
#[test]
fn test_integration_comparison_operators() {
    let input = "
        service calc {
            def check: (int, int) -> bool =
                fn (x: int, y: int) =>
                    (x < y) && (x > 0);
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify boolean equality check
#[test]
fn test_integration_binop_eq_bool() {
    let input = "
        service calc {
            def is_equal: bool = (true && false) == (false || true);
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify equality check mismatch is rejected
#[test]
fn test_integration_binop_eq_mismatch() {
    let input = "
        service logic {
            def check: (bool, int) -> bool =
                fn (a: bool, x: int) => a == x;
        }
    ";
    assert!(check_program(input).is_err())
}

/// Verify logical conjunction and disjunction operators
#[test]
fn test_integration_logic_operators() {
    let input = "
        service logic {
            def check: (bool, bool, bool) -> bool =
                fn (a: bool, b: bool, c: bool) =>
                    (a && b) || (b && c) || !a;
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify logical conjunction mismatch is rejected
#[test]
fn test_integration_binop_and_mismatch() {
    let input = "
        service logic {
            def check: (bool) -> bool =
                fn (a: bool) => a && 42;
        }
    ";
    assert!(check_program(input).is_err())
}

/// Verify logical negation checks as bool
#[test]
fn test_integration_unop_not() {
    let input = "
        service logic {
            def invert: (bool) -> bool =
                fn (a: bool) => !(!a && a);
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify logical negation mismatch is rejected
#[test]
fn test_integration_unop_not_mismatch() {
    let input = "
        service logic {
            def invert: (int) -> bool =
                fn (x: int) => !x;
        }
    ";
    assert!(check_program(input).is_err())
}

/// Verify arithmetic negation checks as int
#[test]
fn test_integration_unop_neg() {
    let input = "
        service calc {
            def neg: (int) -> int =
                fn (x: int) => -(-x + 10) * -5;
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify arithmetic negation mismatch is rejected
#[test]
fn test_integration_unop_neg_mismatch() {
    let input = "
        service calc {
            def neg: (string) -> int =
                fn (s: string) => -s;
        }
    ";
    assert!(check_program(input).is_err())
}

/// Verify empty tuple checks as unit
#[test]
fn test_integration_empty_tuple() {
    let input = "
        service tuple_service {
            def empty: () -> unit =
                fn () => {};
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify nesting of tuple types
#[test]
fn test_integration_nested_tuple() {
    let input = "
        service tuple_service {
            def transform: (((int, bool), string)) -> ((int, bool), string) =
                fn (x: ((int, bool), string)) => x;
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify checking of tertiary tuple
#[test]
fn test_integration_tuple_arity_three() {
    let input = "
        service tuple_service {
            def transform: ((int, bool, string)) -> (int, bool, string) =
                fn (x: (int, bool, string)) => x;
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify that mismatch in tuple arity is rejected
#[test]
fn test_integration_tuple_arity_mismatch_three() {
    let input = "
        service tuple_service {
            def transform: ((int, bool, string)) -> (int, bool) =
                fn (x: (int, bool, string)) => x;
        }
    ";
    assert!(check_program(input).is_err())
}

/// Verify nesting of list types
#[test]
fn test_integration_nested_lists() {
    let input = "
        service list_service {
            def check_list: (int list list list) -> int list list list =
                fn (xs: int list list list) => xs;
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify empty list without annotation fails inference
#[test]
fn test_integration_list_empty_inference_fail() {
    let input = "
        service list_service {
            def bad = fn x => [x, []];
        }
    ";
    assert!(check_program(input).is_err())
}

/// Verify empty list with annotation checks successfully
#[test]
fn test_integration_list_empty_annotation() {
    let input = "
        service list_service {
            def empty: (int list) -> int list =
                fn (xs: int list) => [];
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify nullary function call checks correctly
#[test]
fn test_integration_nullary_function_call() {
    let input = "
        service fun_service {
            def make_generator: (int) -> () -> int =
                fn (x: int) => fn () => x;
            def test_call: int = make_generator(42)();
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify nullary function call with arguments is rejected
#[test]
fn test_integration_nullary_function_call_with_args() {
    let input = "
        service fun_service {
            def make_generator: (int) -> () -> int =
                fn (x: int) => fn () => x;
            def test_call: int = make_generator(42)(10);
        }
    ";
    assert!(check_program(input).is_err())
}

/// Verify multi argument function call
#[test]
fn test_integration_multi_arg_function_call() {
    let input = "
        service fun_service {
            def process: (int, string, bool) -> ((int, string), bool) =
                fn (x: int, s: string, b: bool) => {{x, s}, b};
            def test_call: ((int, string), bool) =
                process(42, \"hello\", true);
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify multi argument function call argument count mismatch
#[test]
fn test_integration_multi_arg_function_call_mismatch() {
    let input = "
        service fun_service {
            def process: (int, string, bool) -> ((int, string), bool) =
                fn (x: int, s: string, b: bool) => {{x, s}, b};
            def test_call: ((int, string), bool) =
                process(42, \"hello\");
        }
    ";
    assert!(check_program(input).is_err())
}

/// Verify higher order function type checking
#[test]
fn test_integration_higher_order_function() {
    let input = "
        service fun_service {
            def apply_twice: ((int) -> int, int) -> int =
                fn (f: (int) -> int, x: int) => f(f(x));
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify valid conditional expressions
#[test]
fn test_integration_if_expr_valid() {
    let input = "
        service cond_service {
            def choose: (bool, int, int) -> int =
                fn (cond: bool, a: int, b: int) =>
                    (if (cond && (a > b)) then a + 10 else b - 10);
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify conditional expression with non boolean cond is rejected
#[test]
fn test_integration_if_expr_cond_mismatch() {
    let input = "
        service cond_service {
            def choose: (int, int, int) -> int =
                fn (cond: int, a: int, b: int) =>
                    (if (cond) then a else b);
        }
    ";
    assert!(check_program(input).is_err())
}

/// Verify conditional expression with mismatched branches is rejected
#[test]
fn test_integration_if_expr_branch_mismatch() {
    let input = "
        service cond_service {
            def choose: (bool, int, string) -> int =
                fn (cond: bool, a: int, b: string) =>
                    (if (cond) then a else b);
        }
    ";
    assert!(check_program(input).is_err())
}

/// Verify range expressions check as int list
#[test]
fn test_integration_range_expr_valid() {
    let input = "
        service range_service {
            def build_range: (int, int) -> int list =
                fn (start: int, end: int) => (start + 1)..(end - 1);
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify range expressions with non integer bounds are rejected
#[test]
fn test_integration_range_expr_mismatch() {
    let input = "
        service range_service {
            def build_range: (int, string) -> int list =
                fn (start: int, end: string) => start..end;
        }
    ";
    assert!(check_program(input).is_err())
}

/// Verify annotated action let statements inside tests
#[test]
fn test_integration_action_let_annotated() {
    let input = "
        service test_s {}
        @test(test_s) {
            let x: ((int, string) list) = [
                {1, \"first\"},
                {2, \"second\"}
            ];
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify unannotated action let statements inside tests
#[test]
fn test_integration_action_let_unannotated() {
    let input = "
        service test_s {}
        @test(test_s) {
            let x = [
                {1, \"first\"},
                {2, \"second\"}
            ];
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify valid mutable assignment in tests
#[test]
fn test_integration_action_assign_valid() {
    let input = "
        service test_s {
            var counter: int = 0;
        }
        @test(test_s) {
            counter = (counter + 1) * 10 / 2;
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify mutable assignment type mismatch in tests
#[test]
fn test_integration_action_assign_mismatch() {
    let input = "
        service test_s {
            var counter: int = 0;
        }
        @test(test_s) {
            counter = \"invalid\";
        }
    ";
    assert!(check_program(input).is_err())
}

/// Verify assert statements in test blocks
#[test]
fn test_integration_action_assert_valid() {
    let input = "
        service test_s {
            def check: (int) -> bool = fn (x: int) => x > 0;
        }
        @test(test_s) {
            assert(check(10) && !check(-5));
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify assert statements with non boolean expressions fail
#[test]
fn test_integration_action_assert_mismatch() {
    let input = "
        service test_s {
            def check: (int) -> int = fn (x: int) => x;
        }
        @test(test_s) {
            assert(check(10));
        }
    ";
    assert!(check_program(input).is_err())
}

/// Verify valid for loops in test blocks
#[test]
fn test_integration_action_for_valid() {
    let input = "
        service test_s {}
        @test(test_s) {
            let limits: int list = 0..10;
            for i in limits {
                let x: int = i * i;
            }
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify for loops over non iterables are rejected
#[test]
fn test_integration_action_for_mismatch() {
    let input = "
        service test_s {}
        @test(test_s) {
            for i in fn (x: int) => x {
                let x = i;
            }
        }
    ";
    assert!(check_program(input).is_err())
}

/// Verify member access across services
#[test]
fn test_integration_cross_service_access() {
    let input = "
        service db_service {
            def user: (int, string) = {42, \"Bob\"};
        }
        service api_service {
            def fetch_user: () -> (int, string) =
                fn () => db_service.user;
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify member access type mismatch is rejected
#[test]
fn test_integration_cross_service_access_mismatch() {
    let input = "
        service db_service {
            def user: (int, string) = {42, \"Bob\"};
        }
        service api_service {
            def fetch_user: () -> (bool, string) =
                fn () => db_service.user;
        }
    ";
    assert!(check_program(input).is_err())
}

/// Verify unbound member access across services is rejected
#[test]
fn test_integration_cross_service_access_unbound() {
    let input = "
        service db_service {}
        service api_service {
            def fetch_user: () -> (int, string) =
                fn () => db_service.user;
        }
    ";
    assert!(check_program(input).is_err())
}

/// Verify type checking of nested function signatures
#[test]
fn test_integration_nested_function_types() {
    let input = "
        service test_s {
            def f: (int) -> (bool) -> (string) -> (unit) -> int =
                fn (x: int) => fn (b: bool) =>
                    fn (s: string) => fn (u: unit) => x;
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify unannotated self referencing variables are rejected
#[test]
fn test_integration_self_referencing_def_fail() {
    let input = "
        service test_s {
            def x = fn () => x();
        }
    ";
    assert!(check_program(input).is_err())
}

/// Verify HTML literals are typed as string
#[test]
fn test_integration_html_literal() {
    let input = "
        service test_s {
            def html_val: string = (<div>The value is {10 + 20}</div>);
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify basic string operations type check
#[test]
fn test_integration_string_operations() {
    let input = "
        service test_s {
            def a: string = \"hello\";
            def b: string = a;
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify multiple services with independent scopes
#[test]
fn test_integration_multiple_services_independent() {
    let input = "
        service s1 {
            def f: (int) -> int = fn (x: int) => x;
        }
        service s2 {
            def f: (string) -> string = fn (s: string) => s;
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify empty service definition
#[test]
fn test_integration_empty_service() {
    let input = "
        service test_s {}
    ";
    assert!(check_program(input).is_ok())
}

/// Verify type inference on unannotated definitions
#[test]
fn test_integration_var_decl_type_inferred() {
    let input = "
        service test_s {
            def x = {42, \"hello\"};
            def y: (int, string) = x;
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify let scoping rules within test blocks
#[test]
fn test_integration_multiple_lets_scoping() {
    let input = "
        service test_s {}
        @test(test_s) {
            let a: (int) -> int = fn (x: int) => x + 1;
            let b: (int) -> int = fn (y: int) => a(y) * 2;
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify shadowing of let variables in nested blocks
#[test]
fn test_integration_shadowing_let_valid() {
    let input = "
        service test_s {}
        @test(test_s) {
            let x: int = 10;
            for i in 0..2 {
                let x: (string, bool) = {\"shadowed\", true};
                let y: (string, bool) = x;
            }
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify unannotated def definitions infer types
#[test]
fn test_integration_unannotated_def() {
    let input = "
        service test_s {
            def x = 42;
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify annotated def definitions check types
#[test]
fn test_integration_annotated_def() {
    let input = "
        service test_s {
            def x: int = 42;
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify recursive lambda via mutable service variable
#[test]
fn test_integration_recursive_lambda_via_mutable_var() {
    let input = "
        service test_s {
            var f: (int) -> int = fn (x: int) => x;
        }
        @test(test_s) {
            f = fn (x: int) =>
                (if (x == 0) then 1 else f(x - 1));
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify deeply nested lambda variable capturing and scoping
#[test]
fn test_integration_nested_lambdas_scoping() {
    let input = "
        service nested_lambda_service {
            def run: (int) -> (int) -> (int) -> int =
                fn (x: int) => fn (y: int) =>
                    fn (z: int) => x + y + z;
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify annotated immediately invoked function expressions
#[test]
fn test_integration_iifa_annotated() {
    let input = "
        service iifa_service {
            def val: int = (fn (x: int) => x + 10)(32);
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify unannotated immediately invoked function expressions fail
#[test]
fn test_integration_iifa_unannotated_fails() {
    let input = "
        service iifa_service {
            def val: int = (fn x => x + 10)(32);
        }
    ";
    assert!(check_program(input).is_err())
}

/// Verify action expressions without type annotations
#[test]
fn test_integration_action_expression_unannotated() {
    let input = "
        service action_service {
            def run = fn () => action {
                let x = 10;
            };
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify unannotated action closures
#[test]
fn test_integration_action_closure_unannotated() {
    let input = "
        service action_service {
            var state: int = 0;
            def update_state: (int) -> unit =
                fn x => action {
                    state = x;
                };
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify chains of indirect service member references
#[test]
fn test_integration_indirect_member_reference() {
    let input = "
        service s1 {
            def val: int = 42;
        }
        service s2 {
            def get: () -> int = fn () => s1.val;
        }
        service s3 {
            def get: () -> int = fn () => s2.get();
        }
    ";
    assert!(check_program(input).is_ok())
}

/// Verify that a well-typed watch statement passes checks
#[test]
fn test_integration_watch_well_typed() {
    let input = "
        watch 1 + 2;
    ";
    assert!(check_program(input).is_ok())
}

/// Verify that an ill-typed watch statement is rejected
#[test]
fn test_integration_watch_ill_typed() {
    let input = "
        watch 1 + \"hello\";
    ";
    assert!(check_program(input).is_err())
}

/// Verify that member type inference across imported service boundaries
/// correctly infers types without requiring annotations
#[test]
fn test_cross_service_member_type_inference() {
    let input = "
        import s1
        service s1 {
            pub def val = 42;
        }
        service s2 {
            pub def get_val = fn () => s1.val + 1;
        }
    ";
    assert!(check_program(input).is_ok());
}

/// Verify that type mismatches referencing imported service members
/// are rejected statically
#[test]
fn test_cross_service_type_mismatch_errors() {
    let input = "
        import s1
        service s1 {
            pub def text = \"hello\";
        }
        service s2 {
            pub def num: int = s1.text;
        }
    ";
    assert!(check_program(input).is_err());
}

/// Verify that mutual cyclic member access across imported services
/// produces a static type error (`DependencyCycle`)
#[test]
fn test_cross_service_member_cycle_errors() {
    let input = "
        import s1
        import s2
        service s1 {
            pub def a = s2.b;
        }
        service s2 {
            pub def b = s1.a;
        }
    ";
    assert!(check_program(input).is_err());
}

/// Verify that @test blocks for imported services strictly type check
/// assertions against inferred member types
#[test]
fn test_test_block_type_checks_imported_member() {
    let input = "
        import s1
        service s1 {
            pub def count = 10;
        }
        @test(s1) {
            assert(count == \"not_an_int\");
        }
    ";
    assert!(check_program(input).is_err());
}
