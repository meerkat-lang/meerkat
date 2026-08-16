//! This module implements the Node struct which owns the global
//! service type definitions, performs static validation checks,
//! and transitions to the runtime Manager

//! TODO: The intent of this module right now is to slowly migrate
//! from `Manager` to `Node` over time, as documented on GitHub under
//! Issue 106 "Meerkat Node and Program Representations"

use std::collections::HashMap;
use std::path::Path;

use libp2p::identity::Keypair;

use crate::error::{Error, Result};
use crate::net::network_layer::NetworkLayer;
use crate::net::types::MeerkatMessage;
use crate::net::{
    codec, Address, MessageId, NetworkActor, NetworkCommand, NetworkEvent, NetworkReply, NodeType,
};
use crate::runtime::ast::{apply_updates_to_ast, Stmt};
use crate::runtime::graphs::analysis::compute_dependencies;
use crate::runtime::imports::Imports;
use crate::runtime::interner::Interner;
use crate::runtime::limits::{IMPORT_POLL_INTERVAL_MS, IMPORT_RETRY_DELAY_MS};
use crate::runtime::tt::types::ServiceType;
use crate::runtime::{nameres, tt, Env, Manager};

/// Root manager for executing a Meerkat node
pub struct Node {
    /// Local services registered on this node
    pub local_services: Env<'static, ServiceType>,
    /// Imported services referenced by this node
    pub imported_services: Env<'static, ServiceType>,
    /// Unified program statements AST
    pub unified_ast: Vec<Stmt>,
    /// Process string interner
    pub interner: Interner,
}

impl Node {
    /// Create a new empty Node representing the process context
    ///
    /// Returns:
    ///   `Self`: Initialized Node instance
    pub fn new() -> Self {
        Node {
            local_services: Env::new(None),
            imported_services: Env::new(None),
            unified_ast: Vec::new(),
            interner: Interner::new(),
        }
    }

    /// Load parsed statements from a file path
    ///
    /// Args:
    ///   `path` (`&str`): The file path to parse
    ///
    /// Returns:
    ///   `Result<Vec<Stmt>>`: Parsed statements or an error
    ///
    /// Errors:
    ///   `Error`: If file parsing fails
    pub fn load_file(&mut self, path: &str) -> Result<Vec<Stmt>> {
        crate::runtime::parser::parse_file(path, &mut self.interner)
            .map_err(|e| Error::Message(e.to_string()))
    }

    /// Sends a message to target address and checks returned NetworkReply
    ///
    /// Args:
    ///   `net` (`&mut NetworkActor`): Active network actor
    ///   `addr` (`Address`): Target multiaddress
    ///   `msg` (`MeerkatMessage`): Message payload to send
    ///
    /// Returns:
    ///   `Result<MessageId>`: Assigned message ID if sent successfully
    ///
    /// Errors:
    ///   `Error`: If network sending fails or reply is unexpected
    async fn send_message(
        net: &mut NetworkActor,
        addr: Address,
        msg: MeerkatMessage,
    ) -> Result<MessageId> {
        let reply = net
            .handle_command(NetworkCommand::SendMessage { addr, msg })
            .await;
        match reply {
            NetworkReply::MessageSent { msg_id } => Ok(msg_id),
            NetworkReply::Failure(e) => Err(Error::Message(format!("Send failed: {}", e))),
            NetworkReply::ListenSuccess { .. } | NetworkReply::LocalAddresses { .. } => Err(
                Error::Message("Unexpected reply from SendMessage command".to_string()),
            ),
        }
    }

    /// Sends `cmd` over `net` and registers resulting message ID with `imports`
    ///
    /// Args:
    ///   `net` (`&mut NetworkActor`): Active network actor
    ///   `imports` (`&mut Imports`): Import tracker instance
    ///   `cmd` (`NetworkCommand`): Command to send
    ///   `service_name` (`String`): Target service name
    ///   `target_url` (`String`): Target service multiaddress
    ///
    /// Returns:
    ///   `Result<()>`: Ok if message sent and registered, or error
    ///
    /// Errors:
    ///   `Error`: If sending fails or reply is unexpected
    async fn send_and_register(
        net: &mut NetworkActor,
        imports: &mut Imports<'_>,
        cmd: NetworkCommand,
        service_name: String,
        target_url: String,
        retry_count: u8,
    ) -> Result<()> {
        match cmd {
            NetworkCommand::SendMessage { addr, msg } => {
                let msg_id = Self::send_message(net, addr, msg).await?;
                imports.register_sent_command(msg_id, service_name, target_url, retry_count);
                Ok(())
            }
            _ => Err(Error::Message(
                "Unexpected non-SendMessage command for import registration".to_string(),
            )),
        }
    }

