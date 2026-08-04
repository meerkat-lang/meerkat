mod repl;

use clap::Parser;
use meerkat_lib::net::network_layer::NetworkLayer;
use meerkat_lib::net::types::NodeType;
use meerkat_lib::net::NetworkActor;
use meerkat_lib::net::{
    codec, Address, MeerkatMessage, NetworkCommand, NetworkEvent, NetworkReply, ServiceNetId,
};
use meerkat_lib::runtime::ast::{AstPrinter, Stmt};
use meerkat_lib::runtime::interner::Interner;
use meerkat_lib::runtime::interpreter::EvalError;
use meerkat_lib::runtime::manager::ParkedRequest;
use meerkat_lib::runtime::txn::WaitKey;
use meerkat_lib::runtime::{parser, Manager, Node};
use std::collections::HashSet;
use std::error::Error;

#[cfg(debug_assertions)]
use meerkat_lib::net::types::LockGroup;
#[cfg(debug_assertions)]
use meerkat_lib::runtime::txn::TxnId;
#[cfg(debug_assertions)]
use std::collections::HashMap;

/// #151: load a persistent libp2p identity keypair from `path`, or create and
/// save one if the file does not yet exist. Using the same file across runs
/// keeps the node's Peer ID stable, so a web page can embed a fixed server
/// address. The keypair is stored in libp2p's protobuf encoding.
///
/// The file holds a private key, so on Unix it is created atomically with
/// owner-only (0600) permissions using `create_new`; this leaves no window in
/// which the key is world-readable and avoids a check-then-write race.
fn load_or_create_identity(
    path: &std::path::Path,
) -> Result<meerkat_lib::net::identity::Keypair, Box<dyn Error>> {
    use meerkat_lib::net::identity::Keypair;
    // Try to load an existing key first; only generate one if it is absent.
    // Reading first (rather than checking `exists()`) avoids a time-of-check
    // to time-of-use gap between the check and the write.
    match std::fs::read(path) {
        Ok(bytes) => Ok(Keypair::from_protobuf_encoding(&bytes)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let keypair = Keypair::generate_ed25519();
            let bytes = keypair.to_protobuf_encoding()?;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            use std::io::Write;
            // `create_new` fails with AlreadyExists if another process created
            // the file in the window since our read above; in that case just
            // load the key it wrote rather than failing startup.
            match options.open(path) {
                Ok(mut file) => {
                    file.write_all(&bytes)?;
                    Ok(keypair)
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let bytes = std::fs::read(path)?;
                    Ok(Keypair::from_protobuf_encoding(&bytes)?)
                }
                Err(e) => Err(Box::new(e)),
            }
        }
        Err(e) => Err(Box::new(e)),
    }
}

/// Helper to send a message over net actor and log if message sending fails
///
/// TODO(GitHub Issue #174): The current routing implementation relies
/// on payload-provided `reply_to` fields, which lack transport-layer
/// authentication context. Moving forward, it may be beneficial to
/// align the trust boundary by utilizing authenticated transport
/// identities.
///
/// Args:
///   `net` (`&mut NetworkActor`): Active network actor
///   `reply_to` (`&str`): Target multiaddress string
///   `msg` (`MeerkatMessage`): Message payload to send
async fn send_net_msg(net: &mut NetworkActor, reply_to: &str, msg: MeerkatMessage) {
    let reply = net
        .handle_command(NetworkCommand::SendMessage {
            addr: Address::new(reply_to),
            msg,
        })
        .await;
    if let NetworkReply::Failure(e) = reply {
        log::warn!("Failed to send network message to {}: {}", reply_to, e);
    }
}

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    /// Input file to run. Omit to launch the interactive REPL.
    #[arg(short = 'f', long = "file")]
    input_file: Option<String>,

    #[arg(short = 'v', long = "verbose", default_value_t = false)]
    verbose: bool,

    /// Server mode: start a server providing the services in the input file
    #[arg(short = 's', long = "server", default_value_t = false)]
    server: bool,

    /// Remote service URLs: -i <url> maps the service slug to a remote address
    #[arg(short = 'i', long = "import-url")]
    import_urls: Vec<String>,

    /// Port to listen on in server mode (default: 9000)
    #[arg(short = 'p', long = "port", default_value_t = 9000)]
    port: u16,

    /// #39: WebSocket port for browser (wasm) clients in server mode.
    /// Browsers cannot dial a raw TCP multiaddr, so the server listens on a
    /// second, WebSocket address. Defaults to `port + 1`.
    #[arg(long = "ws-port")]
    ws_port: Option<u16>,

    /// #151: path to a persistent identity keypair. If the file exists it is
    /// loaded, giving a stable Peer ID across restarts; otherwise a new keypair
    /// is generated and saved there. Omit for an ephemeral random identity.
    #[arg(long = "identity")]
    identity: Option<std::path::PathBuf>,

    /// Bind to loopback/localhost only (force 127.0.0.1 instead of public IP)
    #[arg(long = "local", default_value_t = false)]
    local: bool,

    /// Perform static checks and terminate immediately
    #[arg(long = "check", default_value_t = false)]
    check_only: bool,

    /// Emit AST to `stdout`
    #[arg(long = "ast", default_value_t = false)]
    ast: bool,

    /// Watch mode: subscribe to cross-service dependencies and print change
    /// notifications asynchronously as they arrive (issue #24)
    #[arg(long = "watch", default_value_t = false)]
    watch: bool,

    /// Run lock group cascade test client (debug builds only).
    /// Accepts a test case name; requires -i flags to resolve
    /// remote service addresses.
    #[cfg(debug_assertions)]
    #[arg(long = "test-locks")]
    test_locks: Option<String>,
}

