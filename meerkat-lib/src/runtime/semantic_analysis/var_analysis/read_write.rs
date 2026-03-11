use core::panic;
use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
};

use crate::ast::{Expr};

impl Expr {
    /// return free variables in expr wrt var_binded, used for
    /// 1. for extracting dependency of each def declaration
    /// 2. for evaluation a expression (substitution based evaluation)
    pub fn free_var(
        &self,
        reactive_names: &HashSet<String>,
        var_binded: &HashSet<String>, // should be initialized by all reactive name declared in the service
    ) -> HashSet<String> {
        match self {
            Expr::Literal { .. } | Expr::Table { .. }=> HashSet::new(),
            Expr::Variable { ident } => {
                if var_binded.contains(ident) {
                    HashSet::new()
                } else {
                    HashSet::from([ident.clone()])
                }
            }
            Expr::KeyVal { value, .. } => {
                value.free_var(reactive_names, var_binded)
            }
            Expr::Tuple { val } => {
                let mut free_vars = HashSet::new();
                for item in val {
                    free_vars.extend(item.free_var(reactive_names, var_binded));
                }
                free_vars
            }
            Expr::Unop { op, expr } => expr.free_var(reactive_names, var_binded),
            Expr::Binop { op, expr1, expr2 } => {
                let mut free_vars = expr1.free_var(reactive_names, var_binded);
                free_vars.extend(expr2.free_var(reactive_names, var_binded));
                free_vars
            }
            Expr::If { cond, expr1, expr2 } => {
                let mut free_vars = cond.free_var(reactive_names, var_binded);
                free_vars.extend(expr1.free_var(reactive_names, var_binded));
                free_vars.extend(expr2.free_var(reactive_names, var_binded));
                free_vars
            }
            Expr::Func { params, body } => {
                let mut new_binds = var_binded.clone();
                new_binds.extend(params.iter().cloned());
                body.free_var(reactive_names, &new_binds)
            }
            Expr::Call { func, args } => {
                let mut free_vars = func.free_var(reactive_names, var_binded);
                for arg in args {
                    free_vars.extend(arg.free_var(reactive_names, var_binded));
                }
                free_vars
            }

            Expr::Action(stmts, .. ) => {
                let mut free_vars = HashSet::new();
                for stmt in stmts {
                    // TODO: implement me
                    panic!("free_var for statments is not implemented yet");
                }

                // we exclude reactive names from free_vars in action
                free_vars.difference(reactive_names).cloned().collect()
            }
            Expr::Select { table_name, where_clause, .. } => {
                let mut free_vars = where_clause.free_var(reactive_names, var_binded);
                free_vars.insert(table_name.clone());
                free_vars
            }
            Expr::Fold { operation, identity, ..} => {
                let mut free_vars = HashSet::new();
                free_vars.extend(operation.free_var(reactive_names, var_binded));
                free_vars.extend(identity.free_var(reactive_names, var_binded));
                
                free_vars
            }
        }
    }
}
/*
/// Calculate direct read set
/// used for lock acquisition
pub fn calc_read_sets(assns: &Vec<Assn>, reactive_names: &HashSet<String>) -> HashSet<String> {
    let mut direct_reads = HashSet::new();
    for assn in assns {
        direct_reads.extend(assn.src.free_var(reactive_names, &HashSet::new()));
    }

    direct_reads
}

/// calculate write set (contains var only, no transitive dependency needed)
/// used for lock acquisition
pub fn calc_write_set(assns: &Vec<Assn>) -> HashSet<String> {
    let mut writes = HashSet::new();
    for assn in assns {
        writes.insert(assn.dest.clone());
    }
    writes
}
*/