//! Free variable and cross-service dependency extraction services using `petgraph`

use crate::ast::{ActionStmt, Expr};
use crate::runtime::interner::Symbol;
use petgraph::graphmap::DiGraphMap;
use std::collections::HashSet;

/// Extract all free variables from an expression into a `petgraph` `DiGraphMap`
///
/// Args:
///   `expr` (`&Expr`): The expression AST node
///   `var_binded` (`&HashSet<Symbol>`): Set of bound local variables
///
/// Returns:
///   `DiGraphMap<Symbol, ()>`: Directed graph of free variable nodes
pub fn free_var_graph(expr: &Expr, var_binded: &HashSet<Symbol>) -> DiGraphMap<Symbol, ()> {
    let mut graph = DiGraphMap::new();
    populate_free_var_graph(expr, var_binded, &mut graph);
    graph
}

/// Extract all free variables from an expression with respect to bound variables
///
/// Args:
///   `expr` (`&Expr`): The expression AST node
///   `var_binded` (`&HashSet<Symbol>`): Set of bound local variables
///
/// Returns:
///   `HashSet<Symbol>`: Set of free variable symbols
pub fn free_var(expr: &Expr, var_binded: &HashSet<Symbol>) -> HashSet<Symbol> {
    let graph = free_var_graph(expr, var_binded);
    graph.nodes().collect()
}

/// Recursively populate a `petgraph` `DiGraphMap` with free variable symbols
///
/// Args:
///   `expr` (`&Expr`): The expression AST node
///   `var_binded` (`&HashSet<Symbol>`): Set of bound local variables
///   `graph` (`&mut DiGraphMap<Symbol, ()>`): Output `petgraph` instance
fn populate_free_var_graph(
    expr: &Expr,
    var_binded: &HashSet<Symbol>,
    graph: &mut DiGraphMap<Symbol, ()>,
) {
    match expr {
        Expr::Literal { .. } | Expr::Table { .. } | Expr::MemberAccess { .. } => {}
        Expr::Variable { name } => {
            if !var_binded.contains(name) {
                graph.add_node(*name);
            }
        }
        Expr::KeyVal { value, .. } => {
            populate_free_var_graph(value, var_binded, graph);
        }
        Expr::Tuple { val } => {
            for item in val {
                populate_free_var_graph(item, var_binded, graph);
            }
        }
        Expr::Unop { expr, .. } => {
            populate_free_var_graph(expr, var_binded, graph);
        }
        Expr::Binop { expr1, expr2, .. } => {
            populate_free_var_graph(expr1, var_binded, graph);
            populate_free_var_graph(expr2, var_binded, graph);
        }
        Expr::If { cond, expr1, expr2 } => {
            populate_free_var_graph(cond, var_binded, graph);
            populate_free_var_graph(expr1, var_binded, graph);
            populate_free_var_graph(expr2, var_binded, graph);
        }
        Expr::Func { params, body, .. } => {
            let mut new_binds = var_binded.clone();
            new_binds.extend(params.iter().map(|p| p.name));
            populate_free_var_graph(body, &new_binds, graph);
        }
        Expr::Html(template) => {
            for e in template.embedded_exprs() {
                populate_free_var_graph(e, var_binded, graph);
            }
        }
        Expr::Call { func, args } => {
            populate_free_var_graph(func, var_binded, graph);
            for arg in args {
                populate_free_var_graph(arg, var_binded, graph);
            }
        }
        Expr::Action(stmts) => {
            let mut action_binds = var_binded.clone();
            for stmt in stmts {
                action_binds = populate_free_var_graph_action_stmt(stmt, &action_binds, graph);
            }
        }
        Expr::Select {
            table_name,
            where_clause,
            ..
        } => {
            populate_free_var_graph(where_clause, var_binded, graph);
            if !var_binded.contains(table_name) {
                graph.add_node(*table_name);
            }
        }
        Expr::Fold {
            operation,
            identity,
            ..
        } => {
            populate_free_var_graph(operation, var_binded, graph);
            populate_free_var_graph(identity, var_binded, graph);
        }
        Expr::List(val) => {
            for item in val {
                populate_free_var_graph(item, var_binded, graph);
            }
        }
        Expr::Range { start, end } => {
            populate_free_var_graph(start, var_binded, graph);
            populate_free_var_graph(end, var_binded, graph);
        }
    }
}

