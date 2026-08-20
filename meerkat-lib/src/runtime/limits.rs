//! System limits for Meerkat

/// Maximum allowed length of an identifier in characters
pub const MAX_IDENTIFIER_LENGTH: usize = 64;

/// Maximum allowed length of a string literal in characters
pub const MAX_STRING_LITERAL_LENGTH: usize = 8192;

/// Maximum allowed nesting depth of a type structure during deserialization
pub const MAX_TYPE_DEPTH: usize = 16;

/// #39: max length for network-request path/address strings (file paths and
/// reply addresses). Longer than an identifier but bounded, so a client cannot
/// send an unbounded path or reply_to. Source code in responses is unbounded.
pub const MAX_NET_REQUEST_STRING_LENGTH: usize = 4096;

/// Maximum nesting depth of scope blocks to prevent stack overflows
pub const MAX_SCOPE_DEPTH: usize = 64;

/// Maximum allowed time in seconds for an individual import attempt
pub const INDIVIDUAL_IMPORT_TIMEOUT_SECS: u64 = 2;

/// Maximum number of retry attempts for an individual import request
pub const MAX_IMPORT_RETRIES: u8 = 5;

/// Backoff delay in milliseconds between import retry attempts
pub const IMPORT_RETRY_DELAY_MS: u64 = 200;

/// Polling interval in milliseconds for network import event loop
pub const IMPORT_POLL_INTERVAL_MS: u64 = 10;

/// Maximum length of source code received over network in bytes
pub const MAX_NET_REQUEST_SOURCE_LENGTH: usize = 1_048_576;

/// Maximum number of services allowed to be imported
pub const MAX_IMPORTED_SERVICES: usize = 256;

/// Maximum number of pending update requests allowed per peer in the event queue
pub const MAX_PENDING_UPDATES_PER_PEER: usize = 32;
