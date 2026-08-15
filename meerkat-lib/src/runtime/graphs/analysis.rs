//! Dependency analysis algorithms and eager validation passes
use super::ServiceGraphs;
use crate::ast::{Decl, Stmt};
use crate::runtime::interner::Symbol;
use std::collections::{HashMap, HashSet};

pub fn compute_dependencies(
    program: &[Stmt],
    _pre_initialized: Option<&HashMap<Symbol, HashSet<Symbol>>>,
) -> Vec<ServiceGraphs> {
    let mut all_graphs = Vec::new();
    for stmt in program {
        if let Stmt::Service { decls, .. } = stmt {
            let mut graphs = ServiceGraphs::new();
            let mut declared_symbols = HashSet::new();

            for decl in decls.iter() {
                match decl {
                    Decl::VarDecl { name, .. } => {
                        declared_symbols.insert(*name);
                        graphs.vars.insert(*name);
                        graphs.reactive_graph.add_node(*name);
                    }
                    Decl::DefDecl { name, .. } => {
                        declared_symbols.insert(*name);
                        graphs.defs.insert(*name);
                        graphs.reactive_graph.add_node(*name);
                    }
                    Decl::TableDecl { name, .. } => {
                        declared_symbols.insert(*name);
                        graphs.tables.insert(*name);
                        graphs.reactive_graph.add_node(*name);
                    }
                }
            }

            for decl in decls.iter() {
                match decl {
                    Decl::VarDecl { name, val, .. } | Decl::DefDecl { name, val, .. } => {
                        let cross_deps = super::free_var::cross_service_deps(val);
                        if !cross_deps.is_empty() {
                            graphs.cross_deps.insert(*name, cross_deps);
                        }
                        let all_deps = super::free_var::free_var(val, &HashSet::new());
                        for dep in &all_deps {
                            if declared_symbols.contains(dep) {
                                graphs.reactive_graph.add_edge(*dep, *name, ());
                            }
                        }
                    }
                    Decl::TableDecl { .. } => {}
                }
            }
            all_graphs.push(graphs);
        }
    }
    all_graphs
}
