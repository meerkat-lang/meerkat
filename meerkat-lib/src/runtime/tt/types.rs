//! Type system representations
//!
//! This module defines the core type representation structures used during parsing,
//! type checking, and translation of type annotations

use crate::runtime::interner::Symbol;
use crate::runtime::Env;
use std::hash::{Hash, Hasher};

/// Represents a type in the Meerkat language
///
/// This enum models all valid types including primitives,
/// tuples, and function signatures
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Type {
    Int,
    String,
    Bool,
    Unit,
    Tuple(TupleType),
    Func(Box<Type>, Box<Type>),
    List(Box<Type>),
    UnresolvedService(Symbol),
}

/// Type representation of a Meerkat service
///
/// We pair the generic `Env` with a separate `Vec<Symbol>` to track
/// field declaration ordering. This keeps `Env` modular and highly
/// reusable. Using standard `HashMap` inside `Env` is more performant,
/// and separating ordering concerns leads to a simpler design overall
#[derive(Debug, Clone)]
pub struct ServiceType<'a> {
    fields: Env<'a, Type>,
    field_order: Vec<Symbol>,
}

impl<'a> Default for ServiceType<'a> {
    /// Create a new, empty `ServiceType`
    ///
    /// Returns:
    ///     `Self`: The constructed empty `ServiceType`
    fn default() -> Self {
        Self {
            fields: Env::new(None),
            field_order: Vec::new(),
        }
    }
}

impl<'a> ServiceType<'a> {
    /// Get a reference to the fields environment
    ///
    /// Returns:
    ///     `&Env<'a, Type>`: Reference to the environment
    pub fn fields(&self) -> &Env<'a, Type> {
        &self.fields
    }

    /// Get a slice of the field symbols in declaration order
    ///
    /// Returns:
    ///     `&[Symbol]`: The ordered symbols of the fields
    pub fn field_order(&self) -> &[Symbol] {
        &self.field_order
    }

    /// Add a field to the service type
    ///
    /// Args:
    ///     `name` (`Symbol`): The symbol representing the field name
    ///     `ty` (`Type`): The type of the field to add
    ///
    /// Returns:
    ///     `Result<(), &'static str>`: Unit value on success, or an
    ///     error if the field already exists
    pub fn add_field(&mut self, name: Symbol, ty: Type) -> Result<(), &'static str> {
        if self.fields.find(name).is_some() {
            return Err("field already exists");
        }
        self.fields.bind(name, ty);
        self.field_order.push(name);
        Ok(())
    }

    /// Remove a field from the service type
    ///
    /// Args:
    ///     `name` (`Symbol`): The symbol representing the field name
    ///
    /// Returns:
    ///     `Option<Type>`: The removed field type if it existed
    pub fn remove_field(&mut self, name: Symbol) -> Option<Type> {
        let removed = self.fields.remove(name);
        if removed.is_some() {
            self.field_order.retain(|&x| x != name);
        }
        removed
    }

    /// Update an existing field in the service type
    ///
    /// Args:
    ///     `name` (`Symbol`): The symbol representing the field name
    ///     `ty` (`Type`): The new type for the field
    ///
    /// Returns:
    ///     `Result<Option<Type>, &'static str>`: The old field type
    ///     on success, or an error if the field did not exist
    pub fn update_field(&mut self, name: Symbol, ty: Type) -> Result<Option<Type>, &'static str> {
        if self.fields.find(name).is_none() {
            return Err("field does not exist");
        }
        Ok(self.fields.bind(name, ty))
    }
}

// Standard `HashMap` does not implement `Hash` or support ordered
// comparison out of the box. We implement `PartialEq` manually using
// the `field_order` vector to ensure a deterministic, order-respecting
// field equality check. This enables the live update system to compare
// new and old service signatures to detect schema changes
impl<'a> PartialEq for ServiceType<'a> {
    /// Compare two `ServiceType` instances for equality
    ///
    /// Args:
    ///     `other` (`&Self`): The other instance to compare
    ///
    /// Returns:
    ///     `bool`: True if both instances are equal
    fn eq(&self, other: &Self) -> bool {
        if self.field_order != other.field_order {
            return false;
        }
        for name in &self.field_order {
            let ty_self = self.fields.find(*name);
            let ty_other = other.fields.find(*name);
            match (ty_self, ty_other) {
                (Some(ts), Some(to)) if ts == to => continue,
                _ => return false,
            }
        }
        true
    }
}

// Implement `Eq` manually because the internal `Env` type cannot
// derive `Eq` automatically due to its `HashMap` field
impl<'a> Eq for ServiceType<'a> {}

// Implement `Hash` manually using the `field_order` vector to hash
// service fields in a deterministic order. This resolves the lack
// of a standard `Hash` implementation on `HashMap` and provides
// stable keys for indexing service definitions
impl<'a> Hash for ServiceType<'a> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.field_order.hash(state);
        for name in &self.field_order {
            self.fields.find(*name).hash(state);
        }
    }
}

