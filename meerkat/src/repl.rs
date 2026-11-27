use meerkat_lib::runtime::ast::{Expr, Stmt, Value};
use meerkat_lib::runtime::interner::Symbol;
use meerkat_lib::runtime::interpreter::{eval, EvalContext};
use meerkat_lib::runtime::parser::ReplParseResult;
use meerkat_lib::runtime::parser::{parse_file, parse_repl};
use meerkat_lib::runtime::txn::VClock;
use meerkat_lib::runtime::Manager;

use directories::ProjectDirs;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use std::env;
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

async fn init_net_layer(manager: &mut Manager) -> Result<(), Box<dyn std::error::Error>> {
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
    Ok(())
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

    //REPL imports may require the net layer even if no remote map was passed
    init_net_layer(&mut manager).await?;

    let mut repl_env: Vec<(Symbol, Value)> = Vec::new();
    let mut session_clock = VClock::new();
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
                        &mut session_clock,
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
    session_clock: &mut VClock,
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
            let svc_name_str = manager.interner.get(service_name).to_string();
            let base_dir = env::current_dir().unwrap();
            if let Some(p) = path {
                if p.starts_with("/ip4/") {
                    let addr = if p.ends_with(&format!("/{}", svc_name_str)) {
                        p.clone()
                    } else {
                        format!("{}/{}", p.trim_end_matches('/'), svc_name_str)
                    };
                    manager
                        .remote_services
                        .insert(service_name, meerkat_lib::net::Address::new(addr));
                    return Ok(Some(format!(
                        "Remote service '{}' registered at {}.",
                        svc_name_str, p
                    )));
                }
                let import_stmts = parse_file(&p, &mut manager.interner)
                    .map_err(|e| format!("Import '{}': {}", p, e))?;
                let mut loaded = Vec::new();
                let found = import_stmts
                    .iter()
                    .any(|stmt| matches!(stmt, Stmt::Service {name, ..} if *name == service_name));
                if !found {
                    return Err(format!("Import '{}': Service not found", p).into());
                }
                if manager.remote_services.contains_key(&service_name) {
                    return Err(format!(
                        "Import '{}': Request service is already registered
                    as a remote service",
                        svc_name_str
                    )
                    .into());
                }
                for s in import_stmts {
                    if let Stmt::Service { name, decls } = s {
                        if !manager.remote_services.contains_key(&name) {
                            manager.create_service(name, decls).await.map_err(|e| {
                                format!("Imported service '{}': {}", manager.interner.get(name), e)
                            })?;
                            loaded.push(manager.interner.get(name).to_string());
                        }
                    }
                }
                Ok(Some(format!("Imported service(s): {}.", loaded.join(", "))))
            } else {
                if let Some(url) = remote_url_map.get(&svc_name_str) {
                    manager
                        .remote_services
                        .insert(service_name, meerkat_lib::net::Address::new(url.as_str()));
                    Ok(Some(format!(
                        "Remote service '{}' registered at {}.",
                        svc_name_str, url
                    )))
                } else {
                    let implicit_path = base_dir.join(format!("{}.mkt", svc_name_str));
                    let import_stmts =
                        parse_file(implicit_path.to_str().unwrap(), &mut manager.interner)
                            .map_err(|e| {
                                format!("Import '{}': {}", implicit_path.to_str().unwrap(), e)
                            })?;
                    let mut loaded = Vec::new();
                    for s in import_stmts {
                        if let Stmt::Service { name, decls } = s {
                            if !manager.remote_services.contains_key(&name) {
                                manager.create_service(name, decls).await.map_err(|e| {
                                    format!(
                                        "Imported service '{}': {}",
                                        manager.interner.get(name),
                                        e
                                    )
                                })?;
                                loaded.push(manager.interner.get(name).to_string());
                            }
                        }
                    }
                    Ok(Some(format!("Imported service(s): {}.", loaded.join(", "))))
                }
            }
        }
        Stmt::ActionStmt(action_stmt) => {
            let value = manager
                .execute_statement(Symbol::empty(), &action_stmt, repl_env, session_clock)
                .await?;
            Ok(value.map(|value| value.to_string()))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn repl_commits_each_statement_in_a_complete_input() {
        let input = r"
            service s {
                var n = 0;
                var unrelated = 0;
                var x = 0;
                pub def bump = action { x = x + 1; };
            }
            @test(s) { n = 3; unrelated = 4; }
            watch s.unrelated;
            let t = s.n; do s.bump;
            for i in 0..2 {
                do s.bump;
                assert(i == 0);
            }
            t + s.x;
        ";
        let mut manager = Manager::default();
        let ReplParseResult::Complete(stmts) = parse_repl(input, &mut manager.interner) else {
            panic!("expected a complete REPL input");
        };
        let mut env = Vec::new();
        let mut clock = VClock::new();
        let mut watches = Vec::new();
        let remotes = Default::default();
        let mut output = None;
        let mut errors = Vec::new();
        for stmt in stmts {
            match exec_stmt(
                stmt,
                &mut manager,
                &mut env,
                &mut clock,
                &mut watches,
                &remotes,
            )
            .await
            {
                Ok(value) => output = value,
                Err(error) => errors.push(error.to_string()),
            }
        }
        assert_eq!(errors, ["Assertion failed: i == 0"]);
        assert_eq!(output.as_deref(), Some("4"));

        // The first bump inherited the let's read. The separate @test block
        // and watch did not add unrelated to its clock.
        let s = manager.interner.insert("s");
        let n = manager.interner.insert("n");
        let x = manager.interner.insert("x");
        assert_eq!(
            manager.services[&s].vars[&x].vector_clock,
            VClock::from([((s, n), 1), ((s, x), 1)])
        );
    }
}
