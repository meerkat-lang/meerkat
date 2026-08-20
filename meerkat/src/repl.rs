use meerkat_lib::runtime::ast::{Expr, Stmt, Value};
use meerkat_lib::runtime::interner::Symbol;
use meerkat_lib::runtime::interpreter::{eval, execute, EvalContext, ExecuteEffect};
use meerkat_lib::runtime::parser::ReplParseResult;
use meerkat_lib::runtime::parser::{parse_file, parse_repl};
use meerkat_lib::runtime::Manager;

use directories::ProjectDirs;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use std::error::Error;
use std::io::{self, IsTerminal};

const PROMPT: &str = "meerkat> ";
const PROMPT_CONT: &str = "       > ";

/// A registered watch represented by `Watch`
///
/// Keeps track of the original source text, the expression, and its
/// last known value
struct Watch {
    label: String,
    expr: Expr,
    last: Option<Value>,
}

/// Re-evaluate all watches and print any that have changed
async fn check_watches(watches: &mut [Watch], manager: &mut Manager, repl_env: &[(Symbol, Value)]) {
    for w in watches.iter_mut() {
        let result = eval(
            &w.expr,
            repl_env,
            &mut EvalContext {
                manager,
                service_name: Symbol::empty(),
                txn: None,
            },
        )
        .await;
        match result {
            Ok(new_val) => {
                let changed = w.last.as_ref() != Some(&new_val);
                if changed {
                    match &w.last {
                        None => println!("[watch] {} = {}", w.label, new_val),
                        Some(old) => println!("[watch] {}: {} => {}", w.label, old, new_val),
                    }
                    w.last = Some(new_val);
                }
            }
            Err(e) => eprintln!("[watch] {}: error: {}", w.label, e),
        }
    }
}

async fn init_network(manager: &mut Manager) -> Result<(), Box<dyn std::error::Error>> {
    let mut n =
        meerkat_lib::net::NetworkActor::new(meerkat_lib::net::types::NodeType::Server).await?;
    let listen_ip = if manager.local {
        "127.0.0.1"
    } else {
        "0.0.0.0"
    };
    let listen_addr = meerkat_lib::net::Address::new(format!("/ip4/{}/tcp/0", listen_ip));
    let reply = n
        .handle_command(meerkat_lib::net::NetworkCommand::Listen { addr: listen_addr })
        .await;
    let addr = crate::listen_success_addr(reply)?;
    let node_ip = manager.get_node_ip();
    let peer_id = n.local_peer_id();
    let addr_str = addr
        .0
        .replace("0.0.0.0", &node_ip)
        .replace("127.0.0.1", &node_ip);
    manager.network = Some(n);
    manager.set_local_address(format!("{}/p2p/{}", addr_str, peer_id));
    Ok(())
}

