//! Augmented bi-directional type checking.
//!
//! This module implements type validation and type inference by
//! propagating expected types downward through expressions, or
//! synthesizing types upward from sub-expressions.
//!
//! # High-Level Design
//!
//! The algorithm splits type validation into two mutually recursive
//! procedures:
//!
//! - Checking mode (`check_expr`): Validates an expression against
//!   a known expected type. Used for constructors (introduction
//!   forms) like lambdas, lists, and tuples where pushing the
//!   expected type down prevents ambiguity
//! - Synthesis mode (`infer`): Computes the type of an expression
//!   independently. Used for eliminators (destructors) like
//!   function calls, operators, and literals
//!
//! # The Bridge Rule
//!
//! When checking an expression that naturally synthesizes its type
//! (e.g., a function call or literal), the checker delegates to
//! `infer` and asserts structural equality between the synthesized
//! and expected types:
//!
//! `check(e, expected) = (infer(e) == expected)`
//!
//! # Augmented BDTC Relaxations
//!
//! Standard bidirectional typing is strictly polarized. We relax
//! this rigidity in two ways:
//!
//! - Synthesizing Constructors: We allow inferring types of lists,
//!   tuples, and annotated lambdas directly
//! - Flexible Bindings: Type annotations on `let`, `var`, and `def`
//!   can be omitted, falling back to `infer` on the right-hand side.
//!   However, context-free unannotated closures (e.g. `fn x => x`)
//!   will fail to infer; they require an annotation on either the
//!   parameter or the binding to push expected types down
//!
//! # Security & Resource Bounds
//!
//! - Recursion depth limit: Enforces `limits::MAX_SCOPE_DEPTH`
//! - Type structure depth limit: Enforces `limits::MAX_TYPE_DEPTH`

use crate::runtime::ast::{ActionStmt, BinOp, Decl, Expr, Stmt, UnOp, Value};
use crate::runtime::interner::Symbol;
use crate::runtime::tt::types::{Param, ServiceType, TupleType, Type};
use crate::runtime::Env;

/// Validate the structural depth of a type representation
///
/// Args:
///     ty (&Type): The type to evaluate
///     depth (usize): The current depth in the type tree
///
/// Returns:
///     Result<(), Error>: Ok if depth limits are not exceeded
///
/// Raises:
///     Error::DepthLimitExceeded: If type depth limit is exceeded
fn check_type(ty: &Type, depth: usize) -> Result<(), Error> {
    if depth > crate::runtime::limits::MAX_TYPE_DEPTH {
        return Err(Error::DepthLimitExceeded);
    }
    match ty {
        Type::Int | Type::String | Type::Bool | Type::Unit | Type::UnresolvedService(_) => Ok(()),
        Type::Tuple(ts) => {
            for t in ts.iter() {
                check_type(t, depth + 1)?;
            }
            Ok(())
        }
        Type::Func(t1, t2) => {
            check_type(t1, depth + 1)?;
            check_type(t2, depth + 1)?;
            Ok(())
        }
        Type::List(inner) => check_type(inner, depth + 1),
    }
}

/// Type checking errors in Meerkat
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    UnboundVariable(Symbol),
    TypeMismatch { expected: Type, found: Type },
    CannotInferType,
    DepthLimitExceeded,
    InvalidTupleArity,
    NotAFunction,
    DependencyCycle { service: Symbol, member: Symbol },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::UnboundVariable(s) => {
                write!(f, "unbound variable: {:?}", s)
            }
            Error::TypeMismatch { expected, found } => {
                write!(f, "type mismatch: expected {}, found {}.", expected, found)
            }
            Error::CannotInferType => {
                write!(f, "cannot infer type.")
            }
            Error::DepthLimitExceeded => {
                write!(f, "depth limit exceeded.")
            }
            Error::InvalidTupleArity => {
                write!(f, "invalid tuple arity.")
            }
            Error::NotAFunction => {
                write!(f, "not a function.")
            }
            Error::DependencyCycle { service, member } => {
                write!(
                    f,
                    "dependency cycle detected at service '{:?}', member '{:?}'.",
                    service, member
                )
            }
        }
    }
}

/// Type checking context containing environment and limits tracking
pub struct Context<'a, 'b> {
    depth: usize,
    program: &'a [Stmt],
    /// Type environments for services that are declared locally in this program
    local_services: &'b mut Env<'static, ServiceType>,
    checking_stack: Vec<(Symbol, Symbol)>,
    current_service: Option<Symbol>,
}

