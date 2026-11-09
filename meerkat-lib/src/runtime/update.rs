//! Atomic update transaction state machine.
//!
//! This module provides the `Transaction` state machine that manages
//! hot-reloading updates on local and distributed services. It isolates
//! staged changes, validates AST and types against the old state, and
//! applies updates atomically once fully evaluated.

use crate::net::LockGroup;
use crate::runtime::ast::{apply_updates_to_ast, Decl, Expr, Stmt, Value};
use crate::runtime::graphs::analysis::service_declarations;
use crate::runtime::interner::{Interner, Symbol};
use crate::runtime::interpreter::evaluator::{eval, EvalContext, EvalError};
use crate::runtime::manager::Manager;
use crate::runtime::txn::{Transaction as LockTxn, TxnId, VClock, VarLock};
use std::collections::{HashMap, HashSet};

/// Encapsulates the temporary isolated state during an atomic update
pub struct Transaction {
    updates: Vec<Stmt>,
    ast: Vec<Stmt>,
    values: HashMap<(Symbol, Symbol), Value>,
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
        Self {
            updates: flatten_atomic(updates),
            ast: Vec::new(),
            values: HashMap::new(),
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
        Self {
            updates: flatten_atomic(updates),
            ast: Vec::new(),
            values: HashMap::new(),
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

    /// Applies the update, then waits until every def it introduces has a
    /// first value (see `apply`)
    ///
    /// Args:
    ///     `manager` (`&mut Manager`): The active runtime manager
    ///
    /// Returns:
    ///     `Result<(), EvalError>`: `Ok(())` on completion or `Err`
    ///
    /// Raises:
    ///     `EvalError`: If lock acquisition, validation, or eval fails, or a
    ///     committed new def gets no value (`Manager::await_first_values`)
    pub async fn poll(&mut self, manager: &mut Manager) -> Result<(), EvalError> {
        let introduced = self.apply(manager).await?;
        manager.await_first_values(&introduced).await
    }

    /// Takes the locks, validates and installs the update, forwards it to the
    /// remote owners it changes, and releases the locks
    ///
    /// Args:
    ///     `manager` (`&mut Manager`): The active runtime manager
    ///
    /// Returns:
    ///     `Result<Vec<(Symbol, Symbol)>, EvalError>`: The defs the committed
    ///     update introduces. A def whose inputs only its own publication can
    ///     complete takes its first value after this returns, through ordinary
    ///     propagation (`Manager::first_values`)
    ///
    /// Raises:
    ///     `EvalError`: If lock acquisition, validation, or eval fails
    pub async fn apply(
        &mut self,
        manager: &mut Manager,
    ) -> Result<Vec<(Symbol, Symbol)>, EvalError> {
        let result = self.try_apply(manager).await;
        // Whatever the outcome, the locks this update holds on this node are
        // released here, once. Remote participants are told only on the paths
        // that do so themselves: commit sends them `Commit`, and a failure in
        // `commit` aborts them. Other failures before the local commit leave
        // their locks held (to be fixed separately). A failure before the
        // local commit leaves nothing installed here; one a remote owner
        // reports afterwards leaves the local part installed and is returned
        self.release_lock_txn(manager);
        result
    }

    /// `apply` up to the release of its locks
    async fn try_apply(
        &mut self,
        manager: &mut Manager,
    ) -> Result<Vec<(Symbol, Symbol)>, EvalError> {
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
                    Decl::VarDecl { name: var_name, .. } | Decl::DefDecl { name: var_name, .. } => {
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

        loop {
            let lock_res = manager
                .acquire_lock_group_internal(&mut txn, &lock_groups)
                .await;

            if let Err(e) = lock_res {
                manager.release_all_locks(&txn);
                if matches!(e, EvalError::WaitDieAbort(_))
                    && txn.id.iteration < crate::runtime::manager::MAX_WAIT_DIE_RETRIES
                {
                    txn = LockTxn::new(txn.id.retry());
                    continue;
                }
                return Err(EvalError::RuntimeError(format!(
                    "Lock acquisition conflict during update: {}",
                    e
                )));
            }
            break;
        }

        self.lock_txn = Some(txn);
        match apply_updates_to_ast(manager.unified_ast(), &self.updates) {
            Ok(patched) => self.ast = patched,
            Err(sym) => {
                let name_str = manager.interner.get(sym);
                return Err(EvalError::RuntimeError(format!(
                    "Target service '{}' for update not found",
                    name_str
                )));
            }
        }

        if let Err(e) = crate::runtime::node::check_program(&self.ast) {
            return Err(EvalError::RuntimeError(format!(
                "Static check failed on update: {}",
                e.message(&manager.interner)
            )));
        }
        // The vars the update declares, each evaluated once, in its final form
        let mut staged_vars: HashSet<(Symbol, Symbol)> = HashSet::new();
        for (svc_name, decls) in declared(&self.updates) {
            for decl in decls {
                match decl {
                    Decl::VarDecl { name, .. } => {
                        staged_vars.insert((*svc_name, *name));
                    }
                    Decl::DefDecl { .. } => {}
                    Decl::TableDecl { .. } => return Err(EvalError::NotImplemented),
                }
            }
        }
        // Evaluate their initializers in the merged order, staging each value
        // in the lock transaction as it is computed. Every member read in the
        // update then sees the update's own vars, new or changed, whether by
        // plain or qualified name, from inside a called function, or in a
        // service on another node, and the running state for the rest. Staged
        // values carry no clock: the update's stamp absorbs the clocks of the
        // running state it read.
        // Defs are not staged: their first values come after the commit, so a
        // var reading a def the update adds or changes fails or sees the old
        // value (see docs/design/code-updates.md)
        let initializers: Vec<(Symbol, Symbol, Expr)> = self
            .ast
            .iter()
            .filter_map(|stmt| match stmt {
                Stmt::Service { name, decls } => Some((name, decls)),
                _ => None,
            })
            .flat_map(|(svc_name, decls)| {
                decls.iter().filter_map(|decl| match decl {
                    Decl::VarDecl { name, val, .. }
                        if staged_vars.contains(&(*svc_name, *name)) =>
                    {
                        Some((*svc_name, *name, val.clone()))
                    }
                    _ => None,
                })
            })
            .collect();
        for (svc_name, var_name, expr) in initializers {
            let val = {
                let mut ctx = EvalContext {
                    manager,
                    service_name: svc_name,
                    txn: self.lock_txn.as_mut(),
                };
                eval(&expr, &[], &mut ctx).await?
            };
            if let Some(txn) = self.lock_txn.as_mut() {
                txn.staged.insert((svc_name, var_name), val.clone());
            }
            self.values.insert((svc_name, var_name), val);
        }

        let introduced = self.unvalued_defs(manager);
        // The declarations passed the static checks; with the remote code
        // the running services hold, their graphs must stay complete and
        // acyclic as well. Those of the services declared may change, as may
        // those of every running service reading a member declared
        let running = manager.running_services();
        let mut services = manager.services_reading(
            declared(&self.updates)
                .flat_map(|(&name, decls)| decls.iter().map(move |decl| (name, decl.name()))),
        );
        services.extend(
            declared(&self.updates)
                .map(|(&name, _)| name)
                .filter(|name| running.contains(name)),
        );
        // The values the declarations evaluated to stand in for those the
        // vars hold
        let staged = self
            .values
            .iter()
            .map(|(&var, value)| (var, value))
            .collect();
        let graphs = manager.graphs_of(&self.ast, services, &staged);
        manager.check_graphs(&graphs)?;
        if let Err(e) = self.commit(manager) {
            if let Some(txn) = &self.lock_txn {
                manager.abort_participants(txn).await;
            }
            return Err(e);
        }

        let updated_services: Vec<_> = declared(&self.updates).collect();
        // The staged values and declarations were installed without
        // yielding; their checked graphs are still the ones to install
        manager.install_graphs(graphs).await;
        manager.retry_deferred_deliveries().await;
        // The service lock guarded the structural change: with the
        // declarations and graphs installed it is released here,
        // before the remote owners are told, so a reader of another
        // member is not held up by the forwarding
        for (svc_name, _) in &updated_services {
            if let Some(service) = manager.services.get_mut(*svc_name) {
                service.service_lock = None;
            }
        }
        let mut remote_failure = None;

        for (svc_name, decls) in updated_services {
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
                            if manager.recompute_def(*svc_name, *decl_name).await {
                                manager.propagate(*svc_name, *decl_name).await;
                            }
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
                    Box::pin(manager.send_and_await_reply(addr, msg, request_id, timeout)).await;
                match res {
                    // No answer, so no confirmed outcome either way
                    Err(ref e) => {
                        eprintln!(
                            "Warning: remote service update to {} failed: {}",
                            svc_str, e
                        );
                        remote_failure.get_or_insert(EvalError::RuntimeError(format!(
                            "Remote service update to '{}' was not confirmed and may have \
                                     been applied: {}",
                            svc_str, e
                        )));
                    }
                    // The owner reports a failed or incomplete update
                    Ok(crate::net::types::MeerkatMessage::UpdateServiceResponse {
                        error: Some(error),
                        ..
                    }) => {
                        remote_failure.get_or_insert(EvalError::RuntimeError(format!(
                            "Remote service update to '{}' failed: {}",
                            svc_str, error
                        )));
                    }
                    Ok(_) => {}
                }
            }
        }
        // Only releases the participants' locks: they buffered no
        // writes to stamp, and the vars this update changed were
        // stamped in `commit`
        if let Some(lock_txn) = &self.lock_txn {
            let _ = manager.commit_participants(lock_txn, &VClock::new()).await;
        }

        remote_failure.map_or(Ok(introduced), Err)
    }

    /// The defs this update declares on local services that have no value
    ///
    /// Args:
    ///     `manager` (`&Manager`): The active runtime manager
    ///
    /// Returns:
    ///     `Vec<(Symbol, Symbol)>`: `(service, def)` pairs
    fn unvalued_defs(&self, manager: &Manager) -> Vec<(Symbol, Symbol)> {
        self.updates
            .iter()
            .filter_map(|stmt| match stmt {
                Stmt::Service { name, decls }
                | Stmt::Update {
                    service_name: name,
                    decls,
                } => Some((*name, decls)),
                _ => None,
            })
            .flat_map(|(svc, decls)| {
                decls.iter().filter_map(move |decl| match decl {
                    Decl::DefDecl { name, .. } => Some((svc, *name)),
                    Decl::VarDecl { .. } | Decl::TableDecl { .. } => None,
                })
            })
            .filter(|(svc, def)| {
                manager
                    .services
                    .get(svc)
                    .is_some_and(|service| !service.vars.contains_key(def))
            })
            .collect()
    }

    /// Consumes the fully evaluated isolated state and overwrites Manager
    ///
    /// Args:
    ///     `manager` (`&mut Manager`): The active runtime manager
    fn commit(&mut self, manager: &mut Manager) -> Result<(), EvalError> {
        let write_set: HashSet<_> = self
            .values
            .keys()
            .filter(|(svc, _)| manager.services.contains_key(svc))
            .copied()
            .collect();
        // Include initializer reads and the prior clocks of overwritten vars,
        // and admit the complete stamp before installing values or declarations.
        let clock = if write_set.is_empty() {
            // Remote owners stamp their own writes; this node installs no clock.
            VClock::new()
        } else {
            let read_clock = self
                .lock_txn
                .as_ref()
                .map(|txn| manager.transaction_history(txn).0)
                .unwrap_or_default();
            manager.write_clock(read_clock, &write_set)?
        };
        for ((svc_name, var_name), val) in std::mem::take(&mut self.values) {
            if let Some(service) = manager.services.get_mut(&svc_name) {
                if let Some(var_state) = service.vars.get_mut(&var_name) {
                    var_state.value = val;
                    var_state.vector_clock = clock.clone();
                    var_state.sources = Some(HashSet::from([(svc_name, var_name)]));
                    var_state.lock = VarLock::Unlocked;
                } else {
                    let mut state = crate::runtime::txn::VarState::new(val);
                    state.vector_clock = clock.clone();
                    state.sources = Some(HashSet::from([(svc_name, var_name)]));
                    service.vars.insert(var_name, state);
                }
            }
        }

        for (name, stmt) in service_declarations(&self.ast) {
            if let (Some(service), Stmt::Service { decls, .. }) =
                (manager.services.get_mut(&name), stmt)
            {
                service.defs = decls
                    .iter()
                    .filter_map(|decl| match decl {
                        Decl::DefDecl { name, val, .. } => Some((*name, val.clone())),
                        _ => None,
                    })
                    .collect();
            }
        }
        manager.install_unified_ast(std::mem::take(&mut self.ast));
        Ok(())
    }
}

/// Format an `ActionStmt` into a valid Meerkat string, resolving symbols with the interner.
fn format_action_stmt(stmt: &crate::runtime::ast::ActionStmt, interner: &Interner) -> String {
    use crate::runtime::ast::ActionStmt;
    match stmt {
        ActionStmt::Let { name, ty, expr } => {
            let name_str = interner.get(*name);
            let expr_str = format_expr(expr, interner);
            if let Some(t) = ty {
                format!("let {}: {} = {}", name_str, t, expr_str)
            } else {
                format!("let {} = {}", name_str, expr_str)
            }
        }
        ActionStmt::Expr(expr) => format_expr(expr, interner),
        ActionStmt::Do(expr) => format!("do {}", format_expr(expr, interner)),
        ActionStmt::Assert(expr, _) => format!("assert {}", format_expr(expr, interner)),
        ActionStmt::Assign { name, expr } => {
            format!("{} = {}", interner.get(*name), format_expr(expr, interner))
        }
        ActionStmt::Insert { row, table_name } => {
            format!(
                "insert {} into {}",
                format_expr(row, interner),
                interner.get(*table_name)
            )
        }
        ActionStmt::For {
            var,
            iterable,
            body,
        } => {
            let body_strs: Vec<String> = body
                .iter()
                .map(|s| format_action_stmt(s, interner))
                .collect();
            format!(
                "for {} in {} {{ {} }}",
                interner.get(*var),
                format_expr(iterable, interner),
                body_strs.join("; ")
            )
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
        Expr::Func {
            params,
            body,
            return_ty,
        } => {
            let params_str: Vec<String> = params.iter().map(|p| p.to_string()).collect();
            if let Some(ref ty) = return_ty {
                format!(
                    "fn({}) -> {}[{}]",
                    params_str.join(","),
                    ty,
                    format_expr(body, interner)
                )
            } else {
                format!(
                    "fn({})[{}]",
                    params_str.join(","),
                    format_expr(body, interner)
                )
            }
        }
        Expr::Call { func, args } => {
            let arg_strs: Vec<String> = args.iter().map(|a| format_expr(a, interner)).collect();
            format!("{}({})", format_expr(func, interner), arg_strs.join(", "))
        }
        Expr::Action(stmts) => {
            let stmts_str: Vec<String> = stmts
                .iter()
                .map(|s| format!("{:?}", format_action_stmt(s, interner)))
                .collect();
            format!("Action([{}])", stmts_str.join(", "))
        }
        Expr::MemberAccess {
            service_name,
            member_name,
        } => format!(
            "{}.{}",
            interner.get(*service_name),
            interner.get(*member_name)
        ),
        Expr::Select { where_clause, .. } => format_expr(where_clause, interner),
        Expr::Table { records, .. } => {
            let records_str: Vec<String> = records
                .iter()
                .map(|r| match r {
                    Expr::Tuple { val } => {
                        let entries: Vec<String> =
                            val.iter().map(|e| format_expr(e, interner)).collect();
                        format!("{{{}}}", entries.join(", "))
                    }
                    other => format!("{{{}}}", format_expr(other, interner)),
                })
                .collect();
            format!("[{}]", records_str.join(", "))
        }
        Expr::Fold { .. } => "fold".to_string(),
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

/// `updates` with every `atomic` block, nested ones included, replaced by
/// the updates it contains, as `Manager::run_update` unwraps one. A
/// transaction locks, stages and merges the same flat list, so an `atomic`
/// block arriving here (from a peer's `UpdateServiceRequest`, say) can't
/// install declarations without their values
fn flatten_atomic(updates: Vec<Stmt>) -> Vec<Stmt> {
    updates
        .into_iter()
        .flat_map(|stmt| match stmt {
            Stmt::Atomic { updates } => flatten_atomic(updates),
            other => vec![other],
        })
        .collect()
}

/// The services `updates` declares, each with its declarations
fn declared(updates: &[Stmt]) -> impl Iterator<Item = (&Symbol, &Vec<Decl>)> {
    updates.iter().filter_map(|stmt| match stmt {
        Stmt::Service { name, decls }
        | Stmt::Update {
            service_name: name,
            decls,
        } => Some((name, decls)),
        _ => None,
    })
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
        assert!(txn.lock_txn.is_none());
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
