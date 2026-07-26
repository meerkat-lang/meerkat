//! Dependency analysis algorithms and eager validation passes using `petgraph`

use super::ServiceGraphs;
use crate::ast::{Decl, Expr, Stmt};
use crate::error::{Error, Result};
use crate::runtime::interner::Symbol;
use petgraph::graphmap::DiGraphMap;
use petgraph::visit::Dfs;
use std::collections::{HashMap, HashSet};

/// Context parameters passed during eager dependency graph construction and validation
struct EagerCtx<'a> {
    declared_symbols: &'a HashSet<Symbol>,
    initialized_symbols: &'a HashSet<Symbol>,
    def_bodies: &'a HashMap<Symbol, Expr>,
    global_def_bodies: &'a HashMap<(Symbol, Symbol), Expr>,
    global_member_order: &'a HashMap<Symbol, Vec<Symbol>>,
}

/// Perform dependency analysis on an entire program sequence of service statements using `petgraph`
///
/// Args:
///   `program` (`&[Stmt]`): Program AST containing service declarations
///
/// Returns:
///   `Result<Vec<ServiceGraphs>>`: Computed dependency graphs for all services
///
/// Errors:
///   `Error`: If an invalid eager forward reference is detected within or across services
pub fn compute_dependencies(program: &[Stmt]) -> Result<Vec<ServiceGraphs>> {
    let mut global_def_bodies = HashMap::new();
    let mut global_member_order = HashMap::new();

    for stmt in program {
        if let Stmt::Service {
            name: svc_name,
            decls,
        } = stmt
        {
            let mut member_list = Vec::new();
            for decl in decls {
                match decl {
                    Decl::VarDecl { name, .. } => {
                        member_list.push(*name);
                    }
                    Decl::DefDecl { name, val, .. } => {
                        member_list.push(*name);
                        global_def_bodies.insert((*svc_name, *name), val.clone());
                    }
                    Decl::TableDecl { name, .. } => {
                        member_list.push(*name);
                    }
                }
            }
            global_member_order.insert(*svc_name, member_list);
        }
    }

    let mut service_graphs = Vec::new();
    for stmt in program {
        if let Stmt::Service { name: _, decls } = stmt {
            let sg =
                analyze_dependencies_with_ctx(decls, &global_def_bodies, &global_member_order)?;
            service_graphs.push(sg);
        }
    }

    Ok(service_graphs)
}

/// Perform dependency analysis on a sequence of service declarations using `petgraph`
///
/// Args:
///   `decls` (`&[Decl]`): The service declarations to analyze
///
/// Returns:
///   `Result<ServiceGraphs>`: Computed dependency graphs or a static check error
///
/// Errors:
///   `Error`: If an invalid eager forward reference is detected
pub fn analyze_dependencies(decls: &[Decl]) -> Result<ServiceGraphs> {
    analyze_dependencies_with_ctx(decls, &HashMap::new(), &HashMap::new())
}

