//! Environment for the Meerkat language compiler and runtime
//!
//! This module implements the hierarchical environments used for both lexical
//! scoping and type namespaces

use crate::runtime::interner::Symbol;
use std::collections::HashMap;

/// A hierarchical environment mapping symbols to generic entries
#[derive(Debug, Clone)]
pub struct Env<'a, T> {
    bindings: HashMap<Symbol, T>,
    parent: Option<&'a Env<'a, T>>,
}

impl<'a, T> Env<'a, T> {
    /// Create a new environment with an optional parent
    ///
    /// Args:
    ///     `parent` (`Option<&'a Env<'a, T>>`): The parent environment reference
    ///
    /// Returns:
    ///     `Env<'a, T>`: The newly created environment
    pub fn new(parent: Option<&'a Env<'a, T>>) -> Self {
        Env {
            bindings: HashMap::new(),
            parent,
        }
    }

    /// Bind a symbol to a value in the current local environment scope
    ///
    /// Args:
    ///     `name` (`Symbol`): The symbol to bind
    ///     `value` (`T`): The value associated with the symbol
    ///
    /// Returns:
    ///     `Option<T>`: The previous value if it was already bound locally
    pub fn bind(&mut self, name: Symbol, value: T) -> Option<T> {
        self.bindings.insert(name, value)
    }

    /// Remove a symbol binding from the current local environment scope
    ///
    /// Args:
    ///     `name` (`&Symbol`): The symbol to remove
    ///
    /// Returns:
    ///     `Option<T>`: The removed value if it was bound locally
    pub fn remove(&mut self, name: &Symbol) -> Option<T> {
        self.bindings.remove(name)
    }

    /// Find a symbol in the environment chain, returning a reference to it
    ///
    /// Args:
    ///     `name` (`&Symbol`): The symbol to search
    ///
    /// Returns:
    ///     `Option<&T>`: Reference to the value if found
    pub fn find(&self, name: &Symbol) -> Option<&T> {
        if let Some(val) = self.bindings.get(name) {
            Some(val)
        } else if let Some(parent) = self.parent {
            parent.find(name)
        } else {
            None
        }
    }

    /// Find a symbol and return both the environment and the value
    ///
    /// Args:
    ///     `name` (`&Symbol`): The symbol to search
    ///
    /// Returns:
    ///     `Option<(&Env<'a, T>, &T)>`: The environment and the value
    pub fn find_with_env(&self, name: &Symbol) -> Option<(&Env<'a, T>, &T)> {
        if let Some(val) = self.bindings.get(name) {
            Some((self, val))
        } else if let Some(parent) = self.parent {
            parent.find_with_env(name)
        } else {
            None
        }
    }
}

impl<'parent, T: Clone> Env<'parent, T> {
    /// Flatten the hierarchical environment into a single flat scope
    ///
    /// This preserves lexical shadowing by ensuring that child bindings
    /// overwrite parent bindings of the same name
    ///
    /// Returns:
    ///     `Env<'out, T>`: A new flat environment with no parent
    pub fn flatten<'out>(&self) -> Env<'out, T> {
        let mut bindings = HashMap::new();
        self.flatten_into(&mut bindings);
        Env {
            bindings,
            parent: None,
        }
    }

    /// Recursively insert parent bindings into the output map
    ///
    /// Traverses the chain from parents to children so that child
    /// bindings override parents to respect lexical shadowing
    ///
    /// Args:
    ///     `out` (`&mut HashMap<Symbol, T>`): The output map
    fn flatten_into(&self, out: &mut HashMap<Symbol, T>) {
        if let Some(parent) = self.parent {
            parent.flatten_into(out);
        }
        for (k, v) in &self.bindings {
            out.insert(k.clone(), v.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// Verify that `Env::new` constructs an empty environment scope
    fn test_unit_env_new() {
        let env: Env<'_, i32> = Env::new(None);
        assert!(env.parent.is_none());
        assert!(env.bindings.is_empty());
    }

    #[test]
    /// Verify that `Env::bind` registers a symbol and `Env::find` retrieves it
    fn test_unit_env_bind_and_find() {
        let mut env = Env::new(None);
        let s = Symbol::empty();
        assert_eq!(env.bind(s.clone(), 42), None);
        assert_eq!(env.find(&s), Some(&42));
        assert_eq!(env.bind(s.clone(), 43), Some(42));
        assert_eq!(env.find(&s), Some(&43));
    }

    #[test]
    /// Verify that `Env::remove` deletes a binding from the local scope
    fn test_unit_env_remove() {
        let mut env = Env::new(None);
        let s = Symbol::empty();
        env.bind(s.clone(), 100);
        assert_eq!(env.remove(&s), Some(100));
        assert_eq!(env.find(&s), None);
        assert_eq!(env.remove(&s), None);
    }

    #[test]
    /// Verify that child `Env` scopes fallback to parent and grandparent scopes
    fn test_unit_env_hierarchical_lookup() {
        let mut parent = Env::new(None);
        let s1 = Symbol::empty();
        parent.bind(s1.clone(), 1);

        let mut child = Env::new(Some(&parent));
        assert_eq!(child.find(&s1), Some(&1));

        let res = child.find_with_env(&s1).unwrap();
        assert!(std::ptr::eq(res.0, &parent));
        assert_eq!(res.1, &1);

        child.bind(s1.clone(), 2);
        assert_eq!(child.find(&s1), Some(&2));
        let res_shadowed = child.find_with_env(&s1).unwrap();
        assert!(std::ptr::eq(res_shadowed.0, &child));
        assert_eq!(res_shadowed.1, &2);
    }

    #[test]
    /// Verify that lookup of unbound symbols returns `None`
    fn test_unit_env_lookup_missing() {
        let env: Env<'_, i32> = Env::new(None);
        let s = Symbol::empty();
        assert_eq!(env.find(&s), None);
        assert!(env.find_with_env(&s).is_none());

        let parent: Env<'_, i32> = Env::new(None);
        let child = Env::new(Some(&parent));
        assert_eq!(child.find(&s), None);
        assert!(child.find_with_env(&s).is_none());
    }

    #[test]
    /// Verify that `Env::find` compiles when borrowing a stack
    /// environment with a parent reference of lifetime `'a`
    fn test_unit_env_stack_borrow_with_parent_lifetime() {
        let mut parent = Env::new(None);
        let s = Symbol::empty();
        parent.bind(s.clone(), 42);

        fn helper<'a>(parent: &'a Env<'a, i32>, s: Symbol) -> Option<i32> {
            let child = Env::new(Some(parent));
            child.find(&s).copied()
        }

        assert_eq!(helper(&parent, s), Some(42));
    }

    #[test]
    /// Verify that `Env::flatten` correctly collapses environment scopes
    /// and preserves lexical shadowing
    fn test_unit_env_flatten_shadowing() {
        let mut parent = Env::new(None);
        let s = Symbol::empty();
        parent.bind(s.clone(), 1);

        let mut child = Env::new(Some(&parent));
        child.bind(s.clone(), 2);

        let flat = child.flatten();
        assert_eq!(flat.find(&s), Some(&2));
    }
}
