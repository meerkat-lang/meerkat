//! Name resolution integration tests
//!
//! This module contains integration tests for validating the static
//! name resolution pass using strings parsed into AST statements

use meerkat_lib::runtime::{
    ast::{Decl, Expr, Stmt, Value},
    nameres::{resolve, Error, ExpectedSort},
    parser::parse_string,
    tt::Param,
    Interner,
};

/// Verify that an empty program resolves successfully
#[test]
fn test_integration_resolve_empty() {
    let stmts = vec![];
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify that a basic service with var and def resolves
#[test]
fn test_integration_resolve_valid_service() {
    let mut interner = Interner::new();
    let input = "
        service s1 {
            var x = 0;
            pub def y = (x + 1);
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify eager forward references are rejected in services
/// Verify that service forward references pass pure lexical name resolution
#[test]
fn test_integration_rejects_forward_reference() {
    let mut interner = Interner::new();
    let input = "
        service s1 {
            pub def y = (x + 1);
            var x = 0;
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify that local let binds shadow service variables in actions
#[test]
fn test_integration_shadowing_service_field() {
    let mut interner = Interner::new();
    let input = "
        service s1 {
            var x = 0;
            pub def y = action {
                let x = 5;
                let z = x;
            };
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify that nested let variables shadow outer let variables
#[test]
fn test_integration_nested_shadowing_let() {
    let mut interner = Interner::new();
    let input = "
        service s1 {
            pub def y = action {
                let x = 5;
                do action {
                    let x = 10;
                    let z = x;
                };
            };
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify that function parameters resolve inside the function body
#[test]
fn test_integration_func_params() {
    let mut interner = Interner::new();
    let input = "
        service s1 {
            pub def y = fn a => (a + 1);
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify that nested function closures capture outer parameters
#[test]
fn test_integration_nested_func_closures() {
    let mut interner = Interner::new();
    let input = "
        service s1 {
            pub def y = fn a => fn b => (a + b);
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify that variables inside watch expressions resolve
#[test]
fn test_integration_watch_expression() {
    let mut interner = Interner::new();
    let input = "
        service s1 {
            var x = 0;
        }
        watch s1.x;
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify that unbound variables in watch expressions trigger error
#[test]
fn test_integration_watch_unbound() {
    let mut interner = Interner::new();
    let input = "
        watch z;
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    let z = interner.insert("z");
    assert_eq!(
        res,
        Err(Error::UnknownIdentifier {
            name: z,
            expected: ExpectedSort::Variable,
            context_name: None,
        })
    );
}

/// Verify that valid update statements resolve successfully
/// during name resolution
#[test]
fn test_integration_update_stmt_resolves() {
    let mut interner = Interner::new();
    let s1 = interner.insert("s1");
    let y = interner.insert("y");

    let s1_stmt = Stmt::Service {
        name: s1,
        decls: vec![],
    };
    let update_stmt = Stmt::Update {
        service_name: s1,
        decls: vec![Decl::DefDecl {
            name: y,
            ty: None,
            val: Expr::Literal {
                val: Value::Int { val: 2 },
            },
            is_pub: false,
        }],
    };

    let stmts = vec![s1_stmt, update_stmt];
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify that update on unbound service name yields an UnknownIdentifier error
#[test]
fn test_integration_update_unbound_errors() {
    let mut interner = Interner::new();
    let s2 = interner.insert("s2");
    let y = interner.insert("y");

    let update_stmt = Stmt::Update {
        service_name: s2,
        decls: vec![Decl::DefDecl {
            name: y,
            ty: None,
            val: Expr::Literal {
                val: Value::Int { val: 2 },
            },
            is_pub: false,
        }],
    };

    let stmts = vec![update_stmt];
    let res = resolve(&stmts);
    assert_eq!(
        res,
        Err(Error::UnknownIdentifier {
            name: s2,
            expected: ExpectedSort::Service,
            context_name: None,
        })
    );
}

/// Verify that select expressions validate their table name
#[test]
fn test_integration_select_expr() {
    let mut interner = Interner::new();
    let input = "
        service s1 {
            table t1 {
                id: int,
                name: string,
            };
            pub def get = select id, name from t1 where true;
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify that fold expressions validate their table name
#[test]
fn test_integration_fold_expr() {
    let mut interner = Interner::new();
    let input = "
        service s1 {
            table t1 {
                val: int,
            };
            pub def total = fold(t1.val, fn a => fn b => (a + b), 0);
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify that imports bind service names for member access
#[test]
fn test_integration_import_member_access() {
    let mut interner = Interner::new();
    let input = "
        import s1
        service s2 {
            pub def y = s1.x;
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify that variables inside assert statements resolve
#[test]
fn test_integration_assert_stmt() {
    let mut interner = Interner::new();
    let input = "
        service s1 {
            var x = 0;
            pub def check_x = action {
                assert (x == 0);
            };
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify that connect statements resolve successfully
#[test]
fn test_integration_connect_stmt() {
    let stmts = vec![Stmt::Connect {
        path: "some_path".to_string(),
        addr: "some_addr".to_string(),
    }];
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify that insert statements resolve table name
#[test]
fn test_integration_insert_stmt() {
    let mut interner = Interner::new();
    let input = "
        service s1 {
            table t1 {
                id: int,
            };
            pub def add = action {
                insert 5 into t1
            };
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify that insert statement with unbound table is ignored with warning
#[test]
fn test_integration_insert_unbound() {
    let mut interner = Interner::new();
    let input = "
        service s1 {
            pub def add = action {
                insert 5 into t2
            };
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify that insert statement row variables are resolved
#[test]
fn test_integration_insert_expr_resolve() {
    let mut interner = Interner::new();
    let input = "
        service s1 {
            table t1 {
                id: int,
            };
            pub def add = action {
                let val = 10;
                insert val into t1
            };
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify that assignment resolves target variable
#[test]
fn test_integration_assign_stmt() {
    let mut interner = Interner::new();
    let input = "
        service s1 {
            var x = 0;
            pub def update_x = action {
                x = 10;
            };
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify that assignment to unbound variable triggers error
#[test]
fn test_integration_assign_unbound() {
    let mut interner = Interner::new();
    let input = "
        service s1 {
            pub def update_x = action {
                z = 10;
            };
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    let s1 = interner.insert("s1");
    let z = interner.insert("z");
    assert_eq!(
        res,
        Err(Error::UnknownIdentifier {
            name: z,
            expected: ExpectedSort::Variable,
            context_name: Some(s1),
        })
    );
}

/// Verify that if expression condition is resolved
#[test]
fn test_integration_if_expr() {
    let mut interner = Interner::new();
    let input = "
        service s1 {
            var cond_var = true;
            pub def check = if cond_var then 1 else 2;
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify that unbound variable in if-then triggers error
#[test]
fn test_integration_if_expr_unbound() {
    let mut interner = Interner::new();
    let input = "
        service s1 {
            pub def check = if true then unbound_val else 2;
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    let s1 = interner.insert("s1");
    let unbound_val = interner.insert("unbound_val");
    assert_eq!(
        res,
        Err(Error::UnknownIdentifier {
            name: unbound_val,
            expected: ExpectedSort::Variable,
            context_name: Some(s1),
        })
    );
}

/// Verify that unop resolves bound variable
#[test]
fn test_integration_unop_expr() {
    let mut interner = Interner::new();
    let input = "
        service s1 {
            var flag = false;
            pub def inverted = !flag;
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify that binop resolves both operands
#[test]
fn test_integration_binop_expr() {
    let mut interner = Interner::new();
    let input = "
        service s1 {
            var a = 1;
            var b = 2;
            pub def sum = (a + b);
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify that tuple expression resolves elements
#[test]
fn test_integration_tuple_expr() {
    let mut interner = Interner::new();
    let input = "
        service s1 {
            var a = 1;
            pub def t = {a, 2};
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify that call expression resolves functions
#[test]
fn test_integration_call_expr() {
    let mut interner = Interner::new();
    let input = "
        service s1 {
            def f = fn x => (x + 1);
            var val = 5;
            pub def res_val = f(val);
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify that call expression with unbound argument triggers error
#[test]
fn test_integration_call_expr_unbound_arg() {
    let mut interner = Interner::new();
    let input = "
        service s1 {
            def f = fn x => x;
            pub def res_val = f(z);
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    let s1 = interner.insert("s1");
    let z = interner.insert("z");
    assert_eq!(
        res,
        Err(Error::UnknownIdentifier {
            name: z,
            expected: ExpectedSort::Variable,
            context_name: Some(s1),
        })
    );
}

/// Verify that call expression with unbound function triggers error
#[test]
fn test_integration_call_expr_unbound_func() {
    let mut interner = Interner::new();
    let input = "
        service s1 {
            pub def res_val = z(5);
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    let s1 = interner.insert("s1");
    let z = interner.insert("z");
    assert_eq!(
        res,
        Err(Error::UnknownIdentifier {
            name: z,
            expected: ExpectedSort::Variable,
            context_name: Some(s1),
        })
    );
}

/// Verify that closure value literals resolve parameters
#[test]
fn test_integration_value_closure_resolves() {
    let mut interner = Interner::new();
    let x = interner.insert("x");
    let closure = Value::Closure {
        params: vec![Param { name: x, ty: None }],
        body: Box::new(Expr::Variable { name: x }),
        env: vec![],
        service_name: interner.insert("s1"),
        return_ty: None,
    };
    let expr = Expr::Literal { val: closure };
    let stmts = vec![Stmt::Watch { expr }];
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify that action closure value literals resolve statements
#[test]
fn test_integration_value_action_closure_resolves() {
    let action = Value::ActionClosure {
        stmts: vec![],
        env: vec![],
        service_net_id: meerkat_lib::net::ServiceNetId::new("s1"),
    };
    let expr = Expr::Literal { val: action };
    let stmts = vec![Stmt::Watch { expr }];
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify that member access with unbound service triggers error
#[test]
fn test_integration_member_access_unbound() {
    let mut interner = Interner::new();
    let input = "
        service s1 {
            pub def y = s2.x;
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    let s1 = interner.insert("s1");
    let s2 = interner.insert("s2");
    assert_eq!(
        res,
        Err(Error::UnknownIdentifier {
            name: s2,
            expected: ExpectedSort::Service,
            context_name: Some(s1),
        })
    );
}

/// Verify that multiple services resolve correctly
#[test]
fn test_integration_multiple_services() {
    let mut interner = Interner::new();
    let input = "
        service s1 {}
        service s2 {
            pub def ref_s1 = s1;
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify that let bindings do not leak across action blocks
#[test]
fn test_integration_nested_blocks_let_isolation() {
    let mut interner = Interner::new();
    let input = "
        service s1 {
            pub def y = action {
                let x = 5;
            };
            pub def z = action {
                let a = x;
            };
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    let s1 = interner.insert("s1");
    let x = interner.insert("x");
    assert_eq!(
        res,
        Err(Error::UnknownIdentifier {
            name: x,
            expected: ExpectedSort::Variable,
            context_name: Some(s1),
        })
    );
}

/// Verify that forward reference to a new member in an update block fails
/// per REBLS operational semantics
#[test]
fn test_integration_update_stmt_forward_reference_fails() {
    let mut interner = Interner::new();
    let s1 = interner.insert("s1");
    let x = interner.insert("x");
    let y = interner.insert("y");

    let s1_stmt = Stmt::Service {
        name: s1,
        decls: vec![],
    };
    let update_stmt = Stmt::Update {
        service_name: s1,
        decls: vec![
            Decl::DefDecl {
                name: y,
                ty: None,
                val: Expr::Variable { name: x },
                is_pub: false,
            },
            Decl::VarDecl {
                name: x,
                ty: None,
                val: Expr::Literal {
                    val: Value::Int { val: 5 },
                },
            },
        ],
    };

    let stmts = vec![s1_stmt, update_stmt];
    let res = resolve(&stmts);
    assert_eq!(
        res,
        Err(Error::UnknownIdentifier {
            name: x,
            expected: ExpectedSort::Variable,
            context_name: Some(s1),
        })
    );
}

/// Verify service update block variables do not leak to global scope
#[test]
fn test_integration_update_stmt_scoping() {
    let mut interner = Interner::new();
    let s1 = interner.insert("s1");
    let y = interner.insert("y");

    let s1_stmt = Stmt::Service {
        name: s1,
        decls: vec![],
    };
    let update_stmt = Stmt::Update {
        service_name: s1,
        decls: vec![Decl::VarDecl {
            name: y,
            ty: None,
            val: Expr::Literal {
                val: Value::Int { val: 5 },
            },
        }],
    };
    let watch_stmt = Stmt::Watch {
        expr: Expr::Variable { name: y },
    };

    let stmts = vec![s1_stmt, update_stmt, watch_stmt];
    let res = resolve(&stmts);
    assert_eq!(
        res,
        Err(Error::UnknownIdentifier {
            name: y,
            expected: ExpectedSort::Variable,
            context_name: None,
        })
    );
}

/// Verify update block inherits service environment
#[test]
fn test_integration_update_inherits_service_env() {
    let mut interner = Interner::new();
    let s1 = interner.insert("s1");
    let a = interner.insert("a");
    let b = interner.insert("b");

    let s1_stmt = Stmt::Service {
        name: s1,
        decls: vec![Decl::VarDecl {
            name: a,
            ty: None,
            val: Expr::Literal {
                val: Value::Int { val: 10 },
            },
        }],
    };
    let update_stmt = Stmt::Update {
        service_name: s1,
        decls: vec![Decl::DefDecl {
            name: b,
            ty: None,
            val: Expr::Variable { name: a },
            is_pub: false,
        }],
    };

    let stmts = vec![s1_stmt, update_stmt];
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify update block sequential scoping (in-order references succeed)
#[test]
fn test_integration_update_sequential_scoping() {
    let mut interner = Interner::new();
    let s1 = interner.insert("s1");
    let a = interner.insert("a");
    let b = interner.insert("b");

    let s1_stmt = Stmt::Service {
        name: s1,
        decls: vec![],
    };
    let update_stmt = Stmt::Update {
        service_name: s1,
        decls: vec![
            Decl::VarDecl {
                name: a,
                ty: None,
                val: Expr::Literal {
                    val: Value::Int { val: 10 },
                },
            },
            Decl::DefDecl {
                name: b,
                ty: None,
                val: Expr::Variable { name: a },
                is_pub: false,
            },
        ],
    };

    let stmts = vec![s1_stmt, update_stmt];
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify update declarations accumulate across sequential update blocks
#[test]
fn test_integration_update_accumulates_across_blocks() {
    let mut interner = Interner::new();
    let s1 = interner.insert("s1");
    let a = interner.insert("a");
    let b = interner.insert("b");

    let s1_stmt = Stmt::Service {
        name: s1,
        decls: vec![],
    };
    let update1 = Stmt::Update {
        service_name: s1,
        decls: vec![Decl::VarDecl {
            name: a,
            ty: None,
            val: Expr::Literal {
                val: Value::Int { val: 5 },
            },
        }],
    };
    let update2 = Stmt::Update {
        service_name: s1,
        decls: vec![Decl::DefDecl {
            name: b,
            ty: None,
            val: Expr::Variable { name: a },
            is_pub: false,
        }],
    };

    let stmts = vec![s1_stmt, update1, update2];
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify atomic update blocks resolve service member references properly
#[test]
fn test_integration_atomic_update_inherits_service_env() {
    let mut interner = Interner::new();
    let s1 = interner.insert("s1");
    let a = interner.insert("a");
    let b = interner.insert("b");

    let s1_stmt = Stmt::Service {
        name: s1,
        decls: vec![Decl::VarDecl {
            name: a,
            ty: None,
            val: Expr::Literal {
                val: Value::Int { val: 1 },
            },
        }],
    };
    let atomic_stmt = Stmt::Atomic {
        updates: vec![Stmt::Update {
            service_name: s1,
            decls: vec![Decl::DefDecl {
                name: b,
                ty: None,
                val: Expr::Variable { name: a },
                is_pub: false,
            }],
        }],
    };

    let stmts = vec![s1_stmt, atomic_stmt];
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify that `@test` blocks can resolve variables
/// inside the target service
#[test]
fn test_integration_test_block_member_resolution() {
    let mut interner = Interner::new();
    let input = "
        service s1 {
            var x = 5;
            pub def get_x = x;
        }
        @test(s1) {
            do get_x;
            assert(x == 5);
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify `@test` blocks can resolve services that
/// are defined after them in the program statements
#[test]
fn test_integration_test_block_hoisting() {
    let mut interner = Interner::new();
    let input = "
        @test(s1) {
            do get_x;
            assert(x == 5);
        }
        service s1 {
            var x = 5;
            pub def get_x = x;
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify that a @test block targeting an imported service is fully
/// name-resolved when the service's declaration is present in the
/// unified AST (as produced by on_node_startup before static_checks)
#[test]
fn test_integration_test_block_imported_resolves() {
    let mut interner = Interner::new();
    let input = "
        import s1
        service s1 {
            var x = 5;
        }
        @test(s1) {
            assert(x == 5);
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify complex out-of-order mutually referential service
/// definitions and out-of-order test blocks resolving symbols
/// across 4 services
#[test]
fn test_integration_complex_arbitrary_order_resolution() {
    let mut interner = Interner::new();
    let input = "
        @test(s3) {
            assert(a == 1);
            do s4.action4;
        }
        service s1 {
            pub def val1 = s2.val2;
            pub def action1 = action {
                do s2.action2;
            };
        }
        @test(s1) {
            do action1;
        }
        service s3 {
            pub def val3 = s4.val4;
            var a = 1;
        }
        service s2 {
            pub def val2 = s3.val3;
            pub def action2 = action {
                do s3.val3;
            };
        }
        service s4 {
            pub def val4 = s1.val1;
            pub def action4 = action {
                do s1.action1;
            };
        }
        @test(s4) {
            do action4;
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify that accessing a non-existent member on a local service returns UnknownIdentifier
#[test]
fn test_integration_local_member_access_misspelled() {
    let mut interner = Interner::new();
    let input = "
        service s1 {
            var x = 1;
        }
        service s2 {
            pub def y = s1.misspelled;
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    let s1 = interner.insert("s1");
    let misspelled = interner.insert("misspelled");
    assert_eq!(
        res,
        Err(Error::UnknownIdentifier {
            name: misspelled,
            expected: ExpectedSort::Variable,
            context_name: Some(s1),
        })
    );
}

/// Verify that accessing a valid member on a local service succeeds
#[test]
fn test_integration_local_member_access_valid() {
    let mut interner = Interner::new();
    let input = "
        service s1 {
            var x = 1;
        }
        service s2 {
            pub def y = s1.x;
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify that accessing an unknown member on an imported service
/// produces an `UnknownIdentifier` error
#[test]
fn test_import_member_unknown_errors() {
    let mut interner = Interner::new();
    let input = "
        import s1
        service s1 {
            var x = 1;
        }
        service s2 {
            pub def y = s1.nonexistent;
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    assert!(res.is_err());
}

/// Verify that referencing an unknown variable inside a @test block for an
/// imported service produces an `UnknownIdentifier` error
#[test]
fn test_import_test_block_member_unknown_errors() {
    let mut interner = Interner::new();
    let input = "
        import s1
        service s1 {
            var x = 1;
        }
        @test(s1) {
            assert(nonexistent_var == 5);
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    assert!(res.is_err());
}

/// Verify that member resolution succeeds transitively across multiple
/// imported services (A -> B -> C)
#[test]
fn test_transitive_import_member_access() {
    let mut interner = Interner::new();
    let input = "
        import s1
        import s2
        service s1 {
            var a = 10;
        }
        service s2 {
            pub def b = s1.a;
        }
        service s3 {
            pub def c = s2.b;
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify delayed forward reference succeeds if variable is
/// initialized before closure invocation
#[test]
fn test_integration_delayed_fwd_ok() {
    let mut interner = Interner::new();
    let input = "
        service s {
            def f = fn () => x;
            var x = 5;
            var y = f();
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify forward references in closures pass pure lexical name resolution
#[test]
fn test_integration_delayed_fwd_err() {
    let mut interner = Interner::new();
    let input = "
        service s {
            def f = fn () => x;
            var y = f();
            var x = 5;
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    assert!(res.is_ok());
}

/// Verify delayed action forward reference succeeds when executed
/// after target variable initialization
#[test]
fn test_integration_action_fwd_ok() {
    let mut interner = Interner::new();
    let input = "
        service s {
            def act = action { let a = x; };
            var x = 10;
            pub def run = action { do act; };
        }
    ";
    let parse_result = parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let stmts = parse_result.unwrap();
    let res = resolve(&stmts);
    assert!(res.is_ok());
}
