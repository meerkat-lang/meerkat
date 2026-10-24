//! Dependency graph analysis module powered by `petgraph`

pub mod analysis;
pub mod free_var;

use crate::runtime::interner::Symbol;
use petgraph::algo::{has_path_connecting, tarjan_scc, toposort, Cycle};
use petgraph::graphmap::DiGraphMap;
use petgraph::visit::{Dfs, DfsPostOrder, EdgeFiltered, EdgeRef, Reversed};
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
    /// Cached source variables per member, stored as (service, variable) pairs
    pub local_sources: HashMap<Symbol, HashSet<(Symbol, Symbol)>>,
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
            local_sources: HashMap::new(),
        }
    }

    /// Prepare complete local source sets when this graph is installed.
    /// Cross-service and unknown sources keep using the regular analysis path.
    pub(crate) fn prepare_local_sources(&mut self, service: Symbol) {
        let mut local_sources = HashMap::new();
        // Potential optimization: reuse upstream source sets in dependency order.
        // For a single-source chain A -> B -> C (input -> dependent), compute A first
        // and reuse its sources for B and C. This reduces total preparation time
        // from O(n^2) to O(n).
        // Handle cycles by grouping nodes into SCCs and processing dependencies first.
        // SCC processing and set merging/copying may outweigh the savings on some graphs.
        for &member in self.vars.iter().chain(&self.defs) {
            let mut cross = Vec::new();
            if let Some(vars) = self.source_vars(member, &mut cross) {
                if cross.is_empty() {
                    local_sources
                        .insert(member, vars.into_iter().map(|var| (service, var)).collect());
                }
            }
        }
        self.local_sources = local_sources;
    }

    /// Extract all reactive propagation listener edges as `(owner, listener)`
    ///
    /// Returns:
    ///   `Vec<(Symbol, Symbol)>`: List of `(owner, listener_def)` pairs
    pub fn reactive_edges(&self) -> Vec<(Symbol, Symbol)> {
        let mut edges = Vec::new();
        for (src, target, _) in self.reactive_graph.all_edges() {
            edges.push((src, target));
        }
        edges
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

    /// Source vars of a member: the vars it reads through reactive edges, the
    /// member itself if it is a var. The walk does not continue past a var:
    /// its incoming edges come from its initializer, which does not make it
    /// derived. A def that reads no var has none
    ///
    /// Returns:
    ///   `Option<HashSet<Symbol>>`: The source vars, or `None` if the walk
    ///   reaches a name that is neither a var nor a def of this service
    pub fn source_vars(
        &self,
        member: Symbol,
        cross: &mut Vec<(Symbol, Symbol)>,
    ) -> Option<HashSet<Symbol>> {
        // In the reversed graph an edge leaves the dependent node, so dropping
        // the edges that leave a var keeps the walk from passing through it
        let deps = EdgeFiltered::from_fn(Reversed(&self.reactive_graph), |edge| {
            !self.vars.contains(&edge.source())
        });
        let mut dfs = Dfs::new(&deps, member);
        let mut sources = HashSet::new();
        while let Some(name) = dfs.next(&deps) {
            if self.vars.contains(&name) {
                sources.insert(name);
            } else if self.defs.contains(&name) {
                cross.extend(self.cross_deps.get(&name).into_iter().flatten());
            } else {
                return None;
            }
        }
        Some(sources)
    }

    /// Compute strongly connected components using `petgraph::algo::tarjan_scc`
    ///
    /// Returns:
    ///   `Vec<Vec<Symbol>>`: List of strongly connected components
    pub fn strongly_connected_components(&self) -> Vec<Vec<Symbol>> {
        tarjan_scc(&self.reactive_graph)
    }

    /// Compute topological sorting order using `petgraph::algo::toposort`
    ///
    /// Returns:
    ///   `Result<Vec<Symbol>, Cycle<Symbol>>`: Topological ordering or cycle error
    pub fn topological_sort(&self) -> Result<Vec<Symbol>, Cycle<Symbol>> {
        toposort(&self.reactive_graph, None)
    }

    /// Check if a path exists between two symbols using `petgraph::algo::has_path_connecting`
    ///
    /// Args:
    ///   `from` (`Symbol`): Source symbol
    ///   `to` (`Symbol`): Target symbol
    ///
    /// Returns:
    ///   `bool`: True if a directed path exists
    pub fn has_path(&self, from: Symbol, to: Symbol) -> bool {
        has_path_connecting(&self.reactive_graph, from, to, None)
    }

    /// Compute post-order DFS traversal sequence using `petgraph::visit::DfsPostOrder`
    ///
    /// Args:
    ///   `start` (`Symbol`): Starting symbol
    ///
    /// Returns:
    ///   `Vec<Symbol>`: Sequence of visited symbols in post-order
    pub fn dfs_post_order(&self, start: Symbol) -> Vec<Symbol> {
        let mut order = Vec::new();
        let mut dfs = DfsPostOrder::new(&self.reactive_graph, start);
        while let Some(node) = dfs.next(&self.reactive_graph) {
            order.push(node);
        }
        order
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
    use crate::runtime::interner::Interner;

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

    #[test]
    fn test_service_graphs_petgraph_algorithms() {
        let mut interner = Interner::new();
        let mut sg = ServiceGraphs::new();
        let a = interner.insert("a");
        let b = interner.insert("b");
        let c = interner.insert("c");

        sg.reactive_graph.add_edge(a, b, ());
        sg.reactive_graph.add_edge(b, c, ());

        let edges = sg.reactive_edges();
        assert_eq!(edges.len(), 2);
        assert!(edges.contains(&(a, b)));
        assert!(edges.contains(&(b, c)));

        assert!(sg.has_path(a, c));
        assert!(!sg.has_path(c, a));

        let trans = sg.transitive_dependencies_of(c);
        assert_eq!(trans.len(), 2);
        assert!(trans.contains(&a));
        assert!(trans.contains(&b));

        let topo = sg.topological_sort();
        assert!(topo.is_ok());
        let topo_nodes = topo.unwrap();
        assert_eq!(topo_nodes.len(), 3);

        let scc = sg.strongly_connected_components();
        assert_eq!(scc.len(), 3);

        let post = sg.dfs_post_order(a);
        assert_eq!(post.len(), 3);
    }

    #[test]
    fn test_service_graphs_cycle_detection() {
        let mut interner = Interner::new();
        let mut sg = ServiceGraphs::new();
        let a = interner.insert("a");
        let b = interner.insert("b");

        sg.reactive_graph.add_edge(a, b, ());
        sg.reactive_graph.add_edge(b, a, ());

        let topo = sg.topological_sort();
        assert!(topo.is_err());

        let scc = sg.strongly_connected_components();
        assert_eq!(scc.len(), 1);
        assert_eq!(scc[0].len(), 2);
    }
}