    /// Orchestrates the network boot sequence and resolves imports
    ///
    /// Args:
    ///   `file` (`&str`): Root program `.mkt` file path
    ///   `remote_url_map` (`HashMap<String, String>`): Remote URLs
    ///   `identity` (`Option<Keypair>`): Optional network identity
    ///
    /// Initialize a server network actor listening on IP loopback
    ///
    /// Args:
    ///   `identity` (`Option<Keypair>`): Keypair for stable Peer ID
    ///
    /// Returns:
    ///   `Result<(NetworkActor, String)>`: Active actor and multiaddress
    ///
    /// Errors:
    ///   `Error`: If network creation or listener command fails
    /// Initialize a server network actor listening on IP loopback
    ///
    /// Args:
    ///   `identity` (`Option<Keypair>`): Keypair for stable Peer ID
    ///
    /// Returns:
    ///   `Result<(NetworkActor, String)>`: Active actor and multiaddress
    ///
    /// Errors:
    ///   `Error`: If network creation or listener command fails
    async fn init_network(&mut self, identity: Option<Keypair>) -> Result<(NetworkActor, String)> {
        let mut net = NetworkActor::new_with_identity(NodeType::Server, identity)
            .await
            .map_err(|e| Error::Message(e.to_string()))?;

        let peer_id = net.local_peer_id();
        let listen_cmd = NetworkCommand::Listen {
            addr: Address::new("/ip4/127.0.0.1/tcp/0"),
        };
        let reply = net.handle_command(listen_cmd).await;
        let my_addr = match reply {
            NetworkReply::ListenSuccess { addr } => format!("{}/p2p/{}", addr.0, peer_id),
            NetworkReply::Failure(e) => {
                return Err(Error::Message(format!("Failed to listen: {}", e)));
            }
            NetworkReply::MessageSent { .. } | NetworkReply::LocalAddresses { .. } => {
                return Err(Error::Message(
                    "Unexpected reply from Listen command".to_string(),
                ));
            }
        };

        Ok((net, my_addr))
    }

    /// Resolve local disk service imports into AST statements
    ///
    /// Args:
    ///   `base_ast` (`&[Stmt]`): Root program statements
    ///   `base_dir` (`&Path`): Directory containing local files
    ///
    /// Returns:
    ///   `Result<Vec<Stmt>>`: Imported AST statements
    ///
    /// Errors:
    ///   `Error`: If local file reading or parsing fails
    fn resolve_local_imports(&mut self, base_ast: &[Stmt], base_dir: &Path) -> Result<Vec<Stmt>> {
        let (imports, _) =
            Imports::new(&mut self.interner, HashMap::new(), base_ast, base_dir, "")?;
        Ok(imports.finalize())
    }

    /// Drive P2P network import resolution loop over an active network actor
    ///
    /// Args:
    ///   `base_ast` (`&[Stmt]`): Root program statements
    ///   `base_dir` (`&Path`): Directory containing local files
    ///   `remote_url_map` (`HashMap<String, String>`): Service URL map
    ///   `my_addr` (`&str`): Canonical listening multiaddress
    ///   `net` (`&mut NetworkActor`): Active network actor
    ///
    /// Returns:
    ///   `Result<(Vec<Stmt>, Vec<NetworkEvent>)>`: Imported statements and events
    ///
    /// Errors:
    ///   `Error`: If network fetching or parsing fails
    async fn fetch_network_imports(
        &mut self,
        base_ast: &[Stmt],
        base_dir: &Path,
        remote_url_map: HashMap<String, String>,
        my_addr: &str,
        net: &mut NetworkActor,
    ) -> Result<(Vec<Stmt>, Vec<NetworkEvent>)> {
        let (mut imports, initial_cmds) = Imports::new(
            &mut self.interner,
            remote_url_map,
            base_ast,
            base_dir,
            my_addr,
        )?;

        let mut buffered_events = Vec::new();

        for (cmd, service_name, target_url, retry_count) in initial_cmds {
            Self::send_and_register(
                net,
                &mut imports,
                cmd,
                service_name,
                target_url,
                retry_count,
            )
            .await?;
        }

        while !imports.is_done() {
            if let Some(event) = net.try_recv_event() {
                match event {
                    NetworkEvent::MessageReceived { peer, msg } => match msg {
                        MeerkatMessage::ServiceCodeResponse { source, path, .. } => {
                            let service_name = codec::decode_source_response(&path, &source)?;
                            let new_cmds =
                                imports.on_recv_source(&source, &service_name, base_dir)?;
                            for (cmd, s_name, t_url, retry) in new_cmds {
                                Self::send_and_register(
                                    net,
                                    &mut imports,
                                    cmd,
                                    s_name,
                                    t_url,
                                    retry,
                                )
                                .await?;
                            }
                        }
                        MeerkatMessage::ServiceCodeRequest {
                            request_id,
                            path,
                            reply_to,
                        } => {
                            let response =
                                codec::serve_service_code(request_id, path, &reply_to, base_dir);
                            Self::send_message(net, Address::new(&reply_to), response).await?;
                        }
                        MeerkatMessage::ServiceCodeError {
                            request_id: _,
                            error,
                        } => {
                            return Err(Error::Message(format!(
                                "Remote peer returned service error: {}",
                                error
                            )));
                        }
                        _ => {
                            buffered_events.push(NetworkEvent::MessageReceived { peer, msg });
                        }
                    },
                    NetworkEvent::SendFailed { msg_id, error: _ } => {
                        if let Some((cmd, s_name, t_url, retry)) =
                            imports.on_send_failure(msg_id)?
                        {
                            tokio::time::sleep(std::time::Duration::from_millis(
                                IMPORT_RETRY_DELAY_MS,
                            ))
                            .await;
                            Self::send_and_register(net, &mut imports, cmd, s_name, t_url, retry)
                                .await?;
                        }
                    }
                    NetworkEvent::PeerConnected { peer: _ } => {
                        buffered_events.push(event);
                    }
                    NetworkEvent::PeerDisconnected { peer: _ } => {
                        buffered_events.push(event);
                    }
                }
            } else {
                let retry_cmds = imports.poll_timeouts()?;
                for (cmd, s_name, t_url, retry) in retry_cmds {
                    Self::send_and_register(net, &mut imports, cmd, s_name, t_url, retry).await?;
                }
                tokio::time::sleep(std::time::Duration::from_millis(IMPORT_POLL_INTERVAL_MS)).await;
            }
        }

        Ok((imports.finalize(), buffered_events))
    }

