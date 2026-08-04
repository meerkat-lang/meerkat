use crate::ast::{BinOp, Expr, UnOp, Value};
use crate::runtime::interner::Symbol;
use crate::runtime::txn::{Transaction, WaitKey};
use crate::runtime::Manager;
use std::collections::HashSet;

#[derive(Debug)]
pub enum EvalError {
    TypeError(String),
    VarNotFound(String),
    ServiceNotFound(String),
    LocalDispatchFailed(String),
    RemoteDispatchFailed(String),
    NotImplemented,
    WaitDieAbort(String),
    WaitOn(WaitKey),
    AssertionError(String),
    RuntimeError(String),
}

/// Implement the `Display` trait for the `EvalError` type
///
/// This prints user-facing descriptions of evaluator errors
impl std::fmt::Display for EvalError {
    /// Format the evaluation error for display
    ///
    /// Args:
    ///     `f` (`&mut std::fmt::Formatter<'_>`): The formatter target
    ///
    /// Returns:
    ///     `std::fmt::Result`: The result of the formatting operation
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EvalError::TypeError(s) => write!(f, "Type error: {}", s),
            EvalError::VarNotFound(s) => write!(f, "Variable not found: {}", s),
            EvalError::ServiceNotFound(s) => write!(f, "Service not found: {}", s),
            EvalError::LocalDispatchFailed(s) => write!(f, "Local dispatch failed: {}", s),
            EvalError::RemoteDispatchFailed(s) => write!(f, "Remote dispatch failed: {}", s),
            EvalError::NotImplemented => write!(f, "Not implemented"),
            EvalError::WaitDieAbort(s) => write!(f, "Wait-die abort: {}", s),
            EvalError::WaitOn(key) => {
                write!(f, "Wait-die wait on {:?}", key)
            }
            EvalError::AssertionError(s) => write!(f, "Assertion failed: {}", s),
            EvalError::RuntimeError(s) => write!(f, "Runtime error: {}", s),
        }
    }
}

impl std::error::Error for EvalError {}

/// Evaluation context represented by `EvalContext`
///
/// Holds the stable execution state that does not change per call
/// frame. Passed as `&mut` so the `manager` can be updated
///
/// The `env` parameter is kept separate since it changes at each
/// function call boundary
pub struct EvalContext<'a> {
    pub manager: &'a mut Manager,
    pub service_name: Symbol,
    /// Active transaction if evaluation is happening inside one
    pub txn: Option<&'a mut Transaction>,
}

