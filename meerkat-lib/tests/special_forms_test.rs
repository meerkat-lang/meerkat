//! The special forms of def, `def f(params) = e;` and `action a(params) { .. }`
//! (issue #20, design note `docs/design/execution-location.md`)
//!
//! A special form is the `fn` member plus a mark (`DefForm`), so these tests
//! pin down what the mark adds: the syntax, the forwarded text of a code
//! update, the rule that a special form can only be called, and the rule that
//! it computes what its `fn` spelling computes.

use meerkat_lib::runtime::ast::{ActionStmt, Decl, DefForm, Expr, Stmt};
use meerkat_lib::runtime::parser::parse_string;
use meerkat_lib::runtime::tt::Type;
use meerkat_lib::runtime::update::{format_update_source, Transaction};
use meerkat_lib::runtime::{Interner, Manager, Node};
use std::collections::HashMap;

/// Parse `src`, which must be valid
fn parse(src: &str) -> (Interner, Vec<Stmt>) {
    let mut interner = Interner::new();
    let stmts = parse_string(src, &mut interner).expect("valid syntax");
    (interner, stmts)
}

/// The declarations of the `n`th statement, a service or an update
fn decls_of(stmts: &[Stmt], n: usize) -> &[Decl] {
    match &stmts[n] {
        Stmt::Service { decls, .. } | Stmt::Update { decls, .. } => decls,
        other => panic!("expected a service or update, got {:?}", other),
    }
}

/// The parts of a def declaration
fn def_parts(decl: &Decl) -> (&Expr, bool, DefForm) {
    match decl {
        Decl::DefDecl {
            val,
            is_pub,
            form,
            ty: None,
            ..
        } => (val, *is_pub, *form),
        other => panic!("expected a def, got {:?}", other),
    }
}

/// The parameter count and result type of a function expression
fn func_shape(val: &Expr) -> (usize, Option<Type>) {
    match val {
        Expr::Func {
            params, return_ty, ..
        } => (params.len(), return_ty.clone()),
        other => panic!("expected a function, got {:?}", other),
    }
}

/// Run the CLI's static checks on `src`; the node on success, the message on
/// failure
fn check(src: &str) -> Result<Node, String> {
    let (interner, stmts) = parse(src);
    let mut node = Node::new();
    node.interner = interner;
    node.unified_ast = stmts;
    match node.static_checks() {
        Ok(()) => Ok(node),
        Err(e) => Err(format!("{:?}", e)),
    }
}

/// Assert that the static checks reject `src` with a message containing `text`
fn assert_rejected(src: &str, text: &str) {
    match check(src) {
        Ok(_) => panic!("expected the static checks to reject:\n{}", src),
        Err(msg) => assert!(
            msg.contains(text),
            "expected {:?} in the error, got {:?}",
            text,
            msg
        ),
    }
}

/// Assert that the static checks accept `src`
fn assert_accepted(src: &str) {
    if let Err(msg) = check(src) {
        panic!("expected the static checks to accept:\n{}\n{}", src, msg);
    }
}

// ----- parser -----

/// Every def form parses, with the mark, `pub`, the parameters and the result
/// type it was written with
#[test]
fn test_parse_def_forms() {
    let (_, stmts) = parse(
        "service s1 {
            var y = 5;
            def f(x: int) = x + y;
            pub def g(x: int, z: int): int = x * z + y;
            def now() = y;
            def t(x: int, z: int,) = x;
            def plain = y;
        }",
    );
    let decls = decls_of(&stmts, 0);

    let (val, is_pub, form) = def_parts(&decls[1]);
    assert_eq!((is_pub, form), (false, DefForm::Function));
    assert_eq!(func_shape(val), (1, None));

    let (val, is_pub, form) = def_parts(&decls[2]);
    assert_eq!((is_pub, form), (true, DefForm::Function));
    assert_eq!(func_shape(val), (2, Some(Type::Int)));

    let (val, _, form) = def_parts(&decls[3]);
    assert_eq!(form, DefForm::Function);
    assert_eq!(func_shape(val), (0, None));

    let (val, _, form) = def_parts(&decls[4]);
    assert_eq!(form, DefForm::Function);
    assert_eq!(func_shape(val).0, 2, "a trailing comma is allowed");

    let (_, _, form) = def_parts(&decls[5]);
    assert_eq!(form, DefForm::Plain);
}

