use crate::net::ServiceNetId;
use crate::runtime::html::{Html, HtmlTemplate};
use crate::runtime::interner::Symbol;
use crate::runtime::tt::{Param, Type};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
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
    /// Whether the value is a function: applying it reads what its body reads
    pub fn is_function(&self) -> bool {
        matches!(self, Value::Closure { .. })
    }

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

impl Expr {
    /// The immediate subexpressions, in source order. A pass that gives some
    /// forms their own meaning recurses over these for all the others
    pub fn children(&self) -> Vec<&Expr> {
        match self {
            Expr::Literal { .. } | Expr::Variable { .. } | Expr::MemberAccess { .. } => Vec::new(),
            Expr::KeyVal { value, .. } => vec![value],
            Expr::Unop { expr, .. } => vec![expr],
            Expr::Binop { expr1, expr2, .. }
            | Expr::Range {
                start: expr1,
                end: expr2,
            }
            | Expr::Fold {
                operation: expr1,
                identity: expr2,
                ..
            } => vec![expr1, expr2],
            Expr::If { cond, expr1, expr2 } => vec![cond, expr1, expr2],
            Expr::Func { body, .. } => vec![body],
            Expr::Call { func, args } => std::iter::once(func.as_ref()).chain(args).collect(),
            Expr::Tuple { val } | Expr::List(val) | Expr::Table { records: val, .. } => {
                val.iter().collect()
            }
            Expr::Html(template) => template.embedded_exprs().collect(),
            Expr::Action(stmts) => stmts.iter().flat_map(ActionStmt::exprs).collect(),
            Expr::Select { where_clause, .. } => vec![where_clause],
        }
    }
}

impl ActionStmt {
    /// The expressions the statement evaluates, in source order, including
    /// those of the statements in a loop body
    pub fn exprs(&self) -> Vec<&Expr> {
        match self {
            ActionStmt::Let { expr, .. }
            | ActionStmt::Expr(expr)
            | ActionStmt::Do(expr)
            | ActionStmt::Assert(expr, _)
            | ActionStmt::Assign { expr, .. }
            | ActionStmt::Insert { row: expr, .. } => vec![expr],
            ActionStmt::For { iterable, body, .. } => std::iter::once(iterable)
                .chain(body.iter().flat_map(ActionStmt::exprs))
                .collect(),
        }
    }
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

impl Decl {
    /// The member the declaration declares
    pub fn name(&self) -> Symbol {
        match self {
            Decl::VarDecl { name, .. }
            | Decl::DefDecl { name, .. }
            | Decl::TableDecl { name, .. } => *name,
        }
    }
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
/// Members keep their order unless one must follow a member it reads, which
/// lets an update add a member and change an existing def to read it: see
/// `order_members`
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
    // For each service changed, how many members it had before: members are
    // ordered once, after every update (an `atomic` block's included) is in
    let mut before: HashMap<usize, usize> = HashMap::new();
    merge_updates(&mut patched_ast, updates, &mut before)?;
    for (at, existing) in before {
        if let Stmt::Service { name, decls } = &mut patched_ast[at] {
            order_members(decls, *name, existing);
        }
    }
    Ok(patched_ast)
}

/// `apply_updates_to_ast` before ordering: merge each update's declarations
/// into its service, recording in `before` the service's member count before
/// its first change
fn merge_updates(
    patched_ast: &mut [Stmt],
    updates: &[Stmt],
    before: &mut HashMap<usize, usize>,
) -> Result<(), Symbol> {
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
                merge_updates(patched_ast, inner_updates, before)?;
                continue;
            }
            Stmt::Connect { .. }
            | Stmt::Import { .. }
            | Stmt::Test { .. }
            | Stmt::ActionStmt(_)
            | Stmt::Watch { .. } => continue,
        };

        let mut found_service = false;
        for (at, existing_stmt) in patched_ast.iter_mut().enumerate() {
            if let Stmt::Service {
                name: existing_svc_name,
                decls: existing_decls,
            } = existing_stmt
            {
                if *existing_svc_name == *updated_svc_name {
                    found_service = true;
                    before.entry(at).or_insert(existing_decls.len());
                    merge_update_decls(existing_decls, updated_decls);
                }
            }
        }

        if !found_service {
            return Err(*updated_svc_name);
        }
    }
    Ok(())
}

