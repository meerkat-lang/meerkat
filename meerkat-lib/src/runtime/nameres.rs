//! Static name resolution analysis for the Meerkat language
//!
//! This module resolves and validates variable usages in abstract
//! syntax trees, failing defensively if an unbound symbol is accessed.
//! When called with a unified AST (local + all imported service source),
//! cross-service member accesses and @test blocks are fully resolved.
//!
//! `Select` expressions are still skipped (no `select` in demo-3).

use crate::runtime::ast::{ActionStmt, Decl, Expr, Field, Stmt, Value};
use crate::runtime::interner::Symbol;
use crate::runtime::limits::MAX_SCOPE_DEPTH;
use crate::runtime::Env;
use std::collections::HashMap;
use std::fmt;

/// The sort of identifier expected during name resolution
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpectedSort {
    Service,
    Table,
    Variable,
}

impl fmt::Display for ExpectedSort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExpectedSort::Service => write!(f, "service"),
            ExpectedSort::Table => write!(f, "table"),
            ExpectedSort::Variable => write!(f, "variable"),
        }
    }
}

/// Errors that can occur during name resolution
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// An identifier was referenced but not declared in scope
    UnknownIdentifier {
        /// The unknown identifier symbol
        name: Symbol,
        /// The expected sort of the identifier
        expected: ExpectedSort,
        /// The name of the surrounding context (e.g. service name), if any
        context_name: Option<Symbol>,
    },
    /// The AST nesting depth exceeded the limit
    DepthLimit,
    /// A value was referenced eagerly before being declared
    ForwardReference(Symbol),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::UnknownIdentifier {
                name,
                expected,
                context_name,
            } => {
                if let Some(ctx) = context_name {
                    write!(
                        f,
                        "Unknown identifier '{:?}' (expected {}) in context '{:?}'",
                        name, expected, ctx
                    )
                } else {
                    write!(f, "Unknown identifier '{:?}' (expected {})", name, expected)
                }
            }
            Error::DepthLimit => {
                write!(f, "Depth limit exceeded")
            }
            Error::ForwardReference(name) => {
                write!(
                    f,
                    "Invalid forward reference to uninitialized value '{:?}'",
                    name
                )
            }
        }
    }
}

impl std::error::Error for Error {}

/// Represents the type of binding stored in the name resolution environment
#[derive(Debug, Clone)]
pub enum Binding {
    /// A local value: `let`, `def`, `var`, or a parameter
    Value,
    /// A service or import name
    Service,
    /// A `map` row, with the table's columns
    Row(Vec<Field>),
}

/// One declaration recorded for member checks. Tables keep their columns.
#[derive(Clone)]
struct ServiceDecl {
    name: Symbol,
    table_fields: Option<Vec<Field>>,
}

fn bind_member(env: &mut Env<'_, Binding>, decl: &ServiceDecl) {
    if let Some(fields) = &decl.table_fields {
        env.bind(decl.name, Binding::Row(fields.clone()));
    } else {
        env.bind(decl.name, Binding::Value);
    }
}

/// The stateful struct that drives static name resolution traversal
///
/// This resolver performs pure lexical, block-level name resolution.
/// It delegates execution-order dependency checking to a separate
/// static analysis pass, ensuring that lexical lookup rules are
/// enforced cleanly without rigid ordering constraints.
pub struct Resolver {
    local_services: HashMap<Symbol, Vec<ServiceDecl>>,
    current_context: Option<Symbol>,
    /// Set while resolving a function or action body. A table used there may
    /// be declared later in the service; an eager use may not.
    deferred: bool,
}

fn owned_decls(decls: &[Decl]) -> Vec<ServiceDecl> {
    decls
        .iter()
        .map(|decl| match decl {
            Decl::VarDecl { name, .. } | Decl::DefDecl { name, .. } => ServiceDecl {
                name: *name,
                table_fields: None,
            },
            Decl::TableDecl { name, fields } => ServiceDecl {
                name: *name,
                table_fields: Some(fields.clone()),
            },
        })
        .collect()
}

impl Default for Resolver {
    fn default() -> Self {
        Self::new()
    }
}

impl Resolver {
    /// Creates a new resolver instance
    ///
    /// Returns:
    ///     Self: The new `Resolver` instance
    pub fn new() -> Self {
        let resolver = Self {
            local_services: HashMap::new(),
            current_context: None,
            deferred: false,
        };
        debug_assert!(resolver.current_context.is_none());
        resolver
    }

    /// Columns of a table declared on the current service, in any order.
    /// Eager references use the environment instead; this is only the
    /// forward-reference fallback inside a function or action.
    fn declared_table(&self, table_name: Symbol) -> Option<Vec<Field>> {
        let svc = self.current_context?;
        self.local_services.get(&svc)?.iter().find_map(|decl| {
            if decl.name == table_name {
                decl.table_fields.clone()
            } else {
                None
            }
        })
    }

    fn resolve_table(&self, table_name: Symbol, env: &Env<'_, Binding>) -> Result<Vec<Field>, Error> {
        let missing = Error::UnknownIdentifier {
            name: table_name,
            expected: ExpectedSort::Table,
            context_name: self.current_context,
        };
        match env.find(table_name) {
            Some(Binding::Row(fields)) => Ok(fields.clone()),
            Some(_) => Err(missing),
            None if self.deferred => self.declared_table(table_name).ok_or(missing),
            None => Err(missing),
        }
    }

    fn declares(&self, service: Symbol, name: Symbol) -> bool {
        self.local_services
            .get(&service)
            .is_some_and(|decls| decls.iter().any(|decl| decl.name == name))
    }

    /// Resolves name bindings for a program represented as a slice
    /// of `Stmt`s
    ///
    /// Args:
    ///     `stmts` (`&mut [Stmt]`): The statements of the program
    ///     `env` (`&mut Env<Binding>`): The current environment
    ///
    /// Returns:
    ///     `Result<(), Error>`: Ok if resolution succeeds, or `Error`
    pub fn resolve_program(
        &mut self,
        stmts: &mut [Stmt],
        env: &mut Env<'_, Binding>,
    ) -> Result<(), Error> {
        // Pass 1: Bind top-level services and imports, and record
        // local service declarations
        for stmt in stmts.iter() {
            match stmt {
                Stmt::Service { name, decls } => {
                    env.bind(*name, Binding::Service);
                    self.local_services.insert(*name, owned_decls(decls));
                }
                Stmt::Import { service_name, .. } => {
                    env.bind(*service_name, Binding::Service);
                }
                Stmt::ActionStmt(_)
                | Stmt::Atomic { .. }
                | Stmt::Update { .. }
                | Stmt::Connect { .. }
                | Stmt::Test { .. }
                | Stmt::Watch { .. } => {}
            }
        }

        // Pass 2: Resolve all statements sequentially, rewriting `x.f`
        for stmt in stmts.iter_mut() {
            self.resolve_stmt(stmt, env)?;
        }

        Ok(())
    }

