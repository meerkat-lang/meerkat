//! Unit and integration tests for the Imports state machine.

use std::collections::HashMap;
use std::path::Path;

use meerkat_lib::runtime::ast::Stmt;
use meerkat_lib::runtime::imports::Imports;
use meerkat_lib::runtime::interner::Interner;
use meerkat_lib::runtime::parser;

/// Test that local imports resolve transitively and circular
/// imports terminate cleanly without infinite recursion
#[test]
fn test_imports_local_resolution_and_circular_prevention() {
    let mut interner = Interner::new();
    let sym_b = interner.insert("B");
    let sym_a = interner.insert("A");
    let base_ast = vec![
        Stmt::Import {
            path: "B.mkt".to_string(),
            service_name: sym_b,
        },
        Stmt::Service {
            name: sym_a,
            decls: Vec::new(),
        },
    ];

    let mut remote_map = HashMap::new();
    remote_map.insert("B".to_string(), "/ip4/127.0.0.1/tcp/9000".to_string());

    let (mut imports, initial_cmds) = Imports::new(
        &mut interner,
        remote_map,
        &base_ast,
        Path::new(""),
        "/ip4/127.0.0.1/tcp/8000/p2p/peer_a",
    )
    .expect("Imports::new success");

    assert_eq!(initial_cmds.len(), 1);
    assert!(!imports.is_done());

    // Feed remote source B which imports A (circular dependency)
    let source_b = "import A\nservice B {\n    var y = 0;\n}";
    let new_cmds = imports
        .on_recv_source(source_b, "B", Path::new(""))
        .expect("on_recv_source success");

    // Since A was registered in base_ast, circular import for A generates 0 new commands
    assert!(new_cmds.is_empty());
    assert!(imports.is_done());

    let final_ast = imports.finalize();
    let has_service_b = final_ast.iter().any(|stmt| {
        if let Stmt::Service { name, .. } = stmt {
            interner.get(*name) == "B"
        } else {
            false
        }
    });

    assert!(has_service_b);
}

/// Test that remote imports mapped via remote_url_map queue network
/// commands instead of attempting local disk resolution
#[test]
fn test_imports_remote_queuing() {
    let mut interner = Interner::new();
    let sym_b = interner.insert("B");
    let base_ast = vec![Stmt::Import {
        path: "B.mkt".to_string(),
        service_name: sym_b,
    }];

    let mut remote_map = HashMap::new();
    remote_map.insert("B".to_string(), "/ip4/127.0.0.1/tcp/9000".to_string());

    let (imports, initial_cmds) = Imports::new(
        &mut interner,
        remote_map,
        &base_ast,
        Path::new(""),
        "/ip4/127.0.0.1/tcp/8000/p2p/peer_a",
    )
    .expect("Imports::new success");

    assert!(!imports.is_done());
    assert_eq!(initial_cmds.len(), 1);
    assert_eq!(initial_cmds[0].1, "B");
}

/// Verify local resolution followed by static analysis checks
/// (name resolution and type checking) over the AST in memory
#[test]
fn test_imports_local_then_static_checks() {
    let mut node = meerkat_lib::runtime::node::Node::new();

    let source_a = "service A {\n    pub def val_a = B.val_b;\n}";
    let source_b = "service B {\n    pub def val_b = 42;\n}";

    let mut stmts = parser::parse_string(source_a, &mut node.interner).expect("parse source A");
    let stmts_b = parser::parse_string(source_b, &mut node.interner).expect("parse source B");
    stmts.extend(stmts_b);

    let res = node.run_static_checks(&stmts);
    assert!(res.is_ok());
}

/// Verify that `on_recv_source` dynamically parses remote source text,
/// merges it into the state machine, and passes static analysis checks
#[test]
fn test_imports_on_recv_source_merges_and_resolves() {
    let mut node = meerkat_lib::runtime::node::Node::new();
    let sym_b = node.interner.insert("B");
    let base_ast = vec![Stmt::Import {
        path: "B.mkt".to_string(),
        service_name: sym_b,
    }];

    let mut remote_map = HashMap::new();
    remote_map.insert("B".to_string(), "/ip4/127.0.0.1/tcp/9000".to_string());

    let (mut imports, _initial_cmds) = Imports::new(
        &mut node.interner,
        remote_map,
        &base_ast,
        Path::new(""),
        "/ip4/127.0.0.1/tcp/8000/p2p/peer_a",
    )
    .expect("Imports::new success");

    let remote_source = "service B {\n    pub def count = 100;\n}";
    let new_cmds = imports
        .on_recv_source(remote_source, "B", Path::new(""))
        .expect("on_recv_source success");

    assert!(new_cmds.is_empty());
    assert!(imports.is_done());

    let imported_ast = imports.finalize();
    let mut all_stmts = base_ast;
    all_stmts.extend(imported_ast);

    let res = node.run_static_checks(&all_stmts);
    assert!(res.is_ok());
}

/// Verify that multi-level transitive imports (A -> B -> C) resolve
/// and pass static analysis checks across all service boundaries
#[test]
fn test_imports_transitive_static_checks() {
    let mut node = meerkat_lib::runtime::node::Node::new();

    let source_a = "service A {\n    pub def get_c = B.val_c;\n}";
    let source_b = "service B {\n    pub def val_c = C.val_base;\n}";
    let source_c = "service C {\n    pub def val_base = 99;\n}";

    let mut stmts = parser::parse_string(source_a, &mut node.interner).expect("parse source A");
    let stmts_b = parser::parse_string(source_b, &mut node.interner).expect("parse source B");
    let stmts_c = parser::parse_string(source_c, &mut node.interner).expect("parse source C");
    stmts.extend(stmts_b);
    stmts.extend(stmts_c);

    let res = node.run_static_checks(&stmts);
    assert!(res.is_ok());
}

/// Verify that cross-service cyclic member dependencies are rejected by
/// static checks with a DependencyCycle error in memory without filesystem I/O
#[test]
fn test_imports_network_cycle_static_check_rejection() {
    let mut node = meerkat_lib::runtime::node::Node::new();

    let source_a = "service A {\n    pub def val_a = B.val_b;\n}";
    let source_b = "service B {\n    pub def val_b = A.val_a;\n}";

    let mut stmts =
        parser::parse_string(source_a, &mut node.interner).expect("failed to parse source A");
    let stmts_b =
        parser::parse_string(source_b, &mut node.interner).expect("failed to parse source B");
    stmts.extend(stmts_b);

    let res = node.run_static_checks(&stmts);
    let err_msg = res.expect_err("expected static check error").to_string();
    assert!(err_msg.contains("dependency cycle detected"));
}
