//! The text a code update forwards to the owner of a service
//! (`format_update_source`) must parse back to the program it was printed
//! from, and the owner must compute what the sender's program computes.
//!
//! The round trip compares the ASTs (`Expr`, `Decl` and `ActionStmt` carry no
//! source spans) after one normalisation: `ActionStmt::Assert` keeps the raw
//! source text of its condition for error messages, and the printer writes the
//! condition from the AST, so that text is cleared on both sides.

use meerkat_lib::runtime::ast::{ActionStmt, Decl, Expr, Stmt, Value};
use meerkat_lib::runtime::parser::parse_string;
use meerkat_lib::runtime::update::format_update_source;
use meerkat_lib::runtime::{Interner, Node};
use std::collections::HashMap;

fn norm_stmts(stmts: &mut [ActionStmt]) {
    for s in stmts {
        match s {
            ActionStmt::Assert(e, text) => {
                text.clear();
                norm_expr(e);
            }
            ActionStmt::Let { expr, .. }
            | ActionStmt::Expr(expr)
            | ActionStmt::Do(expr)
            | ActionStmt::Assign { expr, .. }
            | ActionStmt::Insert { row: expr, .. } => norm_expr(expr),
            ActionStmt::For { iterable, body, .. } => {
                norm_expr(iterable);
                norm_stmts(body);
            }
        }
    }
}

fn norm_expr(e: &mut Expr) {
    match e {
        Expr::Action(stmts) => norm_stmts(stmts),
        Expr::Literal { .. }
        | Expr::Variable { .. }
        | Expr::MemberAccess { .. }
        | Expr::Html(_) => {}
        Expr::KeyVal { value, .. } => norm_expr(value),
        Expr::Unop { expr, .. } => norm_expr(expr),
        Expr::Binop { expr1, expr2, .. } => {
            norm_expr(expr1);
            norm_expr(expr2);
        }
        Expr::Range { start, end } => {
            norm_expr(start);
            norm_expr(end);
        }
        Expr::If { cond, expr1, expr2 } => {
            norm_expr(cond);
            norm_expr(expr1);
            norm_expr(expr2);
        }
        Expr::Func { body, .. } => norm_expr(body),
        Expr::Call { func, args } => {
            norm_expr(func);
            args.iter_mut().for_each(norm_expr);
        }
        Expr::Tuple { val } | Expr::List(val) | Expr::Table { records: val, .. } => {
            val.iter_mut().for_each(norm_expr)
        }
        Expr::Select { where_clause, .. } => norm_expr(where_clause),
        Expr::Fold {
            operation,
            identity,
            ..
        } => {
            norm_expr(operation);
            norm_expr(identity);
        }
    }
}

fn norm_decls(decls: &mut [Decl]) {
    for d in decls {
        match d {
            Decl::VarDecl { val, .. } | Decl::DefDecl { val, .. } => norm_expr(val),
            Decl::TableDecl { .. } => {}
        }
    }
}

/// The declarations and name of the `update` or `service` in `src`
fn parse_decls(
    src: &str,
    interner: &mut Interner,
) -> (meerkat_lib::runtime::interner::Symbol, Vec<Decl>) {
    let stmts =
        parse_string(src, interner).unwrap_or_else(|e| panic!("does not parse: {}\n{}", e, src));
    match stmts.into_iter().next() {
        Some(Stmt::Update {
            service_name,
            decls,
        })
        | Some(Stmt::Service {
            name: service_name,
            decls,
        }) => (service_name, decls),
        other => panic!("expected an update, got {:?}", other),
    }
}

/// Print `decls_src` as a forwarded update, parse the text, and compare
fn assert_round_trip(decls_src: &str) {
    let mut interner = Interner::new();
    let (name, decls) = parse_decls(&format!("update s1 {{ {} }}", decls_src), &mut interner);
    let text = format_update_source(name, &decls, &interner);
    let (_, back) = parse_decls(&text, &mut interner);
    let (mut before, mut after) = (decls, back);
    norm_decls(&mut before);
    norm_decls(&mut after);
    assert_eq!(
        before, after,
        "forwarded text parses to another program\nsource: {}\nprinted: {}",
        decls_src, text
    );
}

// ----- syntax -----

