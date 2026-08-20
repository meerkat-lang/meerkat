//! `AST` pretty printer inspired by Rust syntax
//!
//! This module is primarily invoked by the `--ast` flag for use in
//! testing and development. It supports configurable indentation
//! levels with the `INDENTATION` constant

use crate::runtime::ast::{ActionStmt, Decl, Expr, Field, Stmt, Value};
use crate::runtime::interner::{Interner, Symbol};
use crate::runtime::tt::Type;

const INDENTATION: usize = 2;

/// Pretty printer for formatting and displaying the abstract syntax tree
pub struct AstPrinter<'a> {
    spaces: usize,
    interner: &'a Interner,
}

impl<'a> AstPrinter<'a> {
    /// Creates a new `AstPrinter` instance with default indentation
    pub fn new(interner: &'a Interner) -> Self {
        Self {
            spaces: INDENTATION,
            interner,
        }
    }

    /// Creates a new `AstPrinter` instance with custom indentation level
    pub fn with_spaces(spaces: usize, interner: &'a Interner) -> Self {
        Self { spaces, interner }
    }

    /// Helper function to format a `Symbol` with its ID and string representation
    ///
    /// Args:
    ///     `sym` (`Symbol`): The symbol to format
    ///
    /// Returns:
    ///     `String`: The formatted symbol string representation
    pub fn format_symbol(&self, sym: Symbol) -> String {
        format!("{:?} (\"{}\")", sym, self.interner.get(sym))
    }

    /// Helper function to format an optional type using `Display` representation
    ///
    /// Args:
    ///     `ty` (`&Option<Type>`): The optional type to format
    ///
    /// Returns:
    ///     `String`: The formatted type string representation
    pub fn format_type_opt(&self, ty: &Option<Type>) -> String {
        match ty {
            Some(t) => format!("Some({})", t),
            None => "None".to_string(),
        }
    }

    /// Prints spaces corresponding to the current indentation level
    pub(crate) fn print_indent(&self, indent: usize) {
        print!("{}", " ".repeat(indent * self.spaces));
    }

    /// Prints a sequence of top-level statements representing a program
    pub fn print_program(&self, stmts: &[Stmt]) {
        for stmt in stmts {
            self.print_stmt(stmt, 0);
        }
    }

    /// Prints a single top-level statement with the specified indentation
    pub fn print_stmt(&self, stmt: &Stmt, indent: usize) {
        self.print_indent(indent);
        match stmt {
            Stmt::ActionStmt(action) => {
                println!("ActionStmt:");
                self.print_action_stmt(action, indent + 1);
            }
            Stmt::Atomic { updates } => {
                println!("Atomic:");
                for update in updates {
                    self.print_stmt(update, indent + 1);
                }
            }
            Stmt::Update {
                service_name,
                decls,
            } => {
                let service_name = *service_name;
                println!(
                    "Update: {{ service_name: {} }}",
                    self.format_symbol(service_name)
                );
                for decl in decls {
                    self.print_decl(decl, indent + 1);
                }
            }
            Stmt::Connect { path, addr } => {
                println!("Connect: {{ path: \"{}\", addr: \"{}\" }}", path, addr);
            }
            Stmt::Import {
                path,
                service_name,
                explicit_path,
            } => {
                let service_name = *service_name;
                println!(
                    "Import: {{ path: \"{}\", service_name: {}, explicit_path: {} }}",
                    path,
                    self.format_symbol(service_name),
                    explicit_path,
                );
            }
            Stmt::Service { name, decls } => {
                let name = *name;
                println!("Service: {{ name: {} }}", self.format_symbol(name));
                for decl in decls {
                    self.print_decl(decl, indent + 1);
                }
            }
            Stmt::Test {
                service_name,
                stmts,
            } => {
                let service_name = *service_name;
                println!(
                    "Test: {{ service_name: {} }}",
                    self.format_symbol(service_name)
                );
                for s in stmts {
                    self.print_action_stmt(s, indent + 1);
                }
            }
            Stmt::Watch { expr } => {
                println!("Watch:");
                self.print_expr(expr, indent + 1);
            }
        }
    }

