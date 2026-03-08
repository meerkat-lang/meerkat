use std::{ collections::{ HashMap, HashSet }, vec };

use crate::ast;

use super::DependAnalysis;

impl DependAnalysis {
    pub fn new(ast: &ast::Service) -> DependAnalysis {
        let mut vars: HashSet<String> = HashSet::new();
        let mut defs: HashSet<String> = HashSet::new();
        let mut reactive_names = HashSet::new();
        let mut tables: HashSet<String> = HashSet::new();

        let mut dep_graph: HashMap<String, HashSet<String>> = HashMap::new();

        for decl in ast.decls.iter() {
            match decl {
                ast::Decl::VarDecl { name, .. } => {
                    vars.insert(name.clone());
                    reactive_names.insert(name.clone());
                    dep_graph.insert(name.clone(), HashSet::new());
                }
                ast::Decl::DefDecl { name, val, .. } => {
                    defs.insert(name.clone());
                    reactive_names.insert(name.clone());
                    // we calculated all reactive names so far

                    let deps = val.free_var(&reactive_names, &HashSet::new());
                    dep_graph.insert(name.clone(), deps);
                }
                ast::Decl::TableDecl { name, .. } => {
                    tables.insert(name.clone());
                    dep_graph.insert(name.clone(), HashSet::new());
                }
                _ => {}
            }
        }

        DependAnalysis {
            vars,
            defs,
            tables,
            dep_graph,
            topo_order: Vec::new(),
            dep_transtive: HashMap::new(),
            dep_vars: HashMap::new(),
        }
    }

    /// Performs a depth-first search on the dependency graph to calculate
    /// the transitive dependencies for a given variable or definition.
    /// # Arguments
    /// * `vars` - set of variable names (no dependencies).
    /// * `tables` - set of table names (no dependencies).
    /// * `visited` - set of visited nodes in dfs.
    /// * `calced` - map of def to their computed dependencies, only appeared
    ///    when finished computing for a def.
    /// # Panics
    /// * panic if a cycle is detected in the graph.
    fn dfs_helper(
        graph: &HashMap<String, HashSet<String>>,
        vars: &HashSet<String>,
        tables: &HashSet<String>,
        visited: &mut HashSet<String>,
        finished: &mut Vec<String>,
        calced: &mut HashMap<String, HashSet<String>>,
        name: &String
    ) {
        if calced.contains_key(name) {
            return;
        }

        if visited.contains(name) {
            panic!("Cycle detected in dependency graph of var and defs");
        }

        visited.insert(name.clone());
        // if visit var, notice var is transitively depend on itself
        if vars.contains(name) || tables.contains(name) {
            calced.insert(name.clone(), HashSet::from([name.clone()]));
            finished.push(name.clone());
            return;
        }

        // else visit def
        let mut dep = HashSet::new();

        for dep_name in graph.get(name).expect(&format!("No such name in dep graph: {}", name)) {
            Self::dfs_helper(graph, vars, tables, visited, finished, calced, dep_name);
            dep.extend(
                calced
                    .get(dep_name)
                    .expect(
                        &format!("Not finished transitive dependency 
                        calculation of: {}", dep_name)
                    )
                    .clone()
            );
            dep.insert(dep_name.clone());
        }

        calced.insert(name.clone(), dep);
        finished.push(name.clone());
    }

    pub fn calc_dep_vars(&mut self) {
        let mut visited = HashSet::new();

        for name in self.vars.iter().chain(self.defs.iter().chain(self.tables.iter())) {
            Self::dfs_helper(
                &self.dep_graph,
                &self.vars,
                &self.tables,
                &mut visited,
                &mut self.topo_order,
                &mut self.dep_transtive,
                name
            );
        }

        let vars_and_tables: HashSet<_> = self.vars.union(&self.tables).cloned().collect();
        for name in self.vars.iter().chain(self.defs.iter().chain(self.tables.iter())) {
            self.dep_vars.insert(
                name.clone(),
                self.dep_transtive
                    .get(name)
                    .expect(&format!("cannot find def {} in trans dep", name))
                    .intersection(&vars_and_tables)
                    .cloned()
                    .collect()
            );
        }
    }
}
