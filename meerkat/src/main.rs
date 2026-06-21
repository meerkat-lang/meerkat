mod repl;

use clap::Parser;
use meerkat_lib::net::network_layer::NetworkLayer;
use meerkat_lib::net::types::NodeType;
use meerkat_lib::net::NetworkActor;
use meerkat_lib::net::{
    codec, Address, MeerkatMessage, NetworkCommand, NetworkEvent, NetworkReply, ServiceNetId,
};
use meerkat_lib::runtime::ast::{AstPrinter, Stmt};
use meerkat_lib::runtime::interner::{Interner, Symbol};
use meerkat_lib::runtime::interpreter::EvalError;
use meerkat_lib::runtime::manager::ParkedRequest;
use meerkat_lib::runtime::{parser, Manager};
use std::collections::HashSet;
use std::error::Error;

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

    /// Bind to loopback/localhost only (force 127.0.0.1 instead of public IP)
    #[arg(long = "local", default_value_t = false)]
    local: bool,

    /// Perform static checks and terminate immediately
    #[arg(long = "check", default_value_t = false)]
    check_only: bool,

    /// Emit AST to `stdout`
    #[arg(long = "ast", default_value_t = false)]
    ast: bool,

    /// Launch interactive REPL after executing input file (default: false)
    #[arg(short = 'j', long = "interactive", default_value_t = false)]
    interactive: bool,
}

#[tokio::main]
pub async fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();

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

    let mut interner = Interner::new();

    match args.input_file {
        Some(ref file) => {
            let prog = parser::parse_file(file, &mut interner)
                .map_err(|e| format!("Parse error: {}", e))?;

            // This must appear prior to `check_only` or it will never print. These
            // modes are designed to work both in isolation and in tandem
            if args.ast {
                let printer = AstPrinter::new(&interner);
                printer.print_program(&prog);
            }

            // TODO: Insert static checks here. The static checks must occur at this
            // program point in order to properly sequence the semantics of these
            // CLI flags. All standard checks go here; additional static checks
            // must occur after this AND in the `check_only` branch below; both must
            // be simultaneously maintained

            // This mode must appear before `server` args check in order to properly
            // stop execution. Logic for static checks must not occur in this branch,
            // as the intent of this mode is to simply halt after the static semantics
            // phase of the interpter/compiler. See also: above comment(s)
            if args.check_only {
                return Ok(());
            }

            if args.server {
                run_server(prog, remote_url_map, args.port, args.local, interner).await
            } else {
                run_client(prog, file, remote_url_map, args.local, interner, args.interactive).await
            }
        }
        None => {
            if args.server || args.check_only || args.ast {
                return Err(
                    "Expected a .mkt file (-f) for --server, --check, or --ast mode.".into(),
                );
            }
            let mut manager = Manager::new(interner);
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
                Err(EvalError::WaitOn(svc, var)) => {
                    manager.park_request(
                        svc,
                        var,
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
                        net.handle_command(NetworkCommand::SendMessage {
                            addr: Address::new(&reply_to),
                            msg: response,
                        })
                        .await;
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
                Err(EvalError::WaitOn(svc, var)) => {
                    manager.park_request(
                        svc,
                        var,
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
                        net.handle_command(NetworkCommand::SendMessage {
                            addr: Address::new(&reply_to),
                            msg: response,
                        })
                        .await;
                    }
                }
                Err(e) => {
                    let response = MeerkatMessage::LookupError {
                        request_id,
                        error: e.to_string(),
                    };
                    if let Some(net) = manager.network.as_mut() {
                        net.handle_command(NetworkCommand::SendMessage {
                            addr: Address::new(&reply_to),
                            msg: response,
                        })
                        .await;
                    }
                }
            }
        }
    }
}

/// After a holder releases its locks on commit or abort, re-dispatch the parked
/// requests waiting on the freed variables, oldest first.
async fn wake_ready(manager: &mut Manager, freed: HashSet<(ServiceNetId, Symbol)>) {
    for parked in manager.take_ready_waiters(&freed) {
        run_and_reply_or_park(manager, parked).await;
    }
}

