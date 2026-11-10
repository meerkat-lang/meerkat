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

/// Maximum depth of the dependency analysis' recursion (`graphs::effects`):
/// every expression it visits within another and every application it
/// analyzes within another is one level, so the stack the analysis takes is
/// bounded whatever the shape of the code.
pub const MAX_ANALYSIS_DEPTH: usize = 512;

/// Maximum rounds the result of a recursive application may take to stand
/// still in the dependency analysis before the member is given up on
pub const MAX_ANALYSIS_ROUNDS: usize = 128;

/// How long a node waits for the reply to a request before giving up. A
/// `WaitParked` keepalive from the other node starts the wait again.
pub const REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// How often a node sends `WaitParked` to the requesters it is holding, so
/// their `REPLY_TIMEOUT` doesn't fire while they wait
pub const KEEPALIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

// A keepalive must reach a waiter well before its reply timeout, even if one
// is late or lost
const _: () = assert!(KEEPALIVE_INTERVAL.as_millis() * 2 < REPLY_TIMEOUT.as_millis());

/// How long a code update waits for the defs it introduces to get their
/// first values. The node applying a remote update replies once they have
/// them or this runs out, and keeps the requester waiting meanwhile with
/// keepalives
pub const FIRST_VALUE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

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

/// Maximum number of source-identity pairs accepted from a network message.
/// Two 64-byte identifiers per pair allow at most 1 MiB of identifier bytes.
pub const MAX_NET_SOURCE_PAIRS: usize = 8192;

/// Maximum number of clock dimensions accepted from a network message. A
/// clock may name upstream sources this node has never seen, each interned
/// for good, so an oversized clock is refused whole: truncating it would
/// count the missing dimensions as zero and let an inconsistent batch pass
pub const MAX_NET_CLOCK_ENTRIES: usize = 8192;

/// Maximum counter accepted in a clock dimension from a network message.
/// Local writes advance a counter by one, so a wire counter far below the
/// maximum keeps every counter from ever overflowing
pub const MAX_NET_CLOCK_COUNTER: u64 = u64::MAX / 2;

/// Maximum number of services allowed to be imported
pub const MAX_IMPORTED_SERVICES: usize = 256;

/// Maximum number of code updates a node holds for one peer: applied, and
/// waiting up to `FIRST_VALUE_TIMEOUT` for the first values of their new defs
/// before the peer is answered. Another update from that peer is refused
pub const MAX_PENDING_UPDATES_PER_PEER: usize = 32;