#[tokio::main]
pub async fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();

    // Intercept normal node execution to run the transitive
    // lock-group test client. Only compiled in debug builds;
    // the flag does not exist in release binaries.
    #[cfg(debug_assertions)]
    if let Some(ref test_case) = args.test_locks {
        return run_lock_test_client(test_case, &args.import_urls).await;
    }

    let log_level = if args.verbose {
        log::LevelFilter::Info
    } else {
        log::LevelFilter::Warn
    };
    env_logger::Builder::from_default_env()
        .filter_level(log_level)
        .init();

    // Build slug -> remote address map from -i flags
    let mut remote_url_map: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for url in &args.import_urls {
        if let Some(slug) = url.split('/').next_back() {
            remote_url_map.insert(slug.to_string(), url.clone());
        }
    }

    let mut node = Node::new();

    match args.input_file {
        Some(ref file) => {
            let prog = node
                .load_file(file)
                .map_err(|e| format!("Parse error: {}", e))?;

            // This must appear prior to `check_only` or it will never
            // print. These modes are designed to work both in
            // isolation and in tandem
            if args.ast {
                let printer = AstPrinter::new(&node.interner);
                printer.print_program(&prog);
            }

            let mut opt_server_net: Option<NetworkActor> = None;
            let mut opt_server_addr: Option<String> = None;

            if args.server {
                let identity_keypair = match args.identity.as_ref() {
                    Some(path) => Some(load_or_create_identity(path)?),
                    None => None,
                };
                let mut net =
                    NetworkActor::new_with_identity(NodeType::Server, identity_keypair).await?;
                let listen_ip = if args.local { "127.0.0.1" } else { "0.0.0.0" };
                let listen_addr = Address::new(format!("/ip4/{}/tcp/{}", listen_ip, args.port));
                let reply = net
                    .handle_command(NetworkCommand::Listen { addr: listen_addr })
                    .await;
                let actual_addr = listen_success_addr(reply)?;

                let dummy_manager = Manager::new(node.interner.clone());
                let node_ip = if args.local {
                    "127.0.0.1".to_string()
                } else {
                    dummy_manager.get_node_ip()
                };
                let peer_id = net.local_peer_id();
                let actual_addr_str = actual_addr
                    .0
                    .replace("0.0.0.0", &node_ip)
                    .replace("127.0.0.1", &node_ip);
                let full_addr = format!("{}/p2p/{}", actual_addr_str, peer_id);

                opt_server_net = Some(net);
                opt_server_addr = Some(full_addr);
            }

            // Perform static validation checks on the parsed program
            // statements before executing or starting the server
            node.resolve_imports_with_net(
                file,
                remote_url_map.clone(),
                opt_server_net.as_mut(),
                opt_server_addr.as_deref(),
            )
            .await
            .map_err(|e| format!("Import error: {}", e))?
            .static_checks()
            .map_err(|e| format!("Static check error: {}", e))?;

            // This mode must appear before `server` args check in
            // order to properly stop execution. Logic for static
            // checks must not occur in this branch, as the intent
            // of this mode is to simply halt after the static
            // semantics phase of the interpreter/compiler. See
            // also: above comment(s)
            if args.check_only {
                return Ok(());
            }

            let interner = node.interner.clone();

            if args.server {
                let server_net = opt_server_net
                    .expect("Server network should be initialized when args.server is true");
                let server_addr = opt_server_addr
                    .expect("Server address should be initialized when args.server is true");

                run_server(
                    prog,
                    file,
                    remote_url_map,
                    ServerConfig {
                        port: args.port,
                        ws_port: args.ws_port,
                        local: args.local,
                        identity: args.identity,
                    },
                    interner,
                    Some((server_net, server_addr)),
                )
                .await
            } else {
                run_client(prog, file, remote_url_map, args.local, args.watch, interner).await
            }
        }
        None => {
            if args.server || args.check_only || args.ast || args.watch {
                return Err("Expected a .mkt file (-f) for --server, --check, \
                     --ast, or --watch mode."
                    .into());
            }
            let mut manager = node.start();
            manager.local = args.local;
            repl::run_repl(manager, remote_url_map).await
        }
    }
}

/// Run a participant request (initial dispatch or a woken waiter) and either
/// send its reply, or, if the requesting transaction is older than a current
/// lock holder (wait-die), park it on the contended variable's queue to be
/// re-run when that lock frees.
async fn run_and_reply_or_park(manager: &mut Manager, parked: ParkedRequest) {
    match parked {
        ParkedRequest::Action {
            request_id,
            reply_to,
            service,
            stmts,
            env,
            tid,
        } => {
            match manager
                .execute_action_participant(service, &stmts, &env, tid.clone())
                .await
            {
                Err(EvalError::WaitOn(key)) => {
                    manager.park_request_key(
                        key,
                        ParkedRequest::Action {
                            request_id,
                            reply_to,
                            service,
                            stmts,
                            env,
                            tid,
                        },
                    );
                }
                other => {
                    let response = MeerkatMessage::ActionResponse {
                        request_id,
                        success: other.is_ok(),
                        error: other.err().map(|e| e.to_string()),
                    };
                    if let Some(net) = manager.network.as_mut() {
                        send_net_msg(net, &reply_to, response).await;
                    }
                }
            }
        }
        ParkedRequest::Lookup {
            request_id,
            reply_to,
            service,
            member,
            tid,
        } => {
            match manager
                .remote_read_participant(service, member, tid.clone())
                .await
            {
                Err(EvalError::WaitOn(key)) => {
                    manager.park_request_key(
                        key,
                        ParkedRequest::Lookup {
                            request_id,
                            reply_to,
                            service,
                            member,
                            tid,
                        },
                    );
                }
                Ok(val) => {
                    let response = match codec::encode_value(&val, &manager.interner) {
                        Ok(enc_val) => MeerkatMessage::LookupResponse {
                            request_id,
                            value: enc_val,
                        },
                        Err(e) => MeerkatMessage::LookupError {
                            request_id,
                            error: e.to_string(),
                        },
                    };
                    if let Some(net) = manager.network.as_mut() {
                        send_net_msg(net, &reply_to, response).await;
                    }
                }
                Err(e) => {
                    let response = MeerkatMessage::LookupError {
                        request_id,
                        error: e.to_string(),
                    };
                    if let Some(net) = manager.network.as_mut() {
                        send_net_msg(net, &reply_to, response).await;
                    }
                }
            }
        }
        // Re-dispatch a previously parked LockRequest message.
        // Attempts lock acquisition again under the transaction.
        ParkedRequest::Lock {
            request_id,
            reply_to,
            txn_id,
            services,
        } => {
            match manager
                .handle_lock_request(txn_id.clone(), services.clone())
                .await
            {
                // Defensive check: If lock acquisition is blocked
                // again (e.g., by an older transaction), park the
                // request in the queue to await future release.
                Err(EvalError::WaitOn(key)) => {
                    manager.park_request_key(
                        key,
                        ParkedRequest::Lock {
                            request_id,
                            reply_to,
                            txn_id,
                            services,
                        },
                    );
                }
                // Terminal outcome: Lock request either succeeded
                // completely or aborted (died under wait-die). Send
                // the result back to the originator.
                other => {
                    let response = MeerkatMessage::LockResponse {
                        request_id,
                        txn_id,
                        success: other.is_ok(),
                        error: other.err().map(|e| e.to_string()),
                    };
                    if let Some(net) = manager.network.as_mut() {
                        send_net_msg(net, &reply_to, response).await;
                    }
                }
            }
        }
    }
}

