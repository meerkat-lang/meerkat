//! Runtime components for the Meerkat language.
//!
//! This module coordinates AST, interner, interpreter, txn management,
//! type definitions, and semantic analysis modules

pub mod ast;
pub mod env;
pub mod html;
pub mod imports;
pub mod interner;
pub mod interpreter;
pub mod limits;
pub mod manager;
pub mod nameres;
pub mod node;
pub mod parser;
pub mod semantic_analysis;
pub mod tt;
pub mod txn;

pub use env::Env;
pub use html::Html;
pub use imports::Imports;
pub use interner::{Interner, Symbol};
pub use manager::Manager;
pub use node::Node;
