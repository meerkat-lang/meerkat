//! This module implements the imports state machine for resolving
//! and fetching import dependencies from local disk or network peers.
//! This is designed to support any kind of event system with `tokio`.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Instant;

use crate::error::{Error, Result};
use crate::net::types::MeerkatMessage;
use crate::net::{Address, MessageId, NetworkCommand};
use crate::runtime::ast::Stmt;
use crate::runtime::interner::{Interner, Symbol};
use crate::runtime::limits::{
    INDIVIDUAL_IMPORT_TIMEOUT_SECS, MAX_IMPORTED_SERVICES, MAX_IMPORT_RETRIES,
};
use crate::runtime::parser;

/// Request tracking info for optimistic retries
#[derive(Debug)]
pub struct PendingRequest {
    /// Name of the service being requested
    pub service_name: String,
    /// Target network URL of the peer
    pub target_url: String,
    /// Instant when the request was sent
    pub start_time: Instant,
    /// Number of retry attempts made so far
    pub retry_count: u8,
}

/// Tuple representing a pending import command, target service name,
/// URL, and retry attempt count
pub type ImportCommand = (NetworkCommand, String, String, u8);

/// One imported file, kept whole so the resolved AST can be emitted in
/// dependency order rather than in the order replies happened to arrive.
struct ImportedModule {
    /// Services this file declares.
    declares: Vec<Symbol>,
    /// Services this file imports.
    imports: Vec<Symbol>,
    /// The file's parsed statements.
    stmts: Vec<Stmt>,
}

/// State machine for resolving module import dependencies
pub struct Imports<'a> {
    interner: &'a mut Interner,
    visited_services: HashSet<Symbol>,
    pending_network: HashMap<MessageId, PendingRequest>,
    pending_services: HashSet<String>,
    remote_url_map: HashMap<String, String>,
    modules: Vec<ImportedModule>,
    request_counter: u64,
    my_addr: String,
}

impl<'a> Imports<'a> {
    /// Create a new Imports state machine and process base AST
    ///
    /// Args:
    ///   `interner` (`&'a mut Interner`): Process interner reference
    ///   `remote_url_map` (`HashMap<String, String>`): Service URL map
    ///   `base_ast` (`&[Stmt]`): Root file parsed statements
    ///   `base_dir` (`&Path`): Directory containing local files
    ///   `my_addr` (`&str`): Canonical listening address of this node
    ///
    /// Returns:
    ///   `Result<(Self, Vec<ImportCommand>)>`: Initialized
    ///   state machine and initial network fetch commands
    ///
    /// Errors:
    ///   `Error`: If local import reading or parsing fails
    pub fn new(
        interner: &'a mut Interner,
        remote_url_map: HashMap<String, String>,
        base_ast: &[Stmt],
        base_dir: &Path,
        my_addr: &str,
    ) -> Result<(Self, Vec<ImportCommand>)> {
        let mut visited_services = HashSet::new();

        // Register local services in visited set to prevent loops
        for stmt in base_ast {
            if let Stmt::Service { name, .. } = stmt {
                visited_services.insert(*name);
            }
        }

        let mut imports = Imports {
            interner,
            visited_services,
            pending_network: HashMap::new(),
            pending_services: HashSet::new(),
            remote_url_map,
            modules: Vec::new(),
            request_counter: 0,
            my_addr: my_addr.to_string(),
        };

        let mut initial_cmds = Vec::new();
        let imports_in_base: Vec<Symbol> = base_ast
            .iter()
            .filter_map(|stmt| match stmt {
                Stmt::Import { service_name, .. } => Some(*service_name),
                Stmt::Service { .. } => None,
                Stmt::ActionStmt(_) => None,
                Stmt::Atomic { .. } => None,
                Stmt::Update { .. } => None,
                Stmt::Connect { .. } => None,
                Stmt::Test { .. } => None,
                Stmt::Watch { .. } => None,
            })
            .collect();

        for sym in imports_in_base {
            let cmds = imports.resolve_import(sym, base_dir)?;
            initial_cmds.extend(cmds);
        }

        Ok((imports, initial_cmds))
    }

    /// Register a sent network command message ID for retry tracking
    ///
    /// Args:
    ///   `msg_id` (`MessageId`): Message ID returned by network actor
    ///   `service_name` (`String`): Service name requested
    ///   `target_url` (`String`): Peer target URL
    ///   `retry_count` (`u8`): Current retry attempt count
    pub fn register_sent_command(
        &mut self,
        msg_id: MessageId,
        service_name: String,
        target_url: String,
        retry_count: u8,
    ) {
        // Remove prior pending entries for the same service to prevent leaks
        self.pending_network
            .retain(|_, req| req.service_name != service_name);

        self.pending_network.insert(
            msg_id,
            PendingRequest {
                service_name,
                target_url,
                start_time: Instant::now(),
                retry_count,
            },
        );

        #[cfg(debug_assertions)]
        {
            debug_assert!(
                self.pending_network.contains_key(&msg_id),
                "pending_network must contain newly registered message ID"
            );
        }
    }

