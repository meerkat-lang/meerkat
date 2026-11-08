//! Integration tests for the hierarchical environment system
//!
//! This module contains 24 integration tests for shadowing,
//! lookup behavior, parent pointers, and dynamic mutations

use meerkat_lib::runtime::tt::types::Type;
use meerkat_lib::runtime::{Env, Interner};

#[test]
/// Verify that lookup of unbound symbols in a blank `Env` yields `None`
fn test_env_int_basic_empty() {
    let env: Env<'_, Type> = Env::new(None);
    let mut interner = Interner::new();
    let x = interner.insert("x");
    assert!(env.find(x).is_none());
}

#[test]
/// Verify that binding and resolving a symbol returns the correct type
fn test_env_int_single_bind() {
    let mut env = Env::new(None);
    let mut interner = Interner::new();
    let x = interner.insert("x");
    env.bind(x, Type::Int);
    assert_eq!(env.find(x), Some(&Type::Int));
}

#[test]
/// Verify that variables defined in child scopes shadow parent variables
fn test_env_int_shadowing_direct() {
    let mut interner = Interner::new();
    let x = interner.insert("x");

    let mut parent = Env::new(None);
    parent.bind(x, Type::Int);

    let mut child = Env::new(Some(&parent));
    child.bind(x, Type::Bool);

    assert_eq!(child.find(x), Some(&Type::Bool));
}

#[test]
/// Verify that unresolved symbols in child scopes fall back to parent scopes
fn test_env_int_parent_fallback() {
    let mut interner = Interner::new();
    let x = interner.insert("x");

    let mut parent = Env::new(None);
    parent.bind(x, Type::Int);

    let child = Env::new(Some(&parent));
    assert_eq!(child.find(x), Some(&Type::Int));
}

#[test]
/// Verify that unresolved symbols resolve across multiple parent fallback levels
fn test_env_int_grandparent_fallback() {
    let mut interner = Interner::new();
    let x = interner.insert("x");

    let mut grandparent = Env::new(None);
    grandparent.bind(x, Type::Int);

    let parent = Env::new(Some(&grandparent));
    let child = Env::new(Some(&parent));

    assert_eq!(child.find(x), Some(&Type::Int));
}

#[test]
/// Verify that mutating local bindings in child scopes preserves parent bindings
fn test_env_int_mutation_does_not_affect_parent() {
    let mut interner = Interner::new();
    let x = interner.insert("x");

    let mut parent = Env::new(None);
    parent.bind(x, Type::Int);

    let mut child = Env::new(Some(&parent));
    child.bind(x, Type::String);

    assert_eq!(parent.find(x), Some(&Type::Int));
    assert_eq!(child.find(x), Some(&Type::String));
}

#[test]
/// Verify that multiple parent bindings are shadowed properly by a child
fn test_env_int_shadowing_multiple() {
    let mut interner = Interner::new();
    let x = interner.insert("x");
    let y = interner.insert("y");

    let mut parent = Env::new(None);
    parent.bind(x, Type::Int);
    parent.bind(y, Type::String);

    let mut child = Env::new(Some(&parent));
    child.bind(x, Type::Bool);
    child.bind(y, Type::Unit);

    assert_eq!(child.find(x), Some(&Type::Bool));
    assert_eq!(child.find(y), Some(&Type::Unit));
}

#[test]
/// Verify that removing local child bindings fallback back to parent bindings
fn test_env_int_remove_local() {
    let mut interner = Interner::new();
    let x = interner.insert("x");

    let mut parent = Env::new(None);
    parent.bind(x, Type::Int);

    let mut child = Env::new(Some(&parent));
    child.bind(x, Type::Bool);
    assert_eq!(child.remove(x), Some(Type::Bool));

    assert_eq!(child.find(x), Some(&Type::Int));
}

#[test]
/// Verify that removing a missing symbol in `Env` returns `None`
fn test_env_int_remove_missing() {
    let mut env: Env<'_, Type> = Env::new(None);
    let mut interner = Interner::new();
    let x = interner.insert("x");
    assert!(env.remove(x).is_none());
}