async fn run_server(
    prog: Vec<Stmt>,
    remote_url_map: std::collections::HashMap<String, String>,
    port: u16,
    local: bool,
    interner: Interner,
) -> Result<(), Box<dyn Error>> {
    let mut net = NetworkActor::new(NodeType::Server).await?;
    let mut manager = Manager::new(interner);
    manager.local = local;

    let node_ip = manager.get_node_ip();
    let listen_ip = if local { "127.0.0.1" } else { "0.0.0.0" };
    let listen_addr = Address::new(format!("/ip4/{}/tcp/{}", listen_ip, port));
    let reply = net
        .handle_command(NetworkCommand::Listen { addr: listen_addr })
        .await;
    let actual_addr = match reply {
        NetworkReply::ListenSuccess { addr } => addr,
        NetworkReply::Failure(e) => return Err(e.into()),
        NetworkReply::MessageSent { .. } | NetworkReply::LocalAddresses { .. } => {
            return Err("Unexpected reply".into())
        }
    };

    let peer_id = net.local_peer_id();
    // Replace loopback/unspecified with actual node IP
    let actual_addr_str = actual_addr
        .0
        .replace("0.0.0.0", &node_ip)
        .replace("127.0.0.1", &node_ip);
    let full_addr = format!("{}/p2p/{}", actual_addr_str, peer_id);
    println!("Server listening at: {}", full_addr);

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
                    net.handle_command(NetworkCommand::SendMessage {
                        addr: Address::new(&reply_to),
                        msg: MeerkatMessage::WaitParked { request_id },
                    })
                    .await;
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
                                net.handle_command(NetworkCommand::SendMessage {
                                    addr: Address::new(&reply_to),
                                    msg: response,
                                })
                                .await;
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
                            net.handle_command(NetworkCommand::SendMessage {
                                addr: Address::new(&reply_to),
                                msg: response,
                            })
                            .await;
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
                                net.handle_command(NetworkCommand::SendMessage {
                                    addr: Address::new(&reply_to),
                                    msg: response,
                                })
                                .await;
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
                        net.handle_command(NetworkCommand::SendMessage {
                            addr: Address::new(&reply_to),
                            msg: response,
                        })
                        .await;
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
                        net.handle_command(NetworkCommand::SendMessage {
                            addr: Address::new(&reply_to),
                            msg: MeerkatMessage::AbortResponse { request_id },
                        })
                        .await;
                    }
                    // Wake transactions that were waiting on locks this
                    // abort just released.
                    wake_ready(&mut manager, freed).await;
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
    interner: Interner,
    interactive: bool,
) -> Result<(), Box<dyn Error>> {
    let mut manager = Manager::new(interner);
    manager.local = local;

    // Start network if we have remote imports
    let mut net: Option<NetworkActor> = None;
    let mut local_full_addr: Option<String> = None;
    if !remote_url_map.is_empty() {
        let mut n = NetworkActor::new(NodeType::Server)
            .await
            .map_err(|e| format!("Network error: {}", e))?;
        let listen_ip = if local { "127.0.0.1" } else { "0.0.0.0" };
        let listen_addr = Address::new(format!("/ip4/{}/tcp/0", listen_ip));
        let reply = n
            .handle_command(NetworkCommand::Listen { addr: listen_addr })
            .await;
        if let NetworkReply::ListenSuccess { addr } = reply {
            let node_ip = manager.get_node_ip();
            let peer_id = n.local_peer_id();
            let addr_str = addr
                .0
                .replace("0.0.0.0", &node_ip)
                .replace("127.0.0.1", &node_ip);
            local_full_addr = Some(format!("{}/p2p/{}", addr_str, peer_id));
        }
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
            &Stmt::Import {
                ref path,
                service_name,
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
                    let import_stmts =
                        parser::parse_file(import_path.to_str().unwrap(), &mut manager.interner)
                            .map_err(|e| format!("Import parse error: {}", e))?;
                    for import_stmt in &import_stmts {
                        if let &Stmt::Service { name, ref decls } = import_stmt {
                            manager
                                .create_service(name, decls.clone())
                                .await
                                .map_err(|e| format!("Import service error: {}", e))?;
                            println!("Imported service '{}'", manager.interner.get(name));
                        }
                    }
                }
            }
            &Stmt::ActionStmt(_) => {}
            &Stmt::Update { .. } | &Stmt::Connect { .. } | &Stmt::Watch { .. } => {}
        }
    }

    if interactive {
        println!("Entering interactive REPL mode. Type 'exit' or Ctrl+D to quit.");
        repl::run_repl(manager, remote_url_map).await
    } else {
        Ok(())
    }
}
