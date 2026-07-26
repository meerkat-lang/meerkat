//! String interning module
//!
//! Provides the `Interner` and `Symbol` structures to enable forward
//! and reverse lookup of strings
//!
//! This implementation intentionally copies strings to avoid viral
//! lifetime annotations
//!
//! Callers must validate all inputs against the `limits.rs` file.
//! Returning a `Result` from the interner would require pattern
//! matching across the entire library, bloating the codebase
//! massively. The `assert` provides a fail-fast panic to protect
//! the interner's internal state. Input validation is the caller's
//! responsibility exclusively at zero-trust boundary entry points

use std::collections::HashMap;

use crate::runtime::limits::MAX_IDENTIFIER_LENGTH;

/// A numeric representation of an identifier or symbol
///
/// The inner field `id` is private to the module to prevent arbitrary
/// construction of `Symbol` outside of the `interner` module, enforcing
/// that symbols can only be created by `Interner` or sentinel constructors
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Symbol {
    id: u32,
}

/// Implement the `Debug` trait for the `Symbol` struct
///
/// This provides a structured representation of the symbol for debugging
impl std::fmt::Debug for Symbol {
    /// Format the symbol for debugging
    ///
    /// Args:
    ///     `f` (`&mut std::fmt::Formatter<'_>`): The formatter target
    ///
    /// Returns:
    ///     `std::fmt::Result`: The result of the formatting operation
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Symbol({})", self.id)
    }
}

/// Implement the `Display` trait for the `Symbol` struct
///
/// This outputs the raw integer ID of the symbol for standard display formatting
impl std::fmt::Display for Symbol {
    /// Format the symbol for user display
    ///
    /// Args:
    ///     `f` (`&mut std::fmt::Formatter<'_>`): The formatter target
    ///
    /// Returns:
    ///     `std::fmt::Result`: The result of the formatting operation
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.id)
    }
}

/// Implement the `Default` trait for the `Symbol` struct
///
/// This enables creating an empty sentinel symbol by default
impl Default for Symbol {
    /// Get the default sentinel `Symbol`
    ///
    /// Returns:
    ///     `Self`: The default `Symbol` (representing the empty string)
    fn default() -> Self {
        Self::empty()
    }
}

/// Associated functions for the `Symbol` struct
///
/// This contains sentinel constructors for constructing empty symbols
impl Symbol {
    /// Get the default empty symbol representing the empty string
    ///
    /// Returns:
    ///     `Self`: The default empty `Symbol`
    pub const fn empty() -> Self {
        Symbol { id: 0 }
    }
}

/// A string interner that maps strings to unique `Symbol`s
#[derive(Clone)]
pub struct Interner {
    index: HashMap<String, u32>,
    strings: Vec<String>,
    next_id: u32,
}

impl Default for Interner {
    /// Initialize a new `Interner` instance with default settings
    ///
    /// Returns:
    ///     `Self`: The default `Interner` instance
    fn default() -> Self {
        Self::new()
    }
}

impl Interner {
    /// Initialize a new `Interner` instance
    ///
    /// Creates an `Interner` where `0` is reserved for the empty string
    ///
    /// Returns:
    ///     `Self`: The initialized `Interner` instance
    pub fn new() -> Self {
        let index = HashMap::from([(String::new(), 0)]);
        Self {
            index,
            strings: vec![String::new()],
            next_id: 1,
        }
    }

    /// Intern a string slice and return its unique `Symbol`
    ///
    /// If the string slice already exists in the `Interner`, this
    /// method returns the existing `Symbol`. Otherwise, it inserts the
    /// string, assigns a new identifier, and returns the new `Symbol`
    ///
    /// Args:
    ///     `s` (`&str`): The string slice to intern
    ///
    /// Returns:
    ///     `Symbol`: The unique symbol representing the string
    pub fn insert(&mut self, s: &str) -> Symbol {
        assert!(
            s.len() <= MAX_IDENTIFIER_LENGTH,
            "it exceeded the maximum identifier limit"
        );
        if let Some(&id) = self.index.get(s) {
            return Symbol { id };
        }

        let id = self.next_id;
        let _ = self.index.insert(s.to_string(), id);
        self.strings.push(s.to_string());
        self.next_id += 1;
        Symbol { id }
    }

