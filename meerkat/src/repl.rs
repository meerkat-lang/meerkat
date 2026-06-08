
use meerkat_lib::runtime::ast::{Expr, Stmt, Value};
use meerkat_lib::runtime::interpreter::{eval, execute, EvalContext, ExecuteEffect};
use meerkat_lib::runtime::parser::parser::{parse_file, parse_repl};
use meerkat_lib::runtime::parser::ReplParseResult;
use meerkat_lib::runtime::Manager;

use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;

const PROMPT: &str = "meerkat> ";
const PROMPT_CONT: &str = "       > ";

/// A registered watch: the original source text, the expression, and its last known value.
struct Watch {
    label: String,
    expr: Expr,
    last: Option<Value>,
}

/// Re-evaluate all watches and print any that have changed.
async fn check_watches(
    watches: &mut Vec<Watch>,
    manager: &mut Manager,
    repl_env: &[(String, Value)],
) {
    for w in watches.iter_mut() {
        let result = eval(
            &w.expr,
            repl_env,
            &mut EvalContext {
                manager,
                service_name: "",
                txn: None,
            },
        )
        .await;
        match result {
            Ok(new_val) => {
                let changed = w.last.as_ref().map_or(true, |old| old != &new_val);
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

pub async fn run_repl(
    mut manager: Manager,
    remote_url_map: std::collections::HashMap<String, String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut reader = DefaultEditor::new()?;
    let _ = reader.load_history("meerkat_history.txt");

    println!("Meerkat REPL  (Ctrl-D to exit)");
    println!("Enter service definitions, @test blocks, statements, or expressions.");
    println!();

    if !remote_url_map.is_empty() {
        let mut n = meerkat_lib::net::NetworkActor::new(meerkat_lib::net::types::NodeType::Server)
            .await
            .map_err(|e| format!("Network error: {}", e))?;
        let listen_ip = if manager.local {
            "127.0.0.1"
        } else {
            "0.0.0.0"
        };
        let listen_addr = meerkat_lib::net::Address::new(&format!("/ip4/{}/tcp/0", listen_ip));
        n.handle_command(meerkat_lib::net::NetworkCommand::Listen { addr: listen_addr })
            .await;
        manager.network = Some(n);
    }

    let mut repl_env: Vec<(String, Value)> = Vec::new();
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

        match parse_repl(&buffer) {
            ReplParseResult::Incomplete => {
                continuation = true;
            }
            ReplParseResult::Error(msg) => {
                reader.add_history_entry(buffer.trim_end())?;
                eprintln!("Parse error: {}", msg);
                buffer.clear();
                continuation = false;
            }
            ReplParseResult::Complete(stmts) => {
                reader.add_history_entry(buffer.trim_end())?;
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
    if let Err(e) = reader.save_history("meerkat_history.txt") {
        eprintln!("Warning: failed to save history: {}", e);
    }
    Ok(())
}

async fn exec_stmt(
    stmt: Stmt,
    manager: &mut Manager,
    repl_env: &mut Vec<(String, Value)>,
    watches: &mut Vec<Watch>,
    remote_url_map: &std::collections::HashMap<String, String>,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    match stmt {
        Stmt::Service { name, decls } => {
            manager
                .create_service(name.clone(), decls)
                .await
                .map_err(|e| format!("Service '{}': {}", name, e))?;
            Ok(Some(format!("Service '{}' loaded.", name)))
        }
        Stmt::Test { service, stmts } => {
            manager
                .execute_action(&service, &stmts)
                .await
                .map_err(|e| format!("@test({}): {}", service, e))?;
            Ok(Some(format!("@test({}) passed.", service)))
        }
        Stmt::Import {
            path,
            service: svc_name,
        } => {
            if let Some(url) = remote_url_map.get(&svc_name) {
                manager.remote_services.insert(
                    svc_name.clone(),
                    meerkat_lib::net::Address::new(url.as_str()),
                );
                return Ok(Some(format!(
                    "Remote service '{}' registered at {}.",
                    svc_name, url
                )));
            }
            let import_stmts =
                parse_file(&path).map_err(|e| format!("Import '{}': {}", path, e))?;
            let mut loaded = Vec::new();
            for s in import_stmts {
                if let Stmt::Service { name, decls } = s {
                    manager
                        .create_service(name.clone(), decls)
                        .await
                        .map_err(|e| format!("Imported service '{}': {}", name, e))?;
                    loaded.push(name);
                }
            }
            Ok(Some(format!("Imported service(s): {}.", loaded.join(", "))))
        }
        Stmt::ActionStmt(action_stmt) => {
            let effect = execute(&action_stmt, repl_env, manager, "", None)
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
                    service_name: "",
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
        other => Ok(Some(format!(
            "(not yet supported in REPL: {:?})",
            std::mem::discriminant(&other)
        ))),
    }
}
