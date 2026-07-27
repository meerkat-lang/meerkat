//! Atomic update transaction state machine.
//!
//! This module provides the `Transaction` state machine that manages
//! hot-reloading updates on local and distributed services. It isolates
//! staged changes, validates AST and types against the old state, and
//! applies updates atomically once fully evaluated.

use crate::net::LockGroup;
use crate::runtime::ast::{apply_updates_to_ast, Decl, Expr, Stmt, Value};
use crate::runtime::env::Env;
use crate::runtime::graphs::{analysis::compute_dependencies, ServiceGraphs};
use crate::runtime::interner::{Interner, Symbol};
use crate::runtime::interpreter::evaluator::{eval, EvalContext, EvalError};
use crate::runtime::manager::Manager;
use crate::runtime::nameres;
use crate::runtime::tt::check::{self as tt};
use crate::runtime::tt::types::ServiceType;
use crate::runtime::txn::{Transaction as LockTxn, TxnId, VarLock};
use std::collections::{HashMap, HashSet};

/// The current execution state of an atomic update transaction
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionState {
    /// Initial unstarted state before locks are acquired
    Init,
    /// Lock acquisition phase completed successfully
    LocksAcquired,
    /// Expressions evaluated successfully against the old state
    Evaluated,
}

/// Encapsulates the temporary isolated state during an atomic update
pub struct Transaction {
    state: TransactionState,
    updates: Vec<Stmt>,
    ast: Vec<Stmt>,
    types: Env<'static, ServiceType>,
    values: HashMap<(Symbol, Symbol), Value>,
    deps: HashMap<Symbol, ServiceGraphs>,
    lock_txn: Option<LockTxn>,
}

impl Transaction {
    /// Creates a new unstarted atomic update transaction
    ///
    /// Args:
    ///     `updates` (`Vec<Stmt>`): The service updates to apply
    ///
    /// Returns:
    ///     `Self`: The initialized `Transaction` instance
    pub fn new(updates: Vec<Stmt>) -> Self {
        debug_assert!(
            !updates.is_empty(),
            "Transaction updates should not be empty"
        );
        Self {
            state: TransactionState::Init,
            updates,
            ast: Vec::new(),
            types: Env::new(None),
            values: HashMap::new(),
            deps: HashMap::new(),
            lock_txn: None,
        }
    }

    /// Creates a new unstarted atomic update transaction bound to an existing lock `TxnId`
    ///
    /// Args:
    ///     `txn_id` (`TxnId`): The transaction ID that acquired the locks
    ///     `updates` (`Vec<Stmt>`): The service updates to apply
    ///
    /// Returns:
    ///     `Self`: The initialized `Transaction` instance
    pub fn new_with_id(txn_id: TxnId, updates: Vec<Stmt>) -> Self {
        debug_assert!(
            !updates.is_empty(),
            "Transaction updates should not be empty"
        );
        Self {
            state: TransactionState::Init,
            updates,
            ast: Vec::new(),
            types: Env::new(None),
            values: HashMap::new(),
            deps: HashMap::new(),
            lock_txn: Some(LockTxn::new(txn_id)),
        }
    }

    /// Release any locks currently held by this transaction on the manager
    ///
    /// Args:
    ///     `manager` (`&mut Manager`): The active runtime manager
    fn release_lock_txn(&self, manager: &mut Manager) {
        if let Some(txn) = &self.lock_txn {
            manager.release_all_locks(txn);
        }
    }

