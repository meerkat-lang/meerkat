//! dependency analysis for var/def node in meerkat
//!

use std::{
    collections::{HashMap, HashSet},
    fmt::Display,
};

use crate::ast;

pub mod alpha_rename;
pub mod dep_analysis;
pub mod read_write;

pub struct DependAnalysis {
    pub vars: HashSet<String>,
    pub defs: HashSet<String>,
    pub tables: HashSet<String>,
    pub dep_graph: HashMap<String, HashSet<String>>,
    pub topo_order: Vec<String>, // topological order of vars/defs
    // transitively dependent vars/defs of a name
    pub dep_transtive: HashMap<String, HashSet<String>>,
    // transitively dependent vars of a name
    // dep_vars[name] subset of dep_transtive[name]
    pub dep_vars: HashMap<String, HashSet<String>>,
}

impl Display for DependAnalysis {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Dependency Graph \n")?;
        for (name, deps) in self.dep_graph.iter() {
            write!(f, "{} -> ", name)?;
            for dep in deps.iter() {
                write!(f, "{},", dep)?;
            }
            write!(f, "\n")?;
        }
        write!(f, "Transitive Dependency (Var only) \n")?;
        for (name, deps) in self.dep_vars.iter() {
            write!(f, "{} -> ", name)?;
            for dep in deps.iter() {
                write!(f, "{},", dep)?;
            }
            write!(f, "\n")?;
        }

        write!(f, "Topological Order \n")?;
        for name in self.topo_order.iter() {
            write!(f, "{} ", name)?;
        }
        write!(f, "\n")?;
        Ok(())
    }
}

pub fn calc_dep_srv(ast: &ast::Service) -> DependAnalysis {
    let mut da = DependAnalysis::new(ast);
    da.calc_dep_vars();
    //println!("{}", da);
    da
}

pub fn calc_dep_prog(ast: &ast::Prog) {
    for srv in ast.services.iter() {
        let da = calc_dep_srv(srv);
        println!("{}", da);
    }
}