/// Perform dependency analysis on declarations with global cross-service context
///
/// Args:
///   `decls` (`&[Decl]`): Declarations of the current service
///   `global_def_bodies` (`&HashMap<(Symbol, Symbol), Expr>`): Map of all known def bodies across services
///   `global_member_order` (`&HashMap<Symbol, Vec<Symbol>>`): Map of member declaration order per service
///
/// Returns:
///   `Result<ServiceGraphs>`: Computed dependency graph
pub fn analyze_dependencies_with_ctx(
    decls: &[Decl],
    global_def_bodies: &HashMap<(Symbol, Symbol), Expr>,
    global_member_order: &HashMap<Symbol, Vec<Symbol>>,
) -> Result<ServiceGraphs> {
    if decls.is_empty() {
        return Ok(ServiceGraphs::new());
    }

    let mut graphs = ServiceGraphs::new();
    let mut declared_symbols = HashSet::new();
    let mut initialized_symbols = HashSet::new();
    let mut def_bodies: HashMap<Symbol, Expr> = HashMap::new();

    // First pass: collect all declared service members
    for decl in decls.iter() {
        match decl {
            Decl::VarDecl { name, .. } => {
                declared_symbols.insert(*name);
                graphs.vars.insert(*name);
                graphs.reactive_graph.add_node(*name);
            }
            Decl::DefDecl { name, val, .. } => {
                declared_symbols.insert(*name);
                graphs.defs.insert(*name);
                graphs.reactive_graph.add_node(*name);
                def_bodies.insert(*name, val.clone());
            }
            Decl::TableDecl { name, .. } => {
                declared_symbols.insert(*name);
                graphs.tables.insert(*name);
                graphs.reactive_graph.add_node(*name);
            }
        }
    }

    // Second pass: process declarations in textual order to validate
    // eager initialization and build reactive dependency graph
    for decl in decls.iter() {
        let eager_ctx = EagerCtx {
            declared_symbols: &declared_symbols,
            initialized_symbols: &initialized_symbols,
            def_bodies: &def_bodies,
            global_def_bodies,
            global_member_order,
        };

        match decl {
            Decl::VarDecl { name, val, .. } => {
                validate_eager_and_build_deps(*name, val, &eager_ctx, &mut graphs)?;
                initialized_symbols.insert(*name);
            }
            Decl::DefDecl { name, val, .. } => {
                validate_eager_and_build_deps(*name, val, &eager_ctx, &mut graphs)?;
                initialized_symbols.insert(*name);
            }
            Decl::TableDecl { name, .. } => {
                initialized_symbols.insert(*name);
            }
        }
    }

    Ok(graphs)
}

/// Helper function to validate eager forward references and construct dependency edges
fn validate_eager_and_build_deps(
    decl_name: Symbol,
    expr: &Expr,
    ctx: &EagerCtx<'_>,
    graphs: &mut ServiceGraphs,
) -> Result<()> {
    debug_assert!(
        ctx.declared_symbols.contains(&decl_name),
        "decl_name must be in declared_symbols"
    );

    // 1. Extract cross-service dependencies
    let cross_deps = super::free_var::cross_service_deps(expr);
    if !cross_deps.is_empty() {
        graphs.cross_deps.insert(decl_name, cross_deps);
    }

    // 2. Extract all reactive dependencies for REBLS updates
    let all_deps = super::free_var::free_var(expr, &HashSet::new());
    for dep in &all_deps {
        if ctx.declared_symbols.contains(dep) {
            graphs.reactive_graph.add_edge(*dep, decl_name, ());
        }
    }

    // 3. Construct a local eager dependency graph for this expression using petgraph
    let mut eager_graph = DiGraphMap::new();
    eager_graph.add_node(decl_name);

    let is_closure_definition = matches!(expr, Expr::Func { .. });
    if !is_closure_definition {
        build_eager_call_graph(expr, decl_name, ctx, &mut eager_graph)?;
    }

    // Use petgraph Dfs traversal to find all eagerly reachable nodes
    let mut dfs = Dfs::new(&eager_graph, decl_name);
    while let Some(node) = dfs.next(&eager_graph) {
        if node != decl_name
            && ctx.declared_symbols.contains(&node)
            && !ctx.initialized_symbols.contains(&node)
        {
            return Err(Error::Message(format!(
                "Invalid forward reference to uninitialized value '{}'",
                node
            )));
        }
    }

    Ok(())
}

