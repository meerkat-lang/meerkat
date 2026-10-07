use meerkat_lib::runtime::ast::{Expr, Stmt, Value};
use meerkat_lib::runtime::interner::Symbol;
use meerkat_lib::runtime::interpreter::{eval, execute, EvalContext, ExecuteEffect};
use meerkat_lib::runtime::parser::ReplParseResult;
use meerkat_lib::runtime::parser::{parse_file, parse_repl};
use meerkat_lib::runtime::Manager;

use directories::ProjectDirs;
use rustyline::error::ReadlineError;
use rustyline::{DefaultEditor, ExternalPrinter};
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

/// Read one line on a blocking thread so it can be raced against other
/// async work (the `--watch` network poll) via `tokio::select!`. Moves the
/// editor into the blocking task and hands it back out alongside the
/// result, since `DefaultEditor` isn't `Clone` and `readline()` isn't safe
/// to call from two places at once.
fn spawn_blocking_readline(
    mut reader: DefaultEditor,
    prompt: &'static str,
) -> tokio::task::JoinHandle<(DefaultEditor, Result<String, ReadlineError>)> {
    tokio::task::spawn_blocking(move || {
        let result = reader.readline(prompt);
        (reader, result)
    })
}

fn spawn_next_readline(
    reader: DefaultEditor,
    continuation: bool,
) -> tokio::task::JoinHandle<(DefaultEditor, Result<String, ReadlineError>)> {
    let prompt = if continuation { PROMPT_CONT } else { PROMPT };
    spawn_blocking_readline(reader, prompt)
}

/// Run the `REPL` loop for interactive execution. When `watch` is set, the
/// loop also polls for incoming network updates between keystrokes and
/// prints them via an external printer, so they don't corrupt the line
/// currently being edited.
pub async fn run_repl(
    mut manager: Manager,
    remote_url_map: std::collections::HashMap<String, String>,
    watch: bool,
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

    if manager.network.is_none() && !remote_url_map.is_empty() {
        let mut n = meerkat_lib::net::NetworkActor::new(meerkat_lib::net::types::NodeType::Server)
            .await
            .map_err(|e| format!("Network error: {}", e))?;
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
    }

    let mut repl_env: Vec<(Symbol, Value)> = Vec::new();
    let mut watches: Vec<Watch> = Vec::new();

    let mut buffer = String::new();
    let mut continuation = false;

    // Only try to get an external printer when it's actually needed: it
    // unconditionally errors with ENOTTY when stdin/stdout isn't a real tty
    // (e.g. piped input), which must not break plain `--interactive` or a
    // `--watch` session running non-interactively. Fall back to a plain
    // `println!` (via `None`) rather than propagating that error.
    let mut printer: Option<Box<dyn ExternalPrinter + Send>> = if watch {
        reader
            .create_external_printer()
            .ok()
            .map(|p| Box::new(p) as Box<dyn ExternalPrinter + Send>)
    } else {
        None
    };
    let mut watch_tick = tokio::time::interval(std::time::Duration::from_millis(10));
    let mut read_fut = spawn_next_readline(reader, continuation);

    loop {
        tokio::select! {
            res = &mut read_fut => {
                let (returned_reader, readline) = res?;
                reader = returned_reader;

                let line = match readline {
                    Ok(l) => l,
                    Err(ReadlineError::Interrupted) => {
                        buffer.clear();
                        if is_tty {
                            println!("Interrupt");
                        }
                        continuation = false;
                        read_fut = spawn_next_readline(reader, continuation);
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
                    read_fut = spawn_next_readline(reader, continuation);
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

                read_fut = spawn_next_readline(reader, continuation);
            }
            _ = watch_tick.tick(), if watch => {
                crate::poll_and_print_update(&mut manager, |line| match printer.as_mut() {
                    Some(p) => {
                        let _ = p.print(line);
                    }
                    None => println!("{}", line),
                })
                .await;
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
                .execute_test_block(service_name, &stmts)
                .await
                .map_err(|e| format!("@test({}): {}", manager.interner.get(service_name), e))?;
            Ok(Some(format!(
                "@test({}) passed.",
                manager.interner.get(service_name)
            )))
        }
        Stmt::Import { path, service_name } => {
            let svc_name_str = manager.interner.get(service_name);
            if let Some(url) = remote_url_map.get(svc_name_str) {
                manager
                    .remote_services
                    .insert(service_name, meerkat_lib::net::Address::new(url.as_str()));
                return Ok(Some(format!(
                    "Remote service '{}' registered at {}.",
                    svc_name_str, url
                )));
            }
            let import_stmts = parse_file(&path, &mut manager.interner)
                .map_err(|e| format!("Import '{}': {}", path, e))?;
            let mut loaded = Vec::new();
            for s in import_stmts {
                if let Stmt::Service { name, decls } = s {
                    manager.create_service(name, decls).await.map_err(|e| {
                        format!("Imported service '{}': {}", manager.interner.get(name), e)
                    })?;
                    loaded.push(manager.interner.get(name).to_string());
                }
            }
            Ok(Some(format!("Imported service(s): {}.", loaded.join(", "))))
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
            let label = meerkat_lib::runtime::update::format_expr(&expr, &manager.interner);
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
        Stmt::Atomic { .. } | Stmt::Update { .. } => {
            Ok(Some("(not yet supported in REPL: Update)".to_string()))
        }
        Stmt::Connect { .. } => Ok(Some("(not yet supported in REPL: Connect)".to_string())),
    }
}