    /// Process incoming source code for a service
    ///
    /// Args:
    ///   `source` (`&str`): Raw source text received
    ///   `service_name` (`&str`): Name of the service received
    ///   `base_dir` (`&Path`): Directory for local imports
    ///
    /// Returns:
    ///   `Result<Vec<ImportCommand>>`: Fetch commands
    ///
    /// Errors:
    ///   `Error`: If source parsing or local disk reading fails
    pub fn on_recv_source(
        &mut self,
        source: &str,
        service_name: &str,
        base_dir: &Path,
    ) -> Result<Vec<ImportCommand>> {
        if self.visited_services.len() >= MAX_IMPORTED_SERVICES {
            return Err(Error::LimitExceeded(format!(
                "imported service count exceeds maximum limit of {}",
                MAX_IMPORTED_SERVICES
            )));
        }

        if !self.pending_services.contains(service_name) {
            // Log unsolicited response rather than failing, which aids testing
            // and speculative pushes
            println!(
                "Received response for service '{}' not in pending set",
                service_name
            );
        }

        self.pending_services.remove(service_name);
        self.pending_network
            .retain(|_, req| req.service_name != service_name);

        let parsed_stmts = parser::parse_string(source, self.interner)
            .map_err(|e| Error::Message(e.to_string()))?;

        // Mark all services in received file as visited
        for stmt in &parsed_stmts {
            if let Stmt::Service { name, .. } = stmt {
                self.visited_services.insert(*name);
            }
        }

        // Keep the file as a unit. It is appended in arrival order, which is
        // not a dependency order: a file's own imports are only resolved after
        // it is recorded, so a dependency always lands *after* its dependent.
        // `finalize` sorts this out.
        self.modules.push(ImportedModule {
            declares: parsed_stmts
                .iter()
                .filter_map(|s| match s {
                    Stmt::Service { name, .. } => Some(*name),
                    _ => None,
                })
                .collect(),
            imports: parsed_stmts
                .iter()
                .filter_map(|s| match s {
                    Stmt::Import { service_name, .. } => Some(*service_name),
                    _ => None,
                })
                .collect(),
            stmts: parsed_stmts.clone(),
        });

        let mut new_cmds = Vec::new();
        for stmt in &parsed_stmts {
            match stmt {
                Stmt::Import {
                    service_name: sym, ..
                } => {
                    let cmds = self.resolve_import(*sym, base_dir)?;
                    new_cmds.extend(cmds);
                }
                Stmt::Service { .. } => {}
                Stmt::ActionStmt(_) => {}
                Stmt::Atomic { .. } => {}
                Stmt::Update { .. } => {}
                Stmt::Connect { .. } => {}
                Stmt::Test { .. } => {}
                Stmt::Watch { .. } => {}
            }
        }

        Ok(new_cmds)
    }

    /// Handle network message send failure with optimistic retry logic
    ///
    /// Args:
    ///   `failed_msg_id` (`MessageId`): Message ID that failed
    ///
    /// Returns:
    ///   `Result<Option<ImportCommand>>`: Retry command
    ///
    /// Errors:
    ///   `Error`: If retry limit has been exceeded
    pub fn on_send_failure(&mut self, failed_msg_id: MessageId) -> Result<Option<ImportCommand>> {
        let pending = match self.pending_network.remove(&failed_msg_id) {
            Some(p) => p,
            None => return Ok(None),
        };

        // If service is no longer pending (already resolved), ignore retry
        if !self.pending_services.contains(&pending.service_name) {
            return Ok(None);
        }

        let new_retry_count = pending.retry_count + 1;
        if new_retry_count > MAX_IMPORT_RETRIES {
            return Err(Error::Message(format!(
                "Import fetch failed for service '{}' after {} retries",
                pending.service_name, MAX_IMPORT_RETRIES
            )));
        }

        self.request_counter = self.request_counter.wrapping_add(1);
        let req_id = self.request_counter;
        let path = format!("{}.mkt", pending.service_name);
        let msg = MeerkatMessage::ServiceCodeRequest {
            request_id: req_id,
            path,
            reply_to: self.my_addr.clone(),
        };
        let cmd = NetworkCommand::SendMessage {
            addr: Address::new(pending.target_url.as_str()),
            msg,
        };

        Ok(Some((
            cmd,
            pending.service_name,
            pending.target_url,
            new_retry_count,
        )))
    }