    /// Drives the transaction forward through its state machine stages
    ///
    /// Args:
    ///     `manager` (`&mut Manager`): The active runtime manager
    ///
    /// Returns:
    ///     `Result<(), EvalError>`: `Ok(())` on completion or `Err`
    ///
    /// Raises:
    ///     `EvalError`: If lock acquisition, validation, or eval fails
    pub async fn poll(&mut self, manager: &mut Manager) -> Result<(), EvalError> {
        match self.state {
            TransactionState::Init => {
                let mut lock_groups: HashMap<String, LockGroup> = HashMap::new();
                for stmt in &self.updates {
                    let (name, decls) = match stmt {
                        Stmt::Service { name, decls } => (name, decls),
                        Stmt::Update {
                            service_name: name,
                            decls,
                        } => (name, decls),
                        _ => continue,
                    };
                    let svc_name_str = manager.interner.get(*name).to_string();
                    let mut has_new_field = false;
                    if let Some(service) = manager.services.get(name) {
                        for decl in decls {
                            let decl_name = match decl {
                                Decl::VarDecl { name: n, .. } | Decl::DefDecl { name: n, .. } => *n,
                                Decl::TableDecl { .. } => continue,
                            };
                            if !service.vars.contains_key(&decl_name)
                                && !service.defs.contains_key(&decl_name)
                            {
                                has_new_field = true;
                                break;
                            }
                        }
                    } else {
                        // If service doesn't exist yet, request service-level lock
                        has_new_field = true;
                    }

                    let mut group = LockGroup {
                        service_level_lock: has_new_field,
                        reads: HashSet::new(),
                        writes: HashSet::new(),
                    };
                    for decl in decls {
                        match decl {
                            Decl::VarDecl { name: var_name, .. }
                            | Decl::DefDecl { name: var_name, .. } => {
                                let var_str = manager.interner.get(*var_name).to_string();
                                group.writes.insert(var_str);
                            }
                            Decl::TableDecl { .. } => {
                                return Err(EvalError::NotImplemented);
                            }
                        }
                    }
                    lock_groups.insert(svc_name_str, group);
                }

                let mut txn = self
                    .lock_txn
                    .take()
                    .unwrap_or_else(|| LockTxn::new(TxnId::new(manager.node_id)));
                let lock_res = manager
                    .acquire_lock_group_internal(&mut txn, &lock_groups)
                    .await;

                if lock_res.is_err() {
                    manager.release_all_locks(&txn);
                    return Err(EvalError::RuntimeError(
                        "Lock acquisition conflict during update".to_string(),
                    ));
                }

                self.lock_txn = Some(txn);
                self.state = TransactionState::LocksAcquired;
                match apply_updates_to_ast(&manager.unified_ast, &self.updates) {
                    Ok(patched) => self.ast = patched,
                    Err(sym) => {
                        self.release_lock_txn(manager);
                        let name_str = manager.interner.get(sym);
                        return Err(EvalError::RuntimeError(format!(
                            "Target service '{}' for update not found",
                            name_str
                        )));
                    }
                }

                if let Err(e) = nameres::resolve(&self.ast) {
                    self.release_lock_txn(manager);
                    let msg = match &e {
                        nameres::Error::ForwardReference(sym) => {
                            format!("ForwardReference({})", manager.interner.get(*sym))
                        }
                        nameres::Error::UnknownIdentifier { .. } | nameres::Error::DepthLimit => {
                            format!("{}", e)
                        }
                    };
                    return Err(EvalError::RuntimeError(format!(
                        "Name resolution failed on update: {}",
                        msg
                    )));
                }

                let mut types = Env::new(None);
                if let Err(e) = tt::check(&self.ast, &mut types) {
                    self.release_lock_txn(manager);
                    return Err(EvalError::RuntimeError(format!(
                        "Type check failed on update: {:?}",
                        e
                    )));
                }
                self.types = types;

                let service_graphs_vec = match compute_dependencies(&self.ast) {
                    Ok(graphs) => graphs,
                    Err(e) => {
                        self.release_lock_txn(manager);
                        return Err(EvalError::RuntimeError(e.to_string()));
                    }
                };

                let service_stmts: Vec<Symbol> = self
                    .ast
                    .iter()
                    .filter_map(|stmt| match stmt {
                        Stmt::Service { name, .. } => Some(*name),
                        Stmt::ActionStmt(_)
                        | Stmt::Atomic { .. }
                        | Stmt::Update { .. }
                        | Stmt::Connect { .. }
                        | Stmt::Import { .. }
                        | Stmt::Test { .. }
                        | Stmt::Watch { .. } => None,
                    })
                    .collect();

                debug_assert_eq!(
                    service_stmts.len(),
                    service_graphs_vec.len(),
                    "Service statement count must match graph count"
                );

                for (name, graphs) in service_stmts.into_iter().zip(service_graphs_vec) {
                    self.deps.insert(name, graphs);
                }

                let mut eval_txn = LockTxn::new(TxnId::new(manager.node_id));

                for stmt in &self.updates {
                    let (svc_name, decls) = match stmt {
                        Stmt::Service { name, decls } => (name, decls),
                        Stmt::Update {
                            service_name: name,
                            decls,
                        } => (name, decls),
                        Stmt::ActionStmt(_)
                        | Stmt::Atomic { .. }
                        | Stmt::Connect { .. }
                        | Stmt::Import { .. }
                        | Stmt::Test { .. }
                        | Stmt::Watch { .. } => continue,
                    };
                    for decl in decls {
                        let (var_name, expr) = match decl {
                            Decl::VarDecl { name, val, .. } => (*name, val),
                            Decl::DefDecl { .. } => continue,
                            Decl::TableDecl { .. } => {
                                self.release_lock_txn(manager);
                                return Err(EvalError::NotImplemented);
                            }
                        };

                        let env: Vec<(Symbol, Value)> = Vec::new();
                        let mut ctx = EvalContext {
                            manager,
                            service_name: *svc_name,
                            txn: Some(&mut eval_txn),
                        };

                        let val = match eval(expr, &env, &mut ctx).await {
                            Ok(v) => v,
                            Err(e) => {
                                self.release_lock_txn(manager);
                                return Err(e);
                            }
                        };
                        self.values.insert((*svc_name, var_name), val);
                    }
                }

                self.state = TransactionState::Evaluated;
                self.commit(manager);

                let updated_svc_names: HashSet<Symbol> = self
                    .updates
                    .iter()
                    .filter_map(|stmt| match stmt {
                        Stmt::Service { name, .. } => Some(*name),
                        Stmt::Update {
                            service_name: name, ..
                        } => Some(*name),
                        Stmt::ActionStmt(_)
                        | Stmt::Atomic { .. }
                        | Stmt::Connect { .. }
                        | Stmt::Import { .. }
                        | Stmt::Test { .. }
                        | Stmt::Watch { .. } => None,
                    })
                    .collect();

                for updated_svc_name in updated_svc_names {
                    if let Some(dep) = self.deps.remove(&updated_svc_name) {
                        manager.update_service_graphs(updated_svc_name, dep).await;
                    }
                }

                for stmt in &self.updates {
                    let (svc_name, decls) = match stmt {
                        Stmt::Service { name, decls } => (name, decls),
                        Stmt::Update {
                            service_name: name,
                            decls,
                        } => (name, decls),
                        Stmt::ActionStmt(_)
                        | Stmt::Atomic { .. }
                        | Stmt::Connect { .. }
                        | Stmt::Import { .. }
                        | Stmt::Test { .. }
                        | Stmt::Watch { .. } => continue,
                    };
                    if manager.services.contains_key(svc_name) {
                        for decl in decls {
                            match decl {
                                Decl::VarDecl {
                                    name: decl_name, ..
                                } => {
                                    manager.propagate(*svc_name, *decl_name).await;
                                }
                                Decl::DefDecl {
                                    name: decl_name, ..
                                } => {
                                    manager.recompute_def(*svc_name, *decl_name).await;
                                    manager.propagate(*svc_name, *decl_name).await;
                                }
                                Decl::TableDecl { .. } => {}
                            }
                        }
                    }
                    if let Some(addr) = manager.remote_services.get(svc_name).cloned() {
                        let source = format_update_source(*svc_name, decls, &manager.interner);
                        let svc_str = manager.interner.get(*svc_name).to_string();
                        static NEXT_UPDATE_REQ_ID: std::sync::atomic::AtomicU64 =
                            std::sync::atomic::AtomicU64::new(1);
                        let request_id =
                            NEXT_UPDATE_REQ_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let reply_to = manager.local_reply_addr().await;
                        let msg = crate::net::types::MeerkatMessage::UpdateServiceRequest {
                            request_id,
                            txn_id: self.lock_txn.as_ref().map(|t| t.id.clone()),
                            service_name: svc_str.clone(),
                            source,
                            reply_to,
                        };
                        let timeout = format!(
                            "Timeout waiting for service update response for '{}'",
                            svc_str
                        );
                        let res =
                            Box::pin(manager.send_and_await_reply(addr, msg, request_id, timeout))
                                .await;
                        if let Err(ref e) = res {
                            eprintln!(
                                "Warning: remote service update to {} failed: {}",
                                svc_str, e
                            );
                        }
                    }
                }
                if let Some(lock_txn) = &self.lock_txn {
                    for addr in lock_txn.participants.iter().cloned().collect::<Vec<_>>() {
                        let _ = manager.send_commit(addr, &lock_txn.id).await;
                    }
                }
                self.release_lock_txn(manager);

                Ok(())
            }
            TransactionState::LocksAcquired => {
                debug_assert!(false, "Unreachable poll call in LocksAcquired state");
                Ok(())
            }
            TransactionState::Evaluated => Ok(()),
        }
    }

