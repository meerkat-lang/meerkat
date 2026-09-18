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

/// Test that pending network entries are pruned on retry and when
/// source code is received, and stale send failures are ignored
#[test]
fn test_imports_pending_cleanup() {
    use meerkat_lib::net::MessageId;

    let mut interner = Interner::new();
    let sym_b = interner.insert("B");
    let base_ast = vec![Stmt::Import {
        path: "B.mkt".to_string(),
        service_name: sym_b,
    }];

    let mut remote_map = HashMap::new();
    remote_map.insert("B".to_string(), "/ip4/127.0.0.1/tcp/9000".to_string());

    let (mut imports, _initial_cmds) = Imports::new(
        &mut interner,
        remote_map,
        &base_ast,
        Path::new(""),
        "/ip4/127.0.0.1/tcp/8000/p2p/peer_a",
    )
    .expect("Imports::new success");

    let msg1 = MessageId(101);
    let msg2 = MessageId(102);

    // Register initial request
    imports.register_sent_command(
        msg1,
        "B".to_string(),
        "/ip4/127.0.0.1/tcp/9000".to_string(),
        0,
    );

    // Register retry request for same service; msg1 should be pruned
    imports.register_sent_command(
        msg2,
        "B".to_string(),
        "/ip4/127.0.0.1/tcp/9000".to_string(),
        1,
    );

    // Assert that msg1 was pruned by registering msg2
    let retry1 = imports.on_send_failure(msg1).expect("on_send_failure ok");
    assert!(retry1.is_none());

    // Receive source for B; should remove pending entries for B
    let remote_source = "service B {\n    pub def count = 100;\n}";
    let _ = imports
        .on_recv_source(remote_source, "B", Path::new(""))
        .expect("on_recv_source success");

    // Stale failure notifications for completed service yield None
    let retry2 = imports.on_send_failure(msg2).expect("on_send_failure ok");
    assert!(retry2.is_none());
}

/// Verify that `decode_source_response` validates paths and enforces source
/// length limits, returning `Error::LimitExceeded` for invalid inputs
#[test]
fn test_decode_source_response_validation() {
    use meerkat_lib::net::codec::decode_source_response;
    use meerkat_lib::runtime::limits::MAX_NET_REQUEST_SOURCE_LENGTH;

    // Valid path and source
    let valid_res = decode_source_response("B.mkt", "service B {}");
    assert_eq!(valid_res.expect("valid source response"), "B");

    // Valid path without .mkt extension
    let valid_no_ext = decode_source_response("B", "service B {}");
    assert_eq!(
        valid_no_ext.expect("valid source response without ext"),
        "B"
    );

    // Invalid path containing path traversal
    let invalid_path = decode_source_response("../B.mkt", "service B {}");
    assert!(invalid_path.is_err());

    // Invalid path with bad characters
    let bad_chars = decode_source_response("B-invalid#.mkt", "service B {}");
    assert!(bad_chars.is_err());

    // Oversized source payload
    let oversized_source = "a".repeat(MAX_NET_REQUEST_SOURCE_LENGTH + 1);
    let oversized_res = decode_source_response("B.mkt", &oversized_source);
    assert!(oversized_res.is_err());
}

/// Verify that `on_recv_source` rejects imports exceeding the maximum
/// allowed service count limit with `Error::LimitExceeded`
#[test]
fn test_imports_max_imported_services_limit() {
    use meerkat_lib::runtime::limits::MAX_IMPORTED_SERVICES;

    let mut interner = Interner::new();
    let base_ast = Vec::new();

    let (mut imports, _initial_cmds) =
        Imports::new(&mut interner, HashMap::new(), &base_ast, Path::new(""), "")
            .expect("Imports::new success");

    // Populate visited_services up to the limit
    for i in 0..MAX_IMPORTED_SERVICES {
        let src = format!("service S{} {{}}", i);
        let _ = imports.on_recv_source(&src, &format!("S{}", i), Path::new(""));
    }

    // Exceeding the limit should return Error::LimitExceeded
    let res = imports.on_recv_source("service Overflow {}", "Overflow", Path::new(""));
    assert!(res.is_err());
}

