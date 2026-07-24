use super::*;
use crate::runtime::html::HtmlTemplateBuilder;
use crate::runtime::interner::Interner;
use crate::runtime::limits::{MAX_SCOPE_DEPTH, MAX_TYPE_DEPTH};
use crate::runtime::tt::Param;

/// Verify that type checking empty program passes
#[test]
fn test_empty_program() {
    let mut classes = Env::new(None);
    let res = check(&[], &mut classes);
    assert!(res.is_ok())
}

/// Verify that type depth verification works as expected
#[test]
fn test_type_depth_calculation() {
    assert!(check_type(&Type::Int, 1).is_ok());
    let func = Type::Func(
        Box::new(Type::Int),
        Box::new(Type::Func(Box::new(Type::Bool), Box::new(Type::String))),
    );
    assert!(check_type(&func, 1).is_ok());
}

/// Verify that type annotations at the exact depth limit
/// are accepted, while those exceeding the limit
/// `MAX_TYPE_DEPTH` are rejected
#[test]
fn test_deep_annotation_rejected() {
    let mut interner = Interner::new();
    let name_s = interner.insert("my_service");
    let var_x = interner.insert("x");

    // Exact limit succeeds: depth == `MAX_TYPE_DEPTH`
    let mut ok_ty = Type::Int;
    for _ in 0..(MAX_TYPE_DEPTH - 1) {
        ok_ty = Type::List(Box::new(ok_ty));
    }
    let decls_ok = vec![Decl::VarDecl {
        name: var_x,
        ty: Some(ok_ty),
        val: Expr::List(vec![]),
    }];
    let program_ok = vec![Stmt::Service {
        name: name_s,
        decls: decls_ok,
    }];
    let mut classes = Env::new(None);
    assert!(check(&program_ok, &mut classes).is_ok());

    // Exceeding limit fails: depth == `MAX_TYPE_DEPTH` + `1`
    let mut fail_ty = Type::Int;
    for _ in 0..MAX_TYPE_DEPTH {
        fail_ty = Type::List(Box::new(fail_ty));
    }
    let decls_fail = vec![Decl::VarDecl {
        name: var_x,
        ty: Some(fail_ty),
        val: Expr::List(vec![]),
    }];
    let program_fail = vec![Stmt::Service {
        name: name_s,
        decls: decls_fail,
    }];
    let mut classes = Env::new(None);
    let res = check(&program_fail, &mut classes);
    assert_eq!(res, Err(Error::DepthLimitExceeded));
}

/// Verify basic primitive assignments
#[test]
fn test_primitive_checking() {
    let mut interner = Interner::new();
    let name_s = interner.insert("my_service");
    let var_x = interner.insert("x");
    let decls = vec![Decl::VarDecl {
        name: var_x,
        ty: Some(Type::Int),
        val: Expr::Literal {
            val: Value::Int { val: 42 },
        },
    }];
    let program = vec![Stmt::Service {
        name: name_s,
        decls,
    }];
    let mut classes = Env::new(None);
    let res = check(&program, &mut classes);
    assert!(res.is_ok());
    let st = classes.find(name_s).unwrap();
    assert_eq!(st.fields().find(var_x), Some(&Type::Int))
}

/// Verify type mismatches are rejected
#[test]
fn test_primitive_mismatch() {
    let mut interner = Interner::new();
    let name_s = interner.insert("my_service");
    let var_x = interner.insert("x");
    let decls = vec![Decl::VarDecl {
        name: var_x,
        ty: Some(Type::Int),
        val: Expr::Literal {
            val: Value::String {
                val: "bad".to_string(),
            },
        },
    }];
    let program = vec![Stmt::Service {
        name: name_s,
        decls,
    }];
    let mut classes = Env::new(None);
    let res = check(&program, &mut classes);
    assert_eq!(
        res,
        Err(Error::TypeMismatch {
            expected: Type::Int,
            found: Type::String,
        })
    )
}