    /// Resolves name bindings in a single statement
    ///
    /// Args:
    ///     `stmt` (`&mut Stmt`): The statement to resolve
    ///     `env` (`&mut Env<Binding>`): The environment
    ///
    /// Returns:
    ///     `Result<(), Error>`: Ok if resolution succeeds, or `Error`
    fn resolve_stmt(&mut self, stmt: &mut Stmt, env: &mut Env<'_, Binding>) -> Result<(), Error> {
        match stmt {
            Stmt::ActionStmt(action) => {
                let prev = self.deferred;
                self.deferred = true;
                let res = self.resolve_action_stmt(action, env, 0);
                self.deferred = prev;
                res
            }
            Stmt::Atomic { updates } => {
                for u in updates {
                    self.resolve_stmt(u, env)?;
                }
                Ok(())
            }
            Stmt::Update {
                service_name,
                decls,
            } => {
                if !self.local_services.contains_key(service_name) {
                    return Err(Error::UnknownIdentifier {
                        name: *service_name,
                        expected: ExpectedSort::Service,
                        context_name: self.current_context,
                    });
                }
                let service_name = *service_name;
                let prev_context = self.current_context;
                self.current_context = Some(service_name);
                let prior = self
                    .local_services
                    .get(&service_name)
                    .cloned()
                    .unwrap_or_default();
                let new_tables: Vec<ServiceDecl> = decls
                    .iter()
                    .filter_map(|decl| match decl {
                        Decl::TableDecl { name, fields } => Some(ServiceDecl {
                            name: *name,
                            table_fields: Some(fields.clone()),
                        }),
                        _ => None,
                    })
                    .collect();
                if let Some(svc_decls) = self.local_services.get_mut(&service_name) {
                    svc_decls.extend(new_tables);
                }
                let mut update_env = Env::new(Some(env));
                for decl in &prior {
                    bind_member(&mut update_env, decl);
                }
                for decl in decls.iter_mut() {
                    match decl {
                        Decl::VarDecl { name, val, .. } | Decl::DefDecl { name, val, .. } => {
                            if let Err(e) = self.resolve_expr(val, &update_env, 0) {
                                self.current_context = prev_context;
                                return Err(e);
                            }
                            let name = *name;
                            update_env.bind(name, Binding::Value);
                            if let Some(svc_decls) = self.local_services.get_mut(&service_name) {
                                svc_decls.push(ServiceDecl {
                                    name,
                                    table_fields: None,
                                });
                            }
                        }
                        Decl::TableDecl { name, fields } => {
                            update_env.bind(*name, Binding::Row(fields.clone()));
                        }
                    }
                }
                self.current_context = prev_context;
                Ok(())
            }
            Stmt::Connect { path: _, addr: _ } => Ok(()),
            Stmt::Import {
                path: _,
                service_name,
            } => {
                env.bind(*service_name, Binding::Service);
                Ok(())
            }
            Stmt::Service { name, decls } => {
                env.bind(*name, Binding::Service);
                let mut service_env = Env::new(Some(env));
                let prev_context = self.current_context;
                self.current_context = Some(*name);
                let res = self.resolve_service(decls, &mut service_env);
                self.current_context = prev_context;
                res
            }
            Stmt::Test {
                service_name,
                stmts,
            } => {
                if env.find(*service_name).is_none() {
                    return Err(Error::UnknownIdentifier {
                        name: *service_name,
                        expected: ExpectedSort::Service,
                        context_name: self.current_context,
                    });
                }
                let prev_context = self.current_context;
                self.current_context = Some(*service_name);
                let mut test_env = Env::new(Some(env));
                let members = match self.local_services.get(service_name) {
                    Some(decls) => decls.clone(),
                    None => {
                        self.current_context = prev_context;
                        return Err(Error::UnknownIdentifier {
                            name: *service_name,
                            expected: ExpectedSort::Service,
                            context_name: self.current_context,
                        });
                    }
                };
                for decl in &members {
                    bind_member(&mut test_env, decl);
                }
                let res = self.resolve_action_stmts(stmts, &mut test_env, 0);
                self.current_context = prev_context;
                res
            }
            Stmt::Watch { expr } => self.resolve_expr(expr, env, 0),
        }
    }

    /// Resolves service-level declarations sequentially
    ///
    /// A table is bound, with its columns, when its declaration is reached.
    /// `map`, `fold`, and `insert` look that binding up in the environment.
    /// Args:
    ///     `decls` (`&mut [Decl]`): The declarations in the service
    ///     `env` (`&mut Env<Binding>`): The environment
    ///
    /// Returns:
    ///     `Result<(), Error>`: `Ok` if resolution succeeds, or `Error`
    fn resolve_service(
        &mut self,
        decls: &mut [Decl],
        env: &mut Env<'_, Binding>,
    ) -> Result<(), Error> {
        for decl in decls {
            match decl {
                Decl::VarDecl { name, ty: _, val } => {
                    self.resolve_expr(val, env, 0)?;
                    env.bind(*name, Binding::Value);
                }
                Decl::DefDecl {
                    name,
                    ty: _,
                    val,
                    is_pub: _,
                } => {
                    self.resolve_expr(val, env, 0)?;
                    env.bind(*name, Binding::Value);
                }
                Decl::TableDecl { name, fields } => {
                    env.bind(*name, Binding::Row(fields.clone()));
                }
            }
        }
        Ok(())
    }

    /// Resolves a list of action statements sequentially
    ///
    /// Args:
    ///     `stmts` (`&mut [ActionStmt]`): The action statements to resolve
    ///     `env` (`&mut Env<Binding>`): The current environment
    ///
    /// Returns:
    ///     `Result<(), Error>`: Ok if resolution succeeds, or `Error`
    fn resolve_action_stmts(
        &mut self,
        stmts: &mut [ActionStmt],
        env: &mut Env<'_, Binding>,
        depth: usize,
    ) -> Result<(), Error> {
        for stmt in stmts {
            self.resolve_action_stmt(stmt, env, depth)?;
        }
        Ok(())
    }

    /// Resolves a single action statement
    ///
    /// Args:
    ///     `stmt` (`&'a ActionStmt`): The action statement to resolve
    ///     `env` (`&mut Env<'b, Binding<'a>>`): The environment
    ///
    /// Returns:
    ///     `Result<(), Error>`: Ok if resolution succeeds, or `Error`
    fn resolve_action_stmt(
        &mut self,
        stmt: &mut ActionStmt,
        env: &mut Env<'_, Binding>,
        depth: usize,
    ) -> Result<(), Error> {
        if depth >= MAX_SCOPE_DEPTH {
            return Err(Error::DepthLimit);
        }
        match stmt {
            ActionStmt::Let { name, ty: _, expr } => {
                self.resolve_expr(expr, env, depth + 1)?;
                env.bind(*name, Binding::Value);
                Ok(())
            }
            ActionStmt::Expr(expr) => self.resolve_expr(expr, env, depth + 1),
            ActionStmt::Do(expr) => self.resolve_expr(expr, env, depth + 1),
            ActionStmt::Assert(expr, _text) => self.resolve_expr(expr, env, depth + 1),
            ActionStmt::Assign { name, expr } => {
                if env.find(*name).is_none() {
                    let is_local_member = self
                        .current_context
                        .is_some_and(|ctx| self.declares(ctx, *name));

                    if !is_local_member {
                        return Err(Error::UnknownIdentifier {
                            name: *name,
                            expected: ExpectedSort::Variable,
                            context_name: self.current_context,
                        });
                    }
                }
                self.resolve_expr(expr, env, depth + 1)
            }
            ActionStmt::Insert { row, table_name } => {
                self.resolve_table(*table_name, env)?;
                self.resolve_expr(row, env, depth + 1)
            }
            ActionStmt::For {
                var,
                iterable,
                body,
            } => {
                self.resolve_expr(iterable, env, depth + 1)?;
                let mut loop_env = Env::new(Some(env));
                loop_env.bind(*var, Binding::Value);
                self.resolve_action_stmts(body, &mut loop_env, depth + 1)
            }
        }
    }