    /// Prints a declaration statement with the specified indentation
    pub fn print_decl(&self, decl: &Decl, indent: usize) {
        self.print_indent(indent);
        match decl {
            Decl::VarDecl { name, ty, val } => {
                let name = *name;
                println!(
                    "VarDecl: {{ name: {}, ty: {} }}",
                    self.format_symbol(name),
                    self.format_type_opt(ty)
                );
                self.print_expr(val, indent + 1);
            }
            Decl::DefDecl {
                name,
                ty,
                val,
                is_pub,
            } => {
                let name = *name;
                let is_pub = *is_pub;
                println!(
                    "DefDecl: {{ name: {}, ty: {}, is_pub: {} }}",
                    self.format_symbol(name),
                    self.format_type_opt(ty),
                    is_pub
                );
                self.print_expr(val, indent + 1);
            }
            Decl::TableDecl { name, fields } => {
                let name = *name;
                println!("TableDecl: {{ name: {} }}", self.format_symbol(name));
                for field in fields {
                    self.print_field(field, indent + 1);
                }
            }
        }
    }

    /// Prints a record/table field description with the specified indentation
    fn print_field(&self, field: &Field, indent: usize) {
        self.print_indent(indent);
        println!(
            "Field: {{ name: {}, ty: {:?} }}",
            self.format_symbol(field.name),
            field.ty
        );
    }

    /// Prints an action statement with the specified indentation
    pub fn print_action_stmt(&self, stmt: &ActionStmt, indent: usize) {
        self.print_indent(indent);
        match stmt {
            ActionStmt::Let { name, ty, expr } => {
                let name = *name;
                println!(
                    "Let: {{ name: {}, ty: {} }}",
                    self.format_symbol(name),
                    self.format_type_opt(ty)
                );
                self.print_expr(expr, indent + 1);
            }
            ActionStmt::Expr(expr) => {
                println!("Expr:");
                self.print_expr(expr, indent + 1);
            }
            ActionStmt::Do(expr) => {
                println!("Do:");
                self.print_expr(expr, indent + 1);
            }
            ActionStmt::Assert(expr, text) => {
                println!("Assert: {{ text: {:?} }}", text);
                self.print_expr(expr, indent + 1);
            }
            ActionStmt::Assign { name, expr } => {
                let name = *name;
                println!("Assign: {{ name: {} }}", self.format_symbol(name));
                self.print_expr(expr, indent + 1);
            }
            ActionStmt::Insert { row, table_name } => {
                let table_name = *table_name;
                println!(
                    "Insert: {{ table_name: {} }}",
                    self.format_symbol(table_name)
                );
                self.print_expr(row, indent + 1);
            }
            ActionStmt::For {
                var,
                iterable,
                body,
            } => {
                let var = *var;
                println!("For: {{ var: {} }}", self.format_symbol(var));
                self.print_expr(iterable, indent + 1);
                for stmt in body {
                    self.print_action_stmt(stmt, indent + 1);
                }
            }
        }
    }