    /// Orchestrates the network boot sequence and resolves imports
    ///
    /// Args:
    ///   `file` (`&str`): Root program `.mkt` file path
    ///   `remote_url_map` (`HashMap<String, String>`): Remote URLs
    ///   `identity` (`Option<Keypair>`): Optional network identity
    ///
    /// Returns:
    ///   `Result<(Option<NetworkActor>, Vec<NetworkEvent>, Vec<Stmt>)>`:
    ///   Tuple of network actor, buffered events, and local program
    ///
    /// Errors:
    ///   `Error`: If dependency fetching or static checks fail
    pub async fn on_node_startup(
        &mut self,
        file: &str,
        remote_url_map: HashMap<String, String>,
        identity: Option<Keypair>,
    ) -> Result<(Option<NetworkActor>, Vec<NetworkEvent>, Vec<Stmt>)> {
        let local_prog = self.load_file(file)?;
        let base_dir = Path::new(file).parent().unwrap_or_else(|| Path::new("."));

        let mut opt_net = None;
        let mut buffered_events = Vec::new();

        let imported_ast = if (!remote_url_map.is_empty()) || (identity.is_some()) {
            let (mut net, my_addr) = self.init_network(identity).await?;
            let (stmts, events) = self
                .fetch_network_imports(&local_prog, base_dir, remote_url_map, &my_addr, &mut net)
                .await?;
            opt_net = Some(net);
            buffered_events = events;
            stmts
        } else {
            self.resolve_local_imports(&local_prog, base_dir)?
        };

        self.unified_ast = local_prog.clone();
        self.unified_ast.extend(imported_ast);

        self.static_checks()?;

        Ok((opt_net, buffered_events, local_prog))
    }

    /// Resolve local disk and remote P2P dependencies into unified AST
    ///
    /// Args:
    ///   `file` (`&str`): Root program entrypoint path
    ///   `remote_url_map` (`HashMap<String, String>`): Service map
    ///
    /// Returns:
    ///   `Result<&mut Self>`: Reference to Self for method chaining
    ///
    /// Errors:
    ///   `Error`: If local reading or P2P import fetching fails
    pub async fn resolve_imports(
        &mut self,
        file: &str,
        remote_url_map: HashMap<String, String>,
    ) -> Result<&mut Self> {
        self.resolve_imports_with_net(file, remote_url_map, None, None)
            .await
    }

    /// Resolve local disk and remote P2P dependencies into unified AST with
    /// an optional pre-initialized network actor
    ///
    /// Args:
    ///   `file` (`&str`): Root program entrypoint path
    ///   `remote_url_map` (`HashMap<String, String>`): Service map
    ///   `provided_net` (`Option<&mut NetworkActor>`): Optional network actor
    ///   `provided_addr` (`Option<&str>`): Optional bound multiaddress
    ///
    /// Returns:
    ///   `Result<&mut Self>`: Reference to Self for method chaining
    ///
    /// Errors:
    ///   `Error`: If local reading or P2P import fetching fails
    pub async fn resolve_imports_with_net(
        &mut self,
        file: &str,
        remote_url_map: HashMap<String, String>,
        provided_net: Option<&mut NetworkActor>,
        provided_addr: Option<&str>,
    ) -> Result<&mut Self> {
        debug_assert!(!file.is_empty(), "file path must not be empty");
        let local_prog = self.load_file(file)?;
        let base_dir = Path::new(file).parent().unwrap_or_else(|| Path::new("."));

        let imported_ast = if remote_url_map.is_empty() {
            self.resolve_local_imports(&local_prog, base_dir)?
        } else if let (Some(net), Some(addr)) = (provided_net, provided_addr) {
            let (stmts, _events) = self
                .fetch_network_imports(&local_prog, base_dir, remote_url_map, addr, net)
                .await?;
            stmts
        } else {
            let (mut net, my_addr) = self.init_network(None).await?;
            let (stmts, _events) = self
                .fetch_network_imports(&local_prog, base_dir, remote_url_map, &my_addr, &mut net)
                .await?;
            stmts
        };

        self.unified_ast = local_prog;
        self.unified_ast.extend(imported_ast);
        Ok(self)
    }

    /// Print Service URLs for all hosted services
    ///
    /// Args:
    ///   `local_ast` (`&[Stmt]`): Local program statements
    ///   `full_addr` (`&str`): Full listening multiaddress
    pub fn print_startup_diagnostics(&self, local_ast: &[Stmt], full_addr: &str) {
        for stmt in local_ast {
            if let Stmt::Service { name, .. } = stmt {
                println!("Service URL: {}/{}", full_addr, self.interner.get(*name));
            }
        }
    }