/// Verify annotated function type checking and calls
#[test]
fn test_function_calls() {
    let mut interner = Interner::new();
    let name_s = interner.insert("my_service");
    let var_f = interner.insert("f");
    let var_x = interner.insert("x");
    let decls = vec![
        Decl::VarDecl {
            name: var_f,
            ty: Some(Type::Func(Box::new(Type::Int), Box::new(Type::Int))),
            val: Expr::Func {
                params: vec![Param {
                    name: interner.insert("a"),
                    ty: Some(Type::Int),
                }],
                body: Box::new(Expr::Variable {
                    name: interner.insert("a"),
                }),
                return_ty: Some(Type::Int),
            },
        },
        Decl::VarDecl {
            name: var_x,
            ty: Some(Type::Int),
            val: Expr::Call {
                func: Box::new(Expr::Variable { name: var_f }),
                args: vec![Expr::Literal {
                    val: Value::Int { val: 10 },
                }],
            },
        },
    ];
    let program = vec![Stmt::Service {
        name: name_s,
        decls,
    }];
    let mut classes = Env::new(None);
    let res = check(&program, &mut classes);
    assert!(res.is_ok())
}

/// Verify Error formatting produces expected messages
#[test]
fn test_error_display() {
    assert_eq!(
        Error::DepthLimitExceeded.to_string(),
        "depth limit exceeded."
    );
    assert_eq!(Error::CannotInferType.to_string(), "cannot infer type.");
    assert_eq!(Error::InvalidTupleArity.to_string(), "invalid tuple arity.");
    assert_eq!(Error::NotAFunction.to_string(), "not a function.");
}

/// Verify deeply nested type structures fail depth checking
#[test]
fn test_scope_depth_limit() {
    let mut classes = Env::new(None);
    let mut interner = Interner::new();
    let name_s = interner.insert("s");
    let name_x = interner.insert("x");
    let mut ty = Type::Int;
    for _ in 0..=MAX_TYPE_DEPTH {
        ty = Type::List(Box::new(ty));
    }
    let decls = vec![Decl::VarDecl {
        name: name_x,
        ty: Some(ty),
        val: Expr::Literal {
            val: Value::Int { val: 42 },
        },
    }];
    let program = vec![Stmt::Service {
        name: name_s,
        decls,
    }];
    let res = check(&program, &mut classes);
    assert_eq!(res, Err(Error::DepthLimitExceeded))
}

/// Verify deeply nested expressions trigger DepthLimitExceeded
#[test]
fn test_expression_depth_limit() {
    let mut classes = Env::new(None);
    let mut interner = Interner::new();
    let name_s = interner.insert("s");
    let name_x = interner.insert("x");
    let mut expr = Expr::Literal {
        val: Value::Int { val: 1 },
    };
    for _ in 0..=MAX_SCOPE_DEPTH {
        expr = Expr::Unop {
            op: UnOp::Neg,
            expr: Box::new(expr),
        };
    }
    let decls = vec![Decl::VarDecl {
        name: name_x,
        ty: Some(Type::Int),
        val: expr,
    }];
    let program = vec![Stmt::Service {
        name: name_s,
        decls,
    }];
    let res = check(&program, &mut classes);
    assert_eq!(res, Err(Error::DepthLimitExceeded))
}

/// Verify checking of action statements
#[test]
fn test_action_statements() {
    let mut classes = Env::new(None);
    let mut interner = Interner::new();
    let name_s = interner.insert("s");
    let program = vec![
        Stmt::Service {
            name: name_s,
            decls: vec![Decl::VarDecl {
                name: interner.insert("v"),
                ty: Some(Type::Int),
                val: Expr::Literal {
                    val: Value::Int { val: 1 },
                },
            }],
        },
        Stmt::Test {
            service_name: name_s,
            stmts: vec![
                ActionStmt::Let {
                    name: interner.insert("x"),
                    ty: None,
                    expr: Expr::Literal {
                        val: Value::Bool { val: true },
                    },
                },
                ActionStmt::Assert(
                    Expr::Variable {
                        name: interner.insert("x"),
                    },
                    "x".to_string(),
                ),
                ActionStmt::Assign {
                    name: interner.insert("v"),
                    expr: Expr::Literal {
                        val: Value::Int { val: 10 },
                    },
                },
                ActionStmt::For {
                    var: interner.insert("i"),
                    iterable: Expr::Range {
                        start: Box::new(Expr::Literal {
                            val: Value::Int { val: 0 },
                        }),
                        end: Box::new(Expr::Literal {
                            val: Value::Int { val: 5 },
                        }),
                    },
                    body: vec![ActionStmt::Expr(Expr::Variable {
                        name: interner.insert("i"),
                    })],
                },
            ],
        },
    ];
    let res = check(&program, &mut classes);
    assert!(res.is_ok())
}