/// Add to `reads` the members of `service` that evaluating `expr` certainly
/// reads: plain names, and qualified names in `service` itself. Building a
/// function or an action reads nothing (C-Abs, C-Action), so their bodies are
/// skipped; what applying a function reads (C-App) is left out too. These
/// reads are a subset of those the forward-reference check counts
fn eager_reads(expr: &Expr, service: Symbol, reads: &mut HashSet<Symbol>) {
    match expr {
        Expr::Variable { name } => {
            reads.insert(*name);
        }
        Expr::MemberAccess {
            service_name,
            member_name,
        } => {
            if *service_name == service {
                reads.insert(*member_name);
            }
        }
        Expr::Func { .. } | Expr::Action(_) => {}
        // A table literal holds data; a query's condition is evaluated per row
        Expr::Table { .. } => {}
        Expr::Select { table_name, .. } => {
            reads.insert(*table_name);
        }
        _ => {
            for child in expr.children() {
                eager_reads(child, service, reads);
            }
        }
    }
}

/// Merge an update's declarations into a service's: a changed declaration
/// replaces the old one in place, and a new one goes at the end, in the
/// update's order
fn merge_update_decls(decls: &mut Vec<Decl>, updated: &[Decl]) {
    let mut index: HashMap<Symbol, usize> = decls
        .iter()
        .enumerate()
        .map(|(i, d)| (d.name(), i))
        .collect();
    for up in updated {
        match index.get(&up.name()) {
            Some(&i) => decls[i] = up.clone(),
            None => {
                index.insert(up.name(), decls.len());
                decls.push(up.clone());
            }
        }
    }
}

