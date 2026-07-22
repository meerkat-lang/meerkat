//! Unit and integration tests for the Imports state machine.

use std::collections::HashMap;
use std::fs::{create_dir_all, remove_dir_all, File};
use std::io::Write;

use meerkat_lib::runtime::ast::Stmt;
use meerkat_lib::runtime::imports::Imports;
use meerkat_lib::runtime::interner::Interner;
use meerkat_lib::runtime::parser;

/// Test that local disk imports resolve transitively and circular
/// imports terminate cleanly without infinite recursion
#[test]
fn test_imports_local_resolution_and_circular_prevention() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("valid system time")
        .as_nanos();
    let temp_dir = std::env::temp_dir().join(format!("test_imports_{}", nanos));
    create_dir_all(&temp_dir).expect("failed to create temp dir");

    let file_a = temp_dir.join("A.mkt");
    let mut f_a = File::create(&file_a).expect("failed to create A.mkt");
    writeln!(f_a, "import B\nservice A {{\n    var x = 0;\n}}").expect("failed to write A.mkt");

    let file_b = temp_dir.join("B.mkt");
    let mut f_b = File::create(&file_b).expect("failed to create B.mkt");
    writeln!(f_b, "import A\nservice B {{\n    var y = 0;\n}}").expect("failed to write B.mkt");

    let mut interner = Interner::new();
    let base_ast = parser::parse_file(file_a.to_str().expect("valid string"), &mut interner)
        .expect("parse base_ast");

    let (imports, initial_cmds) =
        Imports::new(&mut interner, HashMap::new(), &base_ast, &temp_dir, "")
            .expect("Imports::new success");

    assert!(initial_cmds.is_empty());
    assert!(imports.is_done());

    let final_ast = imports.finalize();
    let mut has_service_b = false;
    for stmt in final_ast {
        if let Stmt::Service { name, .. } = stmt {
            if interner.get(name) == "B" {
                has_service_b = true;
            }
        }
    }

    assert!(has_service_b);
    let _ = remove_dir_all(&temp_dir);
}

/// Test that remote imports mapped via remote_url_map queue network
/// commands instead of attempting local disk resolution
#[test]
fn test_imports_remote_queuing() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("valid system time")
        .as_nanos();
    let temp_dir = std::env::temp_dir().join(format!("test_remote_{}", nanos));
    create_dir_all(&temp_dir).expect("failed to create temp dir");

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
        &temp_dir,
        "/ip4/127.0.0.1/tcp/8000/p2p/peer_a",
    )
    .expect("Imports::new success");

    assert!(!imports.is_done());
    assert_eq!(initial_cmds.len(), 1);
    assert_eq!(initial_cmds[0].1, "B");

    let _ = remove_dir_all(&temp_dir);
}

/// Verify local disk import resolution followed by static analysis checks
/// (name resolution and type checking) over the unified AST
#[test]
fn test_imports_local_then_static_checks() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("valid system time")
        .as_nanos();
    let temp_dir = std::env::temp_dir().join(format!("test_static_{}", nanos));
    create_dir_all(&temp_dir).expect("failed to create temp dir");

    let file_a = temp_dir.join("A.mkt");
    let mut f_a = File::create(&file_a).expect("failed to create A.mkt");
    writeln!(
        f_a,
        "import B\nservice A {{\n    pub def val_a = B.val_b;\n}}"
    )
    .expect("failed to write A.mkt");

    let file_b = temp_dir.join("B.mkt");
    let mut f_b = File::create(&file_b).expect("failed to create B.mkt");
    writeln!(f_b, "service B {{\n    pub def val_b = 42;\n}}").expect("failed to write B.mkt");

    let mut node = meerkat_lib::runtime::node::Node::new();
    let base_ast = node
        .load_file(file_a.to_str().expect("valid string"))
        .expect("parse base_ast");

    let (imports, initial_cmds) =
        Imports::new(&mut node.interner, HashMap::new(), &base_ast, &temp_dir, "")
            .expect("Imports::new success");

    assert!(initial_cmds.is_empty());
    assert!(imports.is_done());

    let imported_ast = imports.finalize();
    node.unified_ast = base_ast;
    node.unified_ast.extend(imported_ast);

    let res = node.static_checks();
    assert!(res.is_ok());

    let _ = remove_dir_all(&temp_dir);
}

/// Verify that `on_recv_source` dynamically parses remote source text,
/// merges it into the state machine, and passes static analysis checks
#[test]
fn test_imports_on_recv_source_merges_and_resolves() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("valid system time")
        .as_nanos();
    let temp_dir = std::env::temp_dir().join(format!("test_recv_{}", nanos));
    create_dir_all(&temp_dir).expect("failed to create temp dir");

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
        &temp_dir,
        "/ip4/127.0.0.1/tcp/8000/p2p/peer_a",
    )
    .expect("Imports::new success");

    let remote_source = "service B {\n    pub def count = 100;\n}";
    let new_cmds = imports
        .on_recv_source(remote_source, "B", &temp_dir)
        .expect("on_recv_source success");

    assert!(new_cmds.is_empty());
    assert!(imports.is_done());

    let imported_ast = imports.finalize();
    node.unified_ast = base_ast;
    node.unified_ast.extend(imported_ast);

    let res = node.static_checks();
    assert!(res.is_ok());

    let _ = remove_dir_all(&temp_dir);
}

/// Verify that multi-level transitive imports (A -> B -> C) resolve
/// and pass static analysis checks across all service boundaries
#[test]
fn test_imports_transitive_static_checks() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("valid system time")
        .as_nanos();
    let temp_dir = std::env::temp_dir().join(format!("test_trans_{}", nanos));
    create_dir_all(&temp_dir).expect("failed to create temp dir");

    let file_a = temp_dir.join("A.mkt");
    let mut f_a = File::create(&file_a).expect("failed to create A.mkt");
    writeln!(
        f_a,
        "import B\nservice A {{\n    pub def get_c = B.val_c;\n}}"
    )
    .expect("failed to write A.mkt");

    let file_b = temp_dir.join("B.mkt");
    let mut f_b = File::create(&file_b).expect("failed to create B.mkt");
    writeln!(
        f_b,
        "import C\nservice B {{\n    pub def val_c = C.val_base;\n}}"
    )
    .expect("failed to write B.mkt");

    let file_c = temp_dir.join("C.mkt");
    let mut f_c = File::create(&file_c).expect("failed to create C.mkt");
    writeln!(f_c, "service C {{\n    pub def val_base = 99;\n}}").expect("failed to write C.mkt");

    let mut node = meerkat_lib::runtime::node::Node::new();
    let base_ast = node
        .load_file(file_a.to_str().expect("valid string"))
        .expect("parse base_ast");

    let (imports, _initial_cmds) =
        Imports::new(&mut node.interner, HashMap::new(), &base_ast, &temp_dir, "")
            .expect("Imports::new success");

    assert!(imports.is_done());

    let imported_ast = imports.finalize();
    node.unified_ast = base_ast;
    node.unified_ast.extend(imported_ast);

    let res = node.static_checks();
    assert!(res.is_ok());

    let _ = remove_dir_all(&temp_dir);
}
