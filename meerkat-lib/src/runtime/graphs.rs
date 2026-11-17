//! Dependency graphs of a service

pub(crate) mod analysis;
pub(crate) mod effects;
pub mod free_var;

use crate::runtime::interner::Symbol;
use petgraph::graphmap::DiGraphMap;
use petgraph::Direction;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::Hash;

/// Pre-computed dependency graphs and analysis state for a service
pub struct ServiceGraphs {
    /// Reactive propagation graph (edge `A -> B` means `A` notifies `B`)
    reactive_graph: DiGraphMap<usize, ()>,
    /// Node numbers are positions in this graph's owned names, never symbol identities.
    names: Vec<Symbol>,
    nodes: HashMap<Symbol, usize>,
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
            names: Vec::new(),
            nodes: HashMap::new(),
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
        service: &Symbol,
        member: &Symbol,
    ) -> impl Iterator<Item = (Symbol, Symbol)> + '_ {
        let service = service.clone();
        self.nodes
            .get(member)
            .copied()
            .into_iter()
            .flat_map(|node| {
                self.reactive_graph
                    .neighbors_directed(node, Direction::Incoming)
            })
            .map(move |input| (service.clone(), self.names[input].clone()))
            .chain(self.cross_deps.get(member).into_iter().flatten().cloned())
    }

    /// Whether `member` of `service` reads `input`: the relation `inputs`
    /// lists, an edge of the reactive graph for a member of `service` and a
    /// cross-service dependency otherwise
    pub(crate) fn reads(
        &self,
        service: &Symbol,
        member: &Symbol,
        input: &(Symbol, Symbol),
    ) -> bool {
        if &input.0 == service {
            self.contains_edge(&input.1, member)
        } else {
            self.cross_deps
                .get(member)
                .is_some_and(|inputs| inputs.contains(input))
        }
    }

    fn contains_edge(&self, input: &Symbol, member: &Symbol) -> bool {
        self.nodes
            .get(input)
            .zip(self.nodes.get(member))
            .is_some_and(|(&input, &member)| self.reactive_graph.contains_edge(input, member))
    }
}

/// Graphs are equal when they declare the same members with the same edges
impl PartialEq for ServiceGraphs {
    fn eq(&self, other: &Self) -> bool {
        self.vars == other.vars
            && self.defs == other.defs
            && self.tables == other.tables
            && self.cross_deps == other.cross_deps
            && self.incomplete == other.incomplete
            && self.reactive_graph.edge_count() == other.reactive_graph.edge_count()
            && self.reactive_graph.all_edges().all(|(input, member, _)| {
                other.contains_edge(&self.names[input], &self.names[member])
            })
    }
}

/// The graph-local nodes in a strongly connected component of several
/// members, or reading themselves.
fn cyclic_members(graph: &DiGraphMap<usize, ()>) -> impl Iterator<Item = usize> + '_ {
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
    let mut graph = DiGraphMap::new();
    let mut names = Vec::new();
    let mut nodes = HashMap::new();
    let mut failing = BTreeMap::new();
    for (service, graphs) in graphs {
        for def in &graphs.defs {
            let member = node_index(
                &mut graph,
                &mut names,
                &mut nodes,
                (service.clone(), def.clone()),
            );
            for input in graphs.inputs(service, def) {
                let input = node_index(&mut graph, &mut names, &mut nodes, input);
                graph.add_edge(input, member, ());
            }
        }
        for def in &graphs.incomplete {
            failing.insert((service.clone(), def.clone()), Uninstallable::Incomplete);
        }
    }

    failing.extend(cyclic_members(&graph).map(|def| (names[def].clone(), Uninstallable::Cycle)));
    failing
}

/// Return this graph's index for a name, retaining the name until the graph
/// is dropped. Existing names keep their index throughout construction.
fn node_index<N: Clone + Eq + Hash>(
    graph: &mut DiGraphMap<usize, ()>,
    names: &mut Vec<N>,
    nodes: &mut HashMap<N, usize>,
    name: N,
) -> usize {
    *nodes.entry(name).or_insert_with_key(|name| {
        let node = names.len();
        names.push(name.clone());
        graph.add_node(node);
        node
    })
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
