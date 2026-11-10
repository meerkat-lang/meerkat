//! Effective declarations of a program
use crate::ast::Stmt;
use crate::runtime::interner::Symbol;
use std::collections::HashMap;

/// Effective service declarations: later declarations replace earlier imports.
pub(crate) fn service_declarations(program: &[Stmt]) -> HashMap<Symbol, &Stmt> {
    program
        .iter()
        .filter_map(|stmt| match stmt {
            Stmt::Service { name, .. } => Some((*name, stmt)),
            _ => None,
        })
        .collect()
}