    /// Retrieve the string slice associated with a `Symbol`
    ///
    /// Looks up the string represented by the given `Symbol`. If the
    /// `Symbol` is out of bounds or invalid, returns an empty string
    /// slice
    ///
    /// Args:
    ///     `id` (`Symbol`): The symbol to look up
    ///
    /// Returns:
    ///     `&str`: The string slice associated with the `Symbol`
    pub fn get(&self, id: Symbol) -> &str {
        let idx = id.id as usize;
        self.strings.get(idx).map(|s| s.as_str()).unwrap_or("")
    }
}

/// Unit tests for the `Interner` and `Symbol` types
#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that a newly initialized `Interner` reserves index `0` for the empty string
    #[test]
    fn test_new_interner_has_empty_string_at_zero() {
        let interner = Interner::new();
        assert_eq!(interner.get(Symbol { id: 0 }), "");
        assert_eq!(interner.next_id, 1);
    }

    /// Verify that basic string insertion yields a new `Symbol` and correct string retrieval
    #[test]
    fn test_basic_insert_and_get() {
        let mut interner = Interner::new();
        let sym = interner.insert("hello");

        assert_eq!(sym, Symbol { id: 1 });
        assert_eq!(interner.get(sym), "hello");
    }

    /// Verify that inserting duplicate strings returns the same identical `Symbol`
    #[test]
    fn test_deduplication() {
        let mut interner = Interner::new();
        let sym1 = interner.insert("rust");
        let sym2 = interner.insert("rust");

        assert_eq!(sym1, sym2, "Redundant strings must return the same Symbol");
        assert_eq!(
            interner.strings.len(),
            2,
            "Vector should only contain empty string and 'rust'"
        );
    }

    /// Verify that retrieving an out-of-bounds `Symbol` yields the empty string sentinel
    #[test]
    fn test_out_of_bounds_safety() {
        let interner = Interner::new();
        // Index `99` does not exist, should return empty string
        // sentinel
        assert_eq!(interner.get(Symbol { id: 99 }), "");
    }

    /// Verify that inserting multiple unique strings yields unique `Symbol`s for each string
    #[test]
    fn test_multiple_unique_inserts() {
        let mut interner = Interner::new();
        let s1 = interner.insert("a");
        let s2 = interner.insert("b");
        let s3 = interner.insert("c");

        assert_eq!(interner.get(s1), "a");
        assert_eq!(interner.get(s2), "b");
        assert_eq!(interner.get(s3), "c");
        assert_ne!(s1, s2);
    }

    /// Verify that a Symbol's Debug formatting produces the expected string representation
    #[test]
    fn test_symbol_debug_format() {
        let symbol = Symbol { id: 42 };
        assert_eq!(format!("{:?}", symbol), "Symbol(42)");
    }

    /// Verify that Symbol's Default implementation creates the empty sentinel symbol
    #[test]
    fn test_symbol_default() {
        assert_eq!(Symbol::default(), Symbol::empty());
    }

    /// Verify that Interner's Default implementation initializes correctly
    #[test]
    fn test_interner_default() {
        let interner = Interner::default();
        assert_eq!(interner.get(Symbol::empty()), "");
    }

    /// Verify that Symbol implements Display correctly
    #[test]
    fn test_symbol_display_format() {
        let symbol = Symbol { id: 100 };
        assert_eq!(format!("{}", symbol), "100");
    }

    /// Verify Symbol PartialEq, Eq, and Hash properties
    #[test]
    fn test_symbol_eq_and_hash() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let sym1 = Symbol { id: 10 };
        let sym2 = Symbol { id: 10 };
        let sym3 = Symbol { id: 20 };

        assert!(sym1 == sym2);
        assert!(sym1 != sym3);

        let mut h1 = DefaultHasher::new();
        sym1.hash(&mut h1);
        let mut h2 = DefaultHasher::new();
        sym2.hash(&mut h2);

        assert_eq!(h1.finish(), h2.finish());
    }

    /// Verify that inserting an identifier exceeding the length
    /// limit panics
    #[test]
    #[should_panic(expected = "it exceeded the maximum identifier limit")]
    fn test_interner_exceeds_identifier_limit_panics() {
        let mut interner = Interner::new();
        let long_ident = "a".repeat(MAX_IDENTIFIER_LENGTH + 1);
        interner.insert(&long_ident);
    }
}