/// Verify that invalid assign to unbound variable is rejected
#[test]
fn test_action_assign_unbound() {
    let mut classes = Env::new(None);
    let mut interner = Interner::new();
    let name_s = interner.insert("s");
    let program = vec![
        Stmt::Service {
            name: name_s,
            decls: vec![],
        },
        Stmt::Test {
            service_name: name_s,
            stmts: vec![ActionStmt::Assign {
                name: interner.insert("unbound"),
                expr: Expr::Literal {
                    val: Value::Int { val: 1 },
                },
            }],
        },
    ];
    let res = check(&program, &mut classes);
    assert_eq!(res, Err(Error::UnboundVariable(interner.insert("unbound"))))
}

/// Verify for loop rejects non-iterable values
#[test]
fn test_for_loop_non_iterable() {
    let mut classes = Env::new(None);
    let mut interner = Interner::new();
    let name_s = interner.insert("s");
    let program = vec![
        Stmt::Service {
            name: name_s,
            decls: vec![],
        },
        Stmt::Test {
            service_name: name_s,
            stmts: vec![ActionStmt::For {
                var: interner.insert("i"),
                iterable: Expr::Literal {
                    val: Value::Int { val: 42 },
                },
                body: vec![],
            }],
        },
    ];
    let res = check(&program, &mut classes);
    assert!(res.is_err())
}

/// Verify unary and binary operations checking
#[test]
fn test_operator_checking() {
    let mut classes = Env::new(None);
    let mut interner = Interner::new();
    let name_s = interner.insert("s");
    let program = vec![Stmt::Service {
        name: name_s,
        decls: vec![
            Decl::VarDecl {
                name: interner.insert("v1"),
                ty: Some(Type::Bool),
                val: Expr::Unop {
                    op: UnOp::Not,
                    expr: Box::new(Expr::Literal {
                        val: Value::Bool { val: false },
                    }),
                },
            },
            Decl::VarDecl {
                name: interner.insert("v2"),
                ty: Some(Type::Int),
                val: Expr::Binop {
                    op: BinOp::Sub,
                    expr1: Box::new(Expr::Literal {
                        val: Value::Int { val: 10 },
                    }),
                    expr2: Box::new(Expr::Literal {
                        val: Value::Int { val: 5 },
                    }),
                },
            },
            Decl::VarDecl {
                name: interner.insert("v3"),
                ty: Some(Type::Bool),
                val: Expr::Binop {
                    op: BinOp::Eq,
                    expr1: Box::new(Expr::Literal {
                        val: Value::Int { val: 5 },
                    }),
                    expr2: Box::new(Expr::Literal {
                        val: Value::Int { val: 5 },
                    }),
                },
            },
        ],
    }];
    let res = check(&program, &mut classes);
    assert!(res.is_ok())
}

/// Verify type checking of conditional expressions
#[test]
fn test_if_expression() {
    let mut classes = Env::new(None);
    let mut interner = Interner::new();
    let name_s = interner.insert("s");
    let program = vec![Stmt::Service {
        name: name_s,
        decls: vec![Decl::VarDecl {
            name: interner.insert("v"),
            ty: Some(Type::Int),
            val: Expr::If {
                cond: Box::new(Expr::Literal {
                    val: Value::Bool { val: true },
                }),
                expr1: Box::new(Expr::Literal {
                    val: Value::Int { val: 1 },
                }),
                expr2: Box::new(Expr::Literal {
                    val: Value::Int { val: 2 },
                }),
            },
        }],
    }];
    let res = check(&program, &mut classes);
    assert!(res.is_ok())
}

