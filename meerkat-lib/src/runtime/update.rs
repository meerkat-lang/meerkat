//! Atomic update transaction state machine.
//!
//! This module provides the `Transaction` state machine that manages
//! hot-reloading updates on local and distributed services. It isolates
//! staged changes, validates AST and types against the old state, and
//! applies updates atomically once fully evaluated.

use crate::net::LockGroup;
use crate::runtime::ast::{Decl, Stmt, Value};
use crate::runtime::env::Env;
use crate::runtime::graphs::{analysis::compute_dependencies, ServiceGraphs};
use crate::runtime::interner::Symbol;
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
pub struct Transaction<'a> {
    state: TransactionState,
    updates: Vec<Stmt>,
    ast: Vec<Stmt>,
    types: Env<'a, ServiceType<'a>>,
    values: HashMap<(Symbol, Symbol), Value>,
    deps: HashMap<Symbol, ServiceGraphs>,
    lock_txn: Option<LockTxn>,
}

impl<'a> Transaction<'a> {
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

                let mut txn = LockTxn::new(TxnId::new(manager.node_id));
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
                self.ast = manager.unified_ast.clone();

                for stmt in &self.updates {
                    let (updated_svc_name, updated_decls) = match stmt {
                        Stmt::Service { name, decls } => (name, decls),
                        Stmt::Update {
                            service_name: name,
                            decls,
                        } => (name, decls),
                        _ => continue,
                    };
                    let mut found_service = false;
                    for existing_stmt in &mut self.ast {
                        if let Stmt::Service {
                            name: existing_svc_name,
                            decls: existing_decls,
                        } = existing_stmt
                        {
                            if *existing_svc_name == *updated_svc_name {
                                found_service = true;
                                for up_decl in updated_decls {
                                    let up_name = match up_decl {
                                        Decl::VarDecl { name: n, .. }
                                        | Decl::DefDecl { name: n, .. } => *n,
                                        Decl::TableDecl { .. } => {
                                            return Err(EvalError::NotImplemented);
                                        }
                                    };

                                    let mut matched_existing = false;
                                    for ex_decl in existing_decls.iter_mut() {
                                        let ex_name = match ex_decl {
                                            Decl::VarDecl { name: n, .. }
                                            | Decl::DefDecl { name: n, .. } => *n,
                                            Decl::TableDecl { .. } => {
                                                return Err(EvalError::NotImplemented);
                                            }
                                        };
                                        if ex_name == up_name {
                                            *ex_decl = up_decl.clone();
                                            matched_existing = true;
                                            break;
                                        }
                                    }

                                    if !matched_existing {
                                        if matches!(up_decl, Decl::VarDecl { .. }) {
                                            let insert_pos = existing_decls
                                                .iter()
                                                .position(|d| matches!(d, Decl::DefDecl { .. }))
                                                .unwrap_or(existing_decls.len());
                                            existing_decls.insert(insert_pos, up_decl.clone());
                                        } else {
                                            existing_decls.push(up_decl.clone());
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if !found_service {
                        return Err(EvalError::RuntimeError(
                            "Target service for update not found".to_string(),
                        ));
                    }
                }

                if let Err(e) = nameres::resolve(&self.ast) {
                    let msg = match &e {
                        nameres::Error::ForwardReference(sym) => {
                            format!(
                                "ForwardReference({} / {:?})",
                                manager.interner.get(*sym),
                                sym
                            )
                        }
                        other => format!("{:?}", other),
                    };
                    return Err(EvalError::RuntimeError(format!(
                        "Name resolution failed on update: {}",
                        msg
                    )));
                }

                let mut types = Env::new(None);
                if let Err(e) = tt::check(&self.ast, &mut types) {
                    return Err(EvalError::RuntimeError(format!(
                        "Type check failed on update: {:?}",
                        e
                    )));
                }
                self.types = unsafe {
                    std::mem::transmute::<Env<'_, ServiceType<'_>>, Env<'a, ServiceType<'a>>>(types)
                };

                let service_graphs_vec = compute_dependencies(&self.ast)
                    .map_err(|e| EvalError::RuntimeError(e.to_string()))?;

                let service_stmts: Vec<Symbol> = self
                    .ast
                    .iter()
                    .filter_map(|stmt| match stmt {
                        Stmt::Service { name, .. } => Some(*name),
                        _ => None,
                    })
                    .collect();

                debug_assert_eq!(
                    service_stmts.len(),
                    service_graphs_vec.len(),
                    "Service statement count must match computed service graph count"
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
                        _ => continue,
                    };
                    for decl in decls {
                        let (var_name, expr) = match decl {
                            Decl::VarDecl { name, val, .. } => (*name, val),
                            Decl::DefDecl { .. } => continue,
                            Decl::TableDecl { .. } => {
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
                                if let Some(txn) = &self.lock_txn {
                                    manager.release_all_locks(txn);
                                }
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
                        _ => None,
                    })
                    .collect();

                for updated_svc_name in updated_svc_names {
                    if let Some(dep) = self.deps.remove(&updated_svc_name) {
                        manager.update_service_graphs(updated_svc_name, dep).await;
                    }
                }

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
        manager.local_services = unsafe {
            std::mem::transmute::<Env<'_, ServiceType<'_>>, Env<'static, ServiceType<'static>>>(
                std::mem::replace(&mut self.types, Env::new(None)),
            )
        };

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
}