#[async_recursion::async_recursion]
pub async fn eval(
    expr: &Expr,
    env: &[(Symbol, Value)],
    ctx: &mut EvalContext<'_>,
) -> Result<Value, EvalError> {
    match expr {
        Expr::Literal { val } => Ok(val.clone()),

        Expr::Html(template) => {
            // #39: the evaluator owns evaluation (it has the async context) and
            // the html module owns assembling the value, keeping the template
            // and markup representation encapsulated. Evaluate each embedded
            // expression in order, then hand the values to the template to
            // render. Because the embedded expressions are ordinary Exprs,
            // dependency analysis has tracked them, so the html def is
            // recomputed by propagation when a dependency changes.
            let mut values = Vec::new();
            for e in template.embedded_exprs() {
                values.push(eval(e, env, ctx).await?);
            }
            template.render(&values).map(Value::Html).ok_or_else(|| {
                EvalError::RuntimeError("html render: value count mismatch".to_string())
            })
        }

        Expr::Call { func, args } => {
            let func_val = eval(func, env, ctx).await?;
            let mut arg_vals = Vec::new();
            for arg in args {
                arg_vals.push(eval(arg, env, ctx).await?);
            }
            match func_val {
                Value::Closure {
                    params,
                    body,
                    env: closure_env,
                    service_name: closure_svc,
                    ..
                } => {
                    let mut new_env = closure_env.clone();
                    for (param, arg_val) in params.iter().zip(arg_vals) {
                        new_env.push((param.name, arg_val));
                    }
                    eval(
                        &body,
                        &new_env,
                        &mut EvalContext {
                            manager: ctx.manager,
                            service_name: closure_svc,
                            txn: ctx.txn.as_deref_mut(),
                        },
                    )
                    .await
                }
                _ => Err(EvalError::TypeError(
                    "Attempting to call a non-function value".to_string(),
                )),
            }
        }

        &Expr::Variable { name } => {
            for (var_name, var_val) in env.iter().rev() {
                if *var_name == name {
                    return Ok(var_val.clone());
                }
            }
            ctx.manager
                .lookup(name, ctx.service_name, ctx.txn.as_deref_mut())
                .await
        }

        Expr::Binop { op, expr1, expr2 } => {
            let val1 = eval(expr1, env, ctx).await?;
            let val2 = eval(expr2, env, ctx).await?;
            if *op == BinOp::Eq {
                match (&val1, &val2) {
                    (Value::List { vals: v1 }, Value::List { vals: v2 }) => {
                        return Ok(Value::Bool { val: v1 == v2 });
                    }
                    (Value::Range { start: s1, end: e1 }, Value::Range { start: s2, end: e2 }) => {
                        return Ok(Value::Bool {
                            val: s1 == s2 && e1 == e2,
                        });
                    }
                    (Value::List { .. }, Value::Range { .. })
                    | (Value::Range { .. }, Value::List { .. }) => {
                        if let (Some(l1), Some(l2)) =
                            (val1.to_list_elements(), val2.to_list_elements())
                        {
                            return Ok(Value::Bool { val: l1 == l2 });
                        }
                    }
                    _ => {}
                }
            }
            match (op, val1, val2) {
                (BinOp::Add, Value::Int { val: v1 }, Value::Int { val: v2 }) => {
                    Ok(Value::Int { val: v1 + v2 })
                }
                (BinOp::Sub, Value::Int { val: v1 }, Value::Int { val: v2 }) => {
                    Ok(Value::Int { val: v1 - v2 })
                }
                (BinOp::Mul, Value::Int { val: v1 }, Value::Int { val: v2 }) => {
                    Ok(Value::Int { val: v1 * v2 })
                }
                (BinOp::Div, Value::Int { val: v1 }, Value::Int { val: v2 }) => {
                    // NOTE: Division by zero and division overflow (i32::MIN / -1) are the only
                    // integer arithmetic operations that panic in Rust release mode.
                    // If a modulo (%) operator is ever implemented in the future, it must
                    // also include these identical bounds checks (x % 0 and i32::MIN % -1)
                    // to prevent panics.
                    let val = v1.checked_div(v2).ok_or_else(|| {
                        EvalError::RuntimeError(if v2 == 0 {
                            "Division by zero".to_string()
                        } else {
                            "Integer overflow".to_string()
                        })
                    })?;
                    Ok(Value::Int { val })
                }
                (BinOp::Eq, Value::Int { val: v1 }, Value::Int { val: v2 }) => {
                    Ok(Value::Bool { val: v1 == v2 })
                }
                (BinOp::Lt, Value::Int { val: v1 }, Value::Int { val: v2 }) => {
                    Ok(Value::Bool { val: v1 < v2 })
                }
                (BinOp::Gt, Value::Int { val: v1 }, Value::Int { val: v2 }) => {
                    Ok(Value::Bool { val: v1 > v2 })
                }
                (BinOp::And, Value::Bool { val: v1 }, Value::Bool { val: v2 }) => {
                    Ok(Value::Bool { val: v1 && v2 })
                }
                (BinOp::Or, Value::Bool { val: v1 }, Value::Bool { val: v2 }) => {
                    Ok(Value::Bool { val: v1 || v2 })
                }
                _ => Err(EvalError::TypeError(
                    "Type error in binary operation".to_string(),
                )),
            }
        }

        Expr::Unop { op, expr } => {
            let val = eval(expr, env, ctx).await?;
            match (op, val) {
                (UnOp::Neg, Value::Int { val: v }) => Ok(Value::Int { val: -v }),
                (UnOp::Not, Value::Bool { val: v }) => Ok(Value::Bool { val: !v }),
                _ => Err(EvalError::TypeError(
                    "Type error in unary operation".to_string(),
                )),
            }
        }

        Expr::If { cond, expr1, expr2 } => {
            let cond_val = eval(cond, env, ctx).await?;
            match cond_val {
                Value::Bool { val: true } => eval(expr1, env, ctx).await,
                Value::Bool { val: false } => eval(expr2, env, ctx).await,
                _ => Err(EvalError::TypeError(
                    "Condition must be boolean".to_string(),
                )),
            }
        }

        Expr::Func {
            params,
            body,
            return_ty,
        } => {
            let var_binded: HashSet<Symbol> = params.iter().map(|p| p.name).collect();
            let free_vars = body.free_var(&HashSet::new(), &var_binded);
            let captured_env: Vec<(Symbol, Value)> = env
                .iter()
                .filter(|(name, _)| {
                    free_vars.contains(name)
                        && !ctx
                            .manager
                            .services
                            .get(&ctx.service_name)
                            .map(|s| s.vars.contains_key(name) || s.defs.contains_key(name))
                            .unwrap_or(false)
                })
                .cloned()
                .collect();
            Ok(Value::Closure {
                params: params.clone(),
                body: body.clone(),
                env: captured_env,
                service_name: ctx.service_name,
                return_ty: return_ty.clone(),
            })
        }

        Expr::Action(stmts) => {
            // Use `free_var` on the `Action` expression itself to find
            // free variables
            // Service `vars` or `defs` are looked up fresh via the
            // `manager` at execution time
            let action_expr = Expr::Action(stmts.clone());
            let free_vars = action_expr.free_var(
                &std::collections::HashSet::new(),
                &std::collections::HashSet::new(),
            );
            let captured_env: Vec<(Symbol, Value)> = env
                .iter()
                .filter(|(name, _)| {
                    free_vars.contains(name)
                        && !ctx
                            .manager
                            .services
                            .get(&ctx.service_name)
                            .map(|s| s.vars.contains_key(name) || s.defs.contains_key(name))
                            .unwrap_or(false)
                })
                .cloned()
                .collect();
            Ok(Value::ActionClosure {
                stmts: stmts.clone(),
                env: captured_env,
                // Stamp the action with its owning service's identity
                // (which embeds this node's address), so it stays
                // executable wherever the `closure` travels
                service_net_id: ctx.manager.service_net_id_for_name(ctx.service_name),
            })
        }

        &Expr::MemberAccess {
            service_name,
            member_name,
        } => {
            // #24: during a reactive update we check the cache first. If this
            // (service, member) was already fetched for the def being recomputed,
            // use the cached value instead of doing a lookup (which for a remote
            // service would be a network round-trip).
            if let Some(v) = ctx
                .manager
                .reactive_cache
                .as_ref()
                .and_then(|c| c.get(&(service_name, member_name)))
                .cloned()
            {
                return Ok(v);
            }
            // The `Manager` determines whether the service is local or
            // remote
            ctx.manager
                .lookup(member_name, service_name, ctx.txn.as_deref_mut())
                .await
        }
        Expr::Tuple { .. }
        | Expr::KeyVal { .. }
        | Expr::Select { .. }
        | Expr::Table { .. }
        | Expr::Fold { .. } => Err(EvalError::NotImplemented),
        Expr::List(exprs) => {
            let mut vals = Vec::new();
            for expr in exprs {
                vals.push(eval(expr, env, ctx).await?);
            }
            Ok(Value::List { vals })
        }
        Expr::Range { start, end } => {
            let start_val = eval(start, env, ctx).await?;
            let end_val = eval(end, env, ctx).await?;
            match (start_val, end_val) {
                (Value::Int { val: s }, Value::Int { val: e }) => {
                    Ok(Value::Range { start: s, end: e })
                }
                _ => Err(EvalError::TypeError(
                    "Range bounds must evaluate to integers".to_string(),
                )),
            }
        }
    }
}