    /// Poll for pending requests that have timed out and need retrying
    ///
    /// Returns:
    ///   `Result<Vec<ImportCommand>>`: Retry commands for timed out requests
    ///
    /// Errors:
    ///   `Error`: If retry limit has been exceeded for any request
    pub fn poll_timeouts(&mut self) -> Result<Vec<ImportCommand>> {
        let mut to_retry = Vec::new();
        let mut failed_service = None;

        for (msg_id, pending) in &self.pending_network {
            let elapsed = pending.start_time.elapsed().as_secs();
            if elapsed >= INDIVIDUAL_IMPORT_TIMEOUT_SECS {
                let next_retry = pending.retry_count + 1;
                if next_retry > MAX_IMPORT_RETRIES {
                    failed_service = Some((pending.service_name.clone(), MAX_IMPORT_RETRIES));
                    break;
                }
                to_retry.push(*msg_id);
            }
        }

        if let Some((svc, max_retries)) = failed_service {
            return Err(Error::Message(format!(
                "Import fetch failed for service '{}' after {} retries",
                svc, max_retries
            )));
        }

        let mut retry_cmds = Vec::new();
        for msg_id in to_retry {
            if let Some(cmd) = self.on_send_failure(msg_id)? {
                retry_cmds.push(cmd);
            }
        }

        Ok(retry_cmds)
    }

    /// Check if all pending import dependencies are resolved
    ///
    /// Returns:
    ///   `bool`: True if no pending services remain
    pub fn is_done(&self) -> bool {
        self.pending_services.is_empty()
    }

    pub fn get_pending_services(&self) -> Vec<String> {
        self.pending_services.iter().cloned().collect()
    }

    /// Consumes the Imports state machine and returns the resolved AST in
    /// dependency order: a module always precedes every module that imports it
    ///
    /// `tt::check` walks services in AST order against a single flat set of
    /// initialized members, so a service is only checkable once everything it
    /// reads has been declared. Modules are recorded as they arrive, which is
    /// the reverse of what the checker needs for a transitive chain
    /// (`main -> a -> b` records `[a, b]`, but `a` reads `b`), so emit them in
    /// post-order over the import graph instead.
    ///
    /// Returns:
    ///   `Vec<Stmt>`: Dependency-ordered AST of all imported services
    pub fn finalize(self) -> Vec<Stmt> {
        // Which module declares each service, so an import edge can be
        // followed to the module that satisfies it.
        let mut owner: HashMap<Symbol, usize> = HashMap::new();
        for (idx, module) in self.modules.iter().enumerate() {
            for svc in &module.declares {
                owner.entry(*svc).or_insert(idx);
            }
        }

        // Iterative DFS post-order. `done` marks emitted modules; `on_stack`
        // breaks import cycles, which the type checker reports properly once
        // it sees the services (better than looping here).
        let mut ordered: Vec<Stmt> = Vec::new();
        let mut done = vec![false; self.modules.len()];
        let mut on_stack = vec![false; self.modules.len()];
        let mut modules: Vec<Option<Vec<Stmt>>> =
            self.modules.iter().map(|m| Some(m.stmts.clone())).collect();

        for root in 0..self.modules.len() {
            if done[root] {
                continue;
            }
            // (module, whether its dependencies have been pushed already)
            let mut stack: Vec<(usize, bool)> = vec![(root, false)];
            while let Some((idx, expanded)) = stack.pop() {
                if done[idx] {
                    continue;
                }
                if expanded {
                    done[idx] = true;
                    on_stack[idx] = false;
                    if let Some(stmts) = modules[idx].take() {
                        ordered.extend(stmts);
                    }
                    continue;
                }
                on_stack[idx] = true;
                stack.push((idx, true));
                for dep_svc in &self.modules[idx].imports {
                    // An import naming a service declared by the root program
                    // has no module here; nothing to order against.
                    if let Some(&dep) = owner.get(dep_svc) {
                        if dep != idx && !done[dep] && !on_stack[dep] {
                            stack.push((dep, false));
                        }
                    }
                }
            }
        }

        ordered
    }

    /// Private helper to resolve a single import symbol
    fn resolve_import(
        &mut self,
        service_sym: Symbol,
        base_dir: &Path,
    ) -> Result<Vec<ImportCommand>> {
        if self.visited_services.contains(&service_sym) {
            return Ok(Vec::new());
        }

        self.visited_services.insert(service_sym);
        let service_name = self.interner.get(service_sym).to_string();

        if !self.my_addr.is_empty() {
            if let Some(target_url) = self.remote_url_map.get(&service_name).cloned() {
                self.pending_services.insert(service_name.clone());
                self.request_counter = self.request_counter.wrapping_add(1);
                let req_id = self.request_counter;
                let path = format!("{}.mkt", service_name);
                let msg = MeerkatMessage::ServiceCodeRequest {
                    request_id: req_id,
                    path,
                    reply_to: self.my_addr.clone(),
                };
                let cmd = NetworkCommand::SendMessage {
                    addr: Address::new(target_url.as_str()),
                    msg,
                };
                return Ok(vec![(cmd, service_name, target_url, 0)]);
            }
        }

        // Local disk resolution fallback
        let file_path = base_dir.join(format!("{}.mkt", service_name));
        let source = std::fs::read_to_string(&file_path).map_err(|e| {
            Error::Message(format!(
                "Failed to read local import file '{:?}': {}",
                file_path, e
            ))
        })?;

        self.on_recv_source(&source, &service_name, base_dir)
    }
}