    /// Consume the Node and create a runtime Manager
    ///
    /// Args:
    ///   `local` (`bool`): Whether running in local mode
    ///   `network` (`Option<NetworkActor>`): Network actor
    ///   `remote_url_map` (`HashMap<String, String>`): Service map
    ///   `local_ast` (`&[Stmt]`): Local program AST
    ///
    /// Returns:
    ///   `Result<Manager>`: Initialized manager
    ///
    /// Errors:
    ///   `Error`: If service instantiation fails
    pub async fn on_manager_startup(
        self,
        local: bool,
        network: Option<NetworkActor>,
        remote_url_map: HashMap<String, String>,
        local_ast: &[Stmt],
    ) -> Result<Manager> {
        let mut manager = Manager::new(self.interner);
        manager.local = local;
        manager.network = network;
        manager.unified_ast = self.unified_ast.clone();
        manager.local_services = self.local_services;

        for (svc_name, url) in &remote_url_map {
            let svc_sym = manager.interner.insert(svc_name);
            manager
                .remote_services
                .insert(svc_sym, Address::new(url.as_str()));
            println!("Remote service '{}' registered at {}", svc_name, url);
        }

        for stmt in local_ast {
            match stmt {
                Stmt::Service { name, decls } => {
                    manager
                        .create_service(*name, decls.clone())
                        .await
                        .map_err(|e| Error::Message(format!("Service error: {}", e)))?;
                    println!("Service '{}' loaded", manager.interner.get(*name));
                }
                Stmt::Update {
                    service_name,
                    decls: _,
                } => {
                    let mut txn = crate::runtime::update::Transaction::new(vec![stmt.clone()]);
                    txn.poll(&mut manager)
                        .await
                        .map_err(|e| Error::Message(format!("Update error: {}", e)))?;
                    println!("Service '{}' updated", manager.interner.get(*service_name));
                }
                Stmt::Atomic { updates } => {
                    if !updates.is_empty() {
                        let mut txn = crate::runtime::update::Transaction::new(updates.clone());
                        txn.poll(&mut manager)
                            .await
                            .map_err(|e| Error::Message(format!("Atomic update error: {}", e)))?;
                        println!(
                            "Atomic update transaction completed ({} updates)",
                            updates.len()
                        );
                    }
                }
                Stmt::Import { .. }
                | Stmt::Test { .. }
                | Stmt::ActionStmt(_)
                | Stmt::Connect { .. }
                | Stmt::Watch { .. } => {}
            }
        }

        Ok(manager)
    }

    /// Perform static analysis checks across the unified AST
    ///
    /// Validates name resolution, types, and reactive dependencies. For
    /// programs containing sequential updates or atomic update blocks,
    /// verifies intermediate service states incrementally at each atomic
    /// transaction boundary and evaluates top-level statements against
    /// the final accumulated AST.
    ///
    /// Returns:
    ///   `Result<()>`: Ok if static checks pass, or error
    ///
    /// Errors:
    ///   `Error`: If name resolution, type checking, or graph analysis fails
    pub fn static_checks(&mut self) -> Result<()> {
        let mut service_ast: Vec<Stmt> = Vec::new();
        let mut top_level_stmts: Vec<Stmt> = Vec::new();
        let mut update_batches: Vec<Vec<Stmt>> = Vec::new();

        for stmt in &self.unified_ast {
            match stmt {
                Stmt::Service { .. } | Stmt::Import { .. } | Stmt::Connect { .. } => {
                    service_ast.push(stmt.clone());
                }
                Stmt::Watch { .. } | Stmt::ActionStmt(_) | Stmt::Test { .. } => {
                    top_level_stmts.push(stmt.clone());
                }
                Stmt::Update { .. } => {
                    update_batches.push(vec![stmt.clone()]);
                }
                Stmt::Atomic { updates } => {
                    if !updates.is_empty() {
                        update_batches.push(updates.clone());
                    }
                }
            }
        }

        if update_batches.is_empty() {
            nameres::resolve(&self.unified_ast).map_err(|e| self.format_nameres_error(e))?;

            let mut local_services = Env::new(None);
            tt::check(&self.unified_ast, &mut local_services)
                .map_err(|e| self.format_tt_error(e))?;
            self.local_services = local_services;

            compute_dependencies(&self.unified_ast, None);

            return Ok(());
        }

        // Initial validation of base service declarations
        nameres::resolve(&service_ast).map_err(|e| self.format_nameres_error(e))?;
        let mut initial_services = Env::new(None);
        tt::check(&service_ast, &mut initial_services).map_err(|e| self.format_tt_error(e))?;
        compute_dependencies(&service_ast, None);

        let last_batch = update_batches.pop().unwrap();

        // Process (n - 1) intermediate atomic batches
        for batch in update_batches {
            service_ast = apply_updates_to_ast(&service_ast, &batch).map_err(|sym| {
                Error::Message(format!(
                    "Target service '{}' for update not found",
                    self.interner.get(sym)
                ))
            })?;

            nameres::resolve(&service_ast).map_err(|e| self.format_nameres_error(e))?;
            let mut step_services = Env::new(None);
            tt::check(&service_ast, &mut step_services).map_err(|e| self.format_tt_error(e))?;
            compute_dependencies(&service_ast, None);
        }

        // Process final atomic transaction batch
        service_ast = apply_updates_to_ast(&service_ast, &last_batch).map_err(|sym| {
            Error::Message(format!(
                "Target service '{}' for update not found",
                self.interner.get(sym)
            ))
        })?;

        let mut final_ast = service_ast;
        final_ast.extend(top_level_stmts);

        nameres::resolve(&final_ast).map_err(|e| self.format_nameres_error(e))?;
        let mut final_services = Env::new(None);
        tt::check(&final_ast, &mut final_services).map_err(|e| self.format_tt_error(e))?;
        compute_dependencies(&final_ast, None);

        self.local_services = final_services;
        self.unified_ast = final_ast;

        Ok(())
    }

