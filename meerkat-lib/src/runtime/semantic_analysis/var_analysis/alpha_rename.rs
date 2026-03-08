use std::collections::{HashMap, HashSet};

use crate::ast::Expr;

impl Expr {
    /// alpha renaming of expression e
    /// rename x1, x2, ..., x_n to y1, y2, ..., y_n if x is free in expresion e
    pub fn alpha_rename(
        &mut self,
        var_binded: &HashSet<String>,
        renames: &HashMap<String, String>,
    ) {
        match self {
            Expr::Number { .. } | Expr::Bool { .. } | Expr::String { .. } => {}
            Expr::Variable { ident } => {
                if !var_binded.contains(ident) && renames.contains_key(ident) {
                    *ident = renames.get(ident).unwrap().clone();
                }
            }
            Expr::KeyVal { value , .. } => {
                value.alpha_rename(var_binded, renames);
            }
            Expr::Vector { val } => {
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
            Expr::FuncApply { func, args } => {
                func.alpha_rename(var_binded, renames);
                for arg in args {
                    arg.alpha_rename(var_binded, renames);
                }
            }

            Expr::Action { assns, inserts} => {
                for assn in assns {
                    // dest should never be renamed, not influenced by capture
                    // let dest = &mut assn.dest;
                    // if !var_binded.contains(dest) && renames.contains_key(dest){
                    //     *dest = renames.get(dest).unwrap().clone();
                    // }
                    assn.src.alpha_rename(var_binded, renames);
                }
                for insert in inserts {
                    insert.row.alpha_rename(var_binded, renames);       // since rows can contain variables
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
            Expr::TableColumn { .. } => {},
            Expr::Fold { args } => {
                for arg in args {
                    arg.alpha_rename(var_binded, renames);
                }
            },
        
        }
    }
}