/// Every action form parses, with and without the `;` after the block
#[test]
fn test_parse_action_forms() {
    let (_, stmts) = parse(
        "service s1 {
            var y = 5;
            action a(x: int) { y = y * x; }
            pub action reset() { y = 0; };
            action b(x: int, z: int) { y = x + z; }
            action c(x: int,) { y = x; };
        }",
    );
    let decls = decls_of(&stmts, 0);
    let expected = [(false, 1), (true, 0), (false, 2), (false, 1)];
    for (decl, (want_pub, want_params)) in decls[1..].iter().zip(expected) {
        let (val, is_pub, form) = def_parts(decl);
        assert_eq!((is_pub, form), (want_pub, DefForm::Action));
        assert_eq!(func_shape(val), (want_params, None));
        match val {
            Expr::Func { body, .. } => assert!(matches!(**body, Expr::Action(_))),
            _ => unreachable!(),
        }
    }
}

/// A bare parameter name would read as an ordinary def, so it is not accepted
#[test]
fn test_parse_rejects_bare_parameter_name() {
    let mut interner = Interner::new();
    for src in [
        "service s1 { def f x = 1; }",
        "service s1 { action a x { } }",
        "service s1 { def f = 1 }",
    ] {
        assert!(
            parse_string(src, &mut interner).is_err(),
            "must not parse: {}",
            src
        );
    }
}

/// A special form is exactly its `fn` spelling in everything but the mark
#[test]
fn test_parse_desugars_to_the_fn_member() {
    let (_, stmts) = parse(
        "service s1 {
            var y = 5;
            def f(x: int) = x + y;
            def g = fn (x: int) => x + y;
            action a(x: int) { y = y * x; }
            def b = fn (x: int) => action { y = y * x; };
            def n() = y;
            def m = fn () => y;
        }",
    );
    let decls = decls_of(&stmts, 0);
    for (special, spelled) in [(1, 2), (3, 4), (5, 6)] {
        assert_eq!(
            def_parts(&decls[special]).0,
            def_parts(&decls[spelled]).0,
            "member {} differs from its fn spelling",
            special
        );
        assert_ne!(def_parts(&decls[special]).2, DefForm::Plain);
        assert_eq!(def_parts(&decls[spelled]).2, DefForm::Plain);
    }
}

// ----- forwarded text of a code update -----