    /// Consumes the fully evaluated isolated state and overwrites Manager
    ///
    /// Args:
    ///     `manager` (`&mut Manager`): The active runtime manager
    fn commit(&mut self, manager: &mut Manager) {
        debug_assert_eq!(self.state, TransactionState::Evaluated);
        manager.unified_ast = std::mem::take(&mut self.ast);
        manager.local_services = std::mem::replace(&mut self.types, Env::new(None));

        for ((svc_name, var_name), val) in std::mem::take(&mut self.values) {
            if let Some(service) = manager.services.get_mut(&svc_name) {
                if let Some(var_state) = service.vars.get_mut(&var_name) {
                    var_state.value = val;
                    var_state.lock = VarLock::Unlocked;
                } else {
                    service.vars.insert(
                        var_name,
                        crate::runtime::txn::VarState {
                            value: val,
                            lock: VarLock::Unlocked,
                            latest_write_txn: None,
                        },
                    );
                }
            }
        }

        for stmt in &manager.unified_ast {
            if let Stmt::Service { name, decls } = stmt {
                if let Some(service) = manager.services.get_mut(name) {
                    for decl in decls {
                        if let Decl::DefDecl {
                            name: def_name,
                            val,
                            ..
                        } = decl
                        {
                            service.defs.insert(*def_name, val.clone());
                        }
                    }
                }
            }
        }

        for (svc_name, dep) in std::mem::take(&mut self.deps) {
            if let Some(service) = manager.services.get_mut(&svc_name) {
                service.graphs = dep;
                service.service_lock = None;
            }
        }
    }
}