    /// Resolves variable names within an expression
    ///
    /// Args:
    ///     `expr` (`&mut Expr`): The expression to resolve
    ///     `env` (`&Env<Binding>`): The environment
    ///
    /// Returns:
    ///     `Result<(), Error>`: Ok if resolution succeeds, or `Error`
    fn resolve_expr(
        &mut self,
        expr: &mut Expr,
        env: &Env<'_, Binding>,
        depth: usize,
    ) -> Result<(), Error> {
        if depth >= MAX_SCOPE_DEPTH {
            return Err(Error::DepthLimit);
        }
        let pending = if let Expr::MemberAccess {
            service_name,
            member_name,
        } = &*expr
        {
            Some((*service_name, *member_name))
        } else {
            None
        };
        if let Some((receiver, member)) = pending {
            *expr = self.classify_access(receiver, member, env)?;
            return Ok(());
        }
        match expr {
            Expr::Literal { val } => self.resolve_value(val, env, depth + 1),
            Expr::Html(template) => {
                for e in template.embedded_exprs_mut() {
                    self.resolve_expr(e, env, depth + 1)?;
                }
                Ok(())
            }
            Expr::Variable { name } => {
                if env.find(*name).is_none() {
                    let is_local_member = self
                        .current_context
                        .is_some_and(|ctx| self.declares(ctx, *name));

                    if is_local_member {
                        return Ok(());
                    }

                    return Err(Error::UnknownIdentifier {
                        name: *name,
                        expected: ExpectedSort::Variable,
                        context_name: self.current_context,
                    });
                }
                Ok(())
            }
            Expr::Tuple { val } => {
                for e in val {
                    self.resolve_expr(e, env, depth + 1)?;
                }
                Ok(())
            }
            Expr::KeyVal { name: _, value } => self.resolve_expr(value.as_mut(), env, depth + 1),
            Expr::Unop { op: _, expr } => self.resolve_expr(expr.as_mut(), env, depth + 1),
            Expr::Binop {
                op: _,
                expr1,
                expr2,
            } => {
                self.resolve_expr(expr1.as_mut(), env, depth + 1)?;
                self.resolve_expr(expr2.as_mut(), env, depth + 1)
            }
            Expr::If { cond, expr1, expr2 } => {
                self.resolve_expr(cond.as_mut(), env, depth + 1)?;
                self.resolve_expr(expr1.as_mut(), env, depth + 1)?;
                self.resolve_expr(expr2.as_mut(), env, depth + 1)
            }
            Expr::Func {
                params,
                body,
                return_ty: _,
            } => {
                let mut inner_env = Env::new(Some(env));
                for param in params {
                    inner_env.bind(param.name, Binding::Value);
                }
                let prev = self.deferred;
                self.deferred = true;
                let res = self.resolve_expr(body.as_mut(), &inner_env, depth + 1);
                self.deferred = prev;
                res?;
                Ok(())
            }
            Expr::Call { func, args } => {
                self.resolve_expr(func.as_mut(), env, depth + 1)?;
                for arg in args {
                    self.resolve_expr(arg, env, depth + 1)?;
                }
                Ok(())
            }
            Expr::Action(stmts) => {
                let mut action_env = Env::new(Some(env));
                let prev = self.deferred;
                self.deferred = true;
                let res = self.resolve_action_stmts(stmts, &mut action_env, depth + 1);
                self.deferred = prev;
                res?;
                Ok(())
            }
            Expr::MemberAccess { .. } => {
                unreachable!("member access is rewritten before this match")
            }
            Expr::FieldAccess { .. } | Expr::ServiceAccess { .. } => Ok(()),
            Expr::Select {
                table_name: _,
                column_names: _,
                where_clause: _,
            } => {
                println!(
                    "warning: nameres: ignoring 'select' checks \
                     as not yet implemented"
                );
                Ok(())
            }
            Expr::Table { schema: _, records } => {
                for r in records {
                    self.resolve_expr(r, env, depth + 1)?;
                }
                Ok(())
            }
            Expr::Fold {
                table_name,
                column_name,
                operation,
                identity,
            } => {
                let fields = self.resolve_table(*table_name, env)?;
                if !fields.iter().any(|f| f.name == *column_name) {
                    return Err(Error::UnknownIdentifier {
                        name: *column_name,
                        expected: ExpectedSort::Variable,
                        context_name: Some(*table_name),
                    });
                }
                self.resolve_expr(operation, env, depth + 1)?;
                self.resolve_expr(identity, env, depth + 1)
            }
            Expr::Map {
                var,
                table_name,
                where_clause,
                body,
            } => {
                let fields = self.resolve_table(*table_name, env)?;
                let var = *var;
                let mut row_env = Env::new(Some(env));
                row_env.bind(var, Binding::Row(fields));
                self.resolve_expr(where_clause, &row_env, depth + 1)?;
                self.resolve_expr(body, &row_env, depth + 1)
            }
            Expr::List(exprs) => {
                for expr in exprs {
                    self.resolve_expr(expr, env, depth + 1)?;
                }
                Ok(())
            }
            Expr::Range { start, end } => {
                self.resolve_expr(start.as_mut(), env, depth + 1)?;
                self.resolve_expr(end.as_mut(), env, depth + 1)
            }
        }
    }

    /// Resolves variable names within a value
    ///
    /// Args:
    ///     `val` (`&'a Value`): The value to resolve
    ///     `env` (`&Env<'b, Binding<'a>>`): The current environment
    ///
    /// Returns:
    ///     `Result<(), Error>`: Ok if resolution succeeds, or `Error`
    fn classify_access(
        &self,
        receiver: Symbol,
        member: Symbol,
        env: &Env<'_, Binding>,
    ) -> Result<Expr, Error> {
        match env.find(receiver) {
            Some(Binding::Row(fields)) => {
                if !fields.iter().any(|field| field.name == member) {
                    return Err(Error::UnknownIdentifier {
                        name: member,
                        expected: ExpectedSort::Variable,
                        context_name: Some(receiver),
                    });
                }
                Ok(Expr::FieldAccess {
                    var: receiver,
                    field: member,
                })
            }
            Some(Binding::Value) => Ok(Expr::FieldAccess {
                var: receiver,
                field: member,
            }),
            Some(Binding::Service) => self.service_access(receiver, member),
            None => Err(Error::UnknownIdentifier {
                name: receiver,
                expected: ExpectedSort::Service,
                context_name: self.current_context,
            }),
        }
    }

