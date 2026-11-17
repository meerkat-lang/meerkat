//! Shared, owned names. The index holds only weak references, so a name is
//! removed when its last Symbol is released.

use std::borrow::Borrow;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use crate::runtime::limits::MAX_IDENTIFIER_LENGTH;

type Index = Mutex<HashMap<Arc<str>, Weak<Name>>>;

struct Name {
    text: Arc<str>,
    index: Arc<Index>,
}

impl Drop for Name {
    fn drop(&mut self) {
        let mut index = self.index.lock().unwrap();
        // Another thread may have recreated this spelling after our last
        // strong reference disappeared. Only remove our own entry.
        if index
            .get(&self.text)
            .is_some_and(|entry| std::ptr::eq(entry.as_ptr(), self))
        {
            index.remove(&self.text);
        }
    }
}

/// An owned identifier, compared and hashed by its spelling.
#[derive(Clone)]
pub struct Symbol(Arc<Name>);

impl Symbol {
    pub fn as_str(&self) -> &str {
        &self.0.text
    }

    /// The empty sentinel used by unfinished syntax and default values.
    pub fn empty() -> Self {
        static EMPTY: OnceLock<Symbol> = OnceLock::new();
        EMPTY.get_or_init(|| Interner::new().insert("")).clone()
    }
}

impl Default for Symbol {
    fn default() -> Self {
        Self::empty()
    }
}

impl Borrow<str> for Symbol {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl PartialEq for Symbol {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for Symbol {}

impl PartialOrd for Symbol {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Symbol {
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl Hash for Symbol {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl std::fmt::Debug for Symbol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Symbol").field(&self.as_str()).finish()
    }
}

/// Clones share an index; symbols keep their names alive independently.
#[derive(Clone, Default)]
pub struct Interner {
    index: Arc<Index>,
}

impl Interner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Share an existing name or create it. Callers validate identifiers at
    /// their input boundary, including the identifier length limit.
    pub fn insert(&mut self, text: &str) -> Symbol {
        assert!(
            text.len() <= MAX_IDENTIFIER_LENGTH,
            "it exceeded the maximum identifier limit"
        );
        let mut index = self.index.lock().unwrap();
        // Never drop a Name's last strong reference while holding this lock:
        // both the upgraded and newly created references move to the caller.
        if let Some(name) = index.get(text).and_then(Weak::upgrade) {
            return Symbol(name);
        }
        let text: Arc<str> = Arc::from(text);
        let name = Arc::new(Name {
            text: text.clone(),
            index: self.index.clone(),
        });
        index.insert(text, Arc::downgrade(&name));
        Symbol(name)
    }
}

/// Unit tests for the `Interner` and `Symbol` types
#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that the empty sentinel retains its spelling
    #[test]
    fn test_empty_symbol() {
        assert_eq!(Symbol::empty().as_str(), "");
    }

    /// Verify that basic string insertion yields a new `Symbol` and correct string retrieval
    #[test]
    fn test_basic_insert_and_get() {
        let mut interner = Interner::new();
        let sym = interner.insert("hello");

        assert_eq!(sym.as_str(), "hello");
    }

    /// Verify that inserting duplicate strings returns the same identical `Symbol`
    #[test]
    fn test_deduplication() {
        let mut interner = Interner::new();
        let sym1 = interner.insert("rust");
        let sym2 = interner.insert("rust");

        assert_eq!(sym1, sym2, "Redundant strings must return the same Symbol");
        assert_eq!(
            interner.index.lock().unwrap().len(),
            1,
            "The index should only contain 'rust'"
        );
    }

    /// A symbol remains valid after the interner that created it is dropped.
    #[test]
    fn test_symbol_outlives_interner() {
        let symbol = Interner::new().insert("name");
        assert_eq!(symbol.as_str(), "name");
    }

    /// Verify that inserting multiple unique strings yields unique `Symbol`s for each string
    #[test]
    fn test_multiple_unique_inserts() {
        let mut interner = Interner::new();
        let s1 = interner.insert("a");
        let s2 = interner.insert("b");
        let s3 = interner.insert("c");

        assert_eq!(s1.as_str(), "a");
        assert_eq!(s2.as_str(), "b");
        assert_eq!(s3.as_str(), "c");
        assert_ne!(s1, s2);
    }

    /// Verify that a Symbol's Debug formatting produces the expected string representation
    #[test]
    fn test_symbol_debug_format() {
        let symbol = Interner::new().insert("name");
        assert_eq!(format!("{:?}", symbol), "Symbol(\"name\")");
    }

    /// Verify that Symbol's Default implementation creates the empty sentinel symbol
    #[test]
    fn test_symbol_default() {
        assert_eq!(Symbol::default(), Symbol::empty());
    }

    /// Verify that Interner's Default implementation initializes correctly
    #[test]
    fn test_interner_default() {
        let mut interner = Interner::default();
        assert_eq!(interner.insert("").as_str(), "");
    }

    /// Verify Symbol PartialEq, Eq, and Hash properties
    #[test]
    fn test_symbol_eq_and_hash() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let sym1 = Interner::new().insert("same");
        let sym2 = Interner::new().insert("same");
        let sym3 = Interner::new().insert("other");

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