/// After a holder releases its locks on commit or abort, re-dispatch the parked
/// requests waiting on the freed variables, oldest first.
async fn wake_ready(manager: &mut Manager, freed: HashSet<WaitKey>) {
    for parked in manager.take_ready_waiters(&freed) {
        run_and_reply_or_park(manager, parked).await;
    }
}

fn listen_success_addr(reply: NetworkReply) -> Result<Address, Box<dyn Error>> {
    match reply {
        NetworkReply::ListenSuccess { addr } => Ok(addr),
        NetworkReply::Failure(e) => Err(e.into()),
        NetworkReply::MessageSent { .. } | NetworkReply::LocalAddresses { .. } => {
            Err("Unexpected reply".into())
        }
    }
}

pub(crate) async fn import_from_file(
    manager: &mut Manager,
    explicit_path: bool,
    import_path: std::path::PathBuf,
    service_name: Symbol,
) -> Result<Option<String>, Box<dyn Error>> {
    let import_stmts = parser::parse_file(import_path.to_str().unwrap(), &mut manager.interner)
        .map_err(|e| format!("Import parse error: {}", e))?;
    let mut services = import_stmts.into_iter().filter_map(|stmt| match stmt {
        Stmt::Service { name, decls } => Some((name, decls)),
        _ => None,
    });
    if explicit_path {
        if let Some((name, decls)) = services.find(|(name, _)| *name == service_name) {
            manager
                .create_service(name, decls)
                .await
                .map_err(|e| format!("Imported service '{}': {}", manager.interner.get(name), e))?;
            Ok(Some(format!(
                "Imported service: {}.",
                manager.interner.get(service_name)
            )))
        } else {
            Err(format!(
                "Service '{}' not found in '{}'",
                manager.interner.get(service_name),
                import_path.to_str().unwrap(),
            )
            .into())
        }
    } else {
        let mut loaded = Vec::new();
        for (name, decls) in services {
            manager
                .create_service(name, decls)
                .await
                .map_err(|e| format!("Imported service '{}': {}", manager.interner.get(name), e))?;
            loaded.push(manager.interner.get(name).to_string());
        }
        Ok(Some(format!("Imported service(s): {}.", loaded.join(", "))))
    }
}

/// #151: server runtime configuration, grouped so related settings share a
/// single home and `run_server` keeps a small, readable signature.
struct ServerConfig {
    port: u16,
    ws_port: Option<u16>,
    local: bool,
    identity: Option<std::path::PathBuf>,
}