/// Verify list checking and empty list failures
#[test]
fn test_list_checking() {
    let mut classes = Env::new(None);
    let mut interner = Interner::new();
    let name_s = interner.insert("s");
    let program = vec![Stmt::Service {
        name: name_s,
        decls: vec![Decl::VarDecl {
            name: interner.insert("xs"),
            ty: None,
            val: Expr::List(vec![]),
        }],
    }];
    let res = check(&program, &mut classes);
    assert_eq!(res, Err(Error::CannotInferType));

    let program2 = vec![Stmt::Service {
        name: name_s,
        decls: vec![Decl::VarDecl {
            name: interner.insert("xs"),
            ty: Some(Type::List(Box::new(Type::Int))),
            val: Expr::List(vec![]),
        }],
    }];
    let mut classes2 = Env::new(None);
    assert!(check(&program2, &mut classes2).is_ok());
}

/// Verify tuple arity mismatch is rejected
#[test]
fn test_tuple_arity_mismatch() {
    let mut classes = Env::new(None);
    let mut interner = Interner::new();
    let name_s = interner.insert("s");
    let program = vec![Stmt::Service {
        name: name_s,
        decls: vec![Decl::VarDecl {
            name: interner.insert("t"),
            ty: Some(Type::Tuple(
                TupleType::new(vec![Type::Int, Type::Int]).unwrap(),
            )),
            val: Expr::Tuple {
                val: vec![Expr::Literal {
                    val: Value::Int { val: 1 },
                }],
            },
        }],
    }];
    let res = check(&program, &mut classes);
    assert_eq!(res, Err(Error::InvalidTupleArity))
}

/// Verify member dependencies that are cyclic fail inference
#[test]
fn test_circular_dependency() {
    let mut classes = Env::new(None);
    let mut interner = Interner::new();
    let name_s = interner.insert("s");
    let name_a = interner.insert("a");
    let name_b = interner.insert("b");
    let program = vec![Stmt::Service {
        name: name_s,
        decls: vec![
            Decl::VarDecl {
                name: name_a,
                ty: None,
                val: Expr::Variable { name: name_b },
            },
            Decl::VarDecl {
                name: name_b,
                ty: None,
                val: Expr::Variable { name: name_a },
            },
        ],
    }];
    let res = check(&program, &mut classes);
    assert_eq!(
        res,
        Err(Error::DependencyCycle {
            service: name_s,
            member: name_a
        })
    )
}

/// Verify member access across different services
#[test]
fn test_cross_service_member_access() {
    let mut classes = Env::new(None);
    let mut interner = Interner::new();
    let s1 = interner.insert("s1");
    let s2 = interner.insert("s2");
    let val_x = interner.insert("x");
    let program = vec![
        Stmt::Service {
            name: s1,
            decls: vec![Decl::VarDecl {
                name: val_x,
                ty: Some(Type::Int),
                val: Expr::Literal {
                    val: Value::Int { val: 42 },
                },
            }],
        },
        Stmt::Service {
            name: s2,
            decls: vec![Decl::VarDecl {
                name: interner.insert("y"),
                ty: Some(Type::Int),
                val: Expr::MemberAccess {
                    service_name: s1,
                    member_name: val_x,
                },
            }],
        },
    ];
    let res = check(&program, &mut classes);
    assert!(res.is_ok())
}

/// Verify html expressions are typed as string
#[test]
fn test_html_expression() {
    let mut classes = Env::new(None);
    let mut interner = Interner::new();
    let name_s = interner.insert("s");
    let mut builder = HtmlTemplateBuilder::new();
    builder.push_text("hello");
    let program = vec![Stmt::Service {
        name: name_s,
        decls: vec![Decl::VarDecl {
            name: interner.insert("h"),
            ty: Some(Type::String),
            val: Expr::Html(builder.build()),
        }],
    }];
    let res = check(&program, &mut classes);
    assert!(res.is_ok())
}