#[test]
/// Verify that overwriting a binding returns the previous type in `Env`
fn test_env_int_bind_overwrite() {
    let mut env = Env::new(None);
    let mut interner = Interner::new();
    let x = interner.insert("x");

    assert!(env.bind(x, Type::Int).is_none());
    assert_eq!(env.bind(x, Type::Bool), Some(Type::Int));
}

#[test]
/// Verify that `find_with_env` resolves variables shadowed in child scopes
fn test_env_int_find_with_env_child() {
    let mut interner = Interner::new();
    let x = interner.insert("x");

    let mut parent = Env::new(None);
    parent.bind(x, Type::Int);

    let mut child = Env::new(Some(&parent));
    child.bind(x, Type::Bool);

    let res = child.find_with_env(x).unwrap();
    assert!(std::ptr::eq(res.0, &child));
    assert_eq!(res.1, &Type::Bool);
}

#[test]
/// Verify that `find_with_env` resolves parent variables successfully
fn test_env_int_find_with_env_parent() {
    let mut interner = Interner::new();
    let x = interner.insert("x");

    let mut parent = Env::new(None);
    parent.bind(x, Type::Int);

    let child = Env::new(Some(&parent));
    let res = child.find_with_env(x).unwrap();
    assert!(std::ptr::eq(res.0, &parent));
    assert_eq!(res.1, &Type::Int);
}

#[test]
/// Verify that `find_with_env` resolves deep grandparent variables
fn test_env_int_find_with_env_grandparent() {
    let mut interner = Interner::new();
    let x = interner.insert("x");

    let mut grandparent = Env::new(None);
    grandparent.bind(x, Type::Int);

    let parent = Env::new(Some(&grandparent));
    let child = Env::new(Some(&parent));

    let res = child.find_with_env(x).unwrap();
    assert!(std::ptr::eq(res.0, &grandparent));
    assert_eq!(res.1, &Type::Int);
}

#[test]
/// Verify that removing shadowed child bindings unmasks the parent binding
fn test_env_int_remove_shadow_reveals_parent() {
    let mut interner = Interner::new();
    let x = interner.insert("x");

    let mut parent = Env::new(None);
    parent.bind(x, Type::Int);

    let mut child = Env::new(Some(&parent));
    child.bind(x, Type::Bool);

    assert_eq!(child.find(x), Some(&Type::Bool));
    child.remove(x);
    assert_eq!(child.find(x), Some(&Type::Int));
}

#[test]
/// Verify that nested complex function types are stored correctly in `Env`
fn test_env_int_complex_types() {
    let mut env = Env::new(None);
    let mut interner = Interner::new();
    let x = interner.insert("x");
    let fun_ty = Type::Func(Box::new(Type::Int), Box::new(Type::Bool));

    env.bind(x, fun_ty.clone());
    assert_eq!(env.find(x), Some(&fun_ty));
}

#[test]
/// Verify that multiple sibling scopes query the same parent reference safely
fn test_env_int_multiple_siblings() {
    let mut interner = Interner::new();
    let x = interner.insert("x");

    let mut parent = Env::new(None);
    parent.bind(x, Type::Int);

    let child1 = Env::new(Some(&parent));
    let child2 = Env::new(Some(&parent));

    assert_eq!(child1.find(x), Some(&Type::Int));
    assert_eq!(child2.find(x), Some(&Type::Int));
}

#[test]
/// Verify lookup in a grandchild scope where variables shadow at multiple levels
fn test_env_int_grandchild_nested_shadowing() {
    let mut interner = Interner::new();
    let x = interner.insert("x");
    let y = interner.insert("y");
    let z = interner.insert("z");

    let mut grandparent = Env::new(None);
    grandparent.bind(x, Type::Int);
    grandparent.bind(y, Type::Int);
    grandparent.bind(z, Type::Int);

    let mut parent = Env::new(Some(&grandparent));
    parent.bind(x, Type::Bool);
    parent.bind(y, Type::Bool);

    let mut child = Env::new(Some(&parent));
    child.bind(x, Type::String);

    assert_eq!(child.find(x), Some(&Type::String));
    assert_eq!(child.find(y), Some(&Type::Bool));
    assert_eq!(child.find(z), Some(&Type::Int));
}