/// Helper function to populate free variable graph from an `ActionStmt` AST node
///
/// Args:
///   `stmt` (`&ActionStmt`): The action statement AST node
///   `var_binded` (`&HashSet<Symbol>`): Set of bound local variables
///   `graph` (`&mut DiGraphMap<Symbol, ()>`): Output `petgraph` instance
///
/// Returns:
///   `HashSet<Symbol>`: Updated set of bound variables
fn populate_free_var_graph_action_stmt(
    stmt: &ActionStmt,
    var_binded: &HashSet<Symbol>,
    graph: &mut DiGraphMap<Symbol, ()>,
) -> HashSet<Symbol> {
    match stmt {
        ActionStmt::Assign { expr, .. }
        | ActionStmt::Do(expr)
        | ActionStmt::Assert(expr, _)
        | ActionStmt::Let { expr, .. }
        | ActionStmt::Expr(expr) => {
            populate_free_var_graph(expr, var_binded, graph);
        }
        ActionStmt::Insert { row, .. } => {
            populate_free_var_graph(row, var_binded, graph);
        }
        ActionStmt::For {
            var,
            iterable,
            body,
        } => {
            populate_free_var_graph(iterable, var_binded, graph);
            let mut body_binds = var_binded.clone();
            body_binds.insert(*var);
            for s in body {
                body_binds = populate_free_var_graph_action_stmt(s, &body_binds, graph);
            }
        }
    }

    let mut new_binds = var_binded.clone();
    if let ActionStmt::Let { name, .. } = stmt {
        new_binds.insert(*name);
    }
    new_binds
}

/// Extract all cross-service dependency pairs `(service, member)` from an expression
///
/// Args:
///   `expr` (`&Expr`): The expression AST node
///
/// Returns:
///   `HashSet<(Symbol, Symbol)>`: Set of `(service, member)` pairs
pub fn cross_service_deps(expr: &Expr) -> HashSet<(Symbol, Symbol)> {
    match expr {
        Expr::Literal { .. } | Expr::Variable { .. } | Expr::Table { .. } => HashSet::new(),
        Expr::MemberAccess {
            service_name,
            member_name,
        } => HashSet::from([(*service_name, *member_name)]),
        Expr::KeyVal { value, .. } => cross_service_deps(value),
        Expr::Tuple { val } => {
            let mut deps = HashSet::new();
            for item in val {
                deps.extend(cross_service_deps(item));
            }
            deps
        }
        Expr::Unop { expr, .. } => cross_service_deps(expr),
        Expr::Binop { expr1, expr2, .. } => {
            let mut deps = cross_service_deps(expr1);
            deps.extend(cross_service_deps(expr2));
            deps
        }
        Expr::If { cond, expr1, expr2 } => {
            let mut deps = cross_service_deps(cond);
            deps.extend(cross_service_deps(expr1));
            deps.extend(cross_service_deps(expr2));
            deps
        }
        Expr::Func { body, .. } => cross_service_deps(body),
        Expr::Html(template) => {
            let mut deps = HashSet::new();
            for e in template.embedded_exprs() {
                deps.extend(cross_service_deps(e));
            }
            deps
        }
        Expr::Call { func, args } => {
            let mut deps = cross_service_deps(func);
            for arg in args {
                deps.extend(cross_service_deps(arg));
            }
            deps
        }
        Expr::Action(stmts) => {
            let mut deps = HashSet::new();
            for stmt in stmts {
                deps.extend(cross_service_deps_in_action_stmt(stmt));
            }
            deps
        }
        Expr::Select { where_clause, .. } => cross_service_deps(where_clause),
        Expr::Fold {
            operation,
            identity,
            ..
        } => {
            let mut deps = cross_service_deps(operation);
            deps.extend(cross_service_deps(identity));
            deps
        }
        Expr::List(exprs) => {
            let mut deps = HashSet::new();
            for expr in exprs {
                deps.extend(cross_service_deps(expr));
            }
            deps
        }
        Expr::Range { start, end } => {
            let mut deps = cross_service_deps(start);
            deps.extend(cross_service_deps(end));
            deps
        }
    }
}