impl<'a, 'b> Context<'a, 'b> {
    /// Create a new type checking context
    ///
    /// Args:
    ///     `program` (`&'a [Stmt]`): Slices of parsed statements
    ///     `local_services` (`&'b mut Env<'static, ServiceType>`): Mutable
    ///         environment mapping locally-declared service names to their
    ///         accumulated `ServiceType` records
    ///
    /// Returns:
    ///     `Self`: The `Context` instance
    fn new(program: &'a [Stmt], local_services: &'b mut Env<'static, ServiceType>) -> Self {
        Self {
            depth: 0,
            program,
            local_services,
            checking_stack: Vec::new(),
            current_service: None,
        }
    }

    /// Increment depth counter and check against the recursion limit
    ///
    /// Returns:
    ///     Result<(), Error>: Ok if within bounds
    ///
    /// Raises:
    ///     Error::DepthLimitExceeded: If scope depth limit is exceeded
    fn inc_depth(&mut self) -> Result<(), Error> {
        self.depth += 1;
        if self.depth > crate::runtime::limits::MAX_SCOPE_DEPTH {
            Err(Error::DepthLimitExceeded)
        } else {
            Ok(())
        }
    }

    /// Decrement depth counter
    fn dec_depth(&mut self) {
        if self.depth > 0 {
            self.depth -= 1;
        }
    }

    /// Execute type checking on all program statements
    ///
    /// Returns:
    ///     `Result<(), Error>`: Ok on success
    pub fn check_all(&mut self) -> Result<(), Error> {
        // First pass: register all service names (local and imported) so that
        // forward-reference lookups within `type_of_member` can find them.
        // Both local and imported Stmt::Service nodes are present in the
        // unified AST passed from Node::static_checks.
        for stmt in self.program {
            if let Stmt::Service { name, .. } = stmt {
                if self.local_services.find(*name).is_none() {
                    self.local_services.bind(*name, ServiceType::default());
                }
            }
        }

        for stmt in self.program {
            if let Stmt::Service { name, .. } = stmt {
                self.check_service(*name)?;
            }
        }

        let has_updates = self
            .program
            .iter()
            .any(|stmt| matches!(stmt, Stmt::Update { .. } | Stmt::Atomic { .. }));

        for stmt in self.program {
            match stmt {
                Stmt::Test {
                    service_name,
                    stmts,
                } => {
                    if !has_updates {
                        self.check_test(*service_name, stmts)?;
                    }
                }
                Stmt::ActionStmt(action) => {
                    let mut local_env = Env::new(None);
                    self.check_action_stmt(action, &mut local_env)?;
                }
                Stmt::Watch { expr } => {
                    let mut local_env = Env::new(None);
                    self.infer(expr, &mut local_env, 1)?;
                }
                Stmt::Connect { .. }
                | Stmt::Service { .. }
                | Stmt::Import { .. }
                | Stmt::Update { .. }
                | Stmt::Atomic { .. } => {}
            }
        }

        Ok(())
    }

