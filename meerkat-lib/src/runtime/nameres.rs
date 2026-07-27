//! Static name resolution analysis for the Meerkat language
//!
//! This module resolves and validates variable usages in abstract
//! syntax trees, failing defensively if an unbound symbol is accessed.
//! When called with a unified AST (local + all imported service source),
//! cross-service member accesses and @test blocks are fully resolved.
//!
//! The currently skipped operations are listed below
//! - `TableDecl` declarations
//! - `Insert` statements
//! - `Select` expressions
//! - `Fold` expressions

use crate::runtime::ast::{ActionStmt, Decl, Expr, Stmt, Value};
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
pub enum Binding<'a> {
    /// A standard, immediately evaluated value
    Value,
    #[doc(hidden)]
    _Marker(std::marker::PhantomData<&'a ()>),
}

/// The stateful struct that drives static name resolution traversal
///
/// This resolver performs pure lexical, block-level name resolution.
/// It delegates execution-order dependency checking to a separate
/// static analysis pass, ensuring that lexical lookup rules are
/// enforced cleanly without rigid ordering constraints.
pub struct Resolver<'a> {
    local_services: HashMap<Symbol, Vec<&'a Decl>>,
    current_context: Option<Symbol>,
}

impl<'a> Default for Resolver<'a> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> Resolver<'a> {
    /// Creates a new resolver instance
    ///
    /// Returns:
    ///     Self: The new `Resolver` instance
    pub fn new() -> Self {
        let resolver = Self {
            local_services: HashMap::new(),
            current_context: None,
        };
        debug_assert!(resolver.current_context.is_none());
        resolver
    }

    /// Resolves name bindings for a program represented as a slice
    /// of `Stmt`s
    ///
    /// Args:
    ///     `stmts` (`&'a [Stmt]`): The statements of the program
    ///     `env` (`&mut Env<'a, Binding<'a>>`): The current environment
    ///
    /// Returns:
    ///     `Result<(), Error>`: Ok if resolution succeeds, or `Error`
    pub fn resolve_program(
        &mut self,
        stmts: &'a [Stmt],
        env: &mut Env<'a, Binding<'a>>,
    ) -> Result<(), Error> {
        // Pass 1: Bind top-level services and imports, and record
        // local service declarations
        for stmt in stmts {
            match stmt {
                Stmt::Service { name, decls } => {
                    env.bind(*name, Binding::Value);
                    self.local_services.insert(*name, decls.iter().collect());
                }
                Stmt::Import { service_name, .. } => {
                    env.bind(*service_name, Binding::Value);
                }
                Stmt::ActionStmt(_)
                | Stmt::Atomic { .. }
                | Stmt::Update { .. }
                | Stmt::Connect { .. }
                | Stmt::Test { .. }
                | Stmt::Watch { .. } => {}
            }
        }

        // Pass 2: Resolve all statements sequentially
        for stmt in stmts {
            self.resolve_stmt(stmt, env)?;
        }

        Ok(())
    }

    /// Resolves name bindings in a single statement
    ///
    /// Args:
    ///     `stmt` (`&'a Stmt`): The statement to resolve
    ///     `env` (`&mut Env<'b, Binding<'a>>`): The environment
    ///
    /// Returns:
    ///     `Result<(), Error>`: Ok if resolution succeeds, or `Error`
    fn resolve_stmt<'b>(
        &mut self,
        stmt: &'a Stmt,
        env: &mut Env<'b, Binding<'a>>,
    ) -> Result<(), Error> {
        match stmt {
            Stmt::ActionStmt(action) => self.resolve_action_stmt(action, env, 0),
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
                let prev_context = self.current_context;
                self.current_context = Some(*service_name);
                for decl in decls {
                    match decl {
                        Decl::VarDecl { val, .. } | Decl::DefDecl { val, .. } => {
                            if let Err(e) = self.resolve_expr(val, env, 0) {
                                self.current_context = prev_context;
                                return Err(e);
                            }
                        }
                        Decl::TableDecl { .. } => {}
                    }
                    if let Some(svc_decls) = self.local_services.get_mut(service_name) {
                        svc_decls.push(decl);
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
                env.bind(*service_name, Binding::Value);
                Ok(())
            }
            Stmt::Service { name, decls } => {
                env.bind(*name, Binding::Value);
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
                let decls = match self.local_services.get(service_name) {
                    Some(decls) => decls,
                    None => {
                        self.current_context = prev_context;
                        return Err(Error::UnknownIdentifier {
                            name: *service_name,
                            expected: ExpectedSort::Service,
                            context_name: self.current_context,
                        });
                    }
                };
                for decl in decls {
                    match decl {
                        Decl::VarDecl { name: mem, .. }
                        | Decl::DefDecl { name: mem, .. }
                        | Decl::TableDecl { name: mem, .. } => {
                            test_env.bind(*mem, Binding::Value);
                        }
                    }
                }
                self.resolve_action_stmts(stmts, &mut test_env, 0)?;
                self.current_context = prev_context;
                Ok(())
            }
            Stmt::Watch { expr } => self.resolve_expr(expr, env, 0),
        }
    }

    /// Resolves service-level declarations sequentially
    ///
    /// Note that `TableDecl` is registered but its schema is not verified
    ///
    /// Args:
    ///     `decls` (`&'a [Decl]`): The declarations in the service
    ///     `env` (`&mut Env<'b, Binding<'a>>`): The environment
    ///
    /// Returns:
    ///     `Result<(), Error>`: `Ok` if resolution succeeds, or `Error`
    fn resolve_service<'b>(
        &mut self,
        decls: &'a [Decl],
        env: &mut Env<'b, Binding<'a>>,
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
                Decl::TableDecl { name, fields: _ } => {
                    println!(
                        "warning: nameres: ignoring 'table' schema \
                         checks as not yet implemented"
                    );
                    env.bind(*name, Binding::Value);
                }
            }
        }
        Ok(())
    }

    /// Resolves a list of action statements sequentially
    ///
    /// Args:
    ///     `stmts` (`&'a [ActionStmt]`): The action statements to resolve
    ///     `env` (`&mut Env<'b, Binding<'a>>`): The current environment
    ///
    /// Returns:
    ///     `Result<(), Error>`: Ok if resolution succeeds, or `Error`
    fn resolve_action_stmts<'b>(
        &mut self,
        stmts: &'a [ActionStmt],
        env: &mut Env<'b, Binding<'a>>,
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
    fn resolve_action_stmt<'b>(
        &mut self,
        stmt: &'a ActionStmt,
        env: &mut Env<'b, Binding<'a>>,
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
                        .and_then(|ctx| self.local_services.get(&ctx))
                        .is_some_and(|decls| {
                            decls.iter().any(|decl| match decl {
                                Decl::VarDecl { name: mem, .. }
                                | Decl::DefDecl { name: mem, .. }
                                | Decl::TableDecl { name: mem, .. } => mem == name,
                            })
                        });

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
            ActionStmt::Insert {
                row: _,
                table_name: _,
            } => {
                println!(
                    "warning: nameres: ignoring 'insert' checks \
                     as not yet implemented"
                );
                Ok(())
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
    ///     `expr` (`&'a Expr`): The expression to resolve
    ///     `env` (`&Env<'b, Binding<'a>>`): The environment
    ///
    /// Returns:
    ///     `Result<(), Error>`: Ok if resolution succeeds, or `Error`
    fn resolve_expr<'b>(
        &mut self,
        expr: &'a Expr,
        env: &Env<'b, Binding<'a>>,
        depth: usize,
    ) -> Result<(), Error> {
        if depth >= MAX_SCOPE_DEPTH {
            return Err(Error::DepthLimit);
        }
        match expr {
            Expr::Literal { val } => self.resolve_value(val, env, depth + 1),
            Expr::Html(template) => {
                for e in template.embedded_exprs() {
                    self.resolve_expr(e, env, depth + 1)?;
                }
                Ok(())
            }
            Expr::Variable { name } => {
                if env.find(*name).is_none() {
                    let is_local_member = self
                        .current_context
                        .and_then(|ctx| self.local_services.get(&ctx))
                        .is_some_and(|decls| {
                            decls.iter().any(|decl| match decl {
                                Decl::VarDecl { name: mem, .. }
                                | Decl::DefDecl { name: mem, .. }
                                | Decl::TableDecl { name: mem, .. } => mem == name,
                            })
                        });

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
            Expr::KeyVal { name: _, value } => self.resolve_expr(value.as_ref(), env, depth + 1),
            Expr::Unop { op: _, expr } => self.resolve_expr(expr.as_ref(), env, depth + 1),
            Expr::Binop {
                op: _,
                expr1,
                expr2,
            } => {
                self.resolve_expr(expr1.as_ref(), env, depth + 1)?;
                self.resolve_expr(expr2.as_ref(), env, depth + 1)
            }
            Expr::If { cond, expr1, expr2 } => {
                self.resolve_expr(cond.as_ref(), env, depth + 1)?;
                self.resolve_expr(expr1.as_ref(), env, depth + 1)?;
                self.resolve_expr(expr2.as_ref(), env, depth + 1)
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
                self.resolve_expr(body.as_ref(), &inner_env, depth + 1)?;
                Ok(())
            }
            Expr::Call { func, args } => {
                self.resolve_expr(func.as_ref(), env, depth + 1)?;
                for arg in args {
                    self.resolve_expr(arg, env, depth + 1)?;
                }
                Ok(())
            }
            Expr::Action(stmts) => {
                let mut action_env = Env::new(Some(env));
                self.resolve_action_stmts(stmts, &mut action_env, depth + 1)?;
                Ok(())
            }
            Expr::MemberAccess {
                service_name,
                member_name,
            } => {
                if let Some(decls) = self.local_services.get(service_name) {
                    let has_member = decls.iter().any(|decl| match decl {
                        Decl::VarDecl { name: mem, .. }
                        | Decl::DefDecl { name: mem, .. }
                        | Decl::TableDecl { name: mem, .. } => mem == member_name,
                    });
                    if !has_member {
                        return Err(Error::UnknownIdentifier {
                            name: *member_name,
                            expected: ExpectedSort::Variable,
                            context_name: Some(*service_name),
                        });
                    }
                } else if env.find(*service_name).is_none() {
                    return Err(Error::UnknownIdentifier {
                        name: *service_name,
                        expected: ExpectedSort::Service,
                        context_name: self.current_context,
                    });
                }
                Ok(())
            }
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
                table_name: _,
                column_name: _,
                operation: _,
                identity: _,
            } => {
                println!(
                    "warning: nameres: ignoring 'fold' checks \
                     as not yet implemented"
                );
                Ok(())
            }
            Expr::List(exprs) => {
                for expr in exprs {
                    self.resolve_expr(expr, env, depth + 1)?;
                }
                Ok(())
            }
            Expr::Range { start, end } => {
                self.resolve_expr(start.as_ref(), env, depth + 1)?;
                self.resolve_expr(end.as_ref(), env, depth + 1)
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
    fn resolve_value<'b>(
        &mut self,
        val: &'a Value,
        env: &Env<'b, Binding<'a>>,
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
                self.resolve_expr(body.as_ref(), &inner_env, depth + 1)?;
                Ok(())
            }
            Value::ActionClosure {
                stmts,
                env: _,
                service_net_id: _,
            } => {
                let mut action_env = Env::new(Some(env));
                self.resolve_action_stmts(stmts, &mut action_env, depth + 1)?;
                Ok(())
            }
            Value::List { vals } => {
                for val in vals {
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
pub fn resolve(stmts: &[Stmt]) -> Result<(), Error> {
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
        let program = vec![stmt];
        let res = resolver.resolve_program(&program, &mut env);
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
        let program = vec![stmt];
        assert!(resolver.resolve_program(&program, &mut env).is_ok());

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
        let program_bad = vec![bad_stmt];
        let res = bad_resolver.resolve_program(&program_bad, &mut bad_env);
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
        let program = vec![stmt];
        let res = resolver.resolve_program(&program, &mut env);
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
        let program = vec![stmt];
        assert!(resolver.resolve_program(&program, &mut env).is_ok());

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
        let program_escaped = vec![escaped_stmt];
        let res = resolver_escaped.resolve_program(&program_escaped, &mut env_escaped);
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
        let update_stmt = Stmt::Update {
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
        let program = vec![s_stmt, update_stmt];
        let res = resolver.resolve_program(&program, &mut env);
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
        let update_stmt = Stmt::Update {
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
        let program = vec![s_stmt, update_stmt];
        let res = resolver.resolve_program(&program, &mut env);
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
        let test_stmt = Stmt::Test {
            service_name: s,
            stmts: vec![ActionStmt::Expr(Expr::Variable { name: x })],
        };

        let mut env = Env::new(None);
        let mut resolver = Resolver::new();
        let program = vec![s_stmt, test_stmt];

        assert!(resolver.resolve_program(&program, &mut env).is_ok());
    }

    /// Verify `@test` blocks can resolve services that
    /// are defined after them in the program statements
    #[test]
    fn test_unit_test_block_hoisting() {
        let mut interner = Interner::new();
        let s = interner.insert("s");
        let x = interner.insert("x");

        // Representing `@test(s) { x; }`
        let test_stmt = Stmt::Test {
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
        let program = vec![test_stmt, s_stmt];

        assert!(resolver.resolve_program(&program, &mut env).is_ok());
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
        let service_stmt = Stmt::Service {
            name: s,
            decls: vec![Decl::VarDecl {
                name: x,
                ty: None,
                val: Expr::Literal {
                    val: Value::Int { val: 0 },
                },
            }],
        };
        let test_stmt = Stmt::Test {
            service_name: s,
            stmts: vec![ActionStmt::Expr(Expr::Variable { name: x })],
        };

        let mut env = Env::new(None);
        let mut resolver = Resolver::new();
        let program = vec![service_stmt, test_stmt];

        let result = resolver.resolve_program(&program, &mut env);
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
        let test_stmt = Stmt::Test {
            service_name: s,
            stmts: vec![ActionStmt::Expr(Expr::Variable { name: x })],
        };

        let mut env = Env::new(None);
        let mut resolver = Resolver::new();
        let program = vec![test_stmt];

        let result = resolver.resolve_program(&program, &mut env);
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
        let res = resolver.resolve_expr(&deep_expr, &env, 0);
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
        let res = resolver.resolve_action_stmt(&deep_stmt, &mut env, 0);
        assert_eq!(res, Err(Error::DepthLimit))
    }
}
