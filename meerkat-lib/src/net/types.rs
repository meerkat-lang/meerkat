use crate::net::ast::{NetActionStmt, NetValue};
use crate::runtime::txn::TxnId;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// A group of member-level and service-level locks to acquire.
///
/// Details the set of members (variables or definitions) to lock
/// for reading and writing, along with an option to acquire a
/// service-level lock.
///
/// Args:
///     service_level_lock (bool): True to lock the whole service.
///     reads (HashSet<String>): Set of member names to read-lock.
///     writes (HashSet<String>): Set of member names to write-lock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockGroup {
    pub service_level_lock: bool,
    pub reads: HashSet<String>,
    pub writes: HashSet<String>,
}

/// Unique identifier for sent messages (for error tracking)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MessageId(pub u64);

/// Canonical internet-routable address represented by `Address`
///
/// Examples:
/// - Server: `/ip4/203.0.113.10/tcp/9000/p2p/12D3...`
/// - Client: `/ip4/203.0.113.10/tcp/9000/p2p/server-id/ws/p2p/client-id`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct Address(pub String);

impl Address {
    /// Creates a new `Address` from the given string
    pub fn new(addr: impl Into<String>) -> Self {
        Address(addr.into())
    }
}

/// Globally unique identity of a service (as opposed to a node)
///
/// Per the `Historiographer` design, a service identity is its global,
/// internet-routable network address including the service slug, e.g.,
/// `/ip4/203.0.113.10/tcp/9000/p2p/12D3.../my_service`
/// This is used as part of the key for all transaction state that
/// mentions a variable, so that the same variable name in two
/// different services (possibly on different nodes) never collides
///
/// For local-only execution where the node has no network address yet,
/// the identity falls back to the service name. On a single node
/// names are unambiguous; the address-based identity takes over once
/// networking is available (and is required for cross-node
/// transactions)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ServiceNetId(pub String);

impl ServiceNetId {
    /// Creates a new `ServiceNetId` from the given string
    pub fn new(id: impl Into<String>) -> Self {
        ServiceNetId(id.into())
    }
}