/// Format an `Expr` into a valid Meerkat source code string
///
/// Args:
///     `expr` (`&Expr`): The expression AST node
///     `interner` (`&Interner`): Symbol interner for resolving names
///
/// Returns:
///     `String`: Valid Meerkat expression source string
pub fn format_expr(expr: &Expr, interner: &Interner) -> String {
    match expr {
        Expr::Literal { val } => match val {
            Value::Int { val } => val.to_string(),
            Value::Bool { val } => val.to_string(),
            Value::String { val } => format!("\"{}\"", val),
            Value::Html(h) => h.to_string(),
            Value::Closure { .. }
            | Value::ActionClosure { .. }
            | Value::List { .. }
            | Value::Range { .. } => val.to_string(),
        },
        Expr::Html(_) => "<html>".to_string(),
        Expr::Tuple { val } => {
            let elems: Vec<String> = val.iter().map(|e| format_expr(e, interner)).collect();
            format!("({})", elems.join(", "))
        }
        Expr::KeyVal { name, value } => {
            format!("{}: {}", interner.get(*name), format_expr(value, interner))
        }
        Expr::Variable { name } => interner.get(*name).to_string(),
        Expr::Unop { op, expr } => {
            format!("{}{}", op, format_expr(expr, interner))
        }
        Expr::Binop { op, expr1, expr2 } => format!(
            "{} {} {}",
            format_expr(expr1, interner),
            op,
            format_expr(expr2, interner)
        ),
        Expr::If { cond, expr1, expr2 } => format!(
            "if {} then {} else {}",
            format_expr(cond, interner),
            format_expr(expr1, interner),
            format_expr(expr2, interner)
        ),
        Expr::Func { .. } => expr.to_string(),
        Expr::Call { func, args } => {
            let arg_strs: Vec<String> = args.iter().map(|a| format_expr(a, interner)).collect();
            format!("{}({})", format_expr(func, interner), arg_strs.join(", "))
        }
        Expr::Action(_) => expr.to_string(),
        Expr::MemberAccess {
            service_name,
            member_name,
        } => format!(
            "{}.{}",
            interner.get(*service_name),
            interner.get(*member_name)
        ),
        Expr::Select { .. } | Expr::Table { .. } | Expr::Fold { .. } => expr.to_string(),
        Expr::List(exprs) => {
            let elems: Vec<String> = exprs.iter().map(|e| format_expr(e, interner)).collect();
            format!("[{}]", elems.join(", "))
        }
        Expr::Range { start, end } => format!(
            "{}..{}",
            format_expr(start, interner),
            format_expr(end, interner)
        ),
    }
}

/// Format a `Decl` into a valid Meerkat declaration source code string
///
/// Args:
///     `decl` (`&Decl`): The declaration AST node
///     `interner` (`&Interner`): Symbol interner for resolving names
///
/// Returns:
///     `String`: Valid Meerkat declaration source string
pub fn format_decl(decl: &Decl, interner: &Interner) -> String {
    match decl {
        Decl::VarDecl { name, ty, val } => {
            let name_str = interner.get(*name);
            let val_str = format_expr(val, interner);
            if let Some(t) = ty {
                format!("var {}: {} = {};", name_str, t, val_str)
            } else {
                format!("var {} = {};", name_str, val_str)
            }
        }
        Decl::DefDecl {
            name,
            ty,
            val,
            is_pub,
        } => {
            let prefix = if *is_pub { "pub " } else { "" };
            let name_str = interner.get(*name);
            let val_str = format_expr(val, interner);
            if let Some(t) = ty {
                format!("{}def {}: {} = {};", prefix, name_str, t, val_str)
            } else {
                format!("{}def {} = {};", prefix, name_str, val_str)
            }
        }
        Decl::TableDecl { name, .. } => {
            format!("table {};", interner.get(*name))
        }
    }
}