/// The text an update forwards parses back into the same declarations, marks
/// included
#[test]
fn test_forwarded_update_text_round_trips() {
    let (mut interner, stmts) = parse(
        "update s1 {
            def f(x: int) = x + y;
            pub def g(x: int, z: string): string = z;
            def now() = y;
            def h(a: int -> int, b: (int, bool), c: int list, d: () -> unit, e: (int -> int) list): unit = d;
            action a(x: int) { y = y * x; do b(x); let w = 1; }
            pub action reset() { y = 0; }
            def plain = f;
        }",
    );
    let decls = decls_of(&stmts, 0);
    let text = format_update_source(
        match &stmts[0] {
            Stmt::Update { service_name, .. } => *service_name,
            _ => unreachable!(),
        },
        decls,
        &interner,
    );
    let reparsed = parse_string(&text, &mut interner)
        .unwrap_or_else(|e| panic!("forwarded text does not parse: {}\n{}", e, text));
    let back = decls_of(&reparsed, 0);
    assert_eq!(back.len(), decls.len());
    for (before, after) in decls.iter().zip(back) {
        assert_eq!(before.name(), after.name(), "{}", text);
        let (bv, bp, bf) = def_parts(before);
        let (av, ap, af) = def_parts(after);
        assert_eq!((bp, bf), (ap, af), "{}", text);
        if bf == DefForm::Plain {
            continue;
        }
        match (bv, av) {
            (
                Expr::Func {
                    params: bparams,
                    return_ty: bret,
                    ..
                },
                Expr::Func {
                    params: aparams,
                    return_ty: aret,
                    ..
                },
            ) => {
                assert_eq!(bparams, aparams, "{}", text);
                assert_eq!(bret, aret, "{}", text);
            }
            _ => panic!("not functions"),
        }
        // The whole member, body included: every body here is one `format_expr`
        // or `format_action_stmt` can print
        assert_eq!(bv, av, "{}", text);
    }
    // The text shows the declared form, not the `fn` spelling
    assert!(text.contains("def f(x: int) = "), "{}", text);
    assert!(text.contains("pub action reset() {"), "{}", text);
    assert!(!text.contains("fn("), "{}", text);
}

// ----- name resolution -----

const S1: &str = "
    service s1 {
        var y = 5;
        def f(x: int) = x + y;
        action a(x: int) { y = y * x; }
        pub action reset() { y = 0; }
    }
";

/// `S1` followed by `rest`
fn with_s1(rest: &str) -> String {
    format!("{}\n{}", S1, rest)
}

/// A special form that is not the function of a call is rejected, and the
/// message shows the call to write instead
#[test]
fn test_special_form_without_call_is_rejected() {
    let s2 = "service s2 { var z = 0; }";
    assert_rejected(
        &with_s1(&format!("{} @test(s2) {{ let g = s1.f; }}", s2)),
        "s1.f can only be called: write `s1.f(...)`",
    );
    assert_rejected(
        &with_s1(&format!("{} @test(s2) {{ do s1.a; }}", s2)),
        "s1.a can only be called: write `do s1.a(...)`",
    );
    assert_rejected(
        &with_s1(
            "service s2 {
                def ap = fn (g: int -> int, v: int) => g(v);
                def r = ap(s1.f, 3);
            }",
        ),
        "s1.f can only be called",
    );
    assert_rejected(
        &with_s1("service s2 { pub def ui = (<button onclick={s1.reset}>x</button>); }"),
        "s1.reset can only be called: write `do s1.reset(...)`",
    );
    assert_rejected(&with_s1("watch s1.f;"), "s1.f can only be called");
}

/// Inside the service the bare name is rejected the same way, before and after
/// the declaration, and the message keeps the bare name
#[test]
fn test_bare_special_form_as_value_is_rejected() {
    assert_rejected(
        "service s1 { var y = 5; def f(x: int) = x + y; def h = f; }",
        "f can only be called: write `f(...)`",
    );
    assert_rejected(
        "service s1 { var y = 5; def h = f; def f(x: int) = x + y; }",
        "f can only be called: write `f(...)`",
    );
    assert_rejected(
        "service s1 { var y = 5; action a(x: int) { y = y * x; } action b() { do a; } }",
        "a can only be called: write `do a(...)`",
    );
    assert_rejected(
        "service s1 { var y = 5; def f(x: int) = x + y; var v = 0; }
         @test(s1) { let g = f; }",
        "f can only be called",
    );
}