/// Verify select table and fold placeholders type check
#[test]
fn test_placeholder_expressions() {
    let mut classes = Env::new(None);
    let mut interner = Interner::new();
    let name_s = interner.insert("s");
    let program = vec![Stmt::Service {
        name: name_s,
        decls: vec![
            Decl::VarDecl {
                name: interner.insert("sel"),
                ty: Some(Type::List(Box::new(Type::Unit))),
                val: Expr::Select {
                    table_name: interner.insert("t"),
                    column_names: vec![],
                    where_clause: Box::new(Expr::Literal {
                        val: Value::Bool { val: true },
                    }),
                },
            },
            Decl::VarDecl {
                name: interner.insert("tab"),
                ty: Some(Type::Unit),
                val: Expr::Table {
                    schema: vec![],
                    records: vec![],
                },
            },
            Decl::VarDecl {
                name: interner.insert("fld"),
                ty: Some(Type::Unit),
                val: Expr::Fold {
                    table_name: interner.insert("t"),
                    column_name: interner.insert("c"),
                    operation: Box::new(Expr::Literal {
                        val: Value::Int { val: 0 },
                    }),
                    identity: Box::new(Expr::Literal {
                        val: Value::Int { val: 0 },
                    }),
                },
            },
        ],
    }];
    let res = check(&program, &mut classes);
    assert!(res.is_ok())
}

/// Verify that invalid function parameters trigger errors
#[test]
fn test_function_parameter_mismatches() {
    let mut classes = Env::new(None);
    let mut interner = Interner::new();
    let name_s = interner.insert("s");
    let p1 = vec![Stmt::Service {
        name: name_s,
        decls: vec![Decl::VarDecl {
            name: interner.insert("f"),
            ty: Some(Type::Func(Box::new(Type::Int), Box::new(Type::Int))),
            val: Expr::Func {
                params: vec![],
                body: Box::new(Expr::Literal {
                    val: Value::Int { val: 42 },
                }),
                return_ty: None,
            },
        }],
    }];
    assert!(check(&p1, &mut classes).is_err());

    let p2 = vec![Stmt::Service {
        name: name_s,
        decls: vec![Decl::VarDecl {
            name: interner.insert("f"),
            ty: Some(Type::Func(Box::new(Type::Int), Box::new(Type::Int))),
            val: Expr::Func {
                params: vec![
                    Param {
                        name: interner.insert("a"),
                        ty: None,
                    },
                    Param {
                        name: interner.insert("b"),
                        ty: None,
                    },
                ],
                body: Box::new(Expr::Literal {
                    val: Value::Int { val: 42 },
                }),
                return_ty: None,
            },
        }],
    }];
    assert!(check(&p2, &mut classes).is_err());
}

/// Verify function call arguments and function types are validated
#[test]
fn test_call_mismatches() {
    let mut classes = Env::new(None);
    let mut interner = Interner::new();
    let name_s = interner.insert("s");
    let p1 = vec![Stmt::Service {
        name: name_s,
        decls: vec![Decl::VarDecl {
            name: interner.insert("x"),
            ty: None,
            val: Expr::Call {
                func: Box::new(Expr::Literal {
                    val: Value::Int { val: 42 },
                }),
                args: vec![],
            },
        }],
    }];
    assert_eq!(check(&p1, &mut classes), Err(Error::NotAFunction));

    let p2 = vec![Stmt::Service {
        name: name_s,
        decls: vec![
            Decl::VarDecl {
                name: interner.insert("f"),
                ty: Some(Type::Func(Box::new(Type::Unit), Box::new(Type::Int))),
                val: Expr::Func {
                    params: vec![],
                    body: Box::new(Expr::Literal {
                        val: Value::Int { val: 42 },
                    }),
                    return_ty: None,
                },
            },
            Decl::VarDecl {
                name: interner.insert("x"),
                ty: None,
                val: Expr::Call {
                    func: Box::new(Expr::Variable {
                        name: interner.insert("f"),
                    }),
                    args: vec![Expr::Literal {
                        val: Value::Int { val: 1 },
                    }],
                },
            },
        ],
    }];
    assert!(check(&p2, &mut classes).is_err());
}