/// A tuple type wrapping a list of types of arity 2 or greater
///
/// This structure enforces the canonical representation invariant
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TupleType(Vec<Type>);

impl TupleType {
    /// Create a new `TupleType` with at least 2 elements
    ///
    /// Args:
    ///     `types` (`Vec<Type>`): The list of types to wrap
    ///
    /// Returns:
    ///     `Result<Self, &'static str>`: The constructed `TupleType`
    ///     if valid
    pub fn new(types: Vec<Type>) -> Result<Self, &'static str> {
        if types.len() < 2 {
            Err("Tuple must contain at least 2 elements")
        } else {
            Ok(Self(types))
        }
    }
}

impl std::ops::Deref for TupleType {
    type Target = [Type];

    /// Dereference to the underlying slice of `Type`
    ///
    /// Returns:
    ///     `&[Type]`: The slice of types
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::fmt::Display for TupleType {
    /// Format the tuple type for display
    ///
    /// Args:
    ///     `f` (`&mut std::fmt::Formatter<'_>`): Formatter
    ///
    /// Returns:
    ///     `std::fmt::Result`: The result of formatting
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "(")?;
        for (i, t) in self.0.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{}", t)?;
        }
        write!(f, ")")
    }
}

/// Represents a function parameter
///
/// This structure holds the name of a parameter and its optional type annotation
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Param {
    pub name: Symbol,
    pub ty: Option<Type>,
}

/// Implement the `Display` trait for the `Type` type
///
/// Provides a human-readable string representation of a type
impl std::fmt::Display for Type {
    /// Format the type for display
    ///
    /// Args:
    ///     `f` (`&mut std::fmt::Formatter<'_>`): The formatter target
    ///
    /// Returns:
    ///     `std::fmt::Result`: The result of the formatting operation
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Type::Int => write!(f, "int"),
            Type::String => write!(f, "string"),
            Type::Bool => write!(f, "bool"),
            Type::Unit => write!(f, "unit"),
            Type::Tuple(ts) => write!(f, "{}", ts),
            Type::Func(t1, t2) => {
                // Determine if the left-hand side is a function type
                // to preserve right-associativity during formatting
                match t1.as_ref() {
                    Type::Func(..) => {
                        write!(f, "({}) -> {}", t1, t2)
                    }
                    Type::Unit => write!(f, "() -> {}", t2),
                    Type::Int
                    | Type::String
                    | Type::Bool
                    | Type::Tuple(_)
                    | Type::List(_)
                    | Type::UnresolvedService(_) => {
                        write!(f, "{} -> {}", t1, t2)
                    }
                }
            }
            Type::List(t) => match t.as_ref() {
                Type::Func(..) => write!(f, "({}) list", t),
                _ => write!(f, "{} list", t),
            },
            Type::UnresolvedService(s) => write!(f, "unresolved_service({:?})", s),
        }
    }
}

