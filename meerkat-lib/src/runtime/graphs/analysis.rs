//! Dependency analysis algorithms and eager validation passes
use super::effects::Effects;
use super::ServiceGraphs;
use crate::ast::Stmt;
use crate::runtime::interner::Symbol;
use std::collections::{HashMap, HashSet};

/// The dependency graphs of each service `program` declares, in order
pub fn compute_dependencies(
    program: &[Stmt],
    _pre_initialized: Option<&HashMap<Symbol, HashSet<Symbol>>>,
) -> Vec<ServiceGraphs> {
    let mut effects = Effects::new(program, HashMap::new());
    program
        .iter()
        .filter_map(|stmt| match stmt {
            Stmt::Service { name, decls } => Some(effects.graphs(*name, decls)),
            _ => None,
        })
        .collect()
}

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