/// Helper function to compute cross-service dependencies inside an `ActionStmt`
///
/// Args:
///   `stmt` (`&ActionStmt`): The action statement AST node
///
/// Returns:
///   `HashSet<(Symbol, Symbol)>`: Set of `(service, member)` pairs
fn cross_service_deps_in_action_stmt(stmt: &ActionStmt) -> HashSet<(Symbol, Symbol)> {
    match stmt {
        ActionStmt::Let { expr, .. } => cross_service_deps(expr),
        ActionStmt::Expr(expr) => cross_service_deps(expr),
        ActionStmt::Do(expr) => cross_service_deps(expr),
        ActionStmt::Assert(expr, _) => cross_service_deps(expr),
        ActionStmt::Assign { expr, .. } => cross_service_deps(expr),
        ActionStmt::Insert { row, .. } => cross_service_deps(row),
        ActionStmt::For { iterable, body, .. } => {
            let mut deps = cross_service_deps(iterable);
            for s in body {
                deps.extend(cross_service_deps_in_action_stmt(s));
            }
            deps
        }
    }
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

    #[test]
    fn test_cross_service_deps_all_exprs_and_stmts() {
        let mut interner = Interner::new();
        let s1 = interner.insert("s1");
        let m1 = interner.insert("m1");
        let tbl = interner.insert("tbl");
        let i = interner.insert("i");

        let ma = Expr::MemberAccess {
            service_name: s1,
            member_name: m1,
        };
        let deps = cross_service_deps(&ma);
        assert_eq!(deps.len(), 1);
        assert!(deps.contains(&(s1, m1)));

        let tuple_ma = Expr::Tuple {
            val: vec![ma.clone()],
        };
        assert_eq!(cross_service_deps(&tuple_ma).len(), 1);

        let unop_ma = Expr::Unop {
            op: UnOp::Neg,
            expr: Box::new(ma.clone()),
        };
        assert_eq!(cross_service_deps(&unop_ma).len(), 1);

        let binop_ma = Expr::Binop {
            op: BinOp::Add,
            expr1: Box::new(ma.clone()),
            expr2: Box::new(Expr::Literal {
                val: Value::Int { val: 1 },
            }),
        };
        assert_eq!(cross_service_deps(&binop_ma).len(), 1);

        let if_ma = Expr::If {
            cond: Box::new(ma.clone()),
            expr1: Box::new(ma.clone()),
            expr2: Box::new(ma.clone()),
        };
        assert_eq!(cross_service_deps(&if_ma).len(), 1);

        let func_ma = Expr::Func {
            params: vec![],
            body: Box::new(ma.clone()),
            return_ty: None,
        };
        assert_eq!(cross_service_deps(&func_ma).len(), 1);

        let mut builder2 = HtmlTemplateBuilder::new();
        builder2.push_text("test");
        let html_ma = Expr::Html(builder2.build());
        assert!(cross_service_deps(&html_ma).is_empty());

        let call_ma = Expr::Call {
            func: Box::new(ma.clone()),
            args: vec![ma.clone()],
        };
        assert_eq!(cross_service_deps(&call_ma).len(), 1);

        let select_ma = Expr::Select {
            table_name: tbl,
            where_clause: Box::new(ma.clone()),
            column_names: vec![],
        };
        assert_eq!(cross_service_deps(&select_ma).len(), 1);

        let fold_ma = Expr::Fold {
            table_name: tbl,
            column_name: i,
            operation: Box::new(ma.clone()),
            identity: Box::new(ma.clone()),
        };
        assert_eq!(cross_service_deps(&fold_ma).len(), 1);

        let list_ma = Expr::List(vec![ma.clone()]);
        assert_eq!(cross_service_deps(&list_ma).len(), 1);

        let range_ma = Expr::Range {
            start: Box::new(ma.clone()),
            end: Box::new(ma.clone()),
        };
        assert_eq!(cross_service_deps(&range_ma).len(), 1);

        let action_ma = Expr::Action(vec![
            ActionStmt::Let {
                name: i,
                ty: None,
                expr: ma.clone(),
            },
            ActionStmt::Assign {
                name: i,
                expr: ma.clone(),
            },
            ActionStmt::Do(ma.clone()),
            ActionStmt::Assert(ma.clone(), "assert".to_string()),
            ActionStmt::Expr(ma.clone()),
            ActionStmt::Insert {
                table_name: tbl,
                row: ma.clone(),
            },
            ActionStmt::For {
                var: i,
                iterable: ma.clone(),
                body: vec![ActionStmt::Expr(ma.clone())],
            },
        ]);
        assert_eq!(cross_service_deps(&action_ma).len(), 1);
    }
}