/// Implement the `Display` trait for the `Param` type
///
/// Provides a human-readable string representation of a parameter
impl std::fmt::Display for Param {
    /// Format the parameter for display
    ///
    /// Args:
    ///     `f` (`&mut std::fmt::Formatter<'_>`): The formatter target
    ///
    /// Returns:
    ///     `std::fmt::Result`: The result of the formatting operation
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(ty) = &self.ty {
            write!(f, "{:?}: {}", self.name, ty)
        } else {
            write!(f, "{:?}", self.name)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that `Type` formats nested function types to
    /// preserve associativity
    #[test]
    fn test_nested_type_formatting() {
        // case 1: (int -> bool) -> string
        let ty1 = Type::Func(
            Box::new(Type::Func(Box::new(Type::Int), Box::new(Type::Bool))),
            Box::new(Type::String),
        );
        assert_eq!(ty1.to_string(), "(int -> bool) -> string");

        // case 2: int -> bool -> string (which is int -> (bool -> string))
        let ty2 = Type::Func(
            Box::new(Type::Int),
            Box::new(Type::Func(Box::new(Type::Bool), Box::new(Type::String))),
        );
        assert_eq!(ty2.to_string(), "int -> bool -> string");

        // case 3: ((int -> string) -> bool) -> unit
        let ty3 = Type::Func(
            Box::new(Type::Func(
                Box::new(Type::Func(Box::new(Type::Int), Box::new(Type::String))),
                Box::new(Type::Bool),
            )),
            Box::new(Type::Unit),
        );
        assert_eq!(ty3.to_string(), "((int -> string) -> bool) -> unit");

        // case 4: (int -> bool) -> (string -> unit)
        let ty4 = Type::Func(
            Box::new(Type::Func(Box::new(Type::Int), Box::new(Type::Bool))),
            Box::new(Type::Func(Box::new(Type::String), Box::new(Type::Unit))),
        );
        assert_eq!(ty4.to_string(), "(int -> bool) -> string -> unit");

        // case 5: () -> int
        let ty5 = Type::Func(Box::new(Type::Unit), Box::new(Type::Int));
        assert_eq!(ty5.to_string(), "() -> int");

        // case 6: (() -> int) -> bool
        let ty6 = Type::Func(
            Box::new(Type::Func(Box::new(Type::Unit), Box::new(Type::Int))),
            Box::new(Type::Bool),
        );
        assert_eq!(ty6.to_string(), "(() -> int) -> bool");

        // case 7: (unit) -> int
        let ty7 = Type::Func(
            Box::new(Type::Tuple(
                TupleType::new(vec![Type::Unit, Type::Unit]).unwrap(),
            )),
            Box::new(Type::Int),
        );
        assert_eq!(ty7.to_string(), "(unit, unit) -> int");
    }

    /// Verify that `TupleType` enforces arity constraints
    /// and supports `Deref`
    #[test]
    fn test_tuple_type_invariants() {
        // Negative cases
        assert!(TupleType::new(vec![]).is_err());
        assert!(TupleType::new(vec![Type::Int]).is_err());

        // Positive cases
        let types = vec![Type::Int, Type::String];
        let tuple_res = TupleType::new(types.clone());
        assert!(tuple_res.is_ok());
        let tuple = tuple_res.unwrap();

        // Test `Deref` slice behavior
        assert_eq!(tuple.len(), 2);
        assert_eq!(tuple[0], Type::Int);
        assert_eq!(tuple[1], Type::String);
    }

    /// Verify that `ServiceType` encapsulation, mutators,
    /// and `PartialEq` work as expected
    #[test]
    fn test_service_type_mutators_and_equality() {
        let mut interner = crate::runtime::interner::Interner::new();
        let field_a = interner.insert("a");
        let field_b = interner.insert("b");

        let mut st1 = ServiceType::default();
        let mut st2 = ServiceType::default();

        // Initially equal
        assert_eq!(st1, st2);

        // Add fields
        assert!(st1.add_field(field_a, Type::Int).is_ok());
        assert!(st1.add_field(field_b, Type::String).is_ok());

        // Duplicate fields should error
        assert!(st1.add_field(field_a, Type::Bool).is_err());

        // st1 and st2 should not be equal now
        assert_ne!(st1, st2);

        // Make st2 match st1
        assert!(st2.add_field(field_a, Type::Int).is_ok());
        assert!(st2.add_field(field_b, Type::String).is_ok());
        assert_eq!(st1, st2);

        // Update a field
        assert!(st1.update_field(field_a, Type::Bool).is_ok());
        assert_ne!(st1, st2);

        // Updating non-existent field should error
        let field_c = interner.insert("c");
        assert!(st1.update_field(field_c, Type::Int).is_err());

        // Revert update
        assert!(st1.update_field(field_a, Type::Int).is_ok());
        assert_eq!(st1, st2);

        // Remove a field
        assert_eq!(st1.remove_field(field_b), Some(Type::String));
        assert_ne!(st1, st2);

        // Removing non-existent field should return None
        assert_eq!(st1.remove_field(field_c), None);
    }

    /// Verify that `ServiceType::eq` correctly handles missing
    /// entries by treating them as unequal, preventing equality
    /// when the internal state is invalid or corrupted
    #[test]
    fn test_corrupted_service_type_equality() {
        let mut interner = crate::runtime::interner::Interner::new();
        let field_a = interner.insert("a");

        // Construct two ServiceType instances with a field in
        // field_order but missing from the fields map
        let st1 = ServiceType {
            fields: Env::new(None),
            field_order: vec![field_a],
        };
        let st2 = ServiceType {
            fields: Env::new(None),
            field_order: vec![field_a],
        };

        // Previously, st1 == st2 would be true (None == None)
        // With our fix, it must be false
        assert_ne!(st1, st2);
    }

    /// Verify that `ServiceType::hash` correctly handles missing
    /// entries by producing distinct hashes for structurally different
    /// configurations, preventing hash collisions
    #[test]
    fn test_service_type_hash_collisions() {
        use std::collections::hash_map::DefaultHasher;

        let mut interner = crate::runtime::interner::Interner::new();
        let field_a = interner.insert("a");
        let field_b = interner.insert("b");

        // Construct `st1` with field order `[a, b]` but only `a` defined
        let mut fields_st1 = Env::new(None);
        fields_st1.bind(field_a, Type::Int);
        let st1 = ServiceType {
            fields: fields_st1,
            field_order: vec![field_a, field_b],
        };

        // Construct `st2` with field order `[a, b]` but only `b` defined
        let mut fields_st2 = Env::new(None);
        fields_st2.bind(field_b, Type::Int);
        let st2 = ServiceType {
            fields: fields_st2,
            field_order: vec![field_a, field_b],
        };

        // Calculate hashes for both `ServiceType` instances
        let mut h1 = DefaultHasher::new();
        let mut h2 = DefaultHasher::new();
        st1.hash(&mut h1);
        st2.hash(&mut h2);

        // Verify that their hashes are distinct due to the presence markers
        assert_ne!(h1.finish(), h2.finish());
    }
}
