use super::evaluator::{eval, EvalContext, EvalError};
use crate::ast::{ActionStmt, Value};
use crate::runtime::interner::Symbol;
use crate::runtime::txn::Transaction;
use crate::runtime::Manager;

/// The effect produced by executing a single statement using `ExecuteEffect`
pub enum ExecuteEffect {
    /// Statement completed with no binding or value
    None,
    /// A `let` binding: the name and value to add to `env`
    Binding(Symbol, Value),
    /// An expression statement was evaluated: the result value
    ExprValue(Value),
}

#[async_recursion::async_recursion]
pub async fn execute(
    stmt: &ActionStmt,
    env: &[(Symbol, Value)],
    manager: &mut Manager,
    service_name: Symbol,
    mut txn: Option<&mut Transaction>,
) -> Result<ExecuteEffect, EvalError> {
    match stmt {
        ActionStmt::Assign { name, expr } => {
            let value = eval(
                expr,
                env,
                &mut EvalContext {
                    manager,
                    service_name,
                    txn: txn.as_deref_mut(),
                },
            )
            .await?;
            manager.assign(service_name, *name, value, txn).await?;
            Ok(ExecuteEffect::None)
        }
        ActionStmt::Do(expr) => {
            let val = eval(
                expr,
                env,
                &mut EvalContext {
                    manager,
                    service_name,
                    txn: txn.as_deref_mut(),
                },
            )
            .await?;
            match val {
                Value::ActionClosure {
                    stmts,
                    env: closure_env,
                    service_net_id,
                } => {
                    // `service_name_for_net_id` tells us whether the action's
                    // service is local (`Some` => its in-scope name) or remote
                    // (`None`)
                    // For remote, we ship to the owning node using the address
                    // embedded in the `ServiceNetId`, so it runs even if not
                    // imported into this scope
                    match manager.service_name_for_net_id(&service_net_id) {
                        Some(svc_name) => {
                            let mut exec_env = closure_env.clone();
                            for s in &stmts {
                                if let ExecuteEffect::Binding(name, val) =
                                    execute(s, &exec_env, manager, svc_name, txn.as_deref_mut())
                                        .await?
                                {
                                    exec_env.push((name, val));
                                }
                            }
                        }
                        None => {
                            // Ship to its owning node under the shared
                            // transaction; the remote node executes
                            // and holds until our `commit` or `abort`
                            manager
                                .remote_action(
                                    &service_net_id,
                                    stmts,
                                    closure_env,
                                    txn.as_deref_mut(),
                                )
                                .await?;
                        }
                    }
                    Ok(ExecuteEffect::None)
                }
                _ => Err(EvalError::TypeError("do expects an action".to_string())),
            }
        }
        ActionStmt::Assert(expr, text) => {
            let val = eval(
                expr,
                env,
                &mut EvalContext {
                    manager,
                    service_name,
                    txn: txn.as_deref_mut(),
                },
            )
            .await?;
            match val {
                Value::Bool { val: true } => Ok(ExecuteEffect::None),
                Value::Bool { val: false } => Err(EvalError::AssertionError(text.clone())),
                Value::Int { .. }
                | Value::String { .. }
                | Value::Html(..)
                | Value::Closure { .. }
                | Value::ActionClosure { .. }
                | Value::List { .. }
                | Value::Range { .. } => {
                    Err(EvalError::TypeError("assert expects a boolean".to_string()))
                }
            }
        }
        ActionStmt::Let { name, ty: _, expr } => {
            let val = eval(
                expr,
                env,
                &mut EvalContext {
                    manager,
                    service_name,
                    txn: txn.as_deref_mut(),
                },
            )
            .await?;
            Ok(ExecuteEffect::Binding(*name, val))
        }
        ActionStmt::Expr(expr) => {
            let val = eval(
                expr,
                env,
                &mut EvalContext {
                    manager,
                    service_name,
                    txn: txn.as_deref_mut(),
                },
            )
            .await?;
            Ok(ExecuteEffect::ExprValue(val))
        }
        ActionStmt::Insert { .. } => Err(EvalError::NotImplemented),
        ActionStmt::For {
            var,
            iterable,
            body,
        } => {
            let iterable_val = eval(
                iterable,
                env,
                &mut EvalContext {
                    manager,
                    service_name,
                    txn: txn.as_deref_mut(),
                },
            )
            .await?;
            let elements: Box<dyn Iterator<Item = Value> + Send> = match iterable_val {
                Value::List { vals } => Box::new(vals.into_iter()),
                Value::Range { start, end } => {
                    Box::new((start..end).map(|v| Value::Int { val: v }))
                }
                _ => {
                    return Err(EvalError::TypeError(
                        "for loop iterable must be a list or range".to_string(),
                    ))
                }
            };
            let mut loop_env = env.to_vec();
            let base_len = loop_env.len();
            for elem in elements {
                loop_env.truncate(base_len);
                loop_env.push((*var, elem));
                for s in body {
                    if let ExecuteEffect::Binding(name, val) =
                        execute(s, &loop_env, manager, service_name, txn.as_deref_mut()).await?
                    {
                        loop_env.push((name, val));
                    }
                }
            }
            Ok(ExecuteEffect::None)
        }
    }
}

/// Unit tests for the executor
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Expr;
    use crate::runtime::interner::Interner;

    /// Verify that `EvalError::AssertionError` formats
    /// correctly and is returned upon assertion failure
    #[tokio::test]
    async fn test_assertion_error_execution_and_formatting() {
        let err = EvalError::AssertionError("x == 5".to_string());
        assert_eq!(err.to_string(), "Assertion failed: x == 5");

        let mut manager = Manager::new(Interner::new());
        let stmt = ActionStmt::Assert(
            Expr::Literal {
                val: Value::Bool { val: false },
            },
            "x == 5".to_string(),
        );

        let res = execute(&stmt, &[], &mut manager, Symbol::empty(), None).await;

        match res {
            Ok(_) => panic!("Expected assertion to fail"),
            Err(err) => match err {
                EvalError::AssertionError(text) => {
                    assert_eq!(text, "x == 5");
                }
                EvalError::TypeError(_)
                | EvalError::VarNotFound(_)
                | EvalError::ServiceNotFound(_)
                | EvalError::LocalDispatchFailed(_)
                | EvalError::RemoteDispatchFailed(_)
                | EvalError::NotImplemented
                | EvalError::WaitDieAbort(_)
                | EvalError::WaitOn(_)
                | EvalError::RuntimeError(_) => {
                    panic!("Expected EvalError::AssertionError")
                }
            },
        }
    }
}