    fn service_access(&self, service: Symbol, member: Symbol) -> Result<Expr, Error> {
        if let Some(decls) = self.local_services.get(&service) {
            if !decls.iter().any(|decl| decl.name == member) {
                return Err(Error::UnknownIdentifier {
                    name: member,
                    expected: ExpectedSort::Variable,
                    context_name: Some(service),
                });
            }
        }
        Ok(Expr::ServiceAccess { service, member })
    }

    fn resolve_value(
        &mut self,
        val: &mut Value,
        env: &Env<'_, Binding>,
        depth: usize,
    ) -> Result<(), Error> {
        if depth >= MAX_SCOPE_DEPTH {
            return Err(Error::DepthLimit);
        }
        match val {
            Value::Int { val: _ } => Ok(()),
            Value::Bool { val: _ } => Ok(()),
            Value::String { val: _ } => Ok(()),
            Value::Html(_) => Ok(()),
            Value::Closure {
                params,
                body,
                env: _,
                service_name: _,
                return_ty: _,
            } => {
                let mut inner_env = Env::new(Some(env));
                for param in params {
                    inner_env.bind(param.name, Binding::Value);
                }
                let prev = self.deferred;
                self.deferred = true;
                let res = self.resolve_expr(body.as_mut(), &inner_env, depth + 1);
                self.deferred = prev;
                res?;
                Ok(())
            }
            Value::ActionClosure {
                stmts,
                env: _,
                service_net_id: _,
            } => {
                let mut action_env = Env::new(Some(env));
                let prev = self.deferred;
                self.deferred = true;
                let res = self.resolve_action_stmts(stmts, &mut action_env, depth + 1);
                self.deferred = prev;
                res?;
                Ok(())
            }
            Value::List { vals } => {
                for val in vals {
                    self.resolve_value(val, env, depth + 1)?;
                }
                Ok(())
            }
            Value::Record { fields } => {
                for (_, val) in fields {
                    self.resolve_value(val, env, depth + 1)?;
                }
                Ok(())
            }
            Value::Range { start: _, end: _ } => Ok(()),
        }
    }
}

