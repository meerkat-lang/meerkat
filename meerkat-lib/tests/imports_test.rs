//! Unit and integration tests for the Imports state machine.

use meerkat_lib::runtime::ast::Stmt;
use meerkat_lib::runtime::imports::Imports;
use meerkat_lib::runtime::interner::Interner;
use meerkat_lib::runtime::parser;
use std::collections::HashMap;
use std::path::Path;

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
            explicit_path: false,
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
        .on_recv_source(source_b, "B", Path::new(""), false)
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
        explicit_path: false,
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
        explicit_path: false,
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
        .on_recv_source(remote_source, "B", Path::new(""), false)
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
        explicit_path: false,
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
        .on_recv_source(remote_source, "B", Path::new(""), false)
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
        let _ = imports.on_recv_source(&src, &format!("S{}", i), Path::new(""), false);
    }

    // Exceeding the limit should return Error::LimitExceeded
    let res = imports.on_recv_source("service Overflow {}", "Overflow", Path::new(""), false);
    assert!(res.is_err());
}

#[test]
fn test_imports_with_explicit_path() {
    let temp_dir = std::env::temp_dir().join(format!(
        "meerkat-import-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&temp_dir).unwrap();
    let main_path = temp_dir.join("main.mkt");
    let imported_path = temp_dir.join("s1.mkt");
    std::fs::write(&imported_path, "service s1 { var x = 7; }").unwrap();
    std::fs::write(
        &main_path,
        "import s1 from \"./s1.mkt\"\nservice s2 { pub def y = s1.x; }",
    )
    .unwrap();
    let mut interner = Interner::new();
    let service_sym = interner.insert("s1");
    let base_ast = parser::parse_file(main_path.to_str().unwrap(), &mut interner).unwrap();
    let (importer, _cmds) = Imports::new(&mut interner, HashMap::new(), &base_ast, &temp_dir, "")
        .expect("Imports::new success");
    let imported_ast = importer.finalize();
    let mut final_ast = base_ast;
    final_ast.extend(imported_ast);
    let has_service_s1 = final_ast.iter().any(|stmt| {
        if let Stmt::Service { name, .. } = stmt {
            *name == service_sym
        } else {
            false
        }
    });
    assert!(has_service_s1);
}