/// Verify that mixed lists yield a type mismatch error
#[test]
fn test_mixed_list_values() {
    let mut classes = Env::new(None);
    let mut interner = Interner::new();
    let name_s = interner.insert("s");
    let program = vec![Stmt::Service {
        name: name_s,
        decls: vec![Decl::VarDecl {
            name: interner.insert("xs"),
            ty: None,
            val: Expr::Literal {
                val: Value::List {
                    vals: vec![
                        Value::Int { val: 1 },
                        Value::String {
                            val: "bad".to_string(),
                        },
                    ],
                },
            },
        }],
    }];
    let res = check(&program, &mut classes);
    assert!(res.is_err())
}

/// Verify keyval expression type inference
#[test]
fn test_keyval_expression() {
    let mut classes = Env::new(None);
    let mut interner = Interner::new();
    let name_s = interner.insert("s");
    let program = vec![Stmt::Service {
        name: name_s,
        decls: vec![Decl::VarDecl {
            name: interner.insert("kv"),
            ty: Some(Type::Int),
            val: Expr::KeyVal {
                name: interner.insert("k"),
                value: Box::new(Expr::Literal {
                    val: Value::Int { val: 1 },
                }),
            },
        }],
    }];
    let res = check(&program, &mut classes);
    assert!(res.is_ok())
}

/// Verify tuple with single element is rejected during inference
#[test]
fn test_tuple_single_element_inference() {
    let mut classes = Env::new(None);
    let mut interner = Interner::new();
    let name_s = interner.insert("s");
    let program = vec![Stmt::Service {
        name: name_s,
        decls: vec![Decl::VarDecl {
            name: interner.insert("t"),
            ty: None,
            val: Expr::Tuple {
                val: vec![Expr::Literal {
                    val: Value::Int { val: 1 },
                }],
            },
        }],
    }];
    let res = check(&program, &mut classes);
    assert_eq!(res, Err(Error::InvalidTupleArity))
}

/// Verify tuple with single element is checked against Type::Unit
/// and rejected
#[test]
fn test_tuple_single_element_checking() {
    let mut classes = Env::new(None);
    let mut interner = Interner::new();
    let name_s = interner.insert("s");
    let program = vec![Stmt::Service {
        name: name_s,
        decls: vec![Decl::VarDecl {
            name: interner.insert("t"),
            ty: Some(Type::Unit),
            val: Expr::Tuple {
                val: vec![Expr::Literal {
                    val: Value::Int { val: 1 },
                }],
            },
        }],
    }];
    let res = check(&program, &mut classes);
    assert_eq!(res, Err(Error::InvalidTupleArity))
}