/// The public entry point to resolve name bindings in a program
///
/// Args:
///     `stmts` (`&[Stmt]`): The statements of the program
///
/// Returns:
///     `Result<(), Error>`: Ok if all symbols are bound correctly
pub fn resolve(stmts: &mut [Stmt]) -> Result<(), Error> {
    let mut resolver = Resolver::new();
    let mut root_env = Env::new(None);
    resolver.resolve_program(stmts, &mut root_env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::ast::{ActionStmt, Decl, Expr, Stmt, Value};
    use crate::runtime::interner::Interner;
    use crate::runtime::tt::Param;

    /// Verify service accepts forward references in pure lexical resolution
    #[test]
    fn test_unit_service_accepts_forward_reference() {
        let mut interner = Interner::new();
        let s = interner.insert("s");
        let x = interner.insert("x");
        let y = interner.insert("y");

        let decl_x = Decl::VarDecl {
            name: x,
            ty: None,
            val: Expr::Literal {
                val: Value::Int { val: 5 },
            },
        };

        let decl_y = Decl::DefDecl {
            name: y,
            ty: None,
            val: Expr::Variable { name: x },
            is_pub: false,
        };

        let stmt = Stmt::Service {
            name: s,
            decls: vec![decl_y, decl_x],
        };

        let mut env = Env::new(None);
        let mut resolver = Resolver::new();
        let mut program = vec![stmt];
        let res = resolver.resolve_program(&mut program, &mut env);
        assert!(res.is_ok());
    }

    /// Verify sequential block scoping and declaration-before-use
    #[test]
    fn test_unit_sequential_lexical_scoping() {
        let mut interner = Interner::new();
        let s = interner.insert("s");
        let x = interner.insert("x");
        let local_x = interner.insert("x");

        // Service scope has x
        let decl_x = Decl::VarDecl {
            name: x,
            ty: None,
            val: Expr::Literal {
                val: Value::Int { val: 5 },
            },
        };

        // Action block:
        // do x;
        // let x = 10;
        // do x;
        let action_expr = Expr::Action(vec![
            ActionStmt::Expr(Expr::Variable { name: local_x }),
            ActionStmt::Let {
                name: local_x,
                ty: None,
                expr: Expr::Literal {
                    val: Value::Int { val: 10 },
                },
            },
            ActionStmt::Expr(Expr::Variable { name: local_x }),
        ]);

        let decl_action = Decl::DefDecl {
            name: interner.insert("act"),
            ty: None,
            val: action_expr,
            is_pub: true,
        };

        let stmt = Stmt::Service {
            name: s,
            decls: vec![decl_x, decl_action],
        };

        let mut env = Env::new(None);
        let mut resolver = Resolver::new();
        let mut program = vec![stmt];
        assert!(resolver.resolve_program(&mut program, &mut env).is_ok());

        // Test sequential failure (no service variable x exists)
        // action { do x; let x = 10; }
        // The first 'do x' must fail because x is not yet declared
        let bad_action = Expr::Action(vec![
            ActionStmt::Expr(Expr::Variable { name: local_x }),
            ActionStmt::Let {
                name: local_x,
                ty: None,
                expr: Expr::Literal {
                    val: Value::Int { val: 10 },
                },
            },
        ]);

        let bad_decl = Decl::DefDecl {
            name: interner.insert("act2"),
            ty: None,
            val: bad_action,
            is_pub: true,
        };

        let bad_stmt = Stmt::Service {
            name: s,
            decls: vec![bad_decl],
        };

        let mut bad_env = Env::new(None);
        let mut bad_resolver = Resolver::new();
        let mut program_bad = vec![bad_stmt];
        let res = bad_resolver.resolve_program(&mut program_bad, &mut bad_env);
        assert_eq!(
            res,
            Err(Error::UnknownIdentifier {
                name: local_x,
                expected: ExpectedSort::Variable,
                context_name: Some(s),
            })
        );
    }

    /// Verify lexical isolation between sibling action blocks
    #[test]
    fn test_unit_scope_isolation_and_nesting() {
        let mut interner = Interner::new();
        let x = interner.insert("x");

        // Sibling blocks:
        // block1: action { let x = 5; }
        // block2: action { do x; }
        // block2 must fail because block1's let binding does not leak
        let block1 = ActionStmt::Expr(Expr::Action(vec![ActionStmt::Let {
            name: x,
            ty: None,
            expr: Expr::Literal {
                val: Value::Int { val: 5 },
            },
        }]));

        let block2 = ActionStmt::Expr(Expr::Action(vec![ActionStmt::Expr(Expr::Variable {
            name: x,
        })]));

        let decl = Decl::DefDecl {
            name: interner.insert("d"),
            ty: None,
            val: Expr::Action(vec![block1, block2]),
            is_pub: true,
        };

        let s = interner.insert("s");
        let stmt = Stmt::Service {
            name: s,
            decls: vec![decl],
        };

        let mut env = Env::new(None);
        let mut resolver = Resolver::new();
        let mut program = vec![stmt];
        let res = resolver.resolve_program(&mut program, &mut env);
        assert_eq!(
            res,
            Err(Error::UnknownIdentifier {
                name: x,
                expected: ExpectedSort::Variable,
                context_name: Some(s),
            })
        );
    }

    /// Verify nested function parameter binding and scope capture
    #[test]
    fn test_unit_closure_variable_capture() {
        let mut interner = Interner::new();
        let a = interner.insert("a");
        let b = interner.insert("b");

        // fn a => fn b => (a + b)
        let body = Expr::Binop {
            op: crate::runtime::ast::BinOp::Add,
            expr1: Box::new(Expr::Variable { name: a }),
            expr2: Box::new(Expr::Variable { name: b }),
        };

        let inner_closure = Expr::Func {
            params: vec![Param { name: b, ty: None }],
            body: Box::new(body),
            return_ty: None,
        };

        let outer_closure = Expr::Func {
            params: vec![Param { name: a, ty: None }],
            body: Box::new(inner_closure),
            return_ty: None,
        };

        let stmt = Stmt::Watch {
            expr: outer_closure,
        };
        let mut env = Env::new(None);
        let mut resolver = Resolver::new();
        let mut program = vec![stmt];
        assert!(resolver.resolve_program(&mut program, &mut env).is_ok());

        // Verify parameter escaping triggers error:
        // fn a => (fn b => b) + b
        // The last 'b' should fail as b has escaped its lexical closure
        let escaped_body = Expr::Binop {
            op: crate::runtime::ast::BinOp::Add,
            expr1: Box::new(Expr::Func {
                params: vec![Param { name: b, ty: None }],
                body: Box::new(Expr::Variable { name: b }),
                return_ty: None,
            }),
            expr2: Box::new(Expr::Variable { name: b }),
        };

        let escaped_closure = Expr::Func {
            params: vec![Param { name: a, ty: None }],
            body: Box::new(escaped_body),
            return_ty: None,
        };

        let escaped_stmt = Stmt::Watch {
            expr: escaped_closure,
        };
        let mut env_escaped = Env::new(None);
        let mut resolver_escaped = Resolver::new();
        let mut program_escaped = vec![escaped_stmt];
        let res = resolver_escaped.resolve_program(&mut program_escaped, &mut env_escaped);
        assert_eq!(
            res,
            Err(Error::UnknownIdentifier {
                name: b,
                expected: ExpectedSort::Variable,
                context_name: None,
            })
        );
    }

    /// Verify update block resolves declarations using sequential scoping
    #[test]
    fn test_unit_update_block_sequential_scoping() {
        let mut interner = Interner::new();
        let s = interner.insert("s");
        let x = interner.insert("x");
        let y = interner.insert("y");

        let s_stmt = Stmt::Service {
            name: s,
            decls: vec![],
        };

        // update s { var x = 5; def y = x; }
        let mut update_stmt = Stmt::Update {
            service_name: s,
            decls: vec![
                Decl::VarDecl {
                    name: x,
                    ty: None,
                    val: Expr::Literal {
                        val: Value::Int { val: 5 },
                    },
                },
                Decl::DefDecl {
                    name: y,
                    ty: None,
                    val: Expr::Variable { name: x },
                    is_pub: false,
                },
            ],
        };

        let mut env = Env::new(None);
        let mut resolver = Resolver::new();
        let mut program = vec![s_stmt, update_stmt];
        let res = resolver.resolve_program(&mut program, &mut env);
        assert_eq!(res, Ok(()));
    }

    /// Verify update block forward reference to new member fails
    #[test]
    fn test_unit_update_block_forward_ref_fails() {
        let mut interner = Interner::new();
        let s = interner.insert("s");
        let x = interner.insert("x");
        let y = interner.insert("y");

        let s_stmt = Stmt::Service {
            name: s,
            decls: vec![],
        };

        // update s { def y = x; var x = 5; } -- x is new, so y cannot forward ref x
        let mut update_stmt = Stmt::Update {
            service_name: s,
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

        let mut env = Env::new(None);
        let mut resolver = Resolver::new();
        let mut program = vec![s_stmt, update_stmt];
        let res = resolver.resolve_program(&mut program, &mut env);
        assert_eq!(
            res,
            Err(Error::UnknownIdentifier {
                name: x,
                expected: ExpectedSort::Variable,
                context_name: Some(s),
            })
        );
    }

    /// Verify that `@test` blocks can resolve variables
    /// in the target service
    #[test]
    fn test_unit_test_block_member_resolution() {
        let mut interner = Interner::new();
        let s = interner.insert("s");
        let x = interner.insert("x");

        // Representing `service s { var x = 5; }`
        let s_stmt = Stmt::Service {
            name: s,
            decls: vec![Decl::VarDecl {
                name: x,
                ty: None,
                val: Expr::Literal {
                    val: Value::Int { val: 5 },
                },
            }],
        };

        // Representing `@test(s) { x; }`
        let mut test_stmt = Stmt::Test {
            service_name: s,
            stmts: vec![ActionStmt::Expr(Expr::Variable { name: x })],
        };

        let mut env = Env::new(None);
        let mut resolver = Resolver::new();
        let mut program = vec![s_stmt, test_stmt];

        assert!(resolver.resolve_program(&mut program, &mut env).is_ok());
    }

    /// Verify `@test` blocks can resolve services that
    /// are defined after them in the program statements
    #[test]
    fn test_unit_test_block_hoisting() {
        let mut interner = Interner::new();
        let s = interner.insert("s");
        let x = interner.insert("x");

        // Representing `@test(s) { x; }`
        let mut test_stmt = Stmt::Test {
            service_name: s,
            stmts: vec![ActionStmt::Expr(Expr::Variable { name: x })],
        };

        // Representing `service s { var x = 5; }`
        let s_stmt = Stmt::Service {
            name: s,
            decls: vec![Decl::VarDecl {
                name: x,
                ty: None,
                val: Expr::Literal {
                    val: Value::Int { val: 5 },
                },
            }],
        };

        let mut env = Env::new(None);
        let mut resolver = Resolver::new();
        let mut program = vec![test_stmt, s_stmt];

        assert!(resolver.resolve_program(&mut program, &mut env).is_ok());
    }

    /// Verify testing an imported service resolves when its
    /// `Stmt::Service` is present in the unified AST
    #[test]
    fn test_unit_test_block_imported_service_resolves() {
        let mut interner = Interner::new();
        let s = interner.insert("s");
        let x = interner.insert("x");

        // Simulates unified_ast: the imported service's Stmt::Service is
        // present alongside the @test block, as produced by on_node_startup.
        // Representing:
        //   service s { var x = 0; }
        //   @test(s) { x; }
        let mut service_stmt = Stmt::Service {
            name: s,
            decls: vec![Decl::VarDecl {
                name: x,
                ty: None,
                val: Expr::Literal {
                    val: Value::Int { val: 0 },
                },
            }],
        };
        let mut test_stmt = Stmt::Test {
            service_name: s,
            stmts: vec![ActionStmt::Expr(Expr::Variable { name: x })],
        };

        let mut env = Env::new(None);
        let mut resolver = Resolver::new();
        let mut program = vec![service_stmt, test_stmt];

        let result = resolver.resolve_program(&mut program, &mut env);
        assert!(result.is_ok());
    }

    /// Verify testing an undefined service yields an
    /// `UnknownIdentifier` error
    #[test]
    fn test_unit_test_block_undefined_service() {
        let mut interner = Interner::new();
        let s = interner.insert("s");
        let x = interner.insert("x");

        // Representing `@test(s) { x; }`
        let mut test_stmt = Stmt::Test {
            service_name: s,
            stmts: vec![ActionStmt::Expr(Expr::Variable { name: x })],
        };

        let mut env = Env::new(None);
        let mut resolver = Resolver::new();
        let mut program = vec![test_stmt];

        let result = resolver.resolve_program(&mut program, &mut env);
        assert!(result.is_err());
        match result.unwrap_err() {
            Error::UnknownIdentifier {
                name,
                expected,
                context_name,
            } => {
                assert_eq!(name, s);
                assert_eq!(expected, ExpectedSort::Service);
                assert_eq!(context_name, None);
            }
            Error::DepthLimit | Error::ForwardReference(..) => {
                panic!("Expected UnknownIdentifier error");
            }
        }
    }

    /// Verify that deeply nested expressions trigger the depth limit
    #[test]
    fn test_unit_expression_depth_limit() {
        let mut interner = Interner::new();
        let x = interner.insert("x");

        // Create a deeply nested expression structure
        // Start with a simple variable expression
        let mut deep_expr = Expr::Variable { name: x };

        // Nest 130 binary operations to trigger depth limit error
        for _ in 0..130 {
            deep_expr = Expr::Binop {
                op: crate::runtime::ast::BinOp::Add,
                expr1: Box::new(deep_expr),
                expr2: Box::new(Expr::Literal {
                    val: Value::Int { val: 1 },
                }),
            };
        }

        let mut env = Env::new(None);
        env.bind(x, Binding::Value);

        let mut resolver = Resolver::new();
        let res = resolver.resolve_expr(&mut deep_expr, &env, 0);
        assert_eq!(res, Err(Error::DepthLimit))
    }

    /// Verify that deeply nested action statements trigger the depth limit
    #[test]
    fn test_unit_for_loop_depth_limit() {
        let mut interner = Interner::new();
        let x = interner.insert("x");
        let v = interner.insert("v");

        // Create a deeply nested loop structure
        // Start with a simple expression in the innermost loop
        let mut deep_stmt = ActionStmt::Expr(Expr::Literal {
            val: Value::Int { val: 0 },
        });

        // Nest 130 for loops to trigger depth limit error
        for _ in 0..130 {
            deep_stmt = ActionStmt::For {
                var: x,
                iterable: Expr::Variable { name: v },
                body: vec![deep_stmt],
            };
        }

        let mut env = Env::new(None);
        env.bind(v, Binding::Value);

        let mut resolver = Resolver::new();
        let res = resolver.resolve_action_stmt(&mut deep_stmt, &mut env, 0);
        assert_eq!(res, Err(Error::DepthLimit))
    }

    /// Verify that `Stmt::Update` initializer resolution failure
    /// restores `current_context` to `None` on the `Resolver`
    #[test]
    fn test_unit_update_block_restores_context_on_error() {
        let mut interner = Interner::new();
        let s = interner.insert("s");
        let x = interner.insert("x");
        let unbound = interner.insert("unbound");
        let y = interner.insert("y");

        let mut service_stmt = Stmt::Service {
            name: s,
            decls: vec![Decl::VarDecl {
                name: x,
                ty: None,
                val: Expr::Literal {
                    val: Value::Int { val: 0 },
                },
            }],
        };

        let mut update_stmt = Stmt::Update {
            service_name: s,
            decls: vec![Decl::VarDecl {
                name: x,
                ty: None,
                val: Expr::Variable { name: unbound },
            }],
        };

        let mut env = Env::new(None);
        let mut resolver = Resolver::new();

        // Register service definition first
        let reg_res = resolver.resolve_stmt(&mut service_stmt, &mut env);
        assert!(reg_res.is_ok());

        // Resolve update with invalid initializer
        let update_res = resolver.resolve_stmt(&mut update_stmt, &mut env);
        assert!(update_res.is_err());
        assert_eq!(resolver.current_context, None);

        // Verify reusable resolver does not leak stale context_name
        let mut watch_stmt = Stmt::Watch {
            expr: Expr::Variable { name: y },
        };
        let second_res = resolver.resolve_stmt(&mut watch_stmt, &mut env);
        assert!(second_res.is_err());
        match second_res.unwrap_err() {
            Error::UnknownIdentifier { context_name, .. } => {
                assert_eq!(context_name, None);
            }
            _ => panic!("Expected UnknownIdentifier error"),
        }
    }

    /// Verify that `Stmt::Test` action resolution failure
    /// restores `current_context` to `None` on the `Resolver`
    #[test]
    fn test_unit_test_block_restores_context_on_error() {
        let mut interner = Interner::new();
        let s = interner.insert("s");
        let x = interner.insert("x");
        let unbound = interner.insert("unbound");
        let y = interner.insert("y");

        let mut service_stmt = Stmt::Service {
            name: s,
            decls: vec![Decl::VarDecl {
                name: x,
                ty: None,
                val: Expr::Literal {
                    val: Value::Int { val: 0 },
                },
            }],
        };

        let mut test_stmt = Stmt::Test {
            service_name: s,
            stmts: vec![ActionStmt::Expr(Expr::Variable { name: unbound })],
        };

        let mut env = Env::new(None);
        let mut resolver = Resolver::new();

        // Register service definition first
        let reg_res = resolver.resolve_stmt(&mut service_stmt, &mut env);
        assert!(reg_res.is_ok());

        // Resolve test block containing an invalid statement
        let test_res = resolver.resolve_stmt(&mut test_stmt, &mut env);
        assert!(test_res.is_err());
        assert_eq!(resolver.current_context, None);

        // Verify reusable resolver does not leak stale context_name
        let mut watch_stmt = Stmt::Watch {
            expr: Expr::Variable { name: y },
        };
        let second_res = resolver.resolve_stmt(&mut watch_stmt, &mut env);
        assert!(second_res.is_err());
        match second_res.unwrap_err() {
            Error::UnknownIdentifier { context_name, .. } => {
                assert_eq!(context_name, None);
            }
            _ => panic!("Expected UnknownIdentifier error"),
        }
    }

    #[test]
    fn test_map_fold_insert_resolve_against_table() {
        use crate::runtime::ast::{Field, TableType};

        let mut interner = Interner::new();
        let board = interner.insert("board");
        let tasks = interner.insert("tasks");
        let id = interner.insert("id");
        let done = interner.insert("done");
        let m = interner.insert("m");
        let open = interner.insert("open");
        let total = interner.insert("total");

        let table = Decl::TableDecl {
            name: tasks,
            fields: vec![
                Field {
                    name: id,
                    ty: TableType::Int,
                },
                Field {
                    name: done,
                    ty: TableType::Bool,
                },
            ],
        };
        let map_def = Decl::DefDecl {
            name: open,
            ty: None,
            val: Expr::Map {
                var: m,
                table_name: tasks,
                where_clause: Box::new(Expr::Unop {
                    op: crate::runtime::ast::UnOp::Not,
                    expr: Box::new(Expr::MemberAccess {
                        service_name: m,
                        member_name: done,
                    }),
                }),
                body: Box::new(Expr::Tuple {
                    val: vec![Expr::KeyVal {
                        name: id,
                        value: Box::new(Expr::MemberAccess {
                            service_name: m,
                            member_name: id,
                        }),
                    }],
                }),
            },
            is_pub: true,
        };
        let fold_def = Decl::DefDecl {
            name: total,
            ty: None,
            val: Expr::Fold {
                table_name: tasks,
                column_name: id,
                operation: Box::new(Expr::Literal {
                    val: Value::Int { val: 0 },
                }),
                identity: Box::new(Expr::Literal {
                    val: Value::Int { val: 0 },
                }),
            },
            is_pub: true,
        };

        let mut stmt = Stmt::Service {
            name: board,
            decls: vec![table, map_def, fold_def],
        };
        assert!(resolve(std::slice::from_mut(&mut stmt)).is_ok());

        let insert = Stmt::Test {
            service_name: board,
            stmts: vec![ActionStmt::Insert {
                table_name: tasks,
                row: Expr::Tuple {
                    val: vec![
                        Expr::Literal {
                            val: Value::Int { val: 1 },
                        },
                        Expr::Literal {
                            val: Value::Bool { val: false },
                        },
                    ],
                },
            }],
        };
        let mut program = vec![stmt, insert];
        assert!(resolve(&mut program).is_ok());
    }

    #[test]
    fn test_map_unknown_column_fails() {
        use crate::runtime::ast::{Field, TableType};

        let mut interner = Interner::new();
        let board = interner.insert("board");
        let tasks = interner.insert("tasks");
        let id = interner.insert("id");
        let m = interner.insert("m");
        let missing = interner.insert("missing");
        let open = interner.insert("open");

        let table = Decl::TableDecl {
            name: tasks,
            fields: vec![Field {
                name: id,
                ty: TableType::Int,
            }],
        };
        let map_def = Decl::DefDecl {
            name: open,
            ty: None,
            val: Expr::Map {
                var: m,
                table_name: tasks,
                where_clause: Box::new(Expr::Literal {
                    val: Value::Bool { val: true },
                }),
                body: Box::new(Expr::MemberAccess {
                    service_name: m,
                    member_name: missing,
                }),
            },
            is_pub: true,
        };
        let stmt = Stmt::Service {
            name: board,
            decls: vec![table, map_def],
        };
        let mut program = vec![stmt];
        let err = resolve(&mut program).unwrap_err();
        match err {
            Error::UnknownIdentifier { name, .. } => assert_eq!(name, missing),
            other => panic!("expected UnknownIdentifier, got {:?}", other),
        }
    }

    fn table(name: Symbol, columns: &[(&str, crate::runtime::ast::TableType)], interner: &mut Interner) -> Decl {
        Decl::TableDecl {
            name,
            fields: columns
                .iter()
                .map(|(col, ty)| crate::runtime::ast::Field {
                    name: interner.insert(col),
                    ty: ty.clone(),
                })
                .collect(),
        }
    }

    fn int_lit(val: i32) -> Expr {
        Expr::Literal {
            val: Value::Int { val },
        }
    }

    /// A local record hides a service of the same name, so `board.n` is a field.
    #[test]
    fn test_local_binding_shadows_service() {
        let mut interner = Interner::new();
        let board = interner.insert("board");
        let user = interner.insert("user");
        let n = interner.insert("n");
        let id = interner.insert("id");
        let program = vec![
            Stmt::Service {
                name: board,
                decls: vec![Decl::DefDecl {
                    name: n,
                    ty: None,
                    is_pub: true,
                    val: int_lit(1),
                }],
            },
            Stmt::Service {
                name: user,
                decls: vec![
                    Decl::DefDecl {
                        name: interner.insert("y"),
                        ty: None,
                        is_pub: true,
                        val: Expr::MemberAccess {
                            service_name: board,
                            member_name: n,
                        },
                    },
                    Decl::DefDecl {
                        name: board,
                        ty: None,
                        is_pub: true,
                        val: Expr::Tuple {
                            val: vec![Expr::KeyVal {
                                name: id,
                                value: Box::new(int_lit(1)),
                            }],
                        },
                    },
                    Decl::DefDecl {
                        name: interner.insert("x"),
                        ty: None,
                        is_pub: true,
                        val: Expr::MemberAccess {
                            service_name: board,
                            member_name: n,
                        },
                    },
                ],
            },
        ];
        let mut program = program;
        resolve(&mut program).unwrap();
        let Stmt::Service { decls, .. } = &program[1] else {
            panic!("user service");
        };
        assert!(matches!(
            decls[0],
            Decl::DefDecl {
                val: Expr::ServiceAccess { .. },
                ..
            }
        ));
        assert!(matches!(
            decls[2],
            Decl::DefDecl {
                val: Expr::FieldAccess { .. },
                ..
            }
        ));
        let mut types = Env::new(None);
        assert!(matches!(
            crate::runtime::tt::check(&program, &mut types),
            Err(crate::runtime::tt::Error::UnboundVariable(_))
        ));
    }

    /// An outer map row stays in scope, and a `let` can hide the row.
    #[test]
    fn test_row_scope_follows_the_environment() {
        use crate::runtime::ast::{BinOp, TableType};

        let mut interner = Interner::new();
        let s = interner.insert("s");
        let t1 = interner.insert("t1");
        let t2 = interner.insert("t2");
        let a = interner.insert("a");
        let b = interner.insert("b");
        let x = interner.insert("x");
        let bogus = interner.insert("bogus");
        let m = interner.insert("m");
        let tasks = interner.insert("tasks");
        let other = interner.insert("other");

        let nested = Stmt::Service {
            name: s,
            decls: vec![
                table(t1, &[("x", TableType::Int)], &mut interner),
                table(t2, &[("x", TableType::Int)], &mut interner),
                Decl::DefDecl {
                    name: interner.insert("bad"),
                    ty: None,
                    is_pub: false,
                    val: Expr::Map {
                        var: a,
                        table_name: t1,
                        where_clause: Box::new(Expr::Literal {
                            val: Value::Bool { val: true },
                        }),
                        body: Box::new(Expr::Map {
                            var: b,
                            table_name: t2,
                            where_clause: Box::new(Expr::Binop {
                                op: BinOp::Eq,
                                expr1: Box::new(Expr::MemberAccess {
                                    service_name: a,
                                    member_name: bogus,
                                }),
                                expr2: Box::new(Expr::MemberAccess {
                                    service_name: b,
                                    member_name: x,
                                }),
                            }),
                            body: Box::new(Expr::MemberAccess {
                                service_name: b,
                                member_name: x,
                            }),
                        }),
                    },
                },
            ],
        };
        let mut nested = vec![nested];
        let err = resolve(&mut nested).unwrap_err();
        assert!(matches!(
            err,
            Error::UnknownIdentifier { name, .. } if name == bogus
        ));

        let shadowed = Stmt::Service {
            name: s,
            decls: vec![
                table(tasks, &[("x", TableType::Int)], &mut interner),
                Decl::DefDecl {
                    name: interner.insert("ok"),
                    ty: None,
                    is_pub: false,
                    val: Expr::Map {
                        var: m,
                        table_name: tasks,
                        where_clause: Box::new(Expr::Literal {
                            val: Value::Bool { val: true },
                        }),
                        body: Box::new(Expr::Action(vec![
                            ActionStmt::Let {
                                name: m,
                                ty: None,
                                expr: Expr::Tuple {
                                    val: vec![Expr::KeyVal {
                                        name: other,
                                        value: Box::new(int_lit(1)),
                                    }],
                                },
                            },
                            ActionStmt::Expr(Expr::MemberAccess {
                                service_name: m,
                                member_name: other,
                            }),
                        ])),
                    },
                },
            ],
        };
        let mut shadowed = vec![shadowed];
        resolve(&mut shadowed).unwrap();
        let Stmt::Service { decls, .. } = &shadowed[0] else {
            panic!("service");
        };
        let Decl::DefDecl {
            val: Expr::Map { body, .. },
            ..
        } = &decls[1]
        else {
            panic!("map");
        };
        let Expr::Action(stmts) = body.as_ref() else {
            panic!("action");
        };
        assert!(matches!(
            stmts[1],
            ActionStmt::Expr(Expr::FieldAccess { .. })
        ));
    }

    /// A table is visible from its declaration on, except inside a function.
    #[test]
    fn test_table_binding_is_lexical() {
        use crate::runtime::ast::TableType;

        let mut interner = Interner::new();
        let s = interner.insert("s");
        let tasks = interner.insert("tasks");
        let m = interner.insert("m");

        let map_tasks = |interner: &mut Interner| Expr::Map {
            var: m,
            table_name: tasks,
            where_clause: Box::new(Expr::Literal {
                val: Value::Bool { val: true },
            }),
            body: Box::new(Expr::MemberAccess {
                service_name: m,
                member_name: interner.insert("id"),
            }),
        };

        let early = Stmt::Service {
            name: s,
            decls: vec![
                Decl::DefDecl {
                    name: interner.insert("open"),
                    ty: None,
                    is_pub: false,
                    val: map_tasks(&mut interner),
                },
                table(tasks, &[("id", TableType::Int)], &mut interner),
            ],
        };
        let mut early = vec![early];
        let err = resolve(&mut early).unwrap_err();
        assert!(matches!(
            err,
            Error::UnknownIdentifier {
                name,
                expected: ExpectedSort::Table,
                ..
            } if name == tasks
        ));

        let deferred = Stmt::Service {
            name: s,
            decls: vec![
                Decl::DefDecl {
                    name: interner.insert("f"),
                    ty: None,
                    is_pub: false,
                    val: Expr::Func {
                        params: vec![],
                        body: Box::new(map_tasks(&mut interner)),
                        return_ty: None,
                    },
                },
                table(tasks, &[("id", TableType::Int)], &mut interner),
            ],
        };
        let mut deferred = vec![deferred];
        assert!(resolve(&mut deferred).is_ok());

        let hidden = Stmt::Service {
            name: s,
            decls: vec![
                table(tasks, &[("id", TableType::Int)], &mut interner),
                Decl::DefDecl {
                    name: interner.insert("act"),
                    ty: None,
                    is_pub: false,
                    val: Expr::Action(vec![
                        ActionStmt::Let {
                            name: tasks,
                            ty: None,
                            expr: int_lit(1),
                        },
                        ActionStmt::Expr(map_tasks(&mut interner)),
                    ]),
                },
            ],
        };
        let mut hidden = vec![hidden];
        let err = resolve(&mut hidden).unwrap_err();
        assert!(matches!(
            err,
            Error::UnknownIdentifier {
                name,
                expected: ExpectedSort::Table,
                ..
            } if name == tasks
        ));
    }

    /// A field receiver is a dependency of the definition that uses it.
    #[test]
    fn test_field_receiver_is_a_dependency() {
        use crate::runtime::ast::{BinOp, TableType};

        let mut interner = Interner::new();
        let board = interner.insert("board");
        let tasks = interner.insert("tasks");
        let minutes = interner.insert("minutes");
        let config = interner.insert("config");
        let limit = interner.insert("limit");
        let m = interner.insert("m");
        let xs = interner.insert("xs");
        let program = vec![Stmt::Service {
            name: board,
            decls: vec![
                table(tasks, &[("minutes", TableType::Int)], &mut interner),
                Decl::VarDecl {
                    name: config,
                    ty: None,
                    val: Expr::Tuple {
                        val: vec![Expr::KeyVal {
                            name: limit,
                            value: Box::new(int_lit(10)),
                        }],
                    },
                },
                Decl::DefDecl {
                    name: xs,
                    ty: None,
                    is_pub: true,
                    val: Expr::Map {
                        var: m,
                        table_name: tasks,
                        where_clause: Box::new(Expr::Binop {
                            op: BinOp::Gt,
                            expr1: Box::new(Expr::MemberAccess {
                                service_name: m,
                                member_name: minutes,
                            }),
                            expr2: Box::new(Expr::MemberAccess {
                                service_name: config,
                                member_name: limit,
                            }),
                        }),
                        body: Box::new(Expr::MemberAccess {
                            service_name: m,
                            member_name: minutes,
                        }),
                    },
                },
            ],
        }];
        let mut program = program;
        resolve(&mut program).unwrap();
        let graphs = crate::runtime::graphs::analysis::compute_dependencies(&program, None);
        assert!(graphs[0].reactive_graph.contains_edge(config, xs));
        let Stmt::Service { decls, .. } = &program[0] else {
            panic!("service");
        };
        let Decl::DefDecl { val, .. } = &decls[2] else {
            panic!("xs");
        };
        let Expr::Map { where_clause, .. } = val else {
            panic!("map");
        };
        let Expr::Binop { expr2, .. } = where_clause.as_ref() else {
            panic!("comparison");
        };
        assert!(matches!(expr2.as_ref(), Expr::FieldAccess { .. }));
        assert!(graphs[0].cross_deps.get(&xs).is_none());
    }
}