    /// Format a name resolution error into a user-facing error message
    ///
    /// Args:
    ///     `err` (`nameres::Error`): The name resolution error to format
    ///
    /// Returns:
    ///     `Error`: The formatted Error::Message
    fn format_nameres_error(&self, err: nameres::Error) -> Error {
        match err {
            nameres::Error::UnknownIdentifier {
                name,
                expected,
                context_name,
            } => {
                let name_str = self.interner.get(name);
                let msg = match context_name {
                    Some(ctx) => {
                        let ctx_str = self.interner.get(ctx);
                        format!(
                            "Unknown identifier '{}' (expected {}) in service '{}'",
                            name_str, expected, ctx_str
                        )
                    }
                    None => format!("Unknown identifier '{}' (expected {})", name_str, expected),
                };
                Error::Message(msg)
            }
            nameres::Error::ForwardReference(name) => {
                let name_str = self.interner.get(name);
                let msg = format!(
                    "Invalid forward reference to uninitialized value '{}'",
                    name_str
                );
                Error::Message(msg)
            }
            nameres::Error::DepthLimit => Error::Message(nameres::Error::DepthLimit.to_string()),
        }
    }

    /// Format a type checking error into a user-facing error message,
    /// resolving interned symbol identifiers to string representations
    ///
    /// Args:
    ///     `err` (`tt::check::Error`): The type checking error to format
    ///
    /// Returns:
    ///     `Error`: The formatted Error::Message
    fn format_tt_error(&self, err: tt::check::Error) -> Error {
        match err {
            tt::check::Error::RecursiveTypeInference { service, member } => {
                let service_str = self.interner.get(service);
                let member_str = self.interner.get(member);
                Error::Message(format!(
                    "type check error: dependency cycle detected at service '{}', member '{}'.",
                    service_str, member_str
                ))
            }
            tt::check::Error::IllegalDependency(member) => {
                let member_str = self.interner.get(member);
                Error::Message(format!(
                    "type check error: illegal eager forward reference or dependency cycle on '{}'",
                    member_str
                ))
            }
            tt::check::Error::UnboundVariable(s) => {
                let var_str = self.interner.get(s);
                Error::Message(format!(
                    "type check error: unbound variable: '{}'.",
                    var_str
                ))
            }
            tt::check::Error::UnknownUpdateTarget(service) => {
                let service_str = self.interner.get(service);
                Error::Message(format!(
                    "Target service '{}' for update not found",
                    service_str
                ))
            }
            tt::check::Error::TypeMismatch { expected, found } => Error::Message(format!(
                "type check error: type mismatch: expected {}, found {}.",
                expected, found
            )),
            tt::check::Error::CannotInferType => {
                Error::Message("type check error: cannot infer type.".to_string())
            }
            tt::check::Error::DepthLimitExceeded => {
                Error::Message("type check error: depth limit exceeded.".to_string())
            }
            tt::check::Error::InvalidTupleArity => {
                Error::Message("type check error: invalid tuple arity.".to_string())
            }
            tt::check::Error::NotAFunction => {
                Error::Message("type check error: not a function.".to_string())
            }
        }
    }

    /// Perform static analysis checks on external program statements
    ///
    /// Args:
    ///   `program` (`&'a [Stmt]`): Statements slice
    ///
    /// Returns:
    ///   `Result<()>`: Ok if checks pass, or error
    ///
    /// Errors:
    ///   `Error`: If name resolution or type checking fails
    /// Perform static analysis checks on a root program entrypoint,
    /// resolving local disk and remote P2P imports prior to checks
    ///
    /// Args:
    ///   `file` (`&str`): Root program entrypoint path
    ///   `remote_url_map` (`&HashMap<String, String>`): Service map
    ///
    /// Returns:
    ///   `Result<()>`: Ok if static checks pass, or error
    ///
    /// Errors:
    ///   `Error`: If parsing, import resolution, or static checks fail
    pub async fn run_static_checks_with_imports(
        &mut self,
        file: &str,
        remote_url_map: &HashMap<String, String>,
    ) -> Result<()> {
        let _ = self
            .on_node_startup(file, remote_url_map.clone(), None)
            .await?;
        Ok(())
    }

    pub fn run_static_checks(&mut self, program: &[Stmt]) -> Result<()> {
        self.unified_ast = program.to_vec();
        self.static_checks()
    }

    /// Start the runtime manager consuming this Node
    ///
    /// Returns:
    ///   `Manager`: The running manager instance
    pub fn start(self) -> Manager {
        Manager::new(self.interner)
    }
}

impl Default for Node {
    /// Create a new empty Node representing the process context
    ///
    /// Returns:
    ///   `Self`: Default empty Node instance
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{ActionStmt, BinOp, Decl, Expr, Value};
    use crate::runtime::tt::Type;

