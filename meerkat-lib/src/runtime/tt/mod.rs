//! Module for the type system

pub mod check;
pub mod types;

pub use check::check;
pub use check::Error;
pub use types::Param;
pub use types::RecordType;
pub use types::ServiceType;
pub use types::TupleType;
pub use types::Type;
