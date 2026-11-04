//! Dependency graphs of a service

pub mod analysis;
pub(crate) mod effects;
pub mod free_var;

use crate::runtime::interner::Symbol;
use petgraph::graphmap::DiGraphMap;
use petgraph::visit::{Dfs, Reversed};
use petgraph::Direction;
use std::collections::{HashMap, HashSet};

/// Pre-computed dependency graphs and analysis state for a service
pub struct ServiceGraphs {
    /// Reactive propagation graph (edge `A -> B` means `A` notifies `B`)
    pub reactive_graph: DiGraphMap<Symbol, ()>,
    /// Cross-service member dependencies per local definition
    pub cross_deps: HashMap<Symbol, HashSet<(Symbol, Symbol)>>,
    /// Set of local variable symbols defined in the service
    pub vars: HashSet<Symbol>,
    /// Set of local definition symbols defined in the service
    pub defs: HashSet<Symbol>,
    /// Set of table symbols defined in the service
    pub tables: HashSet<Symbol>,
}

impl ServiceGraphs {
    /// Create a new empty `ServiceGraphs` container instance
    ///
    /// Returns:
    ///   `Self`: An empty `ServiceGraphs` instance
    pub fn new() -> Self {
        ServiceGraphs {
            reactive_graph: DiGraphMap::new(),
            cross_deps: HashMap::new(),
            vars: HashSet::new(),
            defs: HashSet::new(),
            tables: HashSet::new(),
        }
    }

    /// The static inputs of `member` as `(service, member)` pairs: its
    /// predecessors in the reactive graph, which belong to `service`, and its
    /// cross-service dependencies
    ///
    /// Args:
    ///   `service` (`Symbol`): The service these graphs belong to
    ///   `member` (`Symbol`): The member whose inputs are listed
    ///
    /// Returns:
    ///   `impl Iterator<Item = (Symbol, Symbol)>`: The inputs of `member`
    pub(crate) fn inputs(
        &self,
        service: Symbol,
        member: Symbol,
    ) -> impl Iterator<Item = (Symbol, Symbol)> + '_ {
        self.reactive_graph
            .neighbors_directed(member, Direction::Incoming)
            .map(move |input| (service, input))
            .chain(self.cross_deps.get(&member).into_iter().flatten().copied())
    }

    /// Extract all transitive dependencies of a member using `petgraph::visit::Dfs`
    ///
    /// Args:
    ///   `member` (`Symbol`): The target member symbol
    ///
    /// Returns:
    ///   `HashSet<Symbol>`: Set of transitive dependency symbols
    pub fn transitive_dependencies_of(&self, member: Symbol) -> HashSet<Symbol> {
        let mut visited = HashSet::new();
        let reversed = Reversed(&self.reactive_graph);
        let mut dfs = Dfs::new(reversed, member);
        while let Some(node) = dfs.next(reversed) {
            if node != member {
                visited.insert(node);
            }
        }
        visited
    }
}

/// Graphs are equal when they declare the same members with the same edges
impl PartialEq for ServiceGraphs {
    fn eq(&self, other: &Self) -> bool {
        let edges = |graphs: &Self| {
            graphs
                .reactive_graph
                .all_edges()
                .map(|(input, member, _)| (input, member))
                .collect::<HashSet<_>>()
        };
        self.vars == other.vars
            && self.defs == other.defs
            && self.tables == other.tables
            && self.cross_deps == other.cross_deps
            && edges(self) == edges(other)
    }
}

impl Default for ServiceGraphs {
    /// Provide standard default initialization for `ServiceGraphs`
    ///
    /// Returns:
    ///   `Self`: Default empty container
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_service_graphs_default_and_new() {
        let sg_def = ServiceGraphs::default();
        let sg_new = ServiceGraphs::new();
        assert!(sg_def.reactive_graph.nodes().next().is_none());
        assert!(sg_new.reactive_graph.nodes().next().is_none());
        assert!(sg_def.vars.is_empty());
        assert!(sg_def.defs.is_empty());
        assert!(sg_def.tables.is_empty());
        assert!(sg_def.cross_deps.is_empty());
    }
}
