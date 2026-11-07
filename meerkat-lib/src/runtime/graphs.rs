//! Dependency graphs of a service

pub(crate) mod analysis;
pub(crate) mod effects;
pub mod free_var;

use crate::runtime::interner::Symbol;
use petgraph::graphmap::DiGraphMap;
use petgraph::Direction;
use std::collections::{BTreeMap, HashMap, HashSet};

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
    /// Defs whose analysis exceeded the depth limit; their edges are what it
    /// found. Never part of installed graphs
    pub incomplete: HashSet<Symbol>,
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
            incomplete: HashSet::new(),
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

    /// Whether `member` of `service` reads `input`: the relation `inputs`
    /// lists, an edge of the reactive graph for a member of `service` and a
    /// cross-service dependency otherwise
    pub(crate) fn reads(
        &self,
        service: Symbol,
        member: Symbol,
        (owner, input): (Symbol, Symbol),
    ) -> bool {
        if owner == service {
            self.reactive_graph.contains_edge(input, member)
        } else {
            self.cross_deps
                .get(&member)
                .is_some_and(|inputs| inputs.contains(&(owner, input)))
        }
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
            && self.incomplete == other.incomplete
            && edges(self) == edges(other)
    }
}

/// The members on a cycle of `graph`: in a strongly connected component of
/// several members, or reading themselves
pub(crate) fn cyclic_members(
    graph: &DiGraphMap<(Symbol, Symbol), ()>,
) -> impl Iterator<Item = (Symbol, Symbol)> + '_ {
    petgraph::algo::tarjan_scc(graph)
        .into_iter()
        .filter(|component| component.len() > 1 || graph.contains_edge(component[0], component[0]))
        .flatten()
}

/// Why a def cannot be installed
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Uninstallable {
    /// Its analysis exceeded the depth limit
    Incomplete,
    /// It is on a cycle of the direct dependency graph (§2.1.4)
    Cycle,
}

/// The defs of `graphs`, the graphs of every running service, that cannot
/// be installed, as `(service, def)`
pub(crate) fn uninstallable(
    graphs: &HashMap<Symbol, ServiceGraphs>,
) -> BTreeMap<(Symbol, Symbol), Uninstallable> {
    let mut graph = DiGraphMap::<(Symbol, Symbol), ()>::new();
    let mut failing = BTreeMap::new();
    for (&service, graphs) in graphs {
        for &def in &graphs.defs {
            graph.add_node((service, def));
            for input in graphs.inputs(service, def) {
                graph.add_edge(input, (service, def), ());
            }
        }
        for &def in &graphs.incomplete {
            failing.insert((service, def), Uninstallable::Incomplete);
        }
    }
    failing.extend(cyclic_members(&graph).map(|def| (def, Uninstallable::Cycle)));
    failing
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
