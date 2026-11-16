//! Free variable extraction

use crate::ast::{ActionStmt, Expr};
use crate::runtime::interner::Symbol;
use std::collections::HashSet;

/// Extract all free variables from an expression with respect to bound variables
///
/// Args:
///   `expr` (`&Expr`): The expression AST node
///   `var_binded` (`&HashSet<Symbol>`): Set of bound local variables
///
/// Returns:
///   `HashSet<Symbol>`: Set of free variable symbols
pub fn free_var(expr: &Expr, var_binded: &HashSet<Symbol>) -> HashSet<Symbol> {
    let mut free = HashSet::new();
    collect_free_vars(expr, var_binded, &mut free);
    free
}

/// Add the free variables of `expr` to `free`
fn collect_free_vars(expr: &Expr, var_binded: &HashSet<Symbol>, free: &mut HashSet<Symbol>) {
    match expr {
        Expr::Variable { name } => {
            if !var_binded.contains(name) {
                free.insert(*name);
            }
        }
        Expr::Func { params, body, .. } => {
            let mut new_binds = var_binded.clone();
            new_binds.extend(params.iter().map(|p| p.name));
            collect_free_vars(body, &new_binds, free);
        }
        Expr::Action(stmts) => {
            let mut action_binds = var_binded.clone();
            for stmt in stmts {
                action_binds = collect_free_vars_action_stmt(stmt, &action_binds, free);
            }
        }
        Expr::Select {
            table_name,
            where_clause,
            ..
        } => {
            collect_free_vars(where_clause, var_binded, free);
            if !var_binded.contains(table_name) {
                free.insert(*table_name);
            }
        }
        // A table literal holds data, not code to evaluate
        Expr::Table { .. } => {}
        _ => {
            for child in expr.children() {
                collect_free_vars(child, var_binded, free);
            }
        }
    }
}