/// Unit tests for the evaluator
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{ActionStmt, BinOp, Expr, Value};
    use crate::runtime::interner::Interner;
    use crate::runtime::tt::Param;
    use crate::runtime::Manager;

    /// Verify that evaluating a literal expression returns the expected runtime `Value`
    #[tokio::test]
    async fn test_literal() {
        let mut manager = Manager::new(Interner::new());
        let mut ctx = EvalContext {
            manager: &mut manager,
            service_name: Symbol::empty(),
            txn: None,
        };
        let expr = Expr::Literal {
            val: Value::Int { val: 42 },
        };
        let result = eval(&expr, &[], &mut ctx).await.unwrap();
        assert_eq!(result, Value::Int { val: 42 });
    }

    /// Verify that evaluating a binary add operation returns the sum of the numbers
    #[tokio::test]
    async fn test_binop_add() {
        let mut manager = Manager::new(Interner::new());
        let mut ctx = EvalContext {
            manager: &mut manager,
            service_name: Symbol::empty(),
            txn: None,
        };
        let expr = Expr::Binop {
            op: BinOp::Add,
            expr1: Box::new(Expr::Literal {
                val: Value::Int { val: 2 },
            }),
            expr2: Box::new(Expr::Literal {
                val: Value::Int { val: 3 },
            }),
        };
        let result = eval(&expr, &[], &mut ctx).await.unwrap();
        assert_eq!(result, Value::Int { val: 5 });
    }

    /// Verify that defining a function and calling it yields the expected computed `Value`
    #[tokio::test]
    async fn test_func_and_call() {
        let mut manager = Manager::new(Interner::new());
        let x = manager.interner.insert("x");
        let mut ctx = EvalContext {
            manager: &mut manager,
            service_name: Symbol::empty(),
            txn: None,
        };
        let func_expr = Expr::Func {
            params: vec![Param { name: x, ty: None }],
            body: Box::new(Expr::Binop {
                op: BinOp::Add,
                expr1: Box::new(Expr::Variable { name: x }),
                expr2: Box::new(Expr::Literal {
                    val: Value::Int { val: 10 },
                }),
            }),
            return_ty: None,
        };
        let call_expr = Expr::Call {
            func: Box::new(func_expr),
            args: vec![Expr::Literal {
                val: Value::Int { val: 5 },
            }],
        };
        let result = eval(&call_expr, &[], &mut ctx).await.unwrap();
        assert_eq!(result, Value::Int { val: 15 });
    }

    /// Verify that evaluating an action statement block produces an action `Value::ActionClosure`
    #[tokio::test]
    async fn test_action_creation() {
        let mut manager = Manager::new(Interner::new());
        let var_name = manager.interner.insert("var_name");
        let mut ctx = EvalContext {
            manager: &mut manager,
            service_name: Symbol::empty(),
            txn: None,
        };
        let action_expr = Expr::Action(vec![ActionStmt::Assign {
            name: var_name,
            expr: Expr::Literal {
                val: Value::Int { val: 5 },
            },
        }]);
        let result = eval(&action_expr, &[], &mut ctx).await.unwrap();
        match result {
            Value::ActionClosure { stmts, .. } => assert_eq!(stmts.len(), 1),
            _ => panic!("Expected ActionClosure"),
        }
    }

    /// Verify that created `Value::Closure`s capture only their referenced free variables from the environment
    #[tokio::test]
    async fn test_closure_captures_only_free_vars() {
        let mut manager = Manager::new(Interner::new());
        let v1 = manager.interner.insert("v1");
        let v2 = manager.interner.insert("v2");
        let v3 = manager.interner.insert("v3");
        let v4 = manager.interner.insert("v4");
        let mut ctx = EvalContext {
            manager: &mut manager,
            service_name: Symbol::empty(),
            txn: None,
        };
        let env = vec![
            (v1, Value::Int { val: 1 }),
            (v2, Value::Int { val: 2 }),
            (v3, Value::Int { val: 3 }),
        ];
        let func_expr = Expr::Func {
            params: vec![Param { name: v4, ty: None }],
            body: Box::new(Expr::Binop {
                op: BinOp::Add,
                expr1: Box::new(Expr::Variable { name: v4 }),
                expr2: Box::new(Expr::Variable { name: v1 }),
            }),
            return_ty: None,
        };
        let result = eval(&func_expr, &env, &mut ctx).await.unwrap();
        match result {
            Value::Closure {
                params,
                body: _,
                env: captured_env,
                ..
            } => {
                assert_eq!(params.len(), 1);
                assert_eq!(captured_env.len(), 1);
                assert_eq!(captured_env[0].0, v1);
                assert_eq!(captured_env[0].1, Value::Int { val: 1 });
            }
            _ => panic!("Expected Closure"),
        }
    }

    #[tokio::test]
    async fn test_division_by_zero_returns_runtime_error() {
        let mut manager = Manager::new(Interner::new());
        let mut ctx = EvalContext {
            manager: &mut manager,
            service_name: Symbol::empty(),
            txn: None,
        };
        let expr = Expr::Binop {
            op: BinOp::Div,
            expr1: Box::new(Expr::Literal {
                val: Value::Int { val: 42 },
            }),
            expr2: Box::new(Expr::Literal {
                val: Value::Int { val: 0 },
            }),
        };
        let result = eval(&expr, &[], &mut ctx).await;
        match result {
            Err(EvalError::RuntimeError(ref s)) => assert_eq!(s, "Division by zero"),
            other => panic!("Expected Err(EvalError::RuntimeError), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_division_overflow_returns_runtime_error() {
        let mut manager = Manager::new(Interner::new());
        let mut ctx = EvalContext {
            manager: &mut manager,
            service_name: Symbol::empty(),
            txn: None,
        };
        let expr = Expr::Binop {
            op: BinOp::Div,
            expr1: Box::new(Expr::Literal {
                val: Value::Int { val: i32::MIN },
            }),
            expr2: Box::new(Expr::Literal {
                val: Value::Int { val: -1 },
            }),
        };
        let result = eval(&expr, &[], &mut ctx).await;
        match result {
            Err(EvalError::RuntimeError(ref s)) => assert_eq!(s, "Integer overflow"),
            other => panic!("Expected Err(EvalError::RuntimeError), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_list_and_range_evaluation() {
        let mut manager = Manager::new(Interner::new());
        let mut ctx = EvalContext {
            manager: &mut manager,
            service_name: Symbol::empty(),
            txn: None,
        };

        // Test list literal [1, 2, 3]
        let list_expr = Expr::List(vec![
            Expr::Literal {
                val: Value::Int { val: 1 },
            },
            Expr::Literal {
                val: Value::Int { val: 2 },
            },
            Expr::Literal {
                val: Value::Int { val: 3 },
            },
        ]);
        let list_val = eval(&list_expr, &[], &mut ctx).await.unwrap();
        assert_eq!(
            list_val,
            Value::List {
                vals: vec![
                    Value::Int { val: 1 },
                    Value::Int { val: 2 },
                    Value::Int { val: 3 },
                ]
            }
        );

        // Test range literal 0..3
        let range_expr = Expr::Range {
            start: Box::new(Expr::Literal {
                val: Value::Int { val: 0 },
            }),
            end: Box::new(Expr::Literal {
                val: Value::Int { val: 3 },
            }),
        };
        let range_val = eval(&range_expr, &[], &mut ctx).await.unwrap();
        assert_eq!(range_val, Value::Range { start: 0, end: 3 });

        // Test range evaluates to list elements correctly
        let elements = range_val.to_list_elements().unwrap();
        assert_eq!(
            elements,
            vec![
                Value::Int { val: 0 },
                Value::Int { val: 1 },
                Value::Int { val: 2 },
            ]
        );

        // Test equality between list and range
        let eq_expr = Expr::Binop {
            op: BinOp::Eq,
            expr1: Box::new(list_expr),
            expr2: Box::new(range_expr),
        };
        let eq_val = eval(&eq_expr, &[], &mut ctx).await.unwrap();
        assert_eq!(eq_val, Value::Bool { val: false });

        // Test equality between range 1..4 and list [1, 2, 3]
        let range_expr_2 = Expr::Range {
            start: Box::new(Expr::Literal {
                val: Value::Int { val: 1 },
            }),
            end: Box::new(Expr::Literal {
                val: Value::Int { val: 4 },
            }),
        };
        let list_expr_2 = Expr::List(vec![
            Expr::Literal {
                val: Value::Int { val: 1 },
            },
            Expr::Literal {
                val: Value::Int { val: 2 },
            },
            Expr::Literal {
                val: Value::Int { val: 3 },
            },
        ]);
        let eq_expr_2 = Expr::Binop {
            op: BinOp::Eq,
            expr1: Box::new(list_expr_2),
            expr2: Box::new(range_expr_2),
        };
        let eq_val_2 = eval(&eq_expr_2, &[], &mut ctx).await.unwrap();
        assert_eq!(eq_val_2, Value::Bool { val: true });
    }

    /// #39: an html template evaluates to a Value::Html whose rendered markup
    /// has the interpolation substituted from the environment.
    #[tokio::test]
    async fn test_html_renders_with_interpolation() {
        use crate::runtime::parser::parse_template;
        let mut manager = Manager::new(Interner::new());
        let count = manager.interner.insert("count");
        let template = parse_template("The count is {count}.", &mut manager.interner)
            .expect("parse html template");
        let expr = Expr::Html(template);
        let env = vec![(count, Value::Int { val: 2 })];
        let mut ctx = EvalContext {
            manager: &mut manager,
            service_name: Symbol::empty(),
            txn: None,
        };
        let result = eval(&expr, &env, &mut ctx).await.unwrap();
        match result {
            Value::Html(html) => assert_eq!(html.as_str(), "The count is 2."),
            other => panic!("expected Value::Html, got {:?}", other),
        }
    }
}