async fn import_from_file(
    manager: &mut Manager,
    explicit_path: bool,
    import_path: std::path::PathBuf,
    service_name: Symbol,
) -> Result<Option<String>, Box<dyn Error>> {
    let import_stmts = parse_file(import_path.to_str().unwrap(), &mut manager.interner)
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

/// Run the `REPL` loop for interactive execution
pub async fn run_repl(
    mut manager: Manager,
    remote_url_map: std::collections::HashMap<String, String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut reader = DefaultEditor::new()?;
    use std::path::PathBuf;

    let proj = ProjectDirs::from("", "", "meerkat");
    let history_path = match proj {
        Some(proj) => {
            let _ = std::fs::create_dir_all(proj.data_dir());
            proj.data_dir().join(".meerkat_history.txt")
        }
        None => PathBuf::from(".meerkat_history.txt"),
    };

    let _ = reader.load_history(&history_path);

    let is_tty = io::stdin().is_terminal();

    if is_tty {
        println!("Meerkat REPL  (Ctrl-D to exit)");
        println!("Enter service definitions, @test blocks, statements, or expressions.");
        println!();
    }

    match init_network(&mut manager).await {
        Ok(_) => {}
        Err(e) => {
            if is_tty {
                println!("Network error: {}", e);
                println!("Continuing without network support. Some features may not work.");
            }
        }
    }

    let mut repl_env: Vec<(Symbol, Value)> = Vec::new();
    let mut watches: Vec<Watch> = Vec::new();

    let mut buffer = String::new();
    let mut continuation = false;

    loop {
        let readline = if continuation {
            reader.readline(PROMPT_CONT)
        } else {
            reader.readline(PROMPT)
        };

        let line = match readline {
            Ok(l) => l,
            Err(ReadlineError::Interrupted) => {
                buffer.clear();
                if is_tty {
                    println!("Interrupt");
                }
                continuation = false;
                continue;
            }
            Err(ReadlineError::Eof) => break,
            Err(e) => return Err(e.into()),
        };

        buffer.push_str(&line);
        buffer.push('\n');

        // Empty line: just check watches and re-prompt
        if buffer.trim().is_empty() {
            buffer.clear();
            continuation = false;
            check_watches(&mut watches, &mut manager, &repl_env).await;
            continue;
        }

        match parse_repl(&buffer, &mut manager.interner) {
            ReplParseResult::Incomplete => {
                continuation = true;
            }
            ReplParseResult::Error(msg) => {
                if let Err(e) = reader.add_history_entry(buffer.trim_end()) {
                    eprintln!("Warning: failed to save history: {}", e);
                }
                eprintln!("Parse error: {}", msg);
                buffer.clear();
                continuation = false;
            }
            ReplParseResult::Complete(stmts) => {
                if let Err(e) = reader.add_history_entry(buffer.trim_end()) {
                    eprintln!("Warning: failed to save history: {}", e);
                }
                for stmt in stmts {
                    match exec_stmt(
                        stmt,
                        &mut manager,
                        &mut repl_env,
                        &mut watches,
                        &remote_url_map,
                    )
                    .await
                    {
                        Ok(Some(output)) => println!("{}", output),
                        Ok(None) => {}
                        Err(e) => eprintln!("Error: {}", e),
                    }
                }
                // Check watches after every complete input
                check_watches(&mut watches, &mut manager, &repl_env).await;
                buffer.clear();
                continuation = false;
            }
        }
    }
    if let Err(e) = reader.save_history(&history_path) {
        eprintln!("Warning: failed to save history: {}", e);
    }
    Ok(())
}

/// Execute a single statement inside the `REPL` loop
async fn exec_stmt(
    stmt: Stmt,
    manager: &mut Manager,
    repl_env: &mut Vec<(Symbol, Value)>,
    watches: &mut Vec<Watch>,
    remote_url_map: &std::collections::HashMap<String, String>,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    match stmt {
        Stmt::Service { name, decls } => {
            manager
                .create_service(name, decls)
                .await
                .map_err(|e| format!("Service '{}': {}", manager.interner.get(name), e))?;
            Ok(Some(format!(
                "Service '{}' loaded.",
                manager.interner.get(name)
            )))
        }
        Stmt::Test {
            service_name,
            stmts,
        } => {
            manager
                .execute_action(service_name, &stmts)
                .await
                .map_err(|e| format!("@test({}): {}", manager.interner.get(service_name), e))?;
            Ok(Some(format!(
                "@test({}) passed.",
                manager.interner.get(service_name)
            )))
        }
        Stmt::Import {
            path,
            service_name,
            explicit_path,
        } => {
            let svc_name_str = manager.interner.get(service_name);
            let address = if path.starts_with("/ip4") {
                Some(path.as_str())
            } else if !explicit_path {
                remote_url_map.get(svc_name_str).map(String::as_str)
            } else {
                None
            };
            if let Some(address) = address {
                manager
                    .remote_services
                    .insert(service_name, meerkat_lib::net::Address::new(address));
                return Ok(Some(format!(
                    "Remote service '{}' registered at {}.",
                    svc_name_str, address
                )));
            }
            import_from_file(
                manager,
                explicit_path,
                std::path::PathBuf::from(&path),
                service_name,
            )
            .await
        }
        Stmt::ActionStmt(action_stmt) => {
            let effect = execute(&action_stmt, repl_env, manager, Symbol::empty(), None)
                .await
                .map_err(|e| format!("{}", e))?;
            match effect {
                ExecuteEffect::Binding(name, val) => {
                    repl_env.push((name, val));
                    Ok(None)
                }
                ExecuteEffect::ExprValue(val) => Ok(Some(val.to_string())),
                ExecuteEffect::None => Ok(None),
            }
        }
        Stmt::Watch { expr } => {
            let label = format!("{}", expr);
            // Evaluate initial value
            let initial = eval(
                &expr,
                repl_env,
                &mut EvalContext {
                    manager,
                    service_name: Symbol::empty(),
                    txn: None,
                },
            )
            .await
            .ok();
            let msg = match &initial {
                Some(v) => format!("Watching: {} (current value: {})", label, v),
                None => format!("Watching: {} (not yet available)", label),
            };
            watches.push(Watch {
                label,
                expr,
                last: initial,
            });
            Ok(Some(msg))
        }
        Stmt::Update { .. } => Ok(Some("(not yet supported in REPL: Update)".to_string())),
        Stmt::Connect { .. } => Ok(Some("(not yet supported in REPL: Connect)".to_string())),
    }
}
