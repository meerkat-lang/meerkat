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
use crate::runtime::ast::Stmt;
use crate::runtime::imports::Imports;
use crate::runtime::interner::Interner;
use crate::runtime::limits::{IMPORT_POLL_INTERVAL_MS, IMPORT_RETRY_DELAY_MS};
use crate::runtime::tt::types::ServiceType;
use crate::runtime::{nameres, tt, Env, Manager};

/// Root manager for executing a Meerkat node
pub struct Node<'a> {
    /// Local services registered on this node
    pub local_services: Env<'a, ServiceType<'a>>,
    /// Imported services referenced by this node
    pub imported_services: Env<'a, ServiceType<'a>>,
    /// Unified program statements AST
    pub unified_ast: Vec<Stmt>,
    /// Process string interner
    pub interner: Interner,
}

impl<'a> Node<'a> {
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
                                imports.on_recv_source(&source, &service_name, base_dir, false)?;
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

        for (svc_name, url) in &remote_url_map {
            let svc_sym = manager.interner.insert(svc_name);
            manager
                .remote_services
                .insert(svc_sym, Address::new(url.as_str()));
            println!("Remote service '{}' registered at {}", svc_name, url);
        }

        for stmt in local_ast {
            if let Stmt::Service { name, decls } = stmt {
                manager
                    .create_service(*name, decls.clone())
                    .await
                    .map_err(|e| Error::Message(format!("Service error: {}", e)))?;
                println!("Service '{}' loaded", manager.interner.get(*name));
            }
        }

        Ok(manager)
    }

    /// Perform static analysis checks on unified service declarations
    ///
    /// Returns:
    ///   `Result<()>`: Ok if checks pass, or error
    ///
    /// Errors:
    ///   `Error`: If name resolution or type checking fails
    pub fn static_checks(&mut self) -> Result<()> {
        nameres::resolve(&self.unified_ast).map_err(|e| match e {
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
            nameres::Error::DepthLimit => Error::Message(e.to_string()),
        })?;

        let mut local_services = Env::new(None);
        tt::check(&self.unified_ast, &mut local_services).map_err(|e| self.format_tt_error(e))
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
            tt::check::Error::DependencyCycle { service, member } => {
                let service_str = self.interner.get(service);
                let member_str = self.interner.get(member);
                Error::Message(format!(
                    "type check error: dependency cycle detected at service '{}', member '{}'.",
                    service_str, member_str
                ))
            }
            tt::check::Error::UnboundVariable(s) => {
                let var_str = self.interner.get(s);
                Error::Message(format!(
                    "type check error: unbound variable: '{}'.",
                    var_str
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

    pub fn run_static_checks(&mut self, program: &'a [Stmt]) -> Result<()> {
        nameres::resolve(program).map_err(|e| match e {
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
            nameres::Error::DepthLimit => Error::Message(e.to_string()),
        })?;

        tt::check(program, &mut self.local_services).map_err(|e| self.format_tt_error(e))
    }

    /// Start the runtime manager consuming this Node
    ///
    /// Returns:
    ///   `Manager`: The running manager instance
    pub fn start(self) -> Manager {
        Manager::new(self.interner)
    }
}

impl<'a> Default for Node<'a> {
    /// Create a new empty Node representing the process context
    ///
    /// Returns:
    ///   `Self`: Default empty Node instance
    fn default() -> Self {
        Self::new()
    }
}
