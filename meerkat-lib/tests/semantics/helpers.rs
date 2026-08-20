use meerkat_lib::runtime::{parser::parse_string, update, Interner, Node};
use std::collections::HashMap;

pub fn check_program(input: &str) -> Result<(), String> {
    let mut node = Node::new();
    let prog =
        parse_string(input, &mut node.interner).map_err(|e| format!("Parse error: {}", e))?;
    node.run_static_checks(&prog).map_err(|e| e.to_string())
}

pub async fn check_update(initial: &str, update: &str) -> Result<(), String> {
    let mut interner = Interner::new();
    let initial_ast = parse_string(initial, &mut interner).map_err(|e| e.to_string())?;
    let mut node = Node::new();
    node.interner = interner;
    node.unified_ast = initial_ast;
    node.static_checks().map_err(|e| e.to_string())?;

    let local_ast = node.unified_ast.clone();
    let mut manager = node
        .on_manager_startup(true, None, HashMap::new(), &local_ast)
        .await
        .map_err(|e| e.to_string())?;

    let update_ast =
        parse_string(update, &mut manager.interner).map_err(|e| format!("Parse error: {}", e))?;
    let mut txn = update::Transaction::new(update_ast);
    txn.poll(&mut manager).await.map_err(|e| e.to_string())
}