/// The unified AST must place imported services before the importing program.
///
/// `tt::check` walks services in AST order and tracks initialized members in a
/// single flat set, so a member is only usable once its declaration has been
/// checked. An importing service depends on what it imports, never the reverse.
/// When imports were appended after the local program instead, any imported
/// service deriving a `def` from its own `var` failed static checks with a
/// spurious `IllegalDependency` -- which blocked every distributed CLI test.
#[tokio::test]
async fn test_imports_precede_local_program_in_unified_ast() {
    let dir = std::env::temp_dir().join(format!(
        "meerkat_import_order_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();

    // The imported service derives a `def` from its own `var`.
    std::fs::write(
        dir.join("dep.mkt"),
        "service dep {\n    var x = 0;\n    pub def y = x + 1;\n}\n",
    )
    .unwrap();
    let main_path = dir.join("main.mkt");
    std::fs::write(
        &main_path,
        "import dep\n\nservice app {\n    pub def z = dep.y * 2;\n}\n",
    )
    .unwrap();

    let mut node = meerkat_lib::runtime::Node::new();
    node.resolve_imports_with_net(main_path.to_str().unwrap(), HashMap::new(), None, None)
        .await
        .expect("imports resolve");

    let order: Vec<String> = node
        .unified_ast
        .iter()
        .filter_map(|s| match s {
            Stmt::Service { name, .. } => Some(node.interner.get(*name).to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(
        order,
        vec!["dep".to_string(), "app".to_string()],
        "imported services must be checked before the program that imports them"
    );

    // And the ordering is what makes the static checks pass.
    node.static_checks()
        .expect("a def over the imported service's own var must type check");

    std::fs::remove_dir_all(&dir).ok();
}

/// Transitive imports must be ordered by dependency, not by arrival.
///
/// `Imports::on_recv_source` records a file before resolving that file's own
/// imports, so `main -> a -> b` accumulates as `[a, b]` -- the reverse of what
/// `tt::check` needs when `a` reads `b.y`. `finalize` therefore emits modules
/// in post-order over the import graph.
#[tokio::test]
async fn test_transitive_imports_are_dependency_ordered() {
    let dir = std::env::temp_dir().join(format!(
        "meerkat_nested_import_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();

    // b is the leaf; a reads b; main reads a.
    std::fs::write(
        dir.join("b.mkt"),
        "service b {\n    var n = 1;\n    pub def y = n + 1;\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("a.mkt"),
        "import b\n\nservice a {\n    pub def q = b.y * 10;\n}\n",
    )
    .unwrap();
    let main_path = dir.join("main.mkt");
    std::fs::write(
        &main_path,
        "import a\n\nservice main_s {\n    pub def r = a.q + 1;\n}\n",
    )
    .unwrap();

    let mut node = meerkat_lib::runtime::Node::new();
    node.resolve_imports_with_net(main_path.to_str().unwrap(), HashMap::new(), None, None)
        .await
        .expect("imports resolve");

    let order: Vec<String> = node
        .unified_ast
        .iter()
        .filter_map(|s| match s {
            Stmt::Service { name, .. } => Some(node.interner.get(*name).to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(
        order,
        vec!["b".to_string(), "a".to_string(), "main_s".to_string()],
        "a transitive dependency must precede the module that imports it"
    );

    node.static_checks()
        .expect("a chain of imports must type check in dependency order");

    std::fs::remove_dir_all(&dir).ok();
}

/// Both entry points that assemble a unified AST must order imports first.
///
/// `resolve_imports_with_net` and `on_node_startup` build it separately, and
/// `run_static_checks_with_imports` runs the checks through the latter. Fixing
/// only one left the other rejecting the same programs.
#[tokio::test]
async fn test_on_node_startup_also_orders_imports_first() {
    let dir = std::env::temp_dir().join(format!(
        "meerkat_startup_order_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("dep.mkt"),
        "service dep {\n    var x = 0;\n    pub def y = x + 1;\n}\n",
    )
    .unwrap();
    let main_path = dir.join("main.mkt");
    std::fs::write(
        &main_path,
        "import dep\n\nservice app {\n    pub def z = dep.y * 2;\n}\n",
    )
    .unwrap();

    let mut node = meerkat_lib::runtime::Node::new();
    let result = node
        .run_static_checks_with_imports(main_path.to_str().unwrap(), &HashMap::new())
        .await;
    std::fs::remove_dir_all(&dir).ok();
    result.expect("static checks through on_node_startup must accept an imported def over a var");
}