/// Verify that contradictory return and parameter type annotations on
/// lambdas in checking mode are rejected
#[test]
fn test_lambda_annotations_in_checking_mode() {
    let mut classes = Env::new(None);
    let mut interner = Interner::new();
    let name_s = interner.insert("s");
    // Test contradictory return type: expected int -> int,
    // annotated int -> string
    let program1 = vec![Stmt::Service {
        name: name_s,
        decls: vec![Decl::VarDecl {
            name: interner.insert("f"),
            ty: Some(Type::Func(Box::new(Type::Int), Box::new(Type::Int))),
            val: Expr::Func {
                params: vec![Param {
                    name: interner.insert("x"),
                    ty: Some(Type::Int),
                }],
                body: Box::new(Expr::Literal {
                    val: Value::Int { val: 5 },
                }),
                return_ty: Some(Type::String),
            },
        }],
    }];
    let res1 = check(&program1, &mut classes);
    assert_eq!(
        res1,
        Err(Error::TypeMismatch {
            expected: Type::Int,
            found: Type::String,
        })
    );
    // Test contradictory parameter type: expected int -> int,
    // annotated string -> int
    let program2 = vec![Stmt::Service {
        name: name_s,
        decls: vec![Decl::VarDecl {
            name: interner.insert("g"),
            ty: Some(Type::Func(Box::new(Type::Int), Box::new(Type::Int))),
            val: Expr::Func {
                params: vec![Param {
                    name: interner.insert("x"),
                    ty: Some(Type::String),
                }],
                body: Box::new(Expr::Literal {
                    val: Value::Int { val: 5 },
                }),
                return_ty: Some(Type::Int),
            },
        }],
    }];
    let res2 = check(&program2, &mut classes);
    assert_eq!(
        res2,
        Err(Error::TypeMismatch {
            expected: Type::Int,
            found: Type::String,
        })
    );
    // Test contradictory tuple parameter type: expected
    // (int, int) -> int, annotated (string, int) -> int
    let program3 = vec![Stmt::Service {
        name: name_s,
        decls: vec![Decl::VarDecl {
            name: interner.insert("h"),
            ty: Some(Type::Func(
                Box::new(Type::Tuple(
                    TupleType::new(vec![Type::Int, Type::Int]).unwrap(),
                )),
                Box::new(Type::Int),
            )),
            val: Expr::Func {
                params: vec![
                    Param {
                        name: interner.insert("x"),
                        ty: Some(Type::String),
                    },
                    Param {
                        name: interner.insert("y"),
                        ty: Some(Type::Int),
                    },
                ],
                body: Box::new(Expr::Literal {
                    val: Value::Int { val: 5 },
                }),
                return_ty: Some(Type::Int),
            },
        }],
    }];
    let res3 = check(&program3, &mut classes);
    assert_eq!(
        res3,
        Err(Error::TypeMismatch {
            expected: Type::Int,
            found: Type::String,
        })
    );
}

/// Verify that a local service referencing a member of an imported service
/// is fully type-checked when the unified AST contains the imported service's
/// declarations (as produced by on_node_startup before static_checks).
#[test]
fn test_import_member_access_resolves_with_unified_ast() {
    let mut interner = Interner::new();
    let remote_svc = interner.insert("na");
    let local_svc = interner.insert("nb");
    let remote_member = interner.insert("get_x");
    let local_val = interner.insert("val");

    // Program simulating unified_ast:
    //   import na           <- marks na as imported
    //   service na { pub def get_x = 42; }   <- from fetched source
    //   service nb { pub def val = na.get_x; }
    let program = vec![
        Stmt::Import {
            path: "na".to_string(),
            service_name: remote_svc,
        },
        Stmt::Service {
            name: remote_svc,
            decls: vec![Decl::DefDecl {
                name: remote_member,
                ty: None,
                is_pub: true,
                val: Expr::Literal {
                    val: Value::Int { val: 42 },
                },
            }],
        },
        Stmt::Service {
            name: local_svc,
            decls: vec![Decl::DefDecl {
                name: local_val,
                ty: None,
                is_pub: false,
                val: Expr::MemberAccess {
                    service_name: remote_svc,
                    member_name: remote_member,
                },
            }],
        },
    ];
    let mut classes = Env::new(None);
    let res = check(&program, &mut classes);
    assert!(res.is_ok())
}

/// Verify that a `MemberAccess` expression targeting a service that is
/// neither declared locally nor imported still produces a hard error.
/// This is the negative/defensive case ensuring the import bypass is
/// precisely scoped to services registered via `Stmt::Import`
#[test]
fn test_unknown_service_member_access_errors() {
    let mut interner = Interner::new();
    let ghost_svc = interner.insert("ghost");
    let local_svc = interner.insert("nb");
    let ghost_member = interner.insert("val");
    let local_def = interner.insert("x");

    // Program: `service nb { pub def x = ghost.val; }` — `ghost` is not imported
    let program = vec![Stmt::Service {
        name: local_svc,
        decls: vec![Decl::DefDecl {
            name: local_def,
            ty: None,
            is_pub: false,
            val: Expr::MemberAccess {
                service_name: ghost_svc,
                member_name: ghost_member,
            },
        }],
    }];
    let mut classes = Env::new(None);
    let res = check(&program, &mut classes);
    assert_eq!(res, Err(Error::UnboundVariable(ghost_svc)))
}