#[test]
/// Verify that lookup of missing variables in deep scope hierarchies yields `None`
fn test_env_int_deep_missing_lookup() {
    let mut interner = Interner::new();
    let w = interner.insert("w");

    let grandparent: Env<'_, Type> = Env::new(None);
    let parent = Env::new(Some(&grandparent));
    let child = Env::new(Some(&parent));

    assert!(child.find(w).is_none());
}

#[test]
/// Verify that binding a symbol again after its removal behaves correctly
fn test_env_int_rebind_after_remove() {
    let mut env = Env::new(None);
    let mut interner = Interner::new();
    let x = interner.insert("x");

    env.bind(x, Type::Int);
    env.remove(x);
    assert!(env.find(x).is_none());

    env.bind(x, Type::Bool);
    assert_eq!(env.find(x), Some(&Type::Bool));
}

#[test]
/// Verify that cloning an `Env` successfully preserves its bound items
fn test_env_int_cloned_env() {
    let mut env = Env::new(None);
    let mut interner = Interner::new();
    let x = interner.insert("x");

    env.bind(x, Type::Int);
    let cloned = env.clone();
    assert_eq!(cloned.find(x), Some(&Type::Int));
}

#[test]
/// Verify lookups when child scopes contain a mixture of shadowed and clean keys
fn test_env_int_multiple_shadows() {
    let mut interner = Interner::new();
    let x = interner.insert("x");
    let y = interner.insert("y");
    let z = interner.insert("z");

    let mut parent = Env::new(None);
    parent.bind(x, Type::Int);
    parent.bind(y, Type::String);
    parent.bind(z, Type::Bool);

    let mut child = Env::new(Some(&parent));
    child.bind(x, Type::Bool);
    child.bind(y, Type::Int);

    assert_eq!(child.find(x), Some(&Type::Bool));
    assert_eq!(child.find(y), Some(&Type::Int));
    assert_eq!(child.find(z), Some(&Type::Bool));
}

#[test]
/// Verify that `find_with_env` on missing keys returns `None`
fn test_env_int_find_with_env_missing() {
    let mut interner = Interner::new();
    let x = interner.insert("x");
    let env: Env<'_, Type> = Env::new(None);
    assert!(env.find_with_env(x).is_none());
}

#[test]
/// Verify that multiple children shadowing the same parent key behave independently
fn test_env_int_multiple_child_shadows() {
    let mut interner = Interner::new();
    let x = interner.insert("x");

    let mut parent = Env::new(None);
    parent.bind(x, Type::Int);

    let mut child1 = Env::new(Some(&parent));
    child1.bind(x, Type::Bool);

    let mut child2 = Env::new(Some(&parent));
    child2.bind(x, Type::String);

    assert_eq!(child1.find(x), Some(&Type::Bool));
    assert_eq!(child2.find(x), Some(&Type::String));
}

#[test]
/// Verify that nested environment find fallback points to the original parent env
fn test_env_int_find_with_env_nested_fallback() {
    let mut interner = Interner::new();
    let x = interner.insert("x");
    let y = interner.insert("y");

    let mut grandparent = Env::new(None);
    grandparent.bind(x, Type::Int);

    let parent = Env::new(Some(&grandparent));

    let mut child = Env::new(Some(&parent));
    child.bind(y, Type::Bool);

    let res_x = child.find_with_env(x).unwrap();
    assert!(std::ptr::eq(res_x.0, &grandparent));
    assert_eq!(res_x.1, &Type::Int);

    let res_y = child.find_with_env(y).unwrap();
    assert!(std::ptr::eq(res_y.0, &child));
    assert_eq!(res_y.1, &Type::Bool);
}