/// Format a service update block into valid Meerkat source code string
///
/// Args:
///     `service_name` (`Symbol`): Symbol for service name
///     `decls` (`&[Decl]`): Declarations in update block
///     `interner` (`&Interner`): Symbol interner for resolving names
///
/// Returns:
///     `String`: Valid Meerkat update statement source string
pub fn format_update_source(service_name: Symbol, decls: &[Decl], interner: &Interner) -> String {
    let svc_str = interner.get(service_name);
    let decl_strs: Vec<String> = decls.iter().map(|d| format_decl(d, interner)).collect();
    format!("update {} {{\n  {}\n}}", svc_str, decl_strs.join("\n  "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::ast::Expr;
    use crate::runtime::interner::Interner;

    /// Test creating a new transaction and validating initial state
    #[test]
    fn test_transaction_new_initialization() {
        let mut interner = Interner::new();
        let svc_sym = interner.insert("s1");
        let var_sym = interner.insert("x");
        let update_stmt = Stmt::Service {
            name: svc_sym,
            decls: vec![Decl::VarDecl {
                name: var_sym,
                ty: None,
                val: Expr::Literal {
                    val: Value::Int { val: 42 },
                },
            }],
        };

        let txn = Transaction::new(vec![update_stmt]);
        assert_eq!(txn.state, TransactionState::Init);
    }

    /// Test lock release when `nameres` fails during update transaction
    ///
    /// Ensures that write locks acquired during transaction initialization
    /// are completely released if name resolution fails on the staged AST
    #[tokio::test]
    async fn test_transaction_poll_releases_locks_on_nameres_failure() {
        let mut interner = Interner::new();
        let code = "service s1 { var x = 10; }";
        let ast = crate::runtime::parser::parse_string(code, &mut interner).unwrap();
        let mut node = crate::runtime::Node::new();
        node.interner = interner.clone();
        node.unified_ast = ast;
        node.static_checks().unwrap();

        let local_ast = node.unified_ast.clone();
        let mut manager = node
            .on_manager_startup(true, None, std::collections::HashMap::new(), &local_ast)
            .await
            .unwrap();

        let update_code = "update s1 { var x = unbound_variable; }";
        let update_ast =
            crate::runtime::parser::parse_string(update_code, &mut manager.interner).unwrap();

        let mut txn = Transaction::new(update_ast);
        let res = txn.poll(&mut manager).await;
        assert!(res.is_err());

        let s1_sym = manager.interner.insert("s1");
        let x_sym = manager.interner.insert("x");
        let service = manager.services.get(&s1_sym).unwrap();
        let var_state = service.vars.get(&x_sym).unwrap();
        assert!(matches!(var_state.lock, VarLock::Unlocked));
    }

    /// Test lock release when `tt::check` fails during update transaction
    ///
    /// Ensures that write locks acquired during transaction initialization
    /// are completely released if type checking fails on the staged AST
    #[tokio::test]
    async fn test_transaction_poll_releases_locks_on_tt_failure() {
        let mut interner = Interner::new();
        let code = "service s1 { var x = 10; }";
        let ast = crate::runtime::parser::parse_string(code, &mut interner).unwrap();
        let mut node = crate::runtime::Node::new();
        node.interner = interner.clone();
        node.unified_ast = ast;
        node.static_checks().unwrap();

        let local_ast = node.unified_ast.clone();
        let mut manager = node
            .on_manager_startup(true, None, std::collections::HashMap::new(), &local_ast)
            .await
            .unwrap();

        let update_code = "update s1 { var x: string = 42; }";
        let update_ast =
            crate::runtime::parser::parse_string(update_code, &mut manager.interner).unwrap();

        let mut txn = Transaction::new(update_ast);
        let res = txn.poll(&mut manager).await;
        assert!(res.is_err());

        let s1_sym = manager.interner.insert("s1");
        let x_sym = manager.interner.insert("x");
        let service = manager.services.get(&s1_sym).unwrap();
        let var_state = service.vars.get(&x_sym).unwrap();
        assert!(matches!(var_state.lock, VarLock::Unlocked));
    }
}