    /// Find the declarations of a service in the program
    ///
    /// Args:
    ///     service_name (Symbol): The service name symbol
    ///
    /// Returns:
    ///     Result<&'a [Decl], Error>: Slice of declarations
    ///
    /// Raises:
    ///     Error::UnboundVariable: If service is not found
    fn find_service_decls(&self, service_name: Symbol) -> Result<&'a [Decl], Error> {
        // TODO(Issue #156): Support looking up declarations
        // from Stmt::Update (deferred per Issue #34)
        for stmt in self.program {
            if let Stmt::Service { name, decls } = stmt {
                if *name == service_name {
                    return Ok(decls);
                }
            }
        }
        Err(Error::UnboundVariable(service_name))
    }

    /// Retrieve or compute the type of a service member on-demand
    ///
    /// Args:
    ///     service_name (Symbol): The service name
    ///     member_name (Symbol): The member field name
    ///
    /// Returns:
    ///     Result<Type, Error>: The type of the member
    ///
    /// Raises:
    ///     Error::DependencyCycle: On cyclic member dependencies
    ///     Error::UnboundVariable: If member is not found
    fn type_of_member(&mut self, service_name: Symbol, member_name: Symbol) -> Result<Type, Error> {
        // Fast path: member type already cached from a prior check
        if let Some(st) = self.local_services.find(service_name) {
            if let Some(ty) = st.fields().find(member_name) {
                return Ok(ty.clone());
            }
        }

        // No cached entry: walk the program to find and type the member.
        // Both local and imported service declarations are present in
        // `self.program` (unified_ast), so we use the normal path for all.

        // The checking_stack detects mutual type recursion (e.g. `def a
        // = b; def b = a;`). Execution-order dependencies are analyzed
        // in a separate compiler pass. This check strictly exists to
        // ensure type inference terminates and avoids stack overflows.
        let key = (service_name, member_name);
        if self.checking_stack.contains(&key) {
            return Err(Error::DependencyCycle {
                service: service_name,
                member: member_name,
            });
        }
        self.checking_stack.push(key);

        let decls = self.find_service_decls(service_name)?;
        let decl = decls
            .iter()
            .find(|d| match d {
                Decl::VarDecl { name, .. } | Decl::DefDecl { name, .. } => *name == member_name,
                Decl::TableDecl { name, .. } => *name == member_name,
            })
            .ok_or(Error::UnboundVariable(member_name))?;

        let ty = match decl {
            Decl::VarDecl {
                ty: annotated, val, ..
            }
            | Decl::DefDecl {
                ty: annotated, val, ..
            } => {
                let mut env = Env::new(None);

                let prev_service = self.current_service;
                self.current_service = Some(service_name);
                let res = if let Some(expected) = annotated {
                    if let Err(e) = check_type(expected, 1) {
                        self.current_service = prev_service;
                        self.checking_stack.pop();
                        return Err(e);
                    }
                    if let Some(mut st) = self.local_services.remove(service_name) {
                        let _ = st.add_field(member_name, expected.clone());
                        self.local_services.bind(service_name, st);
                    }
                    if let Err(e) = self.check_expr(val, expected, &mut env) {
                        if let Some(mut st) = self.local_services.remove(service_name) {
                            st.remove_field(member_name);
                            self.local_services.bind(service_name, st);
                        }
                        self.current_service = prev_service;
                        self.checking_stack.pop();
                        return Err(e);
                    }
                    expected.clone()
                } else {
                    match self.infer(val, &mut env, 1) {
                        Ok(t) => t,
                        Err(e) => {
                            self.current_service = prev_service;
                            self.checking_stack.pop();
                            return Err(e);
                        }
                    }
                };
                self.current_service = prev_service;
                res
            }
            Decl::TableDecl { .. } => Type::Unit,
        };

        self.checking_stack.pop();

        if let Some(mut st) = self.local_services.remove(service_name) {
            let _ = st.add_field(member_name, ty.clone());
            self.local_services.bind(service_name, st);
        }

        Ok(ty)
    }

    /// Type check all declarations within a service
    ///
    /// Args:
    ///     service_name (Symbol): The service name
    ///
    /// Returns:
    ///     Result<(), Error>: Ok on success
    fn check_service(&mut self, service_name: Symbol) -> Result<(), Error> {
        let decls = self.find_service_decls(service_name)?;
        for decl in decls {
            match decl {
                Decl::VarDecl { name, .. } | Decl::DefDecl { name, .. } => {
                    self.type_of_member(service_name, *name)?;
                }
                Decl::TableDecl { .. } => {
                    // TODO(Issue #156): Implement Table type schema
                    // validation (deferred per Issue #34)
                    println!(
                        "warning: tt/check: ignoring 'table' \
                         schema checks as not yet implemented"
                    );
                }
            }
        }
        Ok(())
    }

    /// Type check a test block associated with a service
    ///
    /// Args:
    ///     service_name (Symbol): The service name
    ///     stmts (&[ActionStmt]): The action statements inside the test
    ///
    /// Returns:
    ///     Result<(), Error>: Ok on success
    fn check_test(&mut self, service_name: Symbol, stmts: &[ActionStmt]) -> Result<(), Error> {
        let mut test_env = Env::new(None);
        if let Some(st) = self.local_services.find(service_name) {
            for name in st.field_order() {
                if let Some(ty) = st.fields().find(*name) {
                    test_env.bind(*name, ty.clone());
                }
            }
        }

        let prev_service = self.current_service;
        self.current_service = Some(service_name);
        for stmt in stmts {
            self.check_action_stmt(stmt, &mut test_env)?;
        }
        self.current_service = prev_service;
        Ok(())
    }

    /// Type check an action statement
    ///
    /// Args:
    ///     stmt (&ActionStmt): The action statement to check
    ///     env (&mut Env<'_, Type>): The local types environment
    ///
    /// Returns:
    ///     Result<(), Error>: Ok on success
    fn check_action_stmt(
        &mut self,
        stmt: &ActionStmt,
        env: &mut Env<'_, Type>,
    ) -> Result<(), Error> {
        self.inc_depth()?;
        let res = match stmt {
            ActionStmt::Let { name, ty, expr } => {
                if let Some(expected) = ty {
                    check_type(expected, 1)?;
                    self.check_expr(expr, expected, env)?;
                    env.bind(*name, expected.clone());
                } else {
                    let inferred = self.infer(expr, env, 1)?;
                    env.bind(*name, inferred);
                }
                Ok(())
            }
            ActionStmt::Expr(expr) => {
                self.infer(expr, env, 1)?;
                Ok(())
            }
            ActionStmt::Do(expr) => {
                self.infer(expr, env, 1)?;
                Ok(())
            }
            ActionStmt::Assert(expr, _) => {
                self.check_expr(expr, &Type::Bool, env)?;
                Ok(())
            }
            ActionStmt::Assign { name, expr } => {
                let ty = if let Some(local_ty) = env.find(*name) {
                    local_ty.clone()
                } else if let Some(svc) = self.current_service {
                    self.type_of_member(svc, *name)?
                } else {
                    return Err(Error::UnboundVariable(*name));
                };
                self.check_expr(expr, &ty, env)?;
                Ok(())
            }
            ActionStmt::Insert { row, .. } => {
                // TODO(Issue #156): Validate inserted row against
                // table schema (deferred per Issue #34)
                println!(
                    "warning: tt/check: ignoring 'insert' \
                     checks as not yet implemented"
                );
                self.infer(row, env, 1)?;
                Ok(())
            }
            ActionStmt::For {
                var,
                iterable,
                body,
            } => {
                let iter_ty = self.infer(iterable, env, 1)?;
                if let Type::List(elem_ty) = iter_ty {
                    let mut for_env = Env::new(Some(env));
                    for_env.bind(*var, (*elem_ty).clone());
                    for b_stmt in body {
                        self.check_action_stmt(b_stmt, &mut for_env)?;
                    }
                    Ok(())
                } else {
                    Err(Error::TypeMismatch {
                        expected: Type::List(Box::new(Type::Unit)),
                        found: iter_ty,
                    })
                }
            }
        };
        self.dec_depth();
        res
    }

    /// Type check an expression against an expected type
    ///
    /// Args:
    ///     expr (&Expr): The expression to check
    ///     expected (&Type): The expected type target
    ///     env (&mut Env<'_, Type>): The local environment
    ///
    /// Returns:
    ///     Result<(), Error>: Ok if expression checks successfully
    fn check_expr(
        &mut self,
        expr: &Expr,
        expected: &Type,
        env: &mut Env<'_, Type>,
    ) -> Result<(), Error> {
        self.inc_depth()?;
        let res = match (expr, expected) {
            (Expr::Tuple { val }, Type::Tuple(tuple_ty)) => {
                if val.len() != tuple_ty.len() {
                    return Err(Error::InvalidTupleArity);
                }
                for (i, elem) in val.iter().enumerate() {
                    self.check_expr(elem, &tuple_ty[i], env)?;
                }
                Ok(())
            }
            (Expr::Tuple { val }, Type::Unit) => {
                if val.is_empty() {
                    Ok(())
                } else {
                    let types = val.iter().map(|_| Type::Unit).collect();
                    let tuple_ty = TupleType::new(types).map_err(|_| Error::InvalidTupleArity)?;
                    Err(Error::TypeMismatch {
                        expected: Type::Unit,
                        found: Type::Tuple(tuple_ty),
                    })
                }
            }
            (Expr::List(elems), Type::List(inner)) => {
                for elem in elems {
                    self.check_expr(elem, inner, env)?;
                }
                Ok(())
            }
            (
                Expr::Func {
                    params,
                    body,
                    return_ty,
                },
                Type::Func(expected_param, expected_ret),
            ) => {
                if let Some(ret_ty) = return_ty {
                    if ret_ty != expected_ret.as_ref() {
                        return Err(Error::TypeMismatch {
                            expected: (**expected_ret).clone(),
                            found: ret_ty.clone(),
                        });
                    }
                }
                let mut local_env = Env::new(Some(env));
                if params.is_empty() {
                    if **expected_param != Type::Unit {
                        return Err(Error::TypeMismatch {
                            expected: (**expected_param).clone(),
                            found: Type::Unit,
                        });
                    }
                } else if params.len() == 1 {
                    if let Some(param_ty) = &params[0].ty {
                        if param_ty != expected_param.as_ref() {
                            return Err(Error::TypeMismatch {
                                expected: (**expected_param).clone(),
                                found: param_ty.clone(),
                            });
                        }
                    }
                    local_env.bind(params[0].name, (**expected_param).clone());
                } else {
                    if let Type::Tuple(ts) = expected_param.as_ref() {
                        if params.len() != ts.len() {
                            return Err(Error::InvalidTupleArity);
                        }
                        for (i, param) in params.iter().enumerate() {
                            if let Some(param_ty) = &param.ty {
                                if param_ty != &ts[i] {
                                    return Err(Error::TypeMismatch {
                                        expected: ts[i].clone(),
                                        found: param_ty.clone(),
                                    });
                                }
                            }
                            local_env.bind(param.name, ts[i].clone());
                        }
                    } else {
                        let types = params.iter().map(|_| Type::Unit).collect();
                        let tuple_ty =
                            TupleType::new(types).map_err(|_| Error::InvalidTupleArity)?;
                        return Err(Error::TypeMismatch {
                            expected: (**expected_param).clone(),
                            found: Type::Tuple(tuple_ty),
                        });
                    }
                }
                self.check_expr(body, expected_ret, &mut local_env)?;
                Ok(())
            }
            (Expr::Action(stmts), Type::Unit) => {
                let mut action_env = Env::new(Some(env));
                for stmt in stmts {
                    self.check_action_stmt(stmt, &mut action_env)?;
                }
                Ok(())
            }
            (Expr::If { cond, expr1, expr2 }, _) => {
                self.check_expr(cond, &Type::Bool, env)?;
                self.check_expr(expr1, expected, env)?;
                self.check_expr(expr2, expected, env)?;
                Ok(())
            }
            (e, t) => {
                let inferred = self.infer(e, env, 1)?;
                // Structural equality bridge: if either the inferred type or
                // the expected type is an UnresolvedService sentinel, we bypass
                // the check, treating it as a dynamic/any type
                if inferred == *t
                    || matches!(inferred, Type::UnresolvedService(_))
                    || matches!(t, Type::UnresolvedService(_))
                {
                    Ok(())
                } else {
                    Err(Error::TypeMismatch {
                        expected: t.clone(),
                        found: inferred,
                    })
                }
            }
        };
        self.dec_depth();
        res
    }

    /// Synthesize the type of an expression
    ///
    /// Args:
    ///     expr (&Expr): The expression to evaluate
    ///     env (&mut Env<'_, Type>): The local environment
    ///     type_depth (usize): The current type depth
    ///
    /// Returns:
    ///     Result<Type, Error>: The synthesized type on success
    fn infer(
        &mut self,
        expr: &Expr,
        env: &mut Env<'_, Type>,
        type_depth: usize,
    ) -> Result<Type, Error> {
        if type_depth > crate::runtime::limits::MAX_TYPE_DEPTH {
            return Err(Error::DepthLimitExceeded);
        }
        self.inc_depth()?;
        let res = match expr {
            Expr::Literal { val } => match val {
                Value::Closure {
                    params,
                    body,
                    return_ty,
                    ..
                } => self.infer_function_type(params, body, return_ty.as_ref(), env, type_depth),
                Value::ActionClosure { stmts, .. } => {
                    let mut action_env = Env::new(Some(env));
                    for stmt in stmts {
                        self.check_action_stmt(stmt, &mut action_env)?;
                    }
                    Ok(Type::Unit)
                }
                Value::Int { .. }
                | Value::Bool { .. }
                | Value::String { .. }
                | Value::Html(..)
                | Value::List { .. }
                | Value::Range { .. } => self.infer_value(val, type_depth),
            },
            Expr::Html(_) => Ok(Type::String),
            Expr::Variable { name } => {
                if let Some(ty) = env.find(*name) {
                    Ok(ty.clone())
                } else if let Some(svc) = self.current_service {
                    let ty = self.type_of_member(svc, *name)?;
                    Ok(ty)
                } else {
                    Err(Error::UnboundVariable(*name))
                }
            }
            Expr::Tuple { val } => {
                if val.is_empty() {
                    Ok(Type::Unit)
                } else if val.len() < 2 {
                    Err(Error::InvalidTupleArity)
                } else {
                    let mut types = Vec::new();
                    for elem in val {
                        types.push(self.infer(elem, env, type_depth + 1)?);
                    }
                    let tuple_ty = TupleType::new(types).map_err(|_| Error::InvalidTupleArity)?;
                    Ok(Type::Tuple(tuple_ty))
                }
            }
            Expr::KeyVal { value, .. } => self.infer(value, env, type_depth),
            Expr::Unop { op, expr } => match op {
                UnOp::Neg => {
                    self.check_expr(expr, &Type::Int, env)?;
                    Ok(Type::Int)
                }
                UnOp::Not => {
                    self.check_expr(expr, &Type::Bool, env)?;
                    Ok(Type::Bool)
                }
            },
            Expr::Binop { op, expr1, expr2 } => match op {
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => {
                    self.check_expr(expr1, &Type::Int, env)?;
                    self.check_expr(expr2, &Type::Int, env)?;
                    Ok(Type::Int)
                }
                BinOp::Lt | BinOp::Gt => {
                    self.check_expr(expr1, &Type::Int, env)?;
                    self.check_expr(expr2, &Type::Int, env)?;
                    Ok(Type::Bool)
                }
                BinOp::And | BinOp::Or => {
                    self.check_expr(expr1, &Type::Bool, env)?;
                    self.check_expr(expr2, &Type::Bool, env)?;
                    Ok(Type::Bool)
                }
                BinOp::Eq => {
                    let ty1 = self.infer(expr1, env, type_depth)?;
                    self.check_expr(expr2, &ty1, env)?;
                    Ok(Type::Bool)
                }
            },
            Expr::If { cond, expr1, expr2 } => {
                self.check_expr(cond, &Type::Bool, env)?;
                let ty1 = self.infer(expr1, env, type_depth)?;
                self.check_expr(expr2, &ty1, env)?;
                Ok(ty1)
            }
            Expr::Func {
                params,
                body,
                return_ty,
            } => self.infer_function_type(params, body, return_ty.as_ref(), env, type_depth),
            Expr::Call { func, args } => {
                let func_ty = self.infer(func, env, type_depth)?;
                if let Type::Func(param_ty, ret_ty) = func_ty {
                    if args.is_empty() {
                        if *param_ty != Type::Unit {
                            return Err(Error::TypeMismatch {
                                expected: *param_ty,
                                found: Type::Unit,
                            });
                        }
                    } else if args.len() == 1 {
                        self.check_expr(&args[0], &param_ty, env)?;
                    } else {
                        if let Type::Tuple(ts) = &*param_ty {
                            if args.len() != ts.len() {
                                return Err(Error::InvalidTupleArity);
                            }
                            for (i, arg) in args.iter().enumerate() {
                                self.check_expr(arg, &ts[i], env)?;
                            }
                        } else {
                            let types = args.iter().map(|_| Type::Unit).collect();
                            let tuple_ty =
                                TupleType::new(types).map_err(|_| Error::InvalidTupleArity)?;
                            return Err(Error::TypeMismatch {
                                expected: *param_ty,
                                found: Type::Tuple(tuple_ty),
                            });
                        }
                    }
                    Ok(*ret_ty)
                } else {
                    Err(Error::NotAFunction)
                }
            }
            Expr::Action(stmts) => {
                let mut action_env = Env::new(Some(env));
                for stmt in stmts {
                    self.check_action_stmt(stmt, &mut action_env)?;
                }
                Ok(Type::Unit)
            }
            Expr::MemberAccess {
                service_name,
                member_name,
            } => self.type_of_member(*service_name, *member_name),
            Expr::Select { .. } => Ok(Type::List(Box::new(Type::Unit))),
            Expr::Table { .. } => Ok(Type::Unit),
            Expr::Fold { .. } => Ok(Type::Unit),
            Expr::List(elems) => {
                if elems.is_empty() {
                    Err(Error::CannotInferType)
                } else {
                    let first_ty = self.infer(&elems[0], env, type_depth + 1)?;
                    for elem in elems {
                        self.check_expr(elem, &first_ty, env)?;
                    }
                    Ok(Type::List(Box::new(first_ty)))
                }
            }
            Expr::Range { start, end } => {
                self.check_expr(start, &Type::Int, env)?;
                self.check_expr(end, &Type::Int, env)?;
                Ok(Type::List(Box::new(Type::Int)))
            }
        };
        self.dec_depth();
        res
    }

    /// Infer the type of a function or closure
    ///
    /// Args:
    ///     `params` (`&[Param]`): The function `Param` list
    ///     `body` (`&Expr`): The function body `Expr`
    ///     `return_ty` (`Option<&Type>`): The optional return
    ///         `Type` annotation
    ///     `env` (`&mut Env<'_, Type>`): The local `Env` environment
    ///     `type_depth` (`usize`): The current type depth
    ///
    /// Returns:
    ///     `Result<Type, Error>`: The inferred function `Type`
    ///
    /// Raises:
    ///     `Error`: If type check fails or recursion limit exceeded
    fn infer_function_type(
        &mut self,
        params: &[Param],
        body: &Expr,
        return_ty: Option<&Type>,
        env: &mut Env<'_, Type>,
        type_depth: usize,
    ) -> Result<Type, Error> {
        let mut local_env = Env::new(Some(env));
        let mut param_types = Vec::new();
        for param in params {
            if let Some(p_ty) = &param.ty {
                local_env.bind(param.name, p_ty.clone());
                param_types.push(p_ty.clone());
            } else {
                return Err(Error::CannotInferType);
            }
        }
        let p_ty = if param_types.is_empty() {
            Type::Unit
        } else if param_types.len() == 1 {
            param_types[0].clone()
        } else {
            let tuple_ty = TupleType::new(param_types).map_err(|_| Error::InvalidTupleArity)?;
            Type::Tuple(tuple_ty)
        };
        check_type(&p_ty, type_depth + 1)?;
        let r_ty = if let Some(annotated_ret) = return_ty {
            check_type(annotated_ret, type_depth + 1)?;
            self.check_expr(body, annotated_ret, &mut local_env)?;
            annotated_ret.clone()
        } else {
            self.infer(body, &mut local_env, type_depth + 1)?
        };
        Ok(Type::Func(Box::new(p_ty), Box::new(r_ty)))
    }

    /// Helper to infer type from Value literals directly
    ///
    /// Args:
    ///     val (&Value): The value to infer
    ///     type_depth (usize): The current type depth
    ///
    /// Returns:
    ///     Result<Type, Error>: Inferred type
    fn infer_value(&mut self, val: &Value, type_depth: usize) -> Result<Type, Error> {
        if type_depth > crate::runtime::limits::MAX_TYPE_DEPTH {
            return Err(Error::DepthLimitExceeded);
        }
        match val {
            Value::Int { .. } => Ok(Type::Int),
            Value::Bool { .. } => Ok(Type::Bool),
            Value::String { .. } => Ok(Type::String),
            Value::Html(..) => Ok(Type::String),
            Value::List { vals } => {
                if vals.is_empty() {
                    Err(Error::CannotInferType)
                } else {
                    let inner = self.infer_value(&vals[0], type_depth + 1)?;
                    for v in vals {
                        let v_ty = self.infer_value(v, type_depth + 1)?;
                        if v_ty != inner {
                            return Err(Error::TypeMismatch {
                                expected: inner,
                                found: v_ty,
                            });
                        }
                    }
                    Ok(Type::List(Box::new(inner)))
                }
            }
            Value::Range { .. } => Ok(Type::List(Box::new(Type::Int))),
            Value::ActionClosure { .. } => Ok(Type::Unit),
            Value::Closure {
                params, return_ty, ..
            } => {
                if let Some(ret_ty) = return_ty {
                    let mut param_types = Vec::new();
                    for param in params {
                        if let Some(param_ty) = &param.ty {
                            param_types.push(param_ty.clone());
                        } else {
                            // If any parameter lacks type annotation,
                            // we cannot statically infer the closure's
                            // full type signature without an env.
                            return Err(Error::CannotInferType);
                        }
                    }
                    let expected_param = if param_types.is_empty() {
                        Type::Unit
                    } else if param_types.len() == 1 {
                        param_types[0].clone()
                    } else {
                        let tuple_ty =
                            TupleType::new(param_types).map_err(|_| Error::InvalidTupleArity)?;
                        Type::Tuple(tuple_ty)
                    };
                    check_type(&expected_param, type_depth + 1)?;
                    check_type(ret_ty, type_depth + 1)?;
                    Ok(Type::Func(
                        Box::new(expected_param),
                        Box::new(ret_ty.clone()),
                    ))
                } else {
                    Err(Error::CannotInferType)
                }
            }
        }
    }

    /// Validate that a service update block is sequentially well-typed
    ///
    /// Args:
    ///     `service_name` (`Symbol`): The service name symbol
    ///     `decls` (`&[Decl]`): Declarations introduced or modified in the update
    ///
    /// Returns:
    ///     `Result<(), Error>`: Ok on success
    ///
    /// Raises:
    ///     `Error::UnboundVariable`: If service or member identifier is not found
    fn validate_update_block(&mut self, service_name: Symbol, decls: &[Decl]) -> Result<(), Error> {
        let mut st = self
            .local_services
            .find(service_name)
            .ok_or(Error::UnboundVariable(service_name))?
            .clone();

        let mut update_env = Env::new(None);
        for name in st.field_order() {
            if let Some(ty) = st.fields().find(*name) {
                update_env.bind(*name, ty.clone());
            }
        }

        let prev_service = self.current_service;
        self.current_service = Some(service_name);

        for decl in decls {
            match decl {
                Decl::VarDecl {
                    name,
                    ty: annotated,
                    val,
                }
                | Decl::DefDecl {
                    name,
                    ty: annotated,
                    val,
                    ..
                } => {
                    let member_ty = if let Some(expected) = annotated {
                        if let Err(e) = check_type(expected, 1) {
                            self.current_service = prev_service;
                            return Err(e);
                        }
                        if let Err(e) = self.check_expr(val, expected, &mut update_env) {
                            self.current_service = prev_service;
                            return Err(e);
                        }
                        expected.clone()
                    } else {
                        match self.infer(val, &mut update_env, 1) {
                            Ok(ty) => ty,
                            Err(e) => {
                                self.current_service = prev_service;
                                return Err(e);
                            }
                        }
                    };
                    update_env.bind(*name, member_ty.clone());
                    if st.fields().find(*name).is_some() {
                        let _ = st.update_field(*name, member_ty);
                    } else {
                        let _ = st.add_field(*name, member_ty);
                    }
                }
                Decl::TableDecl { name, .. } => {
                    update_env.bind(*name, Type::Unit);
                    if st.fields().find(*name).is_none() {
                        let _ = st.add_field(*name, Type::Unit);
                    }
                }
            }
        }

        self.current_service = prev_service;
        self.local_services.bind(service_name, st);
        Ok(())
    }
}

