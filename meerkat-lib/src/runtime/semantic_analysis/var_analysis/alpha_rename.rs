use core::panic;
use std::collections::{HashMap, HashSet};

use crate::ast::{Expr, ActionStmt};

impl Expr {
    /// alpha renaming of expression e
    /// rename x1, x2, ..., x_n to y1, y2, ..., y_n if x is free in expresion e
    pub fn alpha_rename(
        &mut self,
        var_binded: &HashSet<String>,
        renames: &HashMap<String, String>,
    ) {
        match self {
            Expr::Literal { .. } => {}
            Expr::Variable { ident } => {
                if !var_binded.contains(ident) && renames.contains_key(ident) {
                    *ident = renames.get(ident).unwrap().clone();
                }
            }
            Expr::KeyVal { value , .. } => {
                value.alpha_rename(var_binded, renames);
            }
            Expr::Tuple { val } => {
                for item in val {
                    item.alpha_rename(var_binded, renames);
                }
            }
            Expr::Unop { op, expr } => {
                expr.alpha_rename(var_binded, renames);
            }
            Expr::Binop { op, expr1, expr2 } => {
                expr1.alpha_rename(var_binded, renames);
                expr2.alpha_rename(var_binded, renames);
            }
            Expr::If { cond, expr1, expr2 } => {
                cond.alpha_rename(var_binded, renames);
                expr1.alpha_rename(var_binded, renames);
                expr2.alpha_rename(var_binded, renames);
            }
            Expr::Func { params, body } => {
                let mut new_binds = var_binded.clone();
                new_binds.extend(params.iter().cloned());
                body.alpha_rename(&new_binds, renames);
            }
            Expr::Call { func, args } => {
                func.alpha_rename(var_binded, renames);
                for arg in args {
                    arg.alpha_rename(var_binded, renames);
                }
            }

            Expr::Action(stmts) => {
                for stmt in stmts {
                    // dest should never be renamed, not influenced by capture
                    // let dest = &mut assn.dest;
                    // if !var_binded.contains(dest) && renames.contains_key(dest){
                    //     *dest = renames.get(dest).unwrap().clone();
                    // }
                    stmt.alpha_rename(var_binded, renames);
                }
            }
            Expr::Select { where_clause, .. } => {
                where_clause.alpha_rename(var_binded, renames);
            }
            Expr::Table { records, .. } => {
                for record in records {
                    record.alpha_rename(var_binded, renames);
                }
            }
            Expr::Fold { table_name, column_name, operation, identity } => {
                operation.alpha_rename(var_binded, renames);
                identity.alpha_rename(var_binded, renames);
            },
        
        }
    }
}

impl ActionStmt {
    pub fn alpha_rename(
        &mut self,
        var_binded: &HashSet<String>,
        renames: &HashMap<String, String>,
    ) {
        // TODO: Implement alpha renaming for ActionStmt based on its structure
        // This is a placeholder - adjust based on ActionStmt's actual fields
        panic!("alpha_rename for ActionStmt is not implemented yet");
    }
}