    /// Prints an expression with the specified indentation
    pub fn print_expr(&self, expr: &Expr, indent: usize) {
        self.print_indent(indent);
        match expr {
            Expr::Literal { val } => {
                println!("Literal:");
                self.print_value(val, indent + 1);
            }
            Expr::Variable { name } => {
                let name = *name;
                println!("Variable: {{ name: {} }}", self.format_symbol(name));
            }
            Expr::Html(template) => {
                println!("Html:");
                // #39: the html module owns how a template prints itself, so the
                // representation stays hidden here.
                template.print_ast(self, indent + 1);
            }
            Expr::Tuple { val } => {
                println!("Tuple:");
                for v in val {
                    self.print_expr(v, indent + 1);
                }
            }
            Expr::KeyVal { name, value } => {
                let name = *name;
                println!("KeyVal: {{ name: {} }}", self.format_symbol(name));
                self.print_expr(value, indent + 1);
            }
            Expr::Unop { op, expr } => {
                let op = *op;
                println!("Unop: {{ op: {:?} }}", op);
                self.print_expr(expr, indent + 1);
            }
            Expr::Binop { op, expr1, expr2 } => {
                let op = *op;
                println!("Binop: {{ op: {:?} }}", op);
                self.print_expr(expr1, indent + 1);
                self.print_expr(expr2, indent + 1);
            }
            Expr::If { cond, expr1, expr2 } => {
                println!("If:");
                self.print_expr(cond, indent + 1);
                self.print_expr(expr1, indent + 1);
                self.print_expr(expr2, indent + 1);
            }
            Expr::Func {
                params,
                body,
                return_ty,
            } => {
                let params_str: Vec<String> = params
                    .iter()
                    .map(|p| {
                        if let Some(ref t) = p.ty {
                            format!("{}: {}", self.format_symbol(p.name), t)
                        } else {
                            self.format_symbol(p.name)
                        }
                    })
                    .collect();
                println!(
                    "Func: {{ params: {:?}, return_ty: {} }}",
                    params_str,
                    self.format_type_opt(return_ty)
                );
                self.print_expr(body, indent + 1);
            }
            Expr::Call { func, args } => {
                println!("Call:");
                self.print_expr(func, indent + 1);
                for arg in args {
                    self.print_expr(arg, indent + 1);
                }
            }
            Expr::Action(stmts) => {
                println!("Action:");
                for stmt in stmts {
                    self.print_action_stmt(stmt, indent + 1);
                }
            }
            Expr::MemberAccess {
                service_name,
                member_name,
            } => {
                let service_name = *service_name;
                let member_name = *member_name;
                println!(
                    "MemberAccess: {{ service_name: {}, member_name: {} }}",
                    self.format_symbol(service_name),
                    self.format_symbol(member_name)
                );
            }
            Expr::Select {
                table_name,
                column_names,
                where_clause,
            } => {
                let table_name = *table_name;
                let cols_str: Vec<String> = column_names
                    .iter()
                    .map(|c| self.format_symbol(*c))
                    .collect();
                println!(
                    "Select: {{ table_name: {}, column_names: {:?} }}",
                    self.format_symbol(table_name),
                    cols_str
                );
                self.print_expr(where_clause, indent + 1);
            }
            Expr::Table { schema, records } => {
                println!("Table:");
                for field in schema {
                    self.print_field(field, indent + 1);
                }
                for record in records {
                    self.print_expr(record, indent + 1);
                }
            }
            Expr::Fold {
                table_name,
                column_name,
                operation,
                identity,
            } => {
                let table_name = *table_name;
                let column_name = *column_name;
                println!(
                    "Fold: {{ table_name: {}, column_name: {} }}",
                    self.format_symbol(table_name),
                    self.format_symbol(column_name)
                );
                self.print_expr(operation, indent + 1);
                self.print_expr(identity, indent + 1);
            }
            Expr::List(exprs) => {
                println!("List:");
                for expr in exprs {
                    self.print_expr(expr, indent + 1);
                }
            }
            Expr::Range { start, end } => {
                println!("Range:");
                self.print_expr(start, indent + 1);
                self.print_expr(end, indent + 1);
            }
        }
    }

    /// Prints a runtime value representation with the specified indentation
    pub fn print_value(&self, val: &Value, indent: usize) {
        self.print_indent(indent);
        match val {
            Value::Int { val } => {
                let val = *val;
                println!("Int: {}", val);
            }
            Value::Bool { val } => {
                let val = *val;
                println!("Bool: {}", val);
            }
            Value::String { val } => {
                println!("String: \"{}\"", val);
            }
            Value::Html(html) => {
                println!("Html: {:?}", html.as_str());
            }
            Value::Closure {
                params,
                body,
                service_name,
                return_ty,
                ..
            } => {
                let service_name = *service_name;
                let params_str: Vec<String> = params
                    .iter()
                    .map(|p| {
                        if let Some(ref t) = p.ty {
                            format!("{}: {}", self.format_symbol(p.name), t)
                        } else {
                            self.format_symbol(p.name)
                        }
                    })
                    .collect();
                println!(
                    "Closure: {{ params: {:?}, return_ty: {}, service_name: {} }}",
                    params_str,
                    self.format_type_opt(return_ty),
                    self.format_symbol(service_name)
                );
                self.print_expr(body, indent + 1);
            }
            Value::ActionClosure {
                stmts,
                service_net_id,
                ..
            } => {
                println!(
                    "ActionClosure: {{ service_net_id: \"{}\" }}",
                    service_net_id.0
                );
                for stmt in stmts {
                    self.print_action_stmt(stmt, indent + 1);
                }
            }
            Value::List { vals } => {
                println!("List:");
                for val in vals {
                    self.print_value(val, indent + 1);
                }
            }
            Value::Range { start, end } => {
                println!("Range: {}..{}", start, end);
            }
        }
    }
}