    /// Verify error mapping when apply_updates_to_ast encounters an unknown
    /// target service symbol
    #[test]
    fn test_update_missing_service_err() {
        let mut node = Node::new();
        let sym = node.interner.insert("unknown_s");

        let update_stmt = Stmt::Update {
            service_name: sym,
            decls: Vec::new(),
        };

        let res = apply_updates_to_ast(&node.unified_ast, &[update_stmt]).map_err(|s| {
            Error::Message(format!(
                "Target service '{}' for update not found",
                node.interner.get(s)
            ))
        });

        assert_eq!(
            res,
            Err(Error::Message(
                "Target service 'unknown_s' for update not found".to_string()
            ))
        );
    }

    /// Verify that format_tt_error properly formats UnknownUpdateTarget errors
    #[test]
    fn test_format_tt_error_unknown_update_target() {
        let mut node = Node::new();
        let sym = node.interner.insert("unknown_s");

        let err = tt::check::Error::UnknownUpdateTarget(sym);
        let res = node.format_tt_error(err);
        assert_eq!(
            res,
            Error::Message("Target service 'unknown_s' for update not found".to_string())
        );
    }

    /// Verify that a valid service update block passes static checks
    #[test]
    fn test_update_block_type_checks() {
        let mut node = Node::new();
        let s = node.interner.insert("s");
        let x = node.interner.insert("x");

        let service_stmt = Stmt::Service {
            name: s,
            decls: vec![],
        };
        let update_stmt = Stmt::Update {
            service_name: s,
            decls: vec![Decl::VarDecl {
                name: x,
                ty: Some(Type::Int),
                val: Expr::Literal {
                    val: Value::Int { val: 42 },
                },
            }],
        };

        let program = vec![service_stmt, update_stmt];
        let res = node.run_static_checks(&program);
        assert!(res.is_ok());
    }

    /// Verify that updating an unknown service returns target not found error
    #[test]
    fn test_update_block_unknown_service_errors() {
        let mut node = Node::new();
        let s = node.interner.insert("unknown_s");
        let x = node.interner.insert("x");

        let update_stmt = Stmt::Update {
            service_name: s,
            decls: vec![Decl::VarDecl {
                name: x,
                ty: Some(Type::Int),
                val: Expr::Literal {
                    val: Value::Int { val: 42 },
                },
            }],
        };

        let program = vec![update_stmt];
        let res = node.run_static_checks(&program);
        assert_eq!(
            res,
            Err(Error::Message(
                "Target service 'unknown_s' for update not found".to_string()
            ))
        );
    }

    /// Verify that an update block with type mismatch yields a static check error
    #[test]
    fn test_update_block_type_mismatch_errors() {
        let mut node = Node::new();
        let s = node.interner.insert("s");
        let x = node.interner.insert("x");

        let service_stmt = Stmt::Service {
            name: s,
            decls: vec![],
        };
        let update_stmt = Stmt::Update {
            service_name: s,
            decls: vec![Decl::VarDecl {
                name: x,
                ty: Some(Type::Int),
                val: Expr::Literal {
                    val: Value::Bool { val: true },
                },
            }],
        };

        let program = vec![service_stmt, update_stmt];
        let res = node.run_static_checks(&program);
        assert!(res.is_err());
    }

    /// Verify update block inherits service environment and sequential scoping
    #[test]
    fn test_update_block_inherits_service_env() {
        let mut node = Node::new();
        let s = node.interner.insert("s");
        let a = node.interner.insert("a");
        let b = node.interner.insert("b");

        let service_stmt = Stmt::Service {
            name: s,
            decls: vec![Decl::VarDecl {
                name: a,
                ty: Some(Type::Int),
                val: Expr::Literal {
                    val: Value::Int { val: 10 },
                },
            }],
        };
        let update_stmt = Stmt::Update {
            service_name: s,
            decls: vec![Decl::DefDecl {
                name: b,
                ty: None,
                val: Expr::Variable { name: a },
                is_pub: false,
            }],
        };

        let program = vec![service_stmt, update_stmt];
        let res = node.run_static_checks(&program);
        assert!(res.is_ok());
    }

    /// Verify that forward reference to a new member in update block fails static checks
    #[test]
    fn test_update_block_forward_ref_fails() {
        let mut node = Node::new();
        let s = node.interner.insert("s");
        let a = node.interner.insert("a");
        let b = node.interner.insert("b");

        let service_stmt = Stmt::Service {
            name: s,
            decls: vec![],
        };

        let update_stmt = Stmt::Update {
            service_name: s,
            decls: vec![
                Decl::DefDecl {
                    name: b,
                    ty: None,
                    val: Expr::Variable { name: a },
                    is_pub: false,
                },
                Decl::VarDecl {
                    name: a,
                    ty: Some(Type::Int),
                    val: Expr::Literal {
                        val: Value::Int { val: 10 },
                    },
                },
            ],
        };

        let program = vec![service_stmt, update_stmt];
        let res = node.run_static_checks(&program);
        assert!(res.is_err());
    }

    /// Verify update accumulates fields across sequential update blocks
    #[test]
    fn test_update_accumulates_across_blocks() {
        let mut node = Node::new();
        let s = node.interner.insert("s");
        let a = node.interner.insert("a");
        let b = node.interner.insert("b");

        let service_stmt = Stmt::Service {
            name: s,
            decls: vec![],
        };
        let update1 = Stmt::Update {
            service_name: s,
            decls: vec![Decl::VarDecl {
                name: a,
                ty: Some(Type::Int),
                val: Expr::Literal {
                    val: Value::Int { val: 5 },
                },
            }],
        };
        let update2 = Stmt::Update {
            service_name: s,
            decls: vec![Decl::DefDecl {
                name: b,
                ty: None,
                val: Expr::Variable { name: a },
                is_pub: false,
            }],
        };

        let program = vec![service_stmt, update1, update2];
        let res = node.run_static_checks(&program);
        assert!(res.is_ok());
    }

