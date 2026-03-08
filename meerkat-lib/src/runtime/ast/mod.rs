use std::fmt::Display;

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
pub struct Assn {
    pub dest: String,
    pub src: Expr,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Insert {                    // insert {id: 1, ..}
    pub row: Expr,                     // each row is a vector
    pub table_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Entry {
    pub name: String, // column name         id
    pub val: Expr, // value to be inserted    1
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Record {
    pub val: Vec<Expr>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Expr {
    /// Basic Lambda Core expressions
    Number {
        val: i32,
    },
    Bool {
        val: bool,
    },
    String {
        val: String,
    },
    Variable {
        ident: String,
    },
    Vector {
        val: Vec<Expr>
    },
    KeyVal {
        key: String,
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
        params: Vec<String>,
        body: Box<Expr>,
    },
    FuncApply {
        func: Box<Expr>,
        args: Vec<Expr>,
    },

    /// Action
    Action {
        assns: Vec<Assn>,
        inserts: Vec<Insert>,
    },

    TableColumn { // table1.id for example will be treated as an expression and evaluated separately
        table_name: String,
        column_name: String,
    },

    Select {
        table_name: String,
        column_names: Vec<String>,
        where_clause: Box<Expr>,
    },

    Table {
        schema: Vec<Field>,
        records: Vec<Expr>,
        /*How do records differ from rows?
          Records only consist of data contained within tables: {1, "A", 18}
          Rows are what are written inside insert statements, insert {id: 1, name: "A", age: 18};
         */
    },
    Fold {
        args: Vec<Expr>
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Decl {
    Import {
        srv_name: String,
    },
    VarDecl {
        name: String,
        val: Expr,
    },
    DefDecl {
        name: String,
        val: Expr,
        is_pub: bool,
    },
    TableDecl {
        name: String,
        fields: Vec<Field>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Field {
    pub name: String,
    pub type_: DataType,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DataType {
    String,
    Number,
    Bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Service {
    pub name: String,
    pub decls: Vec<Decl>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Test {
    pub name: String,
    pub commands: Vec<ReplCmd>, // commands here refer to dos and asserts
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Prog {
    pub services: Vec<Service>,
    pub tests: Vec<Test>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ReplCmd {
    Do(Expr),
    Assert(Expr),
    // service related commands
    // Service(Service),
    // Open(String),
    // Close,
}

impl Default for Expr {
    fn default() -> Self {
        Expr::Number { val: 0 }
    }
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

impl Display for Expr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Expr::Number { val } => write!(f, "{}", val),
            Expr::Bool { val } => write!(f, "{}", val),
            Expr::String { val } => write!(f, "{}", val),
            Expr::Vector { val } => write!(f, "vector"),
            Expr::KeyVal { key, value } => write!(f, "keyval: {}, {}", key, value),
            Expr::Variable { ident } => write!(f, "{}", ident),
            Expr::Unop { op, expr } => write!(f, "{}{}", op, expr),
            Expr::Binop { op, expr1, expr2 } => write!(f, "{} {} {}", expr1, op, expr2),
            Expr::If { cond, expr1, expr2 } => {
                write!(f, "if {} then {} else {}", cond, expr1, expr2)
            }
            Expr::Func { params, body } => write!(f, "fn({})[{}]", params.join(","), body),
            Expr::FuncApply { func, args } =>
                write!(
                    f,
                    "{}({})",
                    func,
                    args.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
                ),
            Expr::Action { assns, inserts } =>
                write!(
                    f,
                    "Action({:?})",
                    assns.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
                ),
            Expr::TableColumn { table_name, column_name } =>
                write!(f, "{}.{}", table_name, column_name),
            Expr::Select { table_name, column_names, where_clause } => write!(f, "{}", where_clause),
            Expr::Table {records , ..} => {
                write!(f, "[",)?;
                for (i, record) in records.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{{")?;
                    match record {
                        Expr::Vector { val } => {
                            for (j, entry) in val.iter().enumerate() {
                                if j > 0 {
                                write!(f, ", ")?;
                                }
                                write!(f, "{}", entry)?;
                            }
                        },
                        other => {
                            write!(f, "{}", other)?;
                        }
                    }
                write!(f, "}}")?;
            }
            write!(f, "]")
            },
            Expr::Fold { .. } => write!(f, "fold")
        }
    }
}

impl Display for Assn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} = {}", self.dest, self.src)
    }
}

impl Display for Decl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Decl::Import { srv_name } => todo!(),
            Decl::VarDecl { name, val } => { write!(f, "var {} = {}", name, val) }
            Decl::DefDecl { name, val, is_pub } => {
                if *is_pub {
                    write!(f, "pub def {} = {}", name, val)
                } else {
                    write!(f, "def {} = {}", name, val)
                }
            }
            Decl::TableDecl { name, fields } => { write!(f, "table {} created", name) }
        }
    }
}