/// Order the members of `service` after its updates are merged, the first
/// `existing` of them being those it had before
///
/// A member may only read members declared before it, so the two sequences,
/// existing members and new ones, are interleaved, each keeping its own order
/// (the existing members theirs, the new members the updates'), so that each
/// member follows the members it certainly reads (`eager_reads`), preferring
/// the merged order: a stable topological sort. That lets an update add a
/// member and change an existing def to read it, while a forward reference
/// among the updates' own members, or among existing members, stays an error
/// for the check to report. It runs once for all the updates merged together,
/// so an `atomic` block is ordered by its final declarations.
///
/// The merged order already keeps both sequences in order, so if it also
/// satisfies these reads it is left as it is; and they are a subset of the
/// reads the forward-reference check counts, so any update the check accepted
/// without ordering is merged exactly as before. Members only move when that
/// order would certainly be rejected. If the constraints form a cycle, the
/// order is left for the check to report.
fn order_members(decls: &mut Vec<Decl>, service: Symbol, existing: usize) {
    let index: HashMap<Symbol, usize> = decls
        .iter()
        .enumerate()
        .map(|(i, d)| (d.name(), i))
        .collect();

    // `readers[j]`: the members that read member `j`; `waiting[i]`: how many
    // members member `i` reads that are not placed yet
    let mut readers: Vec<Vec<usize>> = vec![Vec::new(); decls.len()];
    let mut waiting = vec![0usize; decls.len()];
    // Each sequence keeps its order
    for i in 1..decls.len() {
        if i != existing {
            readers[i - 1].push(i);
            waiting[i] += 1;
        }
    }
    for (i, decl) in decls.iter().enumerate() {
        let mut reads = HashSet::new();
        if let Decl::VarDecl { val, .. } | Decl::DefDecl { val, .. } = decl {
            eager_reads(val, service, &mut reads);
        }
        for read in reads {
            if let Some(&j) = index.get(&read).filter(|&&j| j != i) {
                readers[j].push(i);
                waiting[i] += 1;
            }
        }
    }
    let mut ready: BinaryHeap<Reverse<usize>> = (0..decls.len())
        .filter(|&i| waiting[i] == 0)
        .map(Reverse)
        .collect();
    let mut order = Vec::with_capacity(decls.len());
    while let Some(Reverse(i)) = ready.pop() {
        order.push(i);
        for &reader in &readers[i] {
            waiting[reader] -= 1;
            if waiting[reader] == 0 {
                ready.push(Reverse(reader));
            }
        }
    }
    let unchanged = order.iter().enumerate().all(|(at, &i)| at == i);
    if order.len() < decls.len() || unchanged {
        return;
    }
    let mut slots: Vec<Option<Decl>> = std::mem::take(decls).into_iter().map(Some).collect();
    *decls = order.into_iter().filter_map(|i| slots[i].take()).collect();
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

    /// The member names of the one service in `program` after applying the
    /// updates that follow it
    fn names_after_update(program: &str) -> Vec<String> {
        let mut interner = Interner::new();
        let stmts = crate::runtime::parser::parse_string(program, &mut interner).unwrap();
        let (base, updates): (Vec<Stmt>, Vec<Stmt>) = stmts
            .into_iter()
            .partition(|s| matches!(s, Stmt::Service { .. }));
        let patched = apply_updates_to_ast(&base, &updates).unwrap();
        let Some(Stmt::Service { decls, .. }) = patched.first() else {
            panic!("expected the service");
        };
        decls
            .iter()
            .map(|d| interner.get(d.name()).to_string())
            .collect()
    }

    /// A new member goes just before the first existing member that follows
    /// it in the update, so a changed def can read it (as in
    /// `meerkat/tests/test_update.mkt`)
    #[test]
    fn test_apply_updates_places_new_member_before_the_def_reading_it() {
        let names = names_after_update(
            "service s { var t = 1; pub def warm = t > 0; }
             update s { var t = 2; var c = true; pub def warm = if c then t > 0 else t > 1; }",
        );
        assert_eq!(names, ["t", "c", "warm"]);
    }

    /// Several new members keep their order in the update
    #[test]
    fn test_apply_updates_keeps_the_order_of_new_members() {
        let names = names_after_update(
            "service s { var t = 1; pub def warm = t > 0; }
             update s { var a = 1; var b = 2; pub def warm = a + b > t; }",
        );
        assert_eq!(names, ["t", "a", "b", "warm"]);
    }

    /// A new member that nothing reads goes at the end, and a changed member
    /// keeps its position
    #[test]
    fn test_apply_updates_appends_trailing_new_members() {
        let names = names_after_update(
            "service s { var t = 1; pub def warm = t > 0; }
             update s { var t = 5; pub def hot = t > 9; }",
        );
        assert_eq!(names, ["t", "warm", "hot"]);
    }

    /// A new member nothing reads goes at the end even when the update lists
    /// it before a changed member that reads no new member (#265's amended
    /// decision)
    #[test]
    fn test_apply_updates_appends_a_new_member_listed_before_a_changed_one() {
        let names = names_after_update(
            "service s { var t = 1; pub def warm = t > 0; }
             update s { var h = 3; pub def warm = t > 0; }",
        );
        assert_eq!(names, ["t", "warm", "h"]);
    }

    /// A new member nothing reads goes at the end, even when the update lists
    /// it before a changed member: placing it earlier would put it ahead of a
    /// member it reads (a regression found in review of #266)
    #[test]
    fn test_apply_updates_keeps_an_unread_new_member_after_its_dependencies() {
        let names = names_after_update(
            "service s { var x = 1; pub def flag = x > 0; var tail = 10; }
             update s { var added = tail + 1; pub def flag = x > 1; }",
        );
        assert_eq!(names, ["x", "flag", "tail", "added"]);
    }

    /// A chain of new members, the last read by a changed def, is placed
    /// before that def in dependency order
    #[test]
    fn test_apply_updates_places_a_chain_of_new_members() {
        let names = names_after_update(
            "service s { var t = 1; pub def warm = t > 0; }
             update s { var c = 2; pub def f = c + t; pub def warm = f > 0; }",
        );
        assert_eq!(names, ["t", "c", "f", "warm"]);
    }

    /// Building an action reads nothing, so a name inside an action body
    /// doesn't move anything: the order appending gives, which the check
    /// accepts, stays (a regression found in review of #266)
    #[test]
    fn test_apply_updates_ignores_names_inside_actions() {
        let names = names_after_update(
            "service s { pub def check = action {}; var tail = 10; }
             update s { var c = tail; pub def d = c; pub def check = action { assert(d == 10); }; }",
        );
        assert_eq!(names, ["check", "tail", "c", "d"]);
    }

    /// A qualified read of the member's own service (`s.c`) counts as a read
    #[test]
    fn test_apply_updates_counts_qualified_reads_of_the_same_service() {
        let names = names_after_update(
            "service s { var t = 1; pub def warm = t > 0; }
             update s { var c = 2; pub def warm = s.c > 0; }",
        );
        assert_eq!(names, ["t", "c", "warm"]);
    }

    /// An `atomic` block is ordered by its final declarations: the first
    /// update has `x` read a new `c`, the second reverses that, and `c` must
    /// end up after `x` (a regression found in review of #266)
    #[test]
    fn test_apply_updates_orders_an_atomic_block_by_its_final_declarations() {
        let names = names_after_update(
            "service s { var a = 0; pub def x = a + 1; }
             atomic {
                 update s { var c = 5; pub def x = c + 1; }
                 update s { pub def x = a + 2; pub def c = x + 1; }
             }",
        );
        assert_eq!(names, ["a", "x", "c"]);
    }

    /// Existing members keep their order, so a changed member reading a later
    /// existing one is left for the check to reject
    #[test]
    fn test_apply_updates_keeps_a_forward_reference_between_existing_members() {
        let names = names_after_update(
            "service s { var a = 1; var b = 2; }
             update s { var a = b; }",
        );
        assert_eq!(names, ["a", "b"]);
    }
}