/// Message types in the `Meerkat` protocol
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MeerkatMessage {
    /// Ping for testing
    Ping { content: String },

    /// Pong response
    Pong { content: String },

    /// Peer announcement with their canonical `Address`
    Announce { peer_addr: Address },

    /// Transaction message (for future use)
    Transaction { tx_id: u64, payload: Vec<u8> },

    /// Propagation message (for future use)
    Propagation { var_id: u64, new_value: Vec<u8> },

    /// Request to look up a member of a service on a remote node
    LookupRequest {
        request_id: u64,
        service: String,
        member: String,
        reply_to: String,
        /// When `Some`, the read joins this transaction: the owning node
        /// acquires and holds a read lock under the shared `txn_id`
        /// until `commit`/`abort`
        /// When `None`, it is a plain unlocked read
        txn_id: Option<TxnId>,
    },

    /// Response to a `LookupRequest` with the serialized value
    LookupResponse { request_id: u64, value: NetValue },

    /// Response indicating lookup failed
    LookupError { request_id: u64, error: String },

    /// Execute an action on a remote service
    ActionRequest {
        request_id: u64,
        service: String,
        stmts: Vec<NetActionStmt>,
        env: Vec<(String, NetValue)>,
        reply_to: String,
        /// When `Some`, the action joins the originator's distributed
        /// transaction: execute under this shared `txn_id` and hold
        /// (do not commit) until a later `Commit`/`Abort`
        /// When `None`, execute standalone and commit immediately
        txn_id: Option<TxnId>,
    },

    /// Response to `ActionRequest`
    ActionResponse {
        request_id: u64,
        success: bool,
        error: Option<String>,
    },

    /// Tell a participant node to commit a distributed transaction:
    /// apply the writes it buffered for `txn_id` and release the
    /// locks it holds
    Commit {
        request_id: u64,
        txn_id: TxnId,
        reply_to: String,
    },

    /// Acknowledgement that a `Commit` was applied (or failed) on a
    /// participant
    CommitResponse {
        request_id: u64,
        success: bool,
        error: Option<String>,
    },

    /// Tell a participant node to abort a distributed transaction:
    /// discard the writes it buffered for `txn_id` and release the
    /// locks it holds
    /// Acknowledged so the originator knows the participant's locks
    /// are freed before it returns (and exits)
    Abort {
        request_id: u64,
        txn_id: TxnId,
        reply_to: String,
    },

    /// Acknowledgement that an `Abort` was processed on a participant
    AbortResponse { request_id: u64 },

    /// Sent by an owner when it parks a transactional request on a
    /// wait queue (wait-die wait): tells the waiting originator the
    /// request is alive and still queued, so it keeps waiting
    /// instead of timing out
    WaitParked { request_id: u64 },

    /// #24: subscribe `listener_service.listener_def` (living on the sender's
    /// node) as a listener on `service.member` (on this node). The owner
    /// registers it and replies with an initial `Update` carrying the current
    /// value. Services, members, and defs are carried as strings rather than
    /// symbols: symbols are interner ids local to one node, but these messages
    /// cross nodes, so each side must resolve the names through its own interner
    /// to know which service/member the request refers to.
    RequestUpdates {
        request_id: u64,
        service: String,
        member: String,
        listener_service: String,
        listener_def: String,
        reply_to: String,
    },

    /// #24: tell a remote listener that `source_service.member` changed (also
    /// used for the initial value on subscribe). The receiver caches `value`
    /// and recomputes `listener_def`.
    Update {
        listener_service: String,
        listener_def: String,
        source_service: String,
        member: String,
        value: NetValue,
    },

    /// #39: request the source of a `.mkt` file by path. A (browser) client
    /// sends this to fetch a file it imports so it can process it locally
    /// (creating services and resolving further imports), like loading a
    /// program normally. The path is a resource path, similar to a URL path.
    ServiceCodeRequest {
        request_id: u64,
        path: String,
        reply_to: String,
    },

    /// #39: response to `ServiceCodeRequest` carrying the whole `.mkt` file
    /// source. The requester processes it through the normal program-loading
    /// path. The source is unbounded in length; only the request path and
    /// reply address are length-limited.
    ServiceCodeResponse {
        request_id: u64,
        path: String,
        source: String,
    },

    /// #39: error responding to a `ServiceCodeRequest`, e.g. an invalid
    /// request (oversized path/reply_to) or a missing resource.
    ServiceCodeError { request_id: u64, error: String },

    /// Eagerly request bulk member-level and service-level locks
    /// across multiple local services.
    ///
    /// Sent by an originator node to a participant node during live
    /// code updates or transaction initialization. The receiver
    /// recursively resolves and read-locks or write-locks all
    /// requested members and their transitive local/cross-service
    /// dependencies under the shared `txn_id`.
    LockRequest {
        request_id: u64,
        txn_id: TxnId,
        services: HashMap<String, LockGroup>,
        reply_to: String,
    },

    /// Response to a `LockRequest` indicating success or failure.
    ///
    /// Sent by a participant node back to the originator. If
    /// `success` is true, all requested locks have been successfully
    /// acquired and are held under the transaction. If false,
    /// contains the error (e.g. due to wait-die abort or timeout).
    LockResponse {
        request_id: u64,
        txn_id: TxnId,
        success: bool,
        error: Option<String>,
    },
}

/// Errors that can occur when sending messages
#[derive(Debug, Clone)]
pub enum SendError {
    /// Could not resolve/reach the `Address`
    UnreachableAddress(Address),

    /// Connection dropped before send completed
    ConnectionLost(String),

    /// Message too large or other protocol error
    ProtocolError(String),
}

/// Describes what kind of node we are
///
/// Determines how `translate_address` behaves
pub enum NodeType {
    /// Server node - can dial IP directly, no translation needed
    Server,
    /// Browser client - can only reach the network via WebSocket
    /// to relay server
    BrowserClient {
        /// WebSocket address of our relay server, e.g.,
        /// `/ip4/server1-ip/tcp/9001/ws/p2p/server1-id`
        relay_server: Address,
    },
}