/// Perform static type checking on a program statement list
///
/// Validates both static declarations and post-update environment states
///
/// Args:
///     `program` (`&[Stmt]`): Slices of parsed statements
///     `local_services` (`&mut Env<'static, ServiceType>`): Mutable
///         environment that accumulates type information for each
///         locally-declared service; imported services are skipped
///
/// Returns:
///     `Result<(), Error>`: Ok on success
///
/// Raises:
///     `Error`: Any type error encountered during resolution
pub fn check(
    program: &[Stmt],
    local_services: &mut Env<'static, ServiceType>,
) -> Result<(), Error> {
    let mut context = Context::new(program, local_services);
    context.check_all()?;

    let mut update_stmts = Vec::new();
    for stmt in program {
        match stmt {
            Stmt::Update {
                service_name,
                decls,
            } => {
                context.validate_update_block(*service_name, decls)?;
                update_stmts.push(stmt.clone());
            }
            Stmt::Atomic { updates } => {
                for u in updates {
                    if let Stmt::Update {
                        service_name,
                        decls,
                    } = u
                    {
                        context.validate_update_block(*service_name, decls)?;
                    }
                    update_stmts.push(u.clone());
                }
            }
            Stmt::Connect { .. }
            | Stmt::Service { .. }
            | Stmt::Import { .. }
            | Stmt::Test { .. }
            | Stmt::ActionStmt(_)
            | Stmt::Watch { .. } => {}
        }
    }

    if !update_stmts.is_empty() {
        let post_update_ast = crate::runtime::ast::apply_updates_to_ast(program, &update_stmts)
            .map_err(Error::UnboundVariable)?;

        let mut post_services = Env::new(None);
        let mut post_context = Context::new(&post_update_ast, &mut post_services);
        post_context.check_all()?;

        *local_services = post_services;
    }

    Ok(())
}

#[cfg(test)]
mod tests;