/// Add the free variables of an action statement to `free`, returning the
/// variables bound after it
fn collect_free_vars_action_stmt(
    stmt: &ActionStmt,
    var_binded: &HashSet<Symbol>,
    free: &mut HashSet<Symbol>,
) -> HashSet<Symbol> {
    match stmt {
        ActionStmt::Assign { expr, .. }
        | ActionStmt::Do(expr)
        | ActionStmt::Assert(expr, _)
        | ActionStmt::Let { expr, .. }
        | ActionStmt::Expr(expr) => {
            collect_free_vars(expr, var_binded, free);
        }
        ActionStmt::Insert { row, .. } => {
            collect_free_vars(row, var_binded, free);
        }
        ActionStmt::For {
            var,
            iterable,
            body,
        } => {
            collect_free_vars(iterable, var_binded, free);
            let mut body_binds = var_binded.clone();
            body_binds.insert(*var);
            for s in body {
                body_binds = collect_free_vars_action_stmt(s, &body_binds, free);
            }
        }
    }

    let mut new_binds = var_binded.clone();
    if let ActionStmt::Let { name, .. } = stmt {
        new_binds.insert(*name);
    }
    new_binds
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{BinOp, UnOp, Value};
    use crate::runtime::html::HtmlTemplateBuilder;
    use crate::runtime::interner::Interner;
    use crate::runtime::tt::Param;

    #[test]
    fn test_free_var_basic_literals_and_vars() {
        let mut interner = Interner::new();
        let x = interner.insert("x");
        let y = interner.insert("y");

        let lit = Expr::Literal {
            val: Value::Int { val: 42 },
        };
        assert!(free_var(&lit, &HashSet::new()).is_empty());

        let var_x = Expr::Variable { name: x };
        let free_vars = free_var(&var_x, &HashSet::new());
        assert_eq!(free_vars.len(), 1);
        assert!(free_vars.contains(&x));

        let bound_vars = HashSet::from([x]);
        assert!(free_var(&var_x, &bound_vars).is_empty());

        let kv = Expr::KeyVal {
            name: x,
            value: Box::new(Expr::Variable { name: y }),
        };
        assert!(free_var(&kv, &HashSet::new()).contains(&y));
    }

    #[test]
    fn test_free_var_unop_binop_if_tuple_list_range() {
        let mut interner = Interner::new();
        let a = interner.insert("a");
        let b = interner.insert("b");
        let c = interner.insert("c");

        let unop = Expr::Unop {
            op: UnOp::Neg,
            expr: Box::new(Expr::Variable { name: a }),
        };
        assert!(free_var(&unop, &HashSet::new()).contains(&a));

        let binop = Expr::Binop {
            op: BinOp::Add,
            expr1: Box::new(Expr::Variable { name: a }),
            expr2: Box::new(Expr::Variable { name: b }),
        };
        let bin_vars = free_var(&binop, &HashSet::new());
        assert_eq!(bin_vars.len(), 2);

        let if_expr = Expr::If {
            cond: Box::new(Expr::Variable { name: a }),
            expr1: Box::new(Expr::Variable { name: b }),
            expr2: Box::new(Expr::Variable { name: c }),
        };
        let if_vars = free_var(&if_expr, &HashSet::new());
        assert_eq!(if_vars.len(), 3);

        let tuple_expr = Expr::Tuple {
            val: vec![Expr::Variable { name: a }, Expr::Variable { name: b }],
        };
        assert_eq!(free_var(&tuple_expr, &HashSet::new()).len(), 2);

        let list_expr = Expr::List(vec![Expr::Variable { name: a }]);
        assert_eq!(free_var(&list_expr, &HashSet::new()).len(), 1);

        let range_expr = Expr::Range {
            start: Box::new(Expr::Variable { name: a }),
            end: Box::new(Expr::Variable { name: b }),
        };
        assert_eq!(free_var(&range_expr, &HashSet::new()).len(), 2);
    }

    #[test]
    fn test_free_var_func_call_html_select_fold() {
        let mut interner = Interner::new();
        let p = interner.insert("p");
        let x = interner.insert("x");
        let tbl = interner.insert("tbl");

        let func_expr = Expr::Func {
            params: vec![Param { name: p, ty: None }],
            body: Box::new(Expr::Binop {
                op: BinOp::Add,
                expr1: Box::new(Expr::Variable { name: p }),
                expr2: Box::new(Expr::Variable { name: x }),
            }),
            return_ty: None,
        };
        let func_vars = free_var(&func_expr, &HashSet::new());
        assert_eq!(func_vars.len(), 1);
        assert!(func_vars.contains(&x));
        assert!(!func_vars.contains(&p));

        let call_expr = Expr::Call {
            func: Box::new(func_expr),
            args: vec![Expr::Variable { name: x }],
        };
        assert_eq!(free_var(&call_expr, &HashSet::new()).len(), 1);

        let mut builder = HtmlTemplateBuilder::new();
        builder.push_text("hello");
        let html_expr = Expr::Html(builder.build());
        assert!(free_var(&html_expr, &HashSet::new()).is_empty());

        let select_expr = Expr::Select {
            table_name: tbl,
            where_clause: Box::new(Expr::Variable { name: x }),
            column_names: vec![],
        };
        let sel_vars = free_var(&select_expr, &HashSet::new());
        assert!(sel_vars.contains(&tbl));
        assert!(sel_vars.contains(&x));

        let fold_expr = Expr::Fold {
            table_name: tbl,
            column_name: x,
            operation: Box::new(Expr::Variable { name: x }),
            identity: Box::new(Expr::Literal {
                val: Value::Int { val: 0 },
            }),
        };
        assert!(free_var(&fold_expr, &HashSet::new()).contains(&x));
    }

    #[test]
    fn test_free_var_action_statements() {
        let mut interner = Interner::new();
        let a = interner.insert("a");
        let b = interner.insert("b");
        let i = interner.insert("i");
        let tbl = interner.insert("tbl");

        let action_expr = Expr::Action(vec![
            ActionStmt::Let {
                name: a,
                ty: None,
                expr: Expr::Variable { name: b },
            },
            ActionStmt::Assign {
                name: a,
                expr: Expr::Variable { name: a },
            },
            ActionStmt::Do(Expr::Variable { name: a }),
            ActionStmt::Assert(Expr::Variable { name: a }, "assert".to_string()),
            ActionStmt::Expr(Expr::Variable { name: a }),
            ActionStmt::Insert {
                table_name: tbl,
                row: Expr::Variable { name: b },
            },
            ActionStmt::For {
                var: i,
                iterable: Expr::Variable { name: b },
                body: vec![ActionStmt::Expr(Expr::Variable { name: i })],
            },
        ]);

        let act_vars = free_var(&action_expr, &HashSet::new());
        assert!(act_vars.contains(&b));
        assert!(!act_vars.contains(&i));
    }
}
