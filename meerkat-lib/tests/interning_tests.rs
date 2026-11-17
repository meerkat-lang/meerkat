//! `AST` interning integration tests
//!
//! This module contains integration tests for the string interning
//! functionality, `AstPrinter` symbol formatting, and key indexing
//! behavior in `Transaction` maps and wait queues

use meerkat_lib::net::ServiceNetId;
use meerkat_lib::runtime::ast::{AstPrinter, Decl, Stmt};
use meerkat_lib::runtime::txn::{Transaction, TxnId};
use meerkat_lib::runtime::{Interner, Symbol};
use std::collections::HashMap;

/// Verify `AstPrinter` formatting for symbols
///
/// Ensures that `AstPrinter::format_symbol` formats a `Symbol` using the
/// symbol spelling
#[test]
fn test_ast_printer_format_symbol() {
    let mut interner = Interner::new();
    let symbol = interner.insert("my_variable");
    let printer = AstPrinter::new();
    let formatted = printer.format_symbol(&symbol);

    let expected = format!("{:?}", symbol);
    assert_eq!(formatted, expected);
}

/// Verify `Transaction` maps indexing behavior
///
/// Asserts key typing and insertion for transaction maps: `locked`,
/// `read_cache`, and `written` using `ServiceNetId` and `Symbol`
#[test]
fn test_transaction_maps_indexing() {
    let mut interner = Interner::new();
    let service_id = ServiceNetId::new("/ip4/127.0.0.1/tcp/9000/p2p/some_id/my_service");
    let symbol = interner.insert("my_variable");

    let txn_id = TxnId {
        timestamp: 1000,
        node_id: 1,
        iteration: 0,
    };
    let mut txn = Transaction::new(txn_id);
    let key = (service_id, symbol);

    txn.locked.insert(key.clone());
    assert!(txn.locked.contains(&key));

    let read_value = meerkat_lib::runtime::ast::Value::Int { val: 42 };
    let read_entry = (read_value, Some(HashMap::new()), None);
    txn.read_cache.insert(key.clone(), read_entry.clone());
    assert_eq!(txn.read_cache.get(&key), Some(&read_entry));

    let write_value = meerkat_lib::runtime::ast::Value::Int { val: 100 };
    txn.written.insert(key.clone(), write_value.clone());
    assert_eq!(txn.written.get(&key), Some(&write_value));
}

/// Verify manager wait queue indexing behavior
///
/// Verifies key insertion into the manager wait queue using
/// `ServiceNetId` and `Symbol`
#[test]
fn test_manager_wait_queue_indexing() {
    let mut interner = Interner::new();
    let service_id = ServiceNetId::new("/ip4/127.0.0.1/tcp/9000/p2p/some_id/my_service");
    let symbol = interner.insert("my_variable");

    let mut wait_queue =
        HashMap::<(ServiceNetId, Symbol), Vec<meerkat_lib::runtime::manager::ParkedRequest>>::new();
    let key = (service_id, symbol);

    assert!(!wait_queue.contains_key(&key));
    wait_queue.insert(key.clone(), Vec::new());
    assert!(wait_queue.contains_key(&key));
}

/// Verify new `Interner` initialization
///
/// Ensures that a newly initialized `Interner` has the empty string
/// represented by the empty symbol
#[test]
fn test_interner_new_empty() {
    let mut interner = Interner::new();
    assert_eq!(interner.insert(""), Symbol::empty());
}

/// Verify `Interner` duplicate handling
///
/// Ensures that inserting the same string multiple times returns
/// the same `Symbol`
#[test]
fn test_interner_insert_duplicate() {
    let mut interner = Interner::new();
    let sym1 = interner.insert("duplicate");
    let sym2 = interner.insert("duplicate");
    assert_eq!(sym1, sym2);
}

/// Verify `AstPrinter` formatting for empty symbol
///
/// Ensures that the empty symbol is formatted correctly
#[test]
fn test_ast_printer_format_symbol_empty() {
    let printer = AstPrinter::new();
    let symbol = Symbol::empty();
    let formatted = printer.format_symbol(&symbol);
    let expected = format!("{:?}", symbol);
    assert_eq!(formatted, expected);
}

/// Verify `Symbol` clone semantics
///
/// Ensures that cloning a symbol preserves its identity
#[test]
fn test_symbol_clone_semantics() {
    let sym1 = Symbol::empty();
    let sym2 = sym1.clone();
    assert_eq!(sym1, sym2);
}

/// Verify custom spacing in `AstPrinter`
///
/// Ensures that formatting with a custom indentation level works
/// and formats symbols identically
#[test]
fn test_ast_printer_custom_spaces() {
    let mut interner = Interner::new();
    let symbol = interner.insert("test_spacing");
    let printer = AstPrinter::with_spaces(4);
    let formatted = printer.format_symbol(&symbol);
    let expected = format!("{:?}", symbol);
    assert_eq!(formatted, expected);
}

/// Verify parser interning
///
/// Ensures that parsing a service declaration correctly inserts the
/// service and variable names into the `Interner`
#[test]
fn test_parser_simple_expression_interning() {
    let mut interner = Interner::new();
    let input = "service my_service { var my_var = 42; }";
    let parse_result = meerkat_lib::runtime::parser::parse_string(input, &mut interner);
    assert!(parse_result.is_ok());
    let prog = parse_result.unwrap();
    assert_eq!(prog.len(), 1);

    if let Stmt::Service { name, decls } = &prog[0] {
        assert_eq!(name.as_str(), "my_service");
        assert_eq!(decls.len(), 1);
        if let Decl::VarDecl {
            name: var_name,
            ty: _,
            val: _,
        } = &decls[0]
        {
            assert_eq!(var_name.as_str(), "my_var");
        } else {
            panic!("expected VarDecl statement");
        }
    } else {
        panic!("expected Service statement");
    }
}

/// Verify `ServiceNetId` equality and hashing
///
/// Ensures that `ServiceNetId` behaves correctly when placed in a map
/// and matches keys as expected
#[test]
fn test_servicenetid_equality_and_hashing() {
    let id1 = ServiceNetId::new("my_service");
    let id2 = ServiceNetId::new("my_service");
    let id3 = ServiceNetId::new("other_service");

    assert_eq!(id1, id2);
    assert_ne!(id1, id3);

    let mut map = HashMap::new();
    map.insert(id1.clone(), 100);
    assert_eq!(map.get(&id2), Some(&100));
    assert_eq!(map.get(&id3), None);
}

/// Verify that `AstPrinter` prints optional types using their display format
#[test]
fn test_ast_printer_type_display() {
    let printer = AstPrinter::new();

    use meerkat_lib::runtime::tt::{TupleType, Type};

    // Create a type: (int, string) -> bool
    let test_type = Type::Func(
        Box::new(Type::Tuple(
            TupleType::new(vec![Type::Int, Type::String]).unwrap(),
        )),
        Box::new(Type::Bool),
    );

    let formatted_some = printer.format_type_opt(&Some(test_type));
    assert_eq!(formatted_some, "Some((int, string) -> bool)");

    let formatted_none = printer.format_type_opt(&None);
    assert_eq!(formatted_none, "None");
}