/// Recursively build the eager call graph using `petgraph` `DiGraphMap`
fn build_eager_call_graph(
    expr: &Expr,
    parent_node: Symbol,
    ctx: &EagerCtx<'_>,
    eager_graph: &mut DiGraphMap<Symbol, ()>,
) -> Result<()> {
    match expr {
        Expr::Call { func: _, args: _ } => {
            let mut call_chain = Vec::new();
            let mut current = expr;
            while let Expr::Call { func, args } = current {
                call_chain.push(args);
                current = func.as_ref();
            }

            let mut all_call_args = Vec::new();
            for level_args in &call_chain {
                for arg in level_args.iter() {
                    all_call_args.push(arg);
                    build_eager_call_graph(arg, parent_node, ctx, eager_graph)?;
                }
            }

            let root_body = match current {
                Expr::Variable { name } => {
                    eager_graph.add_edge(parent_node, *name, ());
                    ctx.def_bodies.get(name).cloned()
                }
                Expr::Func { .. } => Some(current.clone()),
                Expr::MemberAccess {
                    service_name,
                    member_name,
                } => {
                    eager_graph.add_edge(parent_node, *member_name, ());
                    if let Some(target_body) =
                        ctx.global_def_bodies.get(&(*service_name, *member_name))
                    {
                        check_remote_member_eager_validity(
                            *service_name,
                            *member_name,
                            target_body,
                            ctx.global_member_order,
                        )?;
                        Some(target_body.clone())
                    } else {
                        None
                    }
                }
                _ => None,
            };

            if let Some(mut body_expr) = root_body {
                for _level_args in call_chain.iter().rev() {
                    match body_expr {
                        Expr::Func { body, params, .. } => {
                            if !matches!(body.as_ref(), Expr::Action(..)) {
                                let bound_params: HashSet<Symbol> =
                                    params.iter().map(|p| p.name).collect();
                                let func_deps = super::free_var::free_var(&body, &bound_params);
                                for dep in func_deps {
                                    eager_graph.add_edge(parent_node, dep, ());
                                }
                            }
                            body_expr = *body;
                        }
                        _ => {
                            build_eager_call_graph(&body_expr, parent_node, ctx, eager_graph)?;
                            break;
                        }
                    }
                }

                // If the final body_expr after peeling all call chain levels is NOT a closure/action,
                // all call levels were discharged! Expand arguments passed across all levels.
                if !matches!(body_expr, Expr::Func { .. } | Expr::Action(..)) {
                    for arg in all_call_args {
                        expand_eager_argument(arg, parent_node, ctx, eager_graph)?;
                    }
                }
            }
        }
        Expr::If { cond, expr1, expr2 } => {
            build_eager_call_graph(cond, parent_node, ctx, eager_graph)?;
            build_eager_call_graph(expr1, parent_node, ctx, eager_graph)?;
            build_eager_call_graph(expr2, parent_node, ctx, eager_graph)?;
        }
        Expr::Binop { expr1, expr2, .. } => {
            build_eager_call_graph(expr1, parent_node, ctx, eager_graph)?;
            build_eager_call_graph(expr2, parent_node, ctx, eager_graph)?;
        }
        Expr::Unop { expr, .. } => {
            build_eager_call_graph(expr, parent_node, ctx, eager_graph)?;
        }
        Expr::KeyVal { value, .. } => {
            build_eager_call_graph(value, parent_node, ctx, eager_graph)?;
        }
        Expr::Tuple { val } => {
            for item in val {
                build_eager_call_graph(item, parent_node, ctx, eager_graph)?;
            }
        }
        Expr::List(exprs) => {
            for e in exprs {
                build_eager_call_graph(e, parent_node, ctx, eager_graph)?;
            }
        }
        Expr::Range { start, end } => {
            build_eager_call_graph(start, parent_node, ctx, eager_graph)?;
            build_eager_call_graph(end, parent_node, ctx, eager_graph)?;
        }
        Expr::Variable { name } => {
            eager_graph.add_edge(parent_node, *name, ());
        }
        _ => {}
    }
    Ok(())
}

/// Helper function to check whether executing a remote service member eagerly
/// accesses any uninitialized forward reference in that remote service.
fn check_remote_member_eager_validity(
    service_name: Symbol,
    member_name: Symbol,
    target_body: &Expr,
    global_member_order: &HashMap<Symbol, Vec<Symbol>>,
) -> Result<()> {
    if let Some(members) = global_member_order.get(&service_name) {
        let mut initialized = HashSet::new();
        for mem in members {
            if *mem == member_name {
                break;
            }
            initialized.insert(*mem);
        }

        let mut body_to_check = target_body;
        while let Expr::Func { body, .. } = body_to_check {
            body_to_check = body.as_ref();
        }

        let free_deps = super::free_var::free_var(body_to_check, &HashSet::new());
        for dep in free_deps {
            if members.contains(&dep) && !initialized.contains(&dep) {
                return Err(Error::Message(format!(
                    "Invalid forward reference to uninitialized value '{}'",
                    dep
                )));
            }
        }
    }
    Ok(())
}

