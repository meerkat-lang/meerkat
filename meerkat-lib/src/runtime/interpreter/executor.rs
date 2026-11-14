use super::evaluator::{call_target, eval, EvalContext, EvalError};
use crate::ast::{ActionStmt, Decl, DefForm, Expr, Value};
use crate::net::codec;
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

/// Execute a statement list in order, threading `let` bindings through `env`
///
/// Every caller that runs a sequence of `ActionStmt`s shares this shape: a
/// `let` extends the environment seen by the statements after it, any other
/// statement runs for its effect, and the first error ends the sequence.
///
/// Returns at the first error, leaving `env` holding the bindings made before
/// it. Callers that need that error as a value rather than as an early return
/// -- the transactional paths, which must still commit or abort before they
/// return it -- take it with `.await.err()`.
pub async fn execute_seq(
    stmts: &[ActionStmt],
    env: &mut Vec<(Symbol, Value)>,
    manager: &mut Manager,
    service_name: Symbol,
    mut txn: Option<&mut Transaction>,
) -> Result<(), EvalError> {
    for stmt in stmts {
        if let ExecuteEffect::Binding(name, val) =
            execute(stmt, env, manager, service_name, txn.as_deref_mut()).await?
        {
            env.push((name, val));
        }
    }
    Ok(())
}

// async_recursion adds a bare #[must_use] to a fn that returns a boxed Future.
#[allow(clippy::double_must_use)]
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
            // A `do` reached with no transaction in progress -- one written
            // directly in a `@test` block, which is not itself a transaction,
            // or typed as a bare statement at the REPL -- runs in a fresh
            // transaction of its own and commits before the next statement. A
            // `do` reached from inside an action that is already under a
            // transaction joins that one instead, so composed and cross-service
            // actions stay atomic.
            if txn.is_none() {
                return manager
                    .execute_action_with_txn(service_name, std::slice::from_ref(stmt), env)
                    .await
                    .map(|()| ExecuteEffect::None);
            }
            // Issue #20, option B: `do s.a` on a remote service `s` is one operation at
            // `s`. Ship the one statement `do a` to the owner instead of first fetching
            // the action's body with a lookup. The owner reads `a` under this
            // transaction, taking the read lock the lookup used to take, and runs it
            // there, so the body never crosses the network. This holds only for an
            // action written in place in `s`'s own declaration, which necessarily
            // belongs to `s`. A member that holds another service's action, such as a
            // forwarding `pub def act = s2.inc;`, would run `s2`'s action at `s`,
            // which may be this very node, and a call back into a node that is
            // waiting for a reply waits for the reply timeout (#213). It is fetched
            // as before. So is any other spelling (`let f = s.a; do f;`,
            // `do s.f(x);`). The answer comes from this node's copy of `s`, taken
            // at import, so an update made at `s`'s node that turns `a` into a
            // forward is not seen here and `do a` can still be sent by name.
            if let Expr::MemberAccess {
                service_name: target,
                member_name,
            } = expr
            {
                if manager.remote_services.contains_key(target)
                    && matches!(
                        manager.member_decl(*target, *member_name),
                        Some(Decl::DefDecl {
                            val: Expr::Action(_),
                            ..
                        })
                    )
                {
                    let owner = manager.owner_net_id(*target);
                    manager
                        .remote_action(
                            &owner,
                            vec![ActionStmt::Do(Expr::Variable { name: *member_name })],
                            Vec::new(),
                            txn.as_deref_mut(),
                        )
                        .await?;
                    return Ok(ExecuteEffect::None);
                }
            }
            // The same for `do s.a(e1, ..)` when this node's copy of `s` declares `a` as
            // `action a(..)`: the copy shows that `a`
            // belonged to `s` when it was imported, so the caller evaluates the
            // arguments and sends the one statement `do s.a(__arg0, ..)` with their
            // values in its environment, one round trip instead of a lookup
            // that returns the action followed by shipping its body back. A special
            // def whose result is an action is not sent this way: which action it
            // returns is the owner's decision, so the call goes through the evaluator
            // and the `do` below runs the result at its owner. The callee is sent as a
            // member access, which never consults the environment, so a member named
            // like an argument (`__argK`) cannot be shadowed on the owner.
            if let Expr::Call { func, args } = expr {
                if let Some((target, member)) = call_target(func, env, service_name) {
                    if manager.remote_services.contains_key(&target)
                        && matches!(
                            manager.member_decl(target, member),
                            Some(Decl::DefDecl {
                                form: DefForm::Action,
                                ..
                            })
                        )
                    {
                        let mut arg_env = Vec::new();
                        let mut arg_exprs = Vec::new();
                        for (i, arg) in args.iter().enumerate() {
                            let value = eval(
                                arg,
                                env,
                                &mut EvalContext {
                                    manager,
                                    service_name,
                                    txn: txn.as_deref_mut(),
                                },
                            )
                            .await?;
                            let name = manager.interner.insert(&format!("__arg{i}"));
                            arg_env.push((name, value));
                            arg_exprs.push(Expr::Variable { name });
                        }
                        // An argument with no wire form (html) cannot travel by name,
                        // but the action may never read it: take the path below, which
                        // fetches `a` and ships only what its body captures. Evaluating
                        // the arguments again there only reads again under the same
                        // transaction
                        let sendable = arg_env.iter().all(|(_, value)| {
                            codec::encode_value(value, &manager.interner).is_ok()
                        });
                        if sendable {
                            let owner = manager.owner_net_id(target);
                            manager
                                .remote_action(
                                    &owner,
                                    vec![ActionStmt::Do(Expr::Call {
                                        func: Box::new(Expr::MemberAccess {
                                            service_name: target,
                                            member_name: member,
                                        }),
                                        args: arg_exprs,
                                    })],
                                    arg_env,
                                    txn.as_deref_mut(),
                                )
                                .await?;
                            return Ok(ExecuteEffect::None);
                        }
                    }
                }
            }
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
                            execute_seq(
                                &stmts,
                                &mut exec_env,
                                manager,
                                svc_name,
                                txn.as_deref_mut(),
                            )
                            .await?;
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
                execute_seq(
                    body,
                    &mut loop_env,
                    manager,
                    service_name,
                    txn.as_deref_mut(),
                )
                .await?;
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
                | EvalError::RuntimeError(_)
                | EvalError::InputNotReady(_) => {
                    panic!("Expected EvalError::AssertionError")
                }
            },
        }
    }
}