    /// Verify that atomic blocks containing update statements pass static checks
    #[test]
    fn test_atomic_block_type_checks() {
        let mut node = Node::new();
        let s1 = node.interner.insert("s1");
        let x = node.interner.insert("x");

        let service_stmt = Stmt::Service {
            name: s1,
            decls: vec![],
        };
        let atomic_stmt = Stmt::Atomic {
            updates: vec![Stmt::Update {
                service_name: s1,
                decls: vec![Decl::VarDecl {
                    name: x,
                    ty: Some(Type::Int),
                    val: Expr::Literal {
                        val: Value::Int { val: 100 },
                    },
                }],
            }],
        };

        let program = vec![service_stmt, atomic_stmt];
        let res = node.run_static_checks(&program);
        assert!(res.is_ok());
    }

    /// Verify that sequential atomic blocks type check properly across boundaries
    #[test]
    fn test_sequential_atomic_blocks_type_check() {
        let mut node = Node::new();
        let s1 = node.interner.insert("s1");
        let x = node.interner.insert("x");
        let y = node.interner.insert("y");

        let service_stmt = Stmt::Service {
            name: s1,
            decls: vec![],
        };
        let atomic_stmt1 = Stmt::Atomic {
            updates: vec![Stmt::Update {
                service_name: s1,
                decls: vec![Decl::VarDecl {
                    name: x,
                    ty: Some(Type::Int),
                    val: Expr::Literal {
                        val: Value::Int { val: 100 },
                    },
                }],
            }],
        };
        let atomic_stmt2 = Stmt::Atomic {
            updates: vec![Stmt::Update {
                service_name: s1,
                decls: vec![Decl::DefDecl {
                    name: y,
                    ty: None,
                    val: Expr::Variable { name: x },
                    is_pub: false,
                }],
            }],
        };

        let program = vec![service_stmt, atomic_stmt1, atomic_stmt2];
        let res = node.run_static_checks(&program);
        assert!(res.is_ok());
    }

    /// Verify that an invalid intermediate state in sequential standalone updates
    /// fails static checks, whereas the same updates succeed in an atomic block
    #[test]
    fn test_non_atomic_sequence_intermediate_type_error_fails() {
        let mut node1 = Node::new();
        let s = node1.interner.insert("s");
        let x = node1.interner.insert("x");
        let y = node1.interner.insert("y");

        let service_stmt = Stmt::Service {
            name: s,
            decls: vec![
                Decl::VarDecl {
                    name: x,
                    ty: Some(Type::Int),
                    val: Expr::Literal {
                        val: Value::Int { val: 1 },
                    },
                },
                Decl::DefDecl {
                    name: y,
                    ty: Some(Type::Int),
                    val: Expr::Binop {
                        op: BinOp::Add,
                        expr1: Box::new(Expr::Variable { name: x }),
                        expr2: Box::new(Expr::Literal {
                            val: Value::Int { val: 1 },
                        }),
                    },
                    is_pub: false,
                },
            ],
        };

        let update1 = Stmt::Update {
            service_name: s,
            decls: vec![Decl::VarDecl {
                name: x,
                ty: Some(Type::String),
                val: Expr::Literal {
                    val: Value::String {
                        val: "hello".to_string(),
                    },
                },
            }],
        };

        let update2 = Stmt::Update {
            service_name: s,
            decls: vec![Decl::DefDecl {
                name: y,
                ty: Some(Type::String),
                val: Expr::Variable { name: x },
                is_pub: false,
            }],
        };

        let sequential_program = vec![service_stmt.clone(), update1.clone(), update2.clone()];
        let res_sequential = node1.run_static_checks(&sequential_program);
        assert!(res_sequential.is_err());

        let mut node2 = Node::new();
        let s_2 = node2.interner.insert("s");
        let x_2 = node2.interner.insert("x");
        let y_2 = node2.interner.insert("y");

        let service_stmt_2 = Stmt::Service {
            name: s_2,
            decls: vec![
                Decl::VarDecl {
                    name: x_2,
                    ty: Some(Type::Int),
                    val: Expr::Literal {
                        val: Value::Int { val: 1 },
                    },
                },
                Decl::DefDecl {
                    name: y_2,
                    ty: Some(Type::Int),
                    val: Expr::Binop {
                        op: BinOp::Add,
                        expr1: Box::new(Expr::Variable { name: x_2 }),
                        expr2: Box::new(Expr::Literal {
                            val: Value::Int { val: 1 },
                        }),
                    },
                    is_pub: false,
                },
            ],
        };

        let update1_2 = Stmt::Update {
            service_name: s_2,
            decls: vec![Decl::VarDecl {
                name: x_2,
                ty: Some(Type::String),
                val: Expr::Literal {
                    val: Value::String {
                        val: "hello".to_string(),
                    },
                },
            }],
        };

        let update2_2 = Stmt::Update {
            service_name: s_2,
            decls: vec![Decl::DefDecl {
                name: y_2,
                ty: Some(Type::String),
                val: Expr::Variable { name: x_2 },
                is_pub: false,
            }],
        };

        let atomic_program = vec![
            service_stmt_2,
            Stmt::Atomic {
                updates: vec![update1_2, update2_2],
            },
        ];
        let res_atomic = node2.run_static_checks(&atomic_program);
        assert!(res_atomic.is_ok());
    }

