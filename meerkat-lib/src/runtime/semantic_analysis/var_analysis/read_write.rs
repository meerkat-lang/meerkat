use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
};

use crate::ast::{Assn, Expr};

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
            Expr::Number { .. } | Expr::Bool { .. } | Expr::String { .. } | Expr::Table { .. }=> HashSet::new(),
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
            Expr::Vector { val } => {
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
            Expr::FuncApply { func, args } => {
                let mut free_vars = func.free_var(reactive_names, var_binded);
                for arg in args {
                    free_vars.extend(arg.free_var(reactive_names, var_binded));
                }
                free_vars
            }

            // x in FV(r) => x in FV(action { ..., l = r, ...}) and x not in reactive_names
            /* it's trying to model:
            1. def f = action{ x = y + z } has no dependencies in dependency graph,
               since we handle actions separately rather than propagating values of
               y, z to  f
            2. def f = fn y, z => action { x = y + z } to correctly evaluate say,
               f(1,2) to action { x = 3 }.
            */
            Expr::Action { assns, .. } => {
                let mut free_vars = HashSet::new();
                for assn in assns {
                    // dest should never be free, we do not allow such pattern
                    // fn x => action { x = ... }
                    // since each var action should be declared before in the service
                    free_vars.extend(assn.src.free_var(reactive_names, var_binded));
                }

                // we exclude reactive names from free_vars in action
                free_vars.difference(reactive_names).cloned().collect()
            }
            Expr::Select { table_name, where_clause, .. } => {
                let mut free_vars = where_clause.free_var(reactive_names, var_binded);
                free_vars.insert(table_name.clone());
                free_vars
            }
            Expr::TableColumn { table_name, .. } => {
                HashSet::from([table_name.to_string()])
            }
            Expr::Fold { args } => { 
                let mut free_vars = HashSet::new();
                free_vars.extend(args[0].free_var(reactive_names, var_binded));
                free_vars.extend(args[1].free_var(reactive_names, var_binded));
                free_vars.extend(args[2].free_var(reactive_names, var_binded));
                
                free_vars
            }
        }
    }
}

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