/// Helper function to expand arguments executed during closure invocation
fn expand_eager_argument(
    arg: &Expr,
    parent_node: Symbol,
    ctx: &EagerCtx<'_>,
    eager_graph: &mut DiGraphMap<Symbol, ()>,
) -> Result<()> {
    match arg {
        Expr::Func { body, params, .. } => {
            if !matches!(body.as_ref(), Expr::Action(..)) {
                let arg_bound: HashSet<Symbol> = params.iter().map(|p| p.name).collect();
                let arg_deps = super::free_var::free_var(body, &arg_bound);
                for dep in arg_deps {
                    eager_graph.add_edge(parent_node, dep, ());
                }
            }
        }
        Expr::MemberAccess {
            service_name,
            member_name,
        } => {
            eager_graph.add_edge(parent_node, *member_name, ());
            if let Some(target_body) = ctx.global_def_bodies.get(&(*service_name, *member_name)) {
                check_remote_member_eager_validity(
                    *service_name,
                    *member_name,
                    target_body,
                    ctx.global_member_order,
                )?;
            }
        }
        Expr::Variable { name } => {
            eager_graph.add_edge(parent_node, *name, ());
            if let Some(arg_def_body) = ctx.def_bodies.get(name) {
                expand_eager_argument(arg_def_body, parent_node, ctx, eager_graph)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{BinOp, UnOp, Value};
    use crate::runtime::interner::Interner;
    use crate::runtime::tt::Param;

    #[test]
    fn test_analyze_dependencies_empty_and_tables() {
        assert!(analyze_dependencies(&[]).is_ok());

        let mut interner = Interner::new();
        let tbl = interner.insert("tbl");

        let decls = vec![Decl::TableDecl {
            name: tbl,
            fields: vec![],
        }];

        let res = analyze_dependencies(&decls);
        assert!(res.is_ok());
        let sg = res.unwrap();
        assert!(sg.tables.contains(&tbl));
    }

    #[test]
    fn test_analyze_dependencies_valid_eager_sequence() {
        let mut interner = Interner::new();
        let x = interner.insert("x");
        let y = interner.insert("y");

        let decls = vec![
            Decl::VarDecl {
                name: x,
                ty: None,
                val: Expr::Literal {
                    val: Value::Int { val: 5 },
                },
            },
            Decl::VarDecl {
                name: y,
                ty: None,
                val: Expr::Variable { name: x },
            },
        ];

        let res = analyze_dependencies(&decls);
        assert!(res.is_ok());
        let sg = res.unwrap();
        assert!(sg.vars.contains(&x));
        assert!(sg.vars.contains(&y));
        assert!(sg.has_path(x, y));
    }

    #[test]
    fn test_analyze_dependencies_eager_forward_ref_fails() {
        let mut interner = Interner::new();
        let x = interner.insert("x");
        let y = interner.insert("y");

        let decls = vec![
            Decl::VarDecl {
                name: y,
                ty: None,
                val: Expr::Variable { name: x },
            },
            Decl::VarDecl {
                name: x,
                ty: None,
                val: Expr::Literal {
                    val: Value::Int { val: 5 },
                },
            },
        ];

        let res = analyze_dependencies(&decls);
        assert!(res.is_err());
    }

    #[test]
    fn test_build_eager_call_graph_ast_constructs() {
        let mut interner = Interner::new();
        let parent = interner.insert("parent");
        let v1 = interner.insert("v1");
        let v2 = interner.insert("v2");
        let v3 = interner.insert("v3");
        let s1 = interner.insert("s1");
        let m1 = interner.insert("m1");
        let p = interner.insert("p");

        let mut declared_symbols = HashSet::new();
        declared_symbols.insert(parent);
        declared_symbols.insert(v1);
        declared_symbols.insert(v2);
        declared_symbols.insert(v3);

        let initialized_symbols = HashSet::new();

        let mut def_bodies = HashMap::new();
        def_bodies.insert(
            v1,
            Expr::Func {
                params: vec![Param { name: p, ty: None }],
                body: Box::new(Expr::Variable { name: v2 }),
                return_ty: None,
            },
        );

        let global_def_bodies = HashMap::new();
        let global_member_order = HashMap::new();

        let eager_ctx = EagerCtx {
            declared_symbols: &declared_symbols,
            initialized_symbols: &initialized_symbols,
            def_bodies: &def_bodies,
            global_def_bodies: &global_def_bodies,
            global_member_order: &global_member_order,
        };

        let mut eager_graph = DiGraphMap::new();
        eager_graph.add_node(parent);

        let if_expr = Expr::If {
            cond: Box::new(Expr::Variable { name: v1 }),
            expr1: Box::new(Expr::Unop {
                op: UnOp::Neg,
                expr: Box::new(Expr::Variable { name: v2 }),
            }),
            expr2: Box::new(Expr::Binop {
                op: BinOp::Add,
                expr1: Box::new(Expr::Variable { name: v3 }),
                expr2: Box::new(Expr::Tuple {
                    val: vec![Expr::List(vec![Expr::Range {
                        start: Box::new(Expr::Variable { name: v1 }),
                        end: Box::new(Expr::KeyVal {
                            name: v1,
                            value: Box::new(Expr::Variable { name: v2 }),
                        }),
                    }])],
                }),
            }),
        };
        let _ = build_eager_call_graph(&if_expr, parent, &eager_ctx, &mut eager_graph);
        assert!(eager_graph.contains_edge(parent, v1));
        assert!(eager_graph.contains_edge(parent, v2));
        assert!(eager_graph.contains_edge(parent, v3));

        let ma_call = Expr::Call {
            func: Box::new(Expr::MemberAccess {
                service_name: s1,
                member_name: m1,
            }),
            args: vec![Expr::Variable { name: v1 }],
        };
        let _ = build_eager_call_graph(&ma_call, parent, &eager_ctx, &mut eager_graph);
        assert!(eager_graph.contains_edge(parent, m1));
    }

    #[test]
    fn test_expand_eager_argument_variants() {
        let mut interner = Interner::new();
        let parent = interner.insert("parent");
        let dep1 = interner.insert("dep1");
        let dep2 = interner.insert("dep2");
        let s1 = interner.insert("s1");
        let m1 = interner.insert("m1");
        let p = interner.insert("p");

        let mut declared_symbols = HashSet::new();
        declared_symbols.insert(parent);
        declared_symbols.insert(dep1);
        declared_symbols.insert(dep2);

        let initialized_symbols = HashSet::new();

        let mut def_bodies = HashMap::new();
        def_bodies.insert(
            dep1,
            Expr::Func {
                params: vec![Param { name: p, ty: None }],
                body: Box::new(Expr::Variable { name: dep2 }),
                return_ty: None,
            },
        );

        let global_def_bodies = HashMap::new();
        let global_member_order = HashMap::new();

        let eager_ctx = EagerCtx {
            declared_symbols: &declared_symbols,
            initialized_symbols: &initialized_symbols,
            def_bodies: &def_bodies,
            global_def_bodies: &global_def_bodies,
            global_member_order: &global_member_order,
        };

        let mut eager_graph = DiGraphMap::new();

        let func_arg = Expr::Func {
            params: vec![],
            body: Box::new(Expr::Variable { name: dep1 }),
            return_ty: None,
        };
        let _ = expand_eager_argument(&func_arg, parent, &eager_ctx, &mut eager_graph);
        assert!(eager_graph.contains_edge(parent, dep1));

        let ma_arg = Expr::MemberAccess {
            service_name: s1,
            member_name: m1,
        };
        let _ = expand_eager_argument(&ma_arg, parent, &eager_ctx, &mut eager_graph);
        assert!(eager_graph.contains_edge(parent, m1));

        let var_arg = Expr::Variable { name: dep1 };
        let _ = expand_eager_argument(&var_arg, parent, &eager_ctx, &mut eager_graph);
        assert!(eager_graph.contains_edge(parent, dep2));
    }
}