    /// Verify that a test block referencing a field introduced in a second sequential update
    /// passes static checks after all updates are applied
    #[test]
    fn test_sequential_updates_with_test_referencing_later_field() {
        let mut node = Node::new();
        let s = node.interner.insert("s");
        let a = node.interner.insert("a");
        let b = node.interner.insert("b");
        let x = node.interner.insert("x");

        let service_stmt = Stmt::Service {
            name: s,
            decls: vec![],
        };

        let update1 = Stmt::Update {
            service_name: s,
            decls: vec![Decl::VarDecl {
                name: a,
                ty: Some(Type::Int),
                val: Expr::Literal {
                    val: Value::Int { val: 10 },
                },
            }],
        };

        let update2 = Stmt::Update {
            service_name: s,
            decls: vec![Decl::VarDecl {
                name: b,
                ty: Some(Type::String),
                val: Expr::Literal {
                    val: Value::String {
                        val: "hello".to_string(),
                    },
                },
            }],
        };

        let test_stmt = Stmt::Test {
            service_name: s,
            stmts: vec![ActionStmt::Let {
                name: x,
                ty: Some(Type::String),
                expr: Expr::Variable { name: b },
            }],
        };

        let program = vec![service_stmt, update1, update2, test_stmt];
        let res = node.run_static_checks(&program);
        assert!(res.is_ok());
    }

    /// Verify that an empty atomic block does not suppress static type checking on tests
    #[test]
    fn test_empty_atomic_block_does_not_suppress_test_validation() {
        let mut node = Node::new();
        let s = node.interner.insert("s");
        let x = node.interner.insert("x");
        let a = node.interner.insert("a");

        let service_stmt = Stmt::Service {
            name: s,
            decls: vec![Decl::VarDecl {
                name: x,
                ty: Some(Type::Int),
                val: Expr::Literal {
                    val: Value::Int { val: 10 },
                },
            }],
        };

        let empty_atomic = Stmt::Atomic { updates: vec![] };

        let test_stmt = Stmt::Test {
            service_name: s,
            stmts: vec![ActionStmt::Let {
                name: a,
                ty: Some(Type::String),
                expr: Expr::Variable { name: x },
            }],
        };

        let program = vec![service_stmt, empty_atomic, test_stmt];
        let res = node.run_static_checks(&program);
        assert!(res.is_err());
    }

    /// Verify that a watch statement referencing a field introduced in a second sequential update
    /// passes static checks after all updates are applied
    #[test]
    fn test_sequential_updates_with_watch_referencing_later_field() {
        let mut node = Node::new();
        let s = node.interner.insert("s");
        let a = node.interner.insert("a");
        let b = node.interner.insert("b");

        let service_stmt = Stmt::Service {
            name: s,
            decls: vec![],
        };

        let update1 = Stmt::Update {
            service_name: s,
            decls: vec![Decl::VarDecl {
                name: a,
                ty: Some(Type::Int),
                val: Expr::Literal {
                    val: Value::Int { val: 10 },
                },
            }],
        };

        let update2 = Stmt::Update {
            service_name: s,
            decls: vec![Decl::VarDecl {
                name: b,
                ty: Some(Type::String),
                val: Expr::Literal {
                    val: Value::String {
                        val: "hello".to_string(),
                    },
                },
            }],
        };

        let watch_stmt = Stmt::Watch {
            expr: Expr::MemberAccess {
                service_name: s,
                member_name: b,
            },
        };

        let program = vec![service_stmt, update1, update2, watch_stmt];
        let res = node.run_static_checks(&program);
        assert!(res.is_ok());
    }

    /// Verify that an action statement referencing a field introduced in a second sequential update
    /// passes static checks after all updates are applied
    #[test]
    fn test_sequential_updates_with_action_referencing_later_field() {
        let mut node = Node::new();
        let s = node.interner.insert("s");
        let a = node.interner.insert("a");
        let b = node.interner.insert("b");
        let x = node.interner.insert("x");

        let service_stmt = Stmt::Service {
            name: s,
            decls: vec![],
        };

        let update1 = Stmt::Update {
            service_name: s,
            decls: vec![Decl::VarDecl {
                name: a,
                ty: Some(Type::Int),
                val: Expr::Literal {
                    val: Value::Int { val: 10 },
                },
            }],
        };

        let update2 = Stmt::Update {
            service_name: s,
            decls: vec![Decl::VarDecl {
                name: b,
                ty: Some(Type::String),
                val: Expr::Literal {
                    val: Value::String {
                        val: "hello".to_string(),
                    },
                },
            }],
        };

        let action_stmt = Stmt::ActionStmt(ActionStmt::Let {
            name: x,
            ty: Some(Type::String),
            expr: Expr::MemberAccess {
                service_name: s,
                member_name: b,
            },
        });

        let program = vec![service_stmt, update1, update2, action_stmt];
        let res = node.run_static_checks(&program);
        assert!(res.is_ok());
    }
}