async fn run_server(
    prog: Vec<Stmt>,
    input_file: &str,
    remote_url_map: std::collections::HashMap<String, String>,
    config: ServerConfig,
    interner: Interner,
    pre_init: Option<(NetworkActor, String)>,
) -> Result<(), Box<dyn Error>> {
    // #39: the directory the server was started from is the root for serving
    // `.mkt` files: a ServiceCodeRequest names a file by path, which is
    // resolved (safely) against this base directory and read on demand.
    let served_base_dir = std::path::Path::new(input_file)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf();

    let mut manager = Manager::new(interner);
    manager.local = config.local;

    let (mut net, full_addr) = match pre_init {
        Some(pair) => pair,
        None => {
            // #151: when an identity file is configured, load (or create) a
            // persistent keypair so the Peer ID is stable across restarts.
            let identity_keypair = match config.identity {
                Some(ref path) => Some(load_or_create_identity(path)?),
                None => None,
            };
            let mut net =
                NetworkActor::new_with_identity(NodeType::Server, identity_keypair).await?;
            let node_ip = manager.get_node_ip();
            let listen_ip = if config.local { "127.0.0.1" } else { "0.0.0.0" };
            let listen_addr = Address::new(format!("/ip4/{}/tcp/{}", listen_ip, config.port));
            let reply = net
                .handle_command(NetworkCommand::Listen { addr: listen_addr })
                .await;
            let actual_addr = listen_success_addr(reply)?;

            let peer_id = net.local_peer_id();
            // Replace loopback/unspecified with actual node IP
            let actual_addr_str = actual_addr
                .0
                .replace("0.0.0.0", &node_ip)
                .replace("127.0.0.1", &node_ip);
            let full_addr = format!("{}/p2p/{}", actual_addr_str, peer_id);
            (net, full_addr)
        }
    };

    let node_ip = manager.get_node_ip();
    let listen_ip = if config.local { "127.0.0.1" } else { "0.0.0.0" };
    let peer_id = net.local_peer_id();
    println!("Server listening at: {}", full_addr);

    // #39: browser (wasm) clients can only speak WebSocket, so listen on a
    // second address for them. The TCP address above stays canonical: native
    // peers dial it, and it is what service URLs and reply addresses use.
    let ws_port = match config.ws_port {
        Some(p) => p,
        None => config
            .port
            .checked_add(1)
            .ok_or("port 65535 has no room for a default WebSocket port; pass --ws-port")?,
    };
    let ws_listen_addr = Address::new(format!("/ip4/{}/tcp/{}/ws", listen_ip, ws_port));
    let ws_reply = net
        .handle_command(NetworkCommand::Listen {
            addr: ws_listen_addr,
        })
        .await;
    let actual_ws_addr = listen_success_addr(ws_reply)?;
    let actual_ws_addr_str = actual_ws_addr
        .0
        .replace("0.0.0.0", &node_ip)
        .replace("127.0.0.1", &node_ip);
    let ws_full_addr = format!("{}/p2p/{}", actual_ws_addr_str, peer_id);
    println!("Browser clients connect at: {}", ws_full_addr);

    // Print service URLs
    for stmt in &prog {
        if let Stmt::Service { name, .. } = stmt {
            println!("Service URL: {}/{}", full_addr, manager.interner.get(*name));
        }
    }

    // Register any remote services from -i flags
    for (svc_name, url) in &remote_url_map {
        let svc_sym = manager.interner.insert(svc_name);
        manager
            .remote_services
            .insert(svc_sym, Address::new(url.as_str()));
        println!("Remote service '{}' registered at {}", svc_name, url);
    }

    // Wire network into manager so server can also do remote lookups
    manager.network = Some(net);
    // Record the canonical address so service identities are stable and match
    // the advertised Service URLs above.
    manager.set_local_address(full_addr.clone());

    // Load services after network and remote services are ready,
    // so that remote lookups during service initialization work correctly
    for stmt in &prog {
        if let Stmt::Service { name, decls } = stmt {
            manager
                .create_service(*name, decls.clone())
                .await
                .map_err(|e| format!("Service error: {}", e))?;
            println!("Service '{}' loaded", manager.interner.get(*name));
        }
    }

    println!("Server running, press Ctrl+C to stop...");

    let mut last_keepalive = tokio::time::Instant::now();
    loop {
        // Periodically reassure parked waiters (wait-die wait) that they are
        // still queued, so their reply timeout never fires while we hold them.
        if last_keepalive.elapsed() >= std::time::Duration::from_secs(5) {
            for (request_id, reply_to) in manager.parked_keepalive_targets() {
                if let Some(net) = manager.network.as_mut() {
                    send_net_msg(net, &reply_to, MeerkatMessage::WaitParked { request_id }).await;
                }
            }
            last_keepalive = tokio::time::Instant::now();
        }
        let event = manager.network.as_mut().and_then(|n| n.try_recv_event());
        if let Some(NetworkEvent::MessageReceived { msg, .. }) = event {
            match msg {
                MeerkatMessage::LookupRequest {
                    request_id,
                    service,
                    member,
                    reply_to,
                    txn_id,
                } => {
                    if let Err(e) = codec::validate_lookup_request(&service, &member) {
                        let response = MeerkatMessage::LookupError {
                            request_id,
                            error: e.to_string(),
                        };
                        if let Some(net) = manager.network.as_mut() {
                            send_net_msg(net, &reply_to, response).await;
                        }
                        continue;
                    }
                    let svc_sym = manager.interner.insert(&service);
                    let mem_sym = manager.interner.insert(&member);
                    match txn_id {
                        // Transactional read: park if older than a holder
                        Some(tid) => {
                            run_and_reply_or_park(
                                &mut manager,
                                ParkedRequest::Lookup {
                                    request_id,
                                    reply_to,
                                    service: svc_sym,
                                    member: mem_sym,
                                    tid,
                                },
                            )
                            .await;
                        }
                        // Plain unlocked read: reply immediately
                        None => {
                            let result = manager.lookup(mem_sym, svc_sym, None).await;
                            let response = match result {
                                Ok(val) => match codec::encode_value(&val, &manager.interner) {
                                    Ok(enc_val) => MeerkatMessage::LookupResponse {
                                        request_id,
                                        value: enc_val,
                                    },
                                    Err(e) => MeerkatMessage::LookupError {
                                        request_id,
                                        error: e.to_string(),
                                    },
                                },
                                Err(e) => MeerkatMessage::LookupError {
                                    request_id,
                                    error: e.to_string(),
                                },
                            };
                            if let Some(net) = manager.network.as_mut() {
                                send_net_msg(net, &reply_to, response).await;
                            }
                        }
                    }
                }
                MeerkatMessage::ActionRequest {
                    request_id,
                    service,
                    stmts,
                    env: action_env,
                    reply_to,
                    txn_id,
                } => {
                    if let Err(e) = codec::validate_action_request(&service) {
                        let response = MeerkatMessage::ActionResponse {
                            request_id,
                            success: false,
                            error: Some(e.to_string()),
                        };
                        if let Some(net) = manager.network.as_mut() {
                            send_net_msg(net, &reply_to, response).await;
                        }
                        continue;
                    }
                    let svc_sym = manager.interner.insert(&service);
                    let mut local_stmts = Vec::new();
                    let mut decode_failed = false;
                    let mut error_msg = None;
                    for s in stmts {
                        match codec::decode_action_stmt(s, &mut manager.interner) {
                            Ok(ds) => local_stmts.push(ds),
                            Err(e) => {
                                decode_failed = true;
                                error_msg = Some(e.to_string());
                                break;
                            }
                        }
                    }
                    let mut local_env = Vec::new();
                    if !decode_failed {
                        for (k, v) in action_env {
                            match codec::decode_value(v, &mut manager.interner) {
                                Ok(dv) => local_env.push((manager.interner.insert(&k), dv)),
                                Err(e) => {
                                    decode_failed = true;
                                    error_msg = Some(e.to_string());
                                    break;
                                }
                            }
                        }
                    }
                    if decode_failed {
                        let response = MeerkatMessage::ActionResponse {
                            request_id,
                            success: false,
                            error: error_msg,
                        };
                        if let Some(net) = manager.network.as_mut() {
                            send_net_msg(net, &reply_to, response).await;
                        }
                        continue;
                    }
                    match txn_id {
                        // Part of a distributed transaction: park if older
                        // than a holder, otherwise reply
                        Some(tid) => {
                            run_and_reply_or_park(
                                &mut manager,
                                ParkedRequest::Action {
                                    request_id,
                                    reply_to,
                                    service: svc_sym,
                                    stmts: local_stmts,
                                    env: local_env,
                                    tid,
                                },
                            )
                            .await;
                        }
                        // Standalone: commit immediately and reply
                        None => {
                            let result = manager
                                .execute_action_with_env(svc_sym, &local_stmts, &local_env)
                                .await;
                            let response = MeerkatMessage::ActionResponse {
                                request_id,
                                success: result.is_ok(),
                                error: result.err().map(|e| e.to_string()),
                            };
                            if let Some(net) = manager.network.as_mut() {
                                send_net_msg(net, &reply_to, response).await;
                            }
                        }
                    }
                }
                MeerkatMessage::Commit {
                    request_id,
                    txn_id,
                    reply_to,
                } => {
                    let result = manager.commit_participant(&txn_id).await;
                    let freed = match &result {
                        Ok(f) => f.clone(),
                        Err(_) => HashSet::new(),
                    };
                    let response = MeerkatMessage::CommitResponse {
                        request_id,
                        success: result.is_ok(),
                        error: result.err().map(|e| e.to_string()),
                    };
                    if let Some(net) = manager.network.as_mut() {
                        send_net_msg(net, &reply_to, response).await;
                    }
                    // Wake transactions that were waiting on locks this
                    // commit just released.
                    wake_ready(&mut manager, freed).await;
                }
                MeerkatMessage::Abort {
                    request_id,
                    txn_id,
                    reply_to,
                } => {
                    let freed = manager.abort_participant(&txn_id).await;
                    // Drop this transaction's own parked requests so they
                    // do not later wake for an abandoned transaction.
                    manager.purge_parked_txn(&txn_id);
                    if let Some(net) = manager.network.as_mut() {
                        send_net_msg(
                            net,
                            &reply_to,
                            MeerkatMessage::AbortResponse { request_id },
                        )
                        .await;
                    }
                    // Wake transactions that were waiting on locks this
                    // abort just released.
                    wake_ready(&mut manager, freed).await;
                }
                // Incoming LockRequest from a remote originator node.
                // Wraps the request as a `ParkedRequest::Lock` and
                // dispatches it: if locks are free they are acquired
                // immediately; if blocked, the request parks on the
                // wait queue and is retried when locks are released.
                MeerkatMessage::LockRequest {
                    request_id,
                    txn_id,
                    services,
                    reply_to,
                } => {
                    if let Err(e) = codec::validate_lock_request(&services) {
                        let response = MeerkatMessage::LockResponse {
                            request_id,
                            txn_id,
                            success: false,
                            error: Some(e.to_string()),
                        };
                        if let Some(net) = manager.network.as_mut() {
                            send_net_msg(net, &reply_to, response).await;
                        }
                        continue;
                    }
                    run_and_reply_or_park(
                        &mut manager,
                        ParkedRequest::Lock {
                            request_id,
                            reply_to,
                            txn_id,
                            services,
                        },
                    )
                    .await;
                }
                MeerkatMessage::RequestUpdates {
                    service,
                    member,
                    listener_service,
                    listener_def,
                    reply_to,
                    ..
                } => {
                    // #24: validate + intern wire names through codec (the sole
                    // interning authority for network data); skip on bad input.
                    let (service_sym, member_sym, listener_def_sym) =
                        match codec::decode_request_updates(
                            &service,
                            &member,
                            &listener_def,
                            &mut manager.interner,
                        ) {
                            Ok(syms) => syms,
                            Err(_) => continue,
                        };
                    manager
                        .handle_request_updates(
                            service_sym,
                            member_sym,
                            ServiceNetId(listener_service),
                            listener_def_sym,
                            reply_to,
                        )
                        .await;
                }
                MeerkatMessage::Update {
                    listener_service,
                    listener_def,
                    source_service,
                    member,
                    value,
                } => {
                    // #24: validate + intern wire names through codec; skip on bad input.
                    let (listener_def_sym, source_sym, member_sym) = match codec::decode_update(
                        &listener_def,
                        &source_service,
                        &member,
                        &mut manager.interner,
                    ) {
                        Ok(syms) => syms,
                        Err(_) => continue,
                    };
                    manager
                        .handle_update(
                            ServiceNetId(listener_service),
                            listener_def_sym,
                            source_sym,
                            member_sym,
                            value,
                        )
                        .await;
                }
                // #39: a client is requesting a .mkt file by path. Validate,
                // safely resolve the path against the served base directory,
                // read that file, and reply with its whole source so the client
                // can process it (services and any imports) through the normal
                // program-loading path. Returning the requested file (not the
                // server's own program) lets a client run code distinct from
                // the server, which is the point of the web client.
                MeerkatMessage::ServiceCodeRequest {
                    request_id,
                    path,
                    reply_to,
                } => {
                    let response = codec::serve_service_code(
                        request_id,
                        path.clone(),
                        &reply_to,
                        &served_base_dir,
                    );
                    if let Some(net) = manager.network.as_mut() {
                        send_net_msg(net, &reply_to, response).await;
                    }
                }
                MeerkatMessage::Ping { .. }
                | MeerkatMessage::Pong { .. }
                | MeerkatMessage::Announce { .. }
                | MeerkatMessage::Transaction { .. }
                | MeerkatMessage::Propagation { .. }
                | MeerkatMessage::LookupResponse { .. }
                | MeerkatMessage::LookupError { .. }
                | MeerkatMessage::ActionResponse { .. }
                | MeerkatMessage::CommitResponse { .. }
                | MeerkatMessage::AbortResponse { .. }
                // `LockResponse` is a reply routed back to an
                // originator via the oneshot reply table. It is not
                // directly handled by a server's message loop.
                | MeerkatMessage::LockResponse { .. }
                // #39: code responses are client-bound replies, not seen at the server.
                | MeerkatMessage::ServiceCodeResponse { .. }
                | MeerkatMessage::ServiceCodeError { .. }
                | MeerkatMessage::WaitParked { .. } => {}
            }
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
    }
}

async fn run_client(
    prog: Vec<Stmt>,
    input_file: &str,
    remote_url_map: std::collections::HashMap<String, String>,
    local: bool,
    watch: bool,
    interner: Interner,
) -> Result<(), Box<dyn Error>> {
    let mut manager = Manager::new(interner);
    manager.local = local;

    // Start the network if we have remote imports, or always in watch mode
    // (watch needs the network to receive change notifications).
    let mut net: Option<NetworkActor> = None;
    let mut local_full_addr: Option<String> = None;
    if watch || !remote_url_map.is_empty() {
        let mut n = NetworkActor::new(NodeType::Server)
            .await
            .map_err(|e| format!("Network error: {}", e))?;
        let listen_ip = if local { "127.0.0.1" } else { "0.0.0.0" };
        let listen_addr = Address::new(format!("/ip4/{}/tcp/0", listen_ip));
        let reply = n
            .handle_command(NetworkCommand::Listen { addr: listen_addr })
            .await;
        let addr = listen_success_addr(reply)?;
        let node_ip = manager.get_node_ip();
        let peer_id = n.local_peer_id();
        let addr_str = addr
            .0
            .replace("0.0.0.0", &node_ip)
            .replace("127.0.0.1", &node_ip);
        local_full_addr = Some(format!("{}/p2p/{}", addr_str, peer_id));
        net = Some(n);
    }

    // Wire network actor into manager
    if let Some(n) = net {
        manager.network = Some(n);
    }
    // Record the canonical address (if networked) so service identities are
    // stable for the life of the process.
    if let Some(addr) = local_full_addr {
        manager.set_local_address(addr);
    }

    for stmt in &prog {
        match stmt {
            &Stmt::Service { name, ref decls } => {
                manager
                    .create_service(name, decls.clone())
                    .await
                    .map_err(|e| format!("Service error: {}", e))?;
                println!("Service '{}' loaded", manager.interner.get(name));
            }
            &Stmt::Test {
                service_name,
                ref stmts,
            } => {
                // Watch mode only observes; it does not run @test actions.
                if !watch {
                    manager
                        .execute_action(service_name, stmts)
                        .await
                        .map_err(|e| {
                            format!(
                                "Test failed in '{}': {}",
                                manager.interner.get(service_name),
                                e
                            )
                        })?;
                    println!("@test({}) passed", manager.interner.get(service_name));
                }
            }
            &Stmt::Import {
                ref path,
                service_name,
                explicit_path,
            } => {
                if let Some(url) = remote_url_map.get(manager.interner.get(service_name)) {
                    manager
                        .remote_services
                        .insert(service_name, Address::new(url.as_str()));
                    println!(
                        "Remote service '{}' registered at {}",
                        manager.interner.get(service_name),
                        url
                    );
                } else {
                    let base_dir = std::path::Path::new(input_file)
                        .parent()
                        .unwrap_or(std::path::Path::new("."));
                    let import_path = base_dir.join(path);
                    if let Some(output) =
                        import_from_file(&mut manager, explicit_path, import_path, service_name)
                            .await?
                    {
                        println!("{}", output);
                    }
                }
            }
            &Stmt::ActionStmt(_) => {}
            &Stmt::Update { .. } | &Stmt::Connect { .. } | &Stmt::Watch { .. } => {}
        }
    }

    if watch {
        println!("Watching for changes, press Ctrl+C to stop...");
        loop {
            let msg = manager
                .network
                .as_mut()
                .and_then(|n| n.try_recv_event())
                .and_then(|ev| match ev {
                    NetworkEvent::MessageReceived { msg, .. } => Some(msg),
                    _ => None,
                });
            if let Some(MeerkatMessage::Update {
                listener_service,
                listener_def,
                source_service,
                member,
                value,
            }) = msg
            {
                if let Ok(parsed) = codec::decode_value(value.clone(), &mut manager.interner) {
                    println!("[update] {}.{} = {:?}", source_service, member, parsed);
                }
                let lid = ServiceNetId(listener_service);
                // #24: validate + intern wire names through codec; skip on bad input.
                let (def_sym, source_sym, member_sym) = match codec::decode_update(
                    &listener_def,
                    &source_service,
                    &member,
                    &mut manager.interner,
                ) {
                    Ok(syms) => syms,
                    Err(_) => continue,
                };
                manager
                    .handle_update(lid.clone(), def_sym, source_sym, member_sym, value)
                    .await;
                if let Some((_, svc)) = manager.services.iter().find(|(_, s)| s.id == lid) {
                    if let Some(vs) = svc.vars.get(&def_sym) {
                        println!("          -> {} = {:?}", listener_def, vs.value);
                    }
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }
    }

    Ok(())
}

/// Lock group cascade integration test client (debug builds only).
///
/// Drives the distributed transitive locking test suite from a
/// dedicated client process. Each test case establishes a set of
/// conflicting or cooperative lock states across the 6-node test
/// cluster and asserts the expected outcome from the lock protocol.
///
/// Args:
///     test_case (&str): Name of the test scenario to run.
///     import_urls (&[String]): Remote service URLs (from -i flags).
///
/// Returns:
///     Result<(), Box<dyn Error>>: Ok on success; Err on test failure.
#[cfg(debug_assertions)]
async fn run_lock_test_client(
    test_case: &str,
    import_urls: &[String],
) -> Result<(), Box<dyn Error>> {
    println!("Starting lock test client for case: {}", test_case);

    let mut net = NetworkActor::new(NodeType::Server).await?;
    let mut manager = Manager::new(Interner::new());
    manager.local = true;

    // Listen on an ephemeral port so we have a valid reply address.
    let listen_addr = Address::new("/ip4/127.0.0.1/tcp/0");
    let reply = net
        .handle_command(NetworkCommand::Listen { addr: listen_addr })
        .await;
    let actual_addr = listen_success_addr(reply)?;
    let peer_id = net.local_peer_id();
    let full_addr = format!("{}/p2p/{}", actual_addr.0, peer_id);
    manager.set_local_address(full_addr);

    manager.network = Some(net);

    // Register all remote service urls from the -i flags so the
    // manager can resolve service names to network addresses.
    for url in import_urls {
        if let Some(slug) = url.split('/').next_back() {
            let sym = manager.interner.insert(slug);
            manager
                .remote_services
                .insert(sym, Address::new(url.clone()));
        }
    }

    match test_case {
        // Test: cascade_lock_success
        // Verifies that a LockRequest arriving at Node A cascades
        // transitively through B/C, D/E, and down to F, and that
        // all transitive members are successfully read-locked.
        "cascade_lock_success" => {
            let svc_a_sym = manager.interner.insert("A");
            let addr_a = manager.remote_addr(svc_a_sym)?;

            let txn_id = TxnId::new(manager.node_id);
            let mut services = HashMap::new();
            services.insert(
                "A".to_string(),
                LockGroup {
                    service_level_lock: false,
                    reads: HashSet::from(["read_bc".to_string()]),
                    writes: HashSet::new(),
                },
            );

            println!("Sending LockRequest to Node A: {:?}", services);
            let reply_to = manager.local_reply_addr().await;
            let reply = manager
                .send_and_await_reply(
                    addr_a,
                    MeerkatMessage::LockRequest {
                        request_id: 1,
                        txn_id: txn_id.clone(),
                        services,
                        reply_to,
                    },
                    1,
                    "Timeout waiting for lock response from Node A".to_string(),
                )
                .await?;

            // Defensive post-condition: assert LockResponse was
            // received and indicates successful lock acquisition.
            match reply {
                MeerkatMessage::LockResponse { success, error, .. } => {
                    assert!(success, "Lock request failed: {:?}", error);
                    println!("PASS: lock cascaded successfully!");
                }
                other => panic!("Unexpected reply: {:?}", other),
            }
        }

        // Test: cascade_abort_wait_die
        // Verifies that a younger transaction starting a cascading
        // LockRequest immediately aborts (dies) when an older
        // transaction already holds a conflicting write lock on
        // Node F, per the wait-die deadlock prevention protocol.
        "cascade_abort_wait_die" => {
            let svc_f_sym = manager.interner.insert("F");
            let addr_f = manager.remote_addr(svc_f_sym)?;

            // Phase 1: Establish a high-priority (old) conflicting
            // write lock on Node F.
            let conflict_txn_id = TxnId {
                timestamp: 1,
                node_id: 999,
                iteration: 0,
            };
            let mut services_f = HashMap::new();
            services_f.insert(
                "F".to_string(),
                LockGroup {
                    service_level_lock: false,
                    reads: HashSet::new(),
                    writes: HashSet::from(["val_f".to_string()]),
                },
            );

            println!("Acquiring conflict write-lock on F...");
            let reply_to = manager.local_reply_addr().await;
            let reply = manager
                .send_and_await_reply(
                    addr_f.clone(),
                    MeerkatMessage::LockRequest {
                        request_id: 1,
                        txn_id: conflict_txn_id.clone(),
                        services: services_f,
                        reply_to,
                    },
                    1,
                    "Timeout write-locking val_f on F".to_string(),
                )
                .await?;
            // Defensive pre-condition: conflict lock must succeed
            // before testing that wait-die correctly aborts the
            // younger conflicting transaction.
            assert!(matches!(
                reply,
                MeerkatMessage::LockResponse { success: true, .. }
            ));

            // Phase 2: Start a low-priority (young) transaction on
            // Node A. It should cascade to F and abort immediately.
            let txn_id = TxnId {
                timestamp: 1000,
                node_id: 1,
                iteration: 0,
            };
            let svc_a_sym = manager.interner.insert("A");
            let addr_a = manager.remote_addr(svc_a_sym)?;

            let mut services_a = HashMap::new();
            services_a.insert(
                "A".to_string(),
                LockGroup {
                    service_level_lock: false,
                    reads: HashSet::from(["read_bc".to_string()]),
                    writes: HashSet::new(),
                },
            );

            println!(
                "Sending LockRequest to A (should conflict and \
                 wait-die)..."
            );
            let reply_to = manager.local_reply_addr().await;
            let reply = manager
                .send_and_await_reply(
                    addr_a,
                    MeerkatMessage::LockRequest {
                        request_id: 2,
                        txn_id: txn_id.clone(),
                        services: services_a,
                        reply_to,
                    },
                    2,
                    "Timeout waiting for lock response from Node A".to_string(),
                )
                .await?;

            // Defensive post-condition: assert the younger txn
            // was rejected with a wait-die abort error string.
            match reply {
                MeerkatMessage::LockResponse { success, error, .. } => {
                    assert!(
                        !success,
                        "Lock request should have failed due to \
                         wait-die"
                    );
                    let err_str = error.unwrap();
                    println!("Lock response failed as expected: {}", err_str);
                    assert!(err_str.contains("died contending"));
                    println!(
                        "PASS: lock cascaded and aborted \
                         successfully via wait-die!"
                    );
                }
                other => panic!("Unexpected reply: {:?}", other),
            }
        }

        // Test: cascade_lock_wait
        // Verifies that an older transaction that blocks on a
        // younger transaction's lock correctly parks and is woken
        // when the younger transaction commits.
        "cascade_lock_wait" => {
            let svc_f_sym = manager.interner.insert("F");
            let addr_f = manager.remote_addr(svc_f_sym)?;

            // Phase 1: Establish a low-priority (young) write lock
            // on Node F to create the blocking condition.
            let conflict_txn_id = TxnId {
                timestamp: 10000,
                node_id: 1,
                iteration: 0,
            };
            let mut services_f = HashMap::new();
            services_f.insert(
                "F".to_string(),
                LockGroup {
                    service_level_lock: false,
                    reads: HashSet::new(),
                    writes: HashSet::from(["val_f".to_string()]),
                },
            );

            println!("Acquiring younger write-lock on F...");
            let reply_to = manager.local_reply_addr().await;
            let reply = manager
                .send_and_await_reply(
                    addr_f.clone(),
                    MeerkatMessage::LockRequest {
                        request_id: 1,
                        txn_id: conflict_txn_id.clone(),
                        services: services_f,
                        reply_to,
                    },
                    1,
                    "Timeout write-locking val_f on F".to_string(),
                )
                .await?;
            // Defensive pre-condition: conflict lock must be held
            // before the older transaction attempts to cascade.
            assert!(matches!(
                reply,
                MeerkatMessage::LockResponse { success: true, .. }
            ));

            // Phase 2: Start a high-priority (old) transaction on
            // Node A that cascades to F and parks waiting.
            let txn_id = TxnId {
                timestamp: 5,
                node_id: 1,
                iteration: 0,
            };
            let svc_a_sym = manager.interner.insert("A");
            let addr_a = manager.remote_addr(svc_a_sym)?;

            let mut services_a = HashMap::new();
            services_a.insert(
                "A".to_string(),
                LockGroup {
                    service_level_lock: false,
                    reads: HashSet::from(["read_bc".to_string()]),
                    writes: HashSet::new(),
                },
            );

            // Phase 3: Spawn a background task that commits the
            // conflict transaction on F after a delay, releasing
            // the lock and waking the parked older transaction.
            let addr_f_clone = addr_f.clone();
            let conflict_txn_id_clone = conflict_txn_id.clone();
            let reply_to_addr = manager.local_reply_addr().await;
            tokio::spawn(async move {
                tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                println!(
                    "Background task: committing conflict \
                     transaction on F to wake older lock request..."
                );
                if let Ok(mut temp_net) = NetworkActor::new(NodeType::Server).await {
                    let msg = MeerkatMessage::Commit {
                        request_id: 99,
                        txn_id: conflict_txn_id_clone,
                        reply_to: reply_to_addr,
                    };
                    let _ = temp_net
                        .handle_command(NetworkCommand::SendMessage {
                            addr: addr_f_clone,
                            msg,
                        })
                        .await;
                }
            });

            println!(
                "Sending LockRequest to A (should block on F, \
                 then get woken up and succeed)..."
            );
            let reply_to = manager.local_reply_addr().await;
            let reply = manager
                .send_and_await_reply(
                    addr_a,
                    MeerkatMessage::LockRequest {
                        request_id: 2,
                        txn_id: txn_id.clone(),
                        services: services_a,
                        reply_to,
                    },
                    2,
                    "Timeout waiting for lock response from Node A".to_string(),
                )
                .await?;

            // Defensive post-condition: the older transaction must
            // have been woken successfully and its lock acquired.
            match reply {
                MeerkatMessage::LockResponse { success, error, .. } => {
                    assert!(success, "Lock request failed: {:?}", error);
                    println!(
                        "PASS: lock cascaded and succeeded after \
                         wait/wake!"
                    );
                }
                other => panic!("Unexpected reply: {:?}", other),
            }
        }

        _ => return Err(format!("Unknown lock test case: {}", test_case).into()),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use meerkat_lib::net::MessageId;

    #[test]
    fn listen_success_addr_returns_bound_address() {
        let addr = Address::new("/ip4/127.0.0.1/tcp/1234");

        let actual = listen_success_addr(NetworkReply::ListenSuccess { addr: addr.clone() })
            .expect("listen success should return the bound address");

        assert_eq!(actual, addr);
    }

    #[test]
    fn listen_success_addr_returns_listen_failure() {
        let err = listen_success_addr(NetworkReply::Failure("bind failed".to_string()))
            .expect_err("listen failure should become an error");

        assert_eq!(err.to_string(), "bind failed");
    }

    #[test]
    fn listen_success_addr_rejects_unexpected_replies() {
        let local_addresses_err =
            listen_success_addr(NetworkReply::LocalAddresses { addrs: Vec::new() })
                .expect_err("local addresses are not a Listen success");
        assert_eq!(local_addresses_err.to_string(), "Unexpected reply");

        let message_sent_err = listen_success_addr(NetworkReply::MessageSent {
            msg_id: MessageId(1),
        })
        .expect_err("message-sent replies are not a Listen success");
        assert_eq!(message_sent_err.to_string(), "Unexpected reply");
    }

    // Verify that imports with an explicit path load the target file
    // and create the imported service.
    #[tokio::test]
    async fn test_import_path_member_access() {
        let temp_dir = std::env::temp_dir().join(format!(
            "meerkat-import-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let main_path = temp_dir.join("main.mkt");
        let imported_path = temp_dir.join("s1.mkt");
        std::fs::write(&imported_path, "service s1 { var x = 7; }").unwrap();
        std::fs::write(
            &main_path,
            "import s1 from \"./s1.mkt\"\nservice s2 { pub def y = s1.x; }",
        )
        .unwrap();

        let mut interner = Interner::new();
        let program = parser::parse_file(main_path.to_str().unwrap(), &mut interner).unwrap();
        let mut manager = Manager::new(interner);

        for stmt in program {
            match stmt {
                Stmt::Service { name, decls } => {
                    manager.create_service(name, decls).await.unwrap();
                }
                Stmt::Import {
                    path,
                    service_name,
                    explicit_path,
                } => {
                    let base_dir = main_path.parent().unwrap();
                    let import_path = base_dir.join(path);

                    let import_stmts =
                        parser::parse_file(import_path.to_str().unwrap(), &mut manager.interner)
                            .unwrap();
                    let mut services = import_stmts.into_iter().filter_map(|stmt| match stmt {
                        Stmt::Service { name, decls } => Some((name, decls)),
                        _ => None,
                    });

                    if explicit_path {
                        if let Some((name, decls)) =
                            services.find(|(name, _)| *name == service_name)
                        {
                            manager.create_service(name, decls).await.unwrap();
                        } else {
                            panic!("service import was not loaded");
                        }
                    } else {
                        panic!("expected explicit path import");
                    }
                }
                _ => {}
            }
        }

        let s1 = manager.interner.insert("s1");
        let x = manager.interner.insert("x");
        let service = manager
            .services
            .get(&s1)
            .expect("imported service should be installed");
        assert!(
            service.vars.contains_key(&x),
            "imported service should contain x"
        );

        let s2 = manager.interner.insert("s2");
        let y = manager.interner.insert("y");

        let value = manager.lookup(y, s2, None).await.unwrap();
        assert_eq!(value, meerkat_lib::runtime::ast::Value::Int { val: 7 });
    }
}
