//! Network representation for `AST` elements
//!
//! This module defines the serialized equivalents of the runtime `AST`
//! types, substituting `Symbol` identifiers with raw `String` names

use crate::net::ServiceNetId;
use serde::{Deserialize, Serialize};

/// Network representation of a field definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetField {
    pub name: String,
    pub ty: NetTableType,
}

/// Network representation of a service type holding ordered fields
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct NetServiceType {
    pub fields: Vec<(String, NetType)>,
}

/// Network representation of a type in the Meerkat language
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum NetType {
    Int,
    String,
    Bool,
    Unit,
    Tuple(Vec<NetType>),
    Func(Box<NetType>, Box<NetType>),
    List(Box<NetType>),
}

/// Network representation of a function parameter
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct NetParam {
    pub name: String,
    pub ty: Option<NetType>,
}

/// Network representation of an action statement
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NetActionStmt {
    Let {
        name: String,
        ty: Option<NetType>,
        expr: NetExpr,
    },
    Expr(NetExpr),
    Do(NetExpr),
    /// An `assert` statement to check invariants
    ///
    /// The `String` parameter captures the exact raw source
    /// string of the assertion condition for error reporting
    Assert(NetExpr, String),
    Assign {
        name: String,
        expr: NetExpr,
    },
    Insert {
        row: NetExpr,
        table_name: String,
    },
    For {
        var: String,
        iterable: NetExpr,
        body: Vec<NetActionStmt>,
    },
}

/// Network representation of a value
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NetValue {
    Int {
        val: i32,
    },
    Bool {
        val: bool,
    },
    String {
        val: String,
    },
    /// A standard closure value with environment
    Closure {
        params: Vec<NetParam>,
        body: Box<NetExpr>,
        env: Vec<(String, NetValue)>,
        service_name: String,
        /// Important: `None` indicates missing type information, not the
        /// `unit` type. In practice, the return types for closures should
        /// generally be `Some` type; however, this was left as an `Option`
        /// for now for maximum flexibility as development continues.
        return_ty: Option<NetType>,
    },
    /// An action closure value with environment and network ID
    ActionClosure {
        stmts: Vec<NetActionStmt>,
        env: Vec<(String, NetValue)>,
        service_net_id: ServiceNetId,
    },
    List {
        vals: Vec<NetValue>,
    },
    Record {
        fields: Vec<(String, NetValue)>,
    },
    Range {
        start: i32,
        end: i32,
    },
}

/// Network representation of an expression
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NetExpr {
    Literal {
        val: NetValue,
    },
    Variable {
        name: String,
    },
    Tuple {
        val: Vec<NetExpr>,
    },
    KeyVal {
        name: String,
        value: Box<NetExpr>,
    },
    Unop {
        op: NetUnOp,
        expr: Box<NetExpr>,
    },
    Binop {
        op: NetBinOp,
        expr1: Box<NetExpr>,
        expr2: Box<NetExpr>,
    },
    If {
        cond: Box<NetExpr>,
        expr1: Box<NetExpr>,
        expr2: Box<NetExpr>,
    },
    Func {
        params: Vec<NetParam>,
        body: Box<NetExpr>,
        return_ty: Option<NetType>,
    },
    Call {
        func: Box<NetExpr>,
        args: Vec<NetExpr>,
    },
    Action(Vec<NetActionStmt>),
    MemberAccess {
        service_name: String,
        member_name: String,
    },
    Select {
        table_name: String,
        column_names: Vec<String>,
        where_clause: Box<NetExpr>,
    },
    Table {
        schema: Vec<NetField>,
        records: Vec<NetExpr>,
    },
    Fold {
        table_name: String,
        column_name: String,
        operation: Box<NetExpr>,
        identity: Box<NetExpr>,
    },
    Map {
        var: String,
        table_name: String,
        where_clause: Box<NetExpr>,
        body: Box<NetExpr>,
    },
    List(Vec<NetExpr>),
    Range {
        start: Box<NetExpr>,
        end: Box<NetExpr>,
    },
}

/// Network representation of a unary operator
///
/// This enum defines the serialized unary operators mapped from the
/// counterparts for transmission over the network
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum NetUnOp {
    Neg,
    Not,
}

/// Network representation of a binary operator
///
/// This enum defines the serialized binary operators mapped from the
/// counterparts for transmission over the network
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum NetBinOp {
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

/// Network representation of a table field data type
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum NetTableType {
    Int,
    String,
    Bool,
}