/// Every declaration here parses, prints, and parses back to the same AST
#[test]
fn test_forwarded_text_round_trips() {
    let corpus = [
        // precedence
        "def f(x: int) = (x + 1) * 2;",
        "def f(x: int) = x + 1 * 2;",
        "def f(x: int, y: int, z: int) = x - (y - z);",
        "def f(x: int, y: int, z: int) = (x - y) - z;",
        "def f(x: int) = x / (2 * 3);",
        "def f(x: int) = -(x + 1);",
        "def f(x: int) = - -x;",
        "def f(x: int) = x - -5;",
        "def f(a: bool, b: bool) = !(a && b) || a;",
        "def f(a: int, b: int, c: int, d: int) = (a < b) == (c > d);",
        "def f(x: int) = (if x > 1 then 1 else 2) + 3;",
        "def f(x: int) = if x > 1 then (if x > 2 then 3 else 4) else 5;",
        "def f(x: int) = 1 .. (x + 5);",
        "def f(x: int) = (x + 1) .. 9;",
        // lambdas
        "def make(x: int) = fn (z: int) => x + z;",
        "def make = fn a => fn b => (a + b);",
        "def make = fn (a: int, b: int) => a + b;",
        "def make = fn () => 1;",
        "def make = fn (a: int -> int) => (fn (b) => a(b))(1) + 2;",
        "def f(x: int) = (fn (a: int) => a + 1)(x);",
        // literals, names, calls
        "def f(x: int) = \"it's a (quoted) {x}; // not a comment\";",
        "def f(x: int) = \"\";",
        "def f(x: int) = true;",
        "def f(x: int) = s2.h(x, [1, 2], {1, \"a\"});",
        "def f(x: int) = s2.g(x)(x + 1);",
        "def f(x: int) = [x, x + 1, [], {}];",
        "def f(x: int) = {x + 1, {x, x}};",
        "var v: int = (1 + 2) * 3;",
        "var l = [(1 + 2), 3];",
        // select, fold, tables
        "table t { id: int, n: string, ok: bool, };",
        "def q = select id, n from t where id > 1 && n == \"x\";",
        "def q = select id from t where (id + 1) * 2 > 3;",
        "def q = (select id from t where id > 1) == (select id from t where id > 2);",
        "def total = fold(t.id, fn (p: int, q: int) => p + q, 0);",
        // actions
        "action a() { assert(true); }",
        "action a(x: int) { assert(x+1 == 2); y = (x + 1) * 2; }",
        "action a(x: int) { for i in 0..3 { y = y + i; } }",
        "action a(x: int) { for i in (x + 1)..(x + 5) { insert {id: i, n: \"x\"} into t insert {id: i + 1, n: \"y\"} into t } }",
        "action a(x: int) { let w: int = (x + 1) * 2; let u = fn (k: int) => k + w; do b(u(w)); }",
        "action a(x: int) { y = if x > 1 then 1 else 2; b(x + 1); }",
        "pub action a(x: int, z: string) { do s2.b(x, z); }",
        "def f(x: int) = action { assert(x > 1); y = x * (x + 1); };",
        "def f(x: int) = action { for i in [1, 2] { insert {id: i * 2, n: \"q\"} into t } };",
        "pub def g(x: int, z: string): string = z;",
        "def h(a: int -> int, b: (int, bool), c: int list): unit = d;",
    ];
    for src in corpus {
        assert_round_trip(src);
    }
}

/// The printed text of the three forms of the report, exactly
#[test]
fn test_printed_text_of_known_cases() {
    let print = |decls_src: &str| {
        let mut interner = Interner::new();
        let (name, decls) = parse_decls(&format!("update s1 {{ {} }}", decls_src), &mut interner);
        format_update_source(name, &decls, &interner)
    };
    let t = print("action a() { assert(true); }");
    assert!(t.contains("assert(true);"), "{}", t);
    let t = print("def make(x: int) = fn (z: int) => x + z;");
    assert!(t.contains("fn (z: int) => x + z"), "{}", t);
    let t = print("def f(x: int) = (x + 1) * 2;");
    assert!(t.contains("(x + 1) * 2"), "{}", t);
    let t = print("def f(x: int) = -(x + 1);");
    assert!(t.contains("-(x + 1)"), "{}", t);
}