/// A parameter named like a member of the service would be ignored inside an
/// action body, so it is rejected
#[test]
fn test_parameter_named_like_member_is_rejected() {
    assert_rejected(
        "service s1 { var y = 5; action a(y: int) { } }",
        "a parameter of s1.a has the name of the member s1.y; rename the parameter",
    );
    assert_rejected(
        "service s1 { var y = 5; def g(y: int) = y; }",
        "a parameter of s1.g has the name of the member s1.y",
    );
    // A member declared further down counts, and so does the form itself
    assert_rejected(
        "service s1 { def g(y: int) = y; var y = 5; }",
        "a parameter of s1.g has the name of the member s1.y",
    );
    assert_rejected(
        "service s1 { def g(g: int) = g; }",
        "a parameter of s1.g has the name of the member s1.g",
    );
    // A member of another service does not
    assert_accepted(
        "service s1 { var y = 5; }
         service s2 { def g(y: int) = y; }",
    );
    // An update's parameters are checked against the members of the service
    assert_rejected(
        "service s1 { var y = 5; }
         update s1 { def g(y: int) = y; }",
        "a parameter of s1.g has the name of the member s1.y",
    );
}

/// A call is the one use of a special form that is accepted: qualified or
/// bare, before or after the declaration, in a `@test` block and in an update
#[test]
fn test_special_form_calls_are_accepted() {
    assert_accepted(&with_s1(
        "service s2 { def r = s1.f(3); action go() { do s1.a(3); } }
         @test(s2) { do s1.a(s1.f(3)); }",
    ));
    // Bare, with the calls written before the declarations. A def that reads
    // a later def eagerly is rejected by the dependency analysis for the `fn`
    // spelling too, so the def is only name-resolved here
    let (_, stmts) = parse(
        "service s1 {
            var y = 5;
            def k = f(3);
            action b() { do a(3); }
            def f(x: int) = x + y;
            action a(x: int) { y = y * x; }
        }
        @test(s1) { do a(3); let q = f(3); do b(); }",
    );
    meerkat_lib::runtime::nameres::resolve(&stmts).expect("names resolve");
    assert_accepted(
        "service s1 {
            var y = 5;
            action b() { do a(3); }
            def f(x: int) = x + y;
            action a(x: int) { y = y * x; }
            def k = f(3);
        }
        @test(s1) { do a(3); let q = f(3); do b(); }",
    );
    // Inside an update block, naming members of the service
    assert_accepted(&with_s1(
        "update s1 { def k = f(3); action b() { do a(3); } }",
    ));
}

/// A local named like a special form wins over it, as for any member
#[test]
fn test_local_shadows_special_form() {
    assert_accepted(
        "service s1 {
            var y = 5;
            def f(x: int) = x + y;
            action c() { let f = 1; y = f + 1; }
            def d = fn (f: int) => f + 1;
        }
        @test(s1) { let f = 1; let q = f + 1; }",
    );
}

// ----- code updates -----

/// Start a manager on `initial`, then run `update` on it
async fn update_result(initial: &str, update: &str) -> Result<(), String> {
    let (interner, ast) = parse(initial);
    let mut node = Node::new();
    node.interner = interner;
    node.unified_ast = ast;
    node.static_checks().unwrap();
    let local_ast = node.unified_ast.clone();
    let mut manager: Manager = node
        .on_manager_startup(true, None, HashMap::new(), &local_ast)
        .await
        .unwrap();
    let update_ast = parse_string(update, &mut manager.interner).unwrap();
    let mut txn = Transaction::new(update_ast);
    txn.poll(&mut manager).await.map_err(|e| format!("{:?}", e))
}

/// An update that turns a def into a special form is rejected while another
/// member uses the def as a value, and accepted when every use is a call
#[tokio::test]
async fn test_update_to_special_form_is_rejected_while_used_as_value() {
    let update = "update s1 { def f(x: int) = x; }";

    let err = update_result(
        "service s1 { var y = 5; def f = fn (x: int) => x; def h = f; }",
        update,
    )
    .await
    .expect_err("h uses f as a value");
    assert!(err.contains("can only be called"), "{}", err);

    update_result(
        "service s1 { var y = 5; def f = fn (x: int) => x; def h = f(1); }",
        update,
    )
    .await
    .expect("every use of f is a call");
}

// ----- type checking -----

