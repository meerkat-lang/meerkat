use crate::net::ServiceNetId;
use crate::runtime::html::{Html, HtmlTemplate};
use crate::runtime::interner::Symbol;
use crate::runtime::tt::{Param, Type};
use std::fmt::Display;

pub mod printer;
pub use printer::AstPrinter;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnOp {
    Neg, // negate
    Not, // logical not
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,

    Eq,
    Lt,
    Gt,

    And,
    Or,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ActionStmt {
    Let {
        name: Symbol,
        ty: Option<Type>,
        expr: Expr,
    },
    Expr(Expr),
    Do(Expr),
    /// An `assert` statement to check invariants
    ///
    /// The `String` parameter captures the exact raw source
    /// string of the assertion condition for error reporting
    Assert(Expr, String),
    Assign {
        name: Symbol,
        expr: Expr,
    },
    Insert {
        row: Expr,
        table_name: Symbol,
    },
    For {
        var: Symbol,
        iterable: Expr,
        body: Vec<ActionStmt>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Stmt {
    ActionStmt(ActionStmt),
    Atomic {
        updates: Vec<Stmt>,
    },
    Update {
        service_name: Symbol,
        decls: Vec<Decl>,
    },
    Connect {
        path: String,
        addr: String,
    },
    Import {
        path: String,
        service_name: Symbol,
        explicit_path: bool,
    },
    Service {
        name: Symbol,
        decls: Vec<Decl>,
    },
    Test {
        service_name: Symbol,
        stmts: Vec<ActionStmt>,
    },
    Watch {
        expr: Expr,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Value {
    Int {
        val: i32,
    },
    Bool {
        val: bool,
    },
    String {
        val: String,
    },
    /// An evaluated HTML value. The representation is encapsulated by the
    /// `Html` ADT; see `runtime::html`.
    Html(Html),
    Closure {
        params: Vec<Param>,
        body: Box<Expr>,
        env: Vec<(Symbol, Value)>,
        service_name: Symbol,
        /// Important: `None` indicates missing type info, not the
        /// `unit` type. In practice, return types for closures should
        /// generally be `Some` type; however, this was left as an
        /// `Option` for now for maximum flexibility as development
        /// continues
        return_ty: Option<Type>,
    },
    ActionClosure {
        stmts: Vec<ActionStmt>,
        env: Vec<(Symbol, Value)>,
        /// Identity of the service this action belongs to
        ///
        /// Carries the owning node's address, so the action can be
        /// executed even if its service is not imported into the scope
        /// where the closure is later used
        service_net_id: ServiceNetId,
    },
    List {
        vals: Vec<Value>,
    },
    Range {
        start: i32,
        end: i32,
    },
}

impl Value {
    pub fn to_list_elements(&self) -> Option<Vec<Value>> {
        match self {
            Value::List { vals } => Some(vals.clone()),
            Value::Range { start, end } => {
                Some((*start..*end).map(|v| Value::Int { val: v }).collect())
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Expr {
    /// Basic Lambda Core expressions
    Literal {
        val: Value,
    },
    /// An HTML template literal. The representation is encapsulated by the
    /// `HtmlTemplate` ADT in `runtime::html`; the embedded expressions are
    /// reached through its interface so dependency analysis tracks them, and
    /// it renders to a `Value::Html` at evaluation time.
    Html(HtmlTemplate),
    Variable {
        name: Symbol,
    },
    Tuple {
        val: Vec<Expr>,
    },
    KeyVal {
        // TODO: replace with a `Record` type (different from `Tuple`) that is a list of key value pairs
        name: Symbol,
        value: Box<Expr>,
    },
    Unop {
        op: UnOp,
        expr: Box<Expr>,
    },
    Binop {
        op: BinOp,
        expr1: Box<Expr>,
        expr2: Box<Expr>,
    },

    If {
        cond: Box<Expr>,
        expr1: Box<Expr>,
        expr2: Box<Expr>,
    },

    Func {
        params: Vec<Param>,
        body: Box<Expr>,
        /// Important: `None` indicates missing type info, not the
        /// `unit` type. In practice, return types for closures should
        /// generally be `Some` type; however, this was left as an
        /// `Option` for now for maximum flexibility as development
        /// continues
        return_ty: Option<Type>,
    },
    Call {
        func: Box<Expr>,
        args: Vec<Expr>,
    },

    /// Action
    Action(Vec<ActionStmt>),

    MemberAccess {
        service_name: Symbol,
        member_name: Symbol,
    },
    Select {
        table_name: Symbol,
        column_names: Vec<Symbol>,
        where_clause: Box<Expr>,
    },

    Table {
        // TODO: remove this, we should just have `Record`s and `Tuple`s
        schema: Vec<Field>,
        records: Vec<Expr>,
        /* How do records differ from rows?
         *
         * - Records only consist of data contained within tables: `{1, "A", 18}`
         * - Rows are what are written inside insert statements: `insert {id: 1, name: "A", age: 18}`
         */
    },
    Fold {
        table_name: Symbol,
        column_name: Symbol,
        operation: Box<Expr>,
        identity: Box<Expr>,
    },
    List(Vec<Expr>),
    Range {
        start: Box<Expr>,
        end: Box<Expr>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Decl {
    VarDecl {
        name: Symbol,
        ty: Option<Type>,
        val: Expr,
    },
    DefDecl {
        name: Symbol,
        ty: Option<Type>,
        val: Expr,
        is_pub: bool,
    },
    TableDecl {
        name: Symbol,
        fields: Vec<Field>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Field {
    pub name: Symbol,
    pub ty: TableType,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TableType {
    String,
    Int,
    Bool,
}

impl Display for UnOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UnOp::Neg => write!(f, "-"),
            UnOp::Not => write!(f, "!"),
        }
    }
}

impl Display for BinOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BinOp::Add => write!(f, "+"),
            BinOp::Sub => write!(f, "-"),
            BinOp::Mul => write!(f, "*"),
            BinOp::Div => write!(f, "/"),
            BinOp::Eq => write!(f, "=="),
            BinOp::Lt => write!(f, "<"),
            BinOp::Gt => write!(f, ">"),
            BinOp::And => write!(f, "&&"),
            BinOp::Or => write!(f, "||"),
        }
    }
}

/// Implement the `Display` trait for the `Value` type
///
/// This prints a human-readable representation of runtime values
impl Display for Value {
    /// Format the value for display
    ///
    /// Args:
    ///     `f` (`&mut std::fmt::Formatter<'_>`): The formatter target
    ///
    /// Returns:
    ///     `std::fmt::Result`: The result of the formatting operation
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Int { val } => write!(f, "{}", val),
            Value::Bool { val } => write!(f, "{}", val),
            Value::String { val } => write!(f, "\"{}\"", val),
            Value::Html(html) => write!(f, "{}", html),
            Value::Closure {
                params,
                body,
                env,
                return_ty,
                ..
            } => {
                let params_str: Vec<String> = params.iter().map(|p| p.to_string()).collect();
                let env_str: Vec<String> =
                    env.iter().map(|(k, v)| format!("{:?}: {}", k, v)).collect();
                if let Some(ref ty) = return_ty {
                    write!(
                        f,
                        "fn({}) -> {}[{:?}]{{{}}}",
                        params_str.join(","),
                        ty,
                        env_str,
                        body
                    )
                } else {
                    write!(f, "fn({})[{:?}]{{{}}}", params_str.join(","), env_str, body)
                }
            }
            Value::ActionClosure {
                stmts,
                env,
                service_net_id,
            } => {
                let env_str: Vec<String> =
                    env.iter().map(|(k, v)| format!("{:?}: {}", k, v)).collect();
                write!(
                    f,
                    "action[{:?}][{}]{{{:?}}}",
                    env_str, service_net_id.0, stmts
                )
            }
            Value::List { vals } => {
                write!(f, "[")?;
                for (i, val) in vals.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", val)?;
                }
                write!(f, "]")
            }
            Value::Range { start, end } => write!(f, "{}..{}", start, end),
        }
    }
}

/// Implement the `Display` trait for the `Expr` type
///
/// This prints a human-readable representation of abstract syntax tree
/// expressions
impl Display for Expr {
    /// Format the expression for display
    ///
    /// Args:
    ///     `f` (`&mut std::fmt::Formatter<'_>`): The formatter target
    ///
    /// Returns:
    ///     `std::fmt::Result`: The result of the formatting operation
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Expr::Literal { val } => write!(f, "{}", val),
            Expr::Html(_) => write!(f, "<html>"),
            Expr::Tuple { .. } => write!(f, "vector"),
            Expr::KeyVal { name, value } => write!(f, "keyval: {:?}, {}", name, value),
            Expr::Variable { name } => write!(f, "{:?}", name),
            Expr::Unop { op, expr } => write!(f, "{}{}", op, expr),
            Expr::Binop { op, expr1, expr2 } => write!(f, "{} {} {}", expr1, op, expr2),
            Expr::If { cond, expr1, expr2 } => {
                write!(f, "if {} then {} else {}", cond, expr1, expr2)
            }
            Expr::Func {
                params,
                body,
                return_ty,
            } => {
                let params_str: Vec<String> = params.iter().map(|p| p.to_string()).collect();
                if let Some(ref ty) = return_ty {
                    write!(f, "fn({}) -> {}[{}]", params_str.join(","), ty, body)
                } else {
                    write!(f, "fn({})[{}]", params_str.join(","), body)
                }
            }
            Expr::Call { func, args } => write!(
                f,
                "{}({})",
                func,
                args.iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Expr::Action(stmts) => write!(
                f,
                "Action({:?})",
                stmts
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Expr::MemberAccess {
                service_name,
                member_name,
            } => write!(f, "{:?}.{:?}", service_name, member_name),
            Expr::Select { where_clause, .. } => write!(f, "{}", where_clause),
            Expr::Table { records, .. } => {
                write!(f, "[",)?;
                for (i, record) in records.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{{")?;
                    match record {
                        Expr::Tuple { val } => {
                            for (j, entry) in val.iter().enumerate() {
                                if j > 0 {
                                    write!(f, ", ")?;
                                }
                                write!(f, "{}", entry)?;
                            }
                        }
                        other => {
                            write!(f, "{}", other)?;
                        }
                    }
                    write!(f, "}}")?;
                }
                write!(f, "]")
            }
            Expr::Fold { .. } => write!(f, "fold"),
            Expr::List(exprs) => {
                write!(f, "[")?;
                for (i, expr) in exprs.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", expr)?;
                }
                write!(f, "]")
            }
            Expr::Range { start, end } => {
                write!(f, "{}..{}", start, end)
            }
        }
    }
}

/// Implement the `Display` trait for the `ActionStmt` type
///
/// This prints a human-readable representation of action statements
impl Display for ActionStmt {
    /// Format the action statement for display
    ///
    /// Args:
    ///     `f` (`&mut std::fmt::Formatter<'_>`): The formatter target
    ///
    /// Returns:
    ///     `std::fmt::Result`: The result of the formatting operation
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ActionStmt::Let { name, ty, expr } => {
                if let Some(t) = ty {
                    write!(f, "let {:?}: {} = {}", name, t, expr)
                } else {
                    write!(f, "let {:?} = {}", name, expr)
                }
            }
            ActionStmt::Expr(expr) => write!(f, "{}", expr),
            ActionStmt::Do(expr) => write!(f, "do {}", expr),
            ActionStmt::Assert(expr, _) => write!(f, "assert {}", expr),
            ActionStmt::Assign { name, expr } => write!(f, "{:?} = {}", name, expr),
            ActionStmt::Insert { row, table_name } => {
                write!(f, "insert into {:?} {}", table_name, row)
            }
            ActionStmt::For {
                var,
                iterable,
                body,
            } => {
                write!(f, "for {:?} in {} {{ ", var, iterable)?;
                for stmt in body {
                    write!(f, "{}; ", stmt)?;
                }
                write!(f, "}}")
            }
        }
    }
}

/// Implement the `Display` trait for the `Decl` type
///
/// This prints a human-readable representation of declarations
impl Display for Decl {
    /// Format the declaration for display
    ///
    /// Args:
    ///     `f` (`&mut std::fmt::Formatter<'_>`): The formatter target
    ///
    /// Returns:
    ///     `std::fmt::Result`: The result of the formatting operation
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Decl::VarDecl { name, ty, val } => {
                if let Some(t) = ty {
                    write!(f, "var {:?}: {} = {}", name, t, val)
                } else {
                    write!(f, "var {:?} = {}", name, val)
                }
            }
            Decl::DefDecl {
                name,
                ty,
                val,
                is_pub,
            } => {
                let prefix = if *is_pub { "pub " } else { "" };
                if let Some(t) = ty {
                    write!(f, "{}def {:?}: {} = {}", prefix, name, t, val)
                } else {
                    write!(f, "{}def {:?} = {}", prefix, name, val)
                }
            }
            Decl::TableDecl { name, .. } => {
                write!(f, "table {:?} created", name)
            }
        }
    }
}

/// Apply a sequence of service update statements to a base AST
///
/// Produces a new AST where target `Stmt::Service` declarations are updated
/// in-place with new or modified declarations, and `Stmt::Update` and
/// `Stmt::Atomic` nodes are removed from the top level
///
/// Args:
///     `base_ast` (`&[Stmt]`): The original AST statements
///     `updates` (`&[Stmt]`): The update statements to apply
///
/// Returns:
///     `Result<Vec<Stmt>, Symbol>`: The updated AST or the missing service Symbol
///
/// Raises:
///     `Symbol`: If a target service for an update is not found
pub fn apply_updates_to_ast(base_ast: &[Stmt], updates: &[Stmt]) -> Result<Vec<Stmt>, Symbol> {
    let mut patched_ast: Vec<Stmt> = base_ast
        .iter()
        .filter(|stmt| !matches!(stmt, Stmt::Update { .. } | Stmt::Atomic { .. }))
        .cloned()
        .collect();

    for stmt in updates {
        let (updated_svc_name, updated_decls) = match stmt {
            Stmt::Service { name, decls } => (name, decls),
            Stmt::Update {
                service_name: name,
                decls,
            } => (name, decls),
            Stmt::Atomic {
                updates: inner_updates,
            } => {
                patched_ast = apply_updates_to_ast(&patched_ast, inner_updates)?;
                continue;
            }
            Stmt::Connect { .. }
            | Stmt::Import { .. }
            | Stmt::Test { .. }
            | Stmt::ActionStmt(_)
            | Stmt::Watch { .. } => continue,
        };

        let mut found_service = false;
        for existing_stmt in &mut patched_ast {
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
                            | Decl::DefDecl { name: n, .. }
                            | Decl::TableDecl { name: n, .. } => *n,
                        };

                        let mut matched_existing = false;
                        for ex_decl in existing_decls.iter_mut() {
                            let ex_name = match ex_decl {
                                Decl::VarDecl { name: n, .. }
                                | Decl::DefDecl { name: n, .. }
                                | Decl::TableDecl { name: n, .. } => *n,
                            };
                            if ex_name == up_name {
                                *ex_decl = up_decl.clone();
                                matched_existing = true;
                                break;
                            }
                        }

                        if !matched_existing {
                            existing_decls.push(up_decl.clone());
                        }
                    }
                }
            }
        }

        if !found_service {
            return Err(*updated_svc_name);
        }
    }

    Ok(patched_ast)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::interner::Interner;

    /// Tests that `apply_updates_to_ast` returns an `Err` containing
    /// the target `Symbol` when an update targets a missing service
    #[test]
    fn test_apply_updates_to_ast_missing_service_returns_err() {
        let mut interner = Interner::new();
        let target_sym = interner.insert("non_existent_service");

        let base_ast: Vec<Stmt> = Vec::new();
        let updates: Vec<Stmt> = vec![Stmt::Update {
            service_name: target_sym,
            decls: Vec::new(),
        }];

        let result = apply_updates_to_ast(&base_ast, &updates);
        assert_eq!(result, Err(target_sym));
    }
}