// ----- meaning -----

/// Print `decls_src` as a forwarded update, make the owner's service from the
/// printed text, run the `@test(s2)` block `test_body` and return `s1.y`
async fn run_forwarded(decls_src: &str, test_body: &str) -> Value {
    let mut interner = Interner::new();
    let (name, decls) = parse_decls(&format!("service s1 {{ {} }}", decls_src), &mut interner);
    let text = format_update_source(name, &decls, &interner);
    let inner = text
        .strip_prefix("update s1 {")
        .and_then(|t| t.strip_suffix('}'))
        .unwrap_or_else(|| panic!("unexpected shape: {}", text));
    let src = format!(
        "service s1 {{ var y = 0; {} action set(v: int) {{ y = v; }} }}
         service s2 {{ var z = 0; }}
         @test(s2) {{ {} }}",
        inner, test_body
    );
    let mut interner = Interner::new();
    let ast = parse_string(&src, &mut interner)
        .unwrap_or_else(|e| panic!("forwarded text does not parse: {}\n{}", e, src));
    let mut node = Node::new();
    node.interner = interner;
    node.unified_ast = ast.clone();
    node.static_checks()
        .unwrap_or_else(|e| panic!("static checks: {:?}\n{}", e, src));
    let local_ast = node.unified_ast.clone();
    let mut manager = node
        .on_manager_startup(true, None, HashMap::new(), &local_ast)
        .await
        .expect("service init must succeed");
    for stmt in &ast {
        if let Stmt::Test {
            service_name,
            stmts,
        } = stmt
        {
            let svc = *service_name;
            manager.execute_test_block(svc, stmts).await.unwrap();
        }
    }
    let s1 = manager.interner.insert("s1");
    let y = manager.interner.insert("y");
    manager.lookup(y, s1, None).await.unwrap()
}

fn int(val: i32) -> Value {
    Value::Int { val }
}

/// The owner computes what the sender's source says, for both spellings
#[tokio::test]
async fn test_forwarded_text_keeps_the_meaning() {
    let cases = [
        // (declarations, argument of `set`, expected y)
        ("def f(x: int) = (x + 1) * 2;", "s1.f(3)", 8),
        ("def f = fn (x: int) => (x + 1) * 2;", "s1.f(3)", 8),
        ("def f(x: int) = x - (10 - 4);", "s1.f(3)", -3),
        ("def f = fn (x: int) => x - (10 - 4);", "s1.f(3)", -3),
        ("def f(x: int) = -(x + 1);", "s1.f(3)", -4),
        ("def f(x: int) = 2 * -(x + 1);", "s1.f(3)", -8),
        (
            "def f(x: int) = (if x > 2 then 1 else 2) + 3;",
            "s1.f(3)",
            4,
        ),
        (
            "def f(x: int) = (if x > 2 then 1 else 2) + 3;",
            "s1.f(1)",
            5,
        ),
        (
            "def f = fn (x: int) => (if x > 2 then 1 else 2) * 10;",
            "s1.f(3)",
            10,
        ),
        (
            "def f(x: int) = (fn (a: int) => fn (b: int) => (a + b) * 2)(x)(x + 1);",
            "s1.f(3)",
            14,
        ),
        (
            "def make(x: int) = fn (z: int) => x + z; def f(x: int) = make(x)(10);",
            "s1.f(3)",
            13,
        ),
        (
            "def f(x: int) = (fn (a: int) => a * 2)(x + 1);",
            "s1.f(3)",
            8,
        ),
    ];
    for (decls, call, expected) in cases {
        let got = run_forwarded(decls, &format!("do s1.set({});", call)).await;
        assert_eq!(got, int(expected), "{} / {}", decls, call);
    }
}

/// An action with `assert`, `let` and `for` runs on the owner after forwarding
#[tokio::test]
async fn test_forwarded_action_runs() {
    let decls =
        "action acc(n: int) { assert(n > 0); let base = (n + 1) * 2; for i in 0..3 { y = y + base + i; } }";
    // base = 8, y = 8 + 9 + 10
    assert_eq!(run_forwarded(decls, "do s1.acc(3);").await, int(27));
}