/// The type of member `member` of service `service` after the static checks
fn member_type(node: &Node, service: &str, member: &str) -> Type {
    // Interning a name that is already there returns its symbol
    let mut interner = node.interner.clone();
    let svc = interner.insert(service);
    let mem = interner.insert(member);
    node.local_services
        .find(svc)
        .expect("service typed")
        .fields()
        .find(mem)
        .expect("member typed")
        .clone()
}

/// A special form has the type of its `fn` spelling
#[test]
fn test_special_forms_have_the_type_of_their_fn_spelling() {
    let node = check(
        "service s1 {
            var y = 5;
            def f(x: int) = x + y;
            def g = fn (x: int) => x + y;
            def t(x: int): int = x;
            def now() = y;
            def now2 = fn () => y;
            action a(x: int) { y = y * x; }
            def b = fn (x: int) => action { y = y * x; };
        }",
    )
    .expect("accepted");
    let int_to_int = Type::Func(Box::new(Type::Int), Box::new(Type::Int));
    assert_eq!(member_type(&node, "s1", "f"), int_to_int);
    assert_eq!(member_type(&node, "s1", "g"), int_to_int);
    assert_eq!(member_type(&node, "s1", "t"), int_to_int);
    assert_eq!(
        member_type(&node, "s1", "now"),
        member_type(&node, "s1", "now2")
    );
    assert_eq!(member_type(&node, "s1", "a"), member_type(&node, "s1", "b"));
    assert_eq!(member_type(&node, "s1", "a").to_string(), "int -> unit");
}

/// A wrong result type and a wrong argument type are type errors
#[test]
fn test_special_forms_are_type_checked() {
    assert_rejected(
        "service s1 { def f(x: int): string = x; }",
        "type check error",
    );
    assert_rejected(
        "service s1 { def f(x: int) = x; }
         service s2 { def r = s1.f(\"a\"); }",
        "type check error",
    );
    assert_rejected(
        "service s1 { var y = 5; action a(x: int) { y = x; } }
         @test(s1) { do a(true); }",
        "type check error",
    );
}

// ----- running -----

/// Run the `@test` blocks of `src` on one node and return `s1.y`
async fn run_and_read_y(src: &str) -> meerkat_lib::runtime::ast::Value {
    let (interner, ast) = parse(src);
    let mut node = Node::new();
    node.interner = interner;
    node.unified_ast = ast.clone();
    node.static_checks().expect("static checks must pass");
    let local_ast = node.unified_ast.clone();
    let mut manager = node
        .on_manager_startup(true, None, HashMap::new(), &local_ast)
        .await
        .expect("service init must succeed");
    let tests: Vec<(String, Vec<ActionStmt>)> = ast
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
    for (service, stmts) in &tests {
        let svc = manager.interner.insert(service);
        manager.execute_test_block(svc, stmts).await.unwrap();
    }
    let s1 = manager.interner.insert("s1");
    let y = manager.interner.insert("y");
    manager.lookup(y, s1, None).await.unwrap()
}

/// `do s1.a(s1.f(3));` run from another local service leaves `s1.y == 40`
/// for both forms and for the `fn` spelling (5 * (3 + 5))
#[tokio::test]
async fn test_special_forms_run_like_their_fn_spelling() {
    let client = "service s2 { var z = 0; } @test(s2) { do s1.a(s1.f(3)); }";
    let special = run_and_read_y(&format!(
        "service s1 {{ var y = 5; def f(x: int) = x + y; action a(x: int) {{ y = y * x; }} }} {}",
        client
    ))
    .await;
    let spelled = run_and_read_y(&format!(
        "service s1 {{ var y = 5; def f = fn (x: int) => x + y;
                       def a = fn (x: int) => action {{ y = y * x; }}; }} {}",
        client
    ))
    .await;
    assert_eq!(special, meerkat_lib::runtime::ast::Value::Int { val: 40 });
    assert_eq!(spelled, special);
}
