//! #39: WebAssembly browser client for Meerkat. Connects to a Meerkat server
//! over libp2p WebSocket, fetches a `.mkt` file by path, parses it, and
//! instantiates its services. Imported services are registered as remote
//! (they live on the server), so their defs are live network lookups that
//! drive reactive updates. A background loop re-renders the `html` def to the
//! DOM as Update messages arrive.

use meerkat_lib::net::{Address, NetworkActor, NodeType};
use meerkat_lib::runtime::ast::{Stmt, Value};
use meerkat_lib::runtime::interner::Interner;
use meerkat_lib::runtime::manager::Manager;
use meerkat_lib::runtime::parser;
use std::cell::RefCell;
use std::rc::Rc;
use wasm_bindgen::prelude::*;

fn js_err<E: std::fmt::Display>(e: E) -> JsValue {
    JsValue::from_str(&e.to_string())
}

/// Write an HTML string into the element with id `render`.
fn render_to_dom(html: &str) {
    if let Some(win) = web_sys::window() {
        if let Some(doc) = win.document() {
            if let Some(el) = doc.get_element_by_id("render") {
                el.set_inner_html(html);
            }
        }
    }
}

/// Write a status line into the element with id `out`.
fn status(msg: &str) {
    if let Some(win) = web_sys::window() {
        if let Some(doc) = win.document() {
            if let Some(el) = doc.get_element_by_id("out") {
                el.set_text_content(Some(msg));
            }
        }
    }
}

/// Read the current rendered value of any loaded service's `html` def.
fn current_html(
    manager: &Manager,
    html_sym: meerkat_lib::runtime::interner::Symbol,
) -> Option<String> {
    for svc in manager.services.values() {
        if let Some(vs) = svc.vars.get(&html_sym) {
            if let Value::Html(h) = &vs.value {
                return Some(h.to_string());
            }
        }
    }
    None
}

/// #153: read the event handlers bound in the current `html` value, as
/// (event, action-value) pairs. Returns an owned copy so no borrow of the
/// manager is held while listeners are attached.
fn current_handlers(
    manager: &Manager,
    html_sym: meerkat_lib::runtime::interner::Symbol,
) -> Vec<(String, Value)> {
    for svc in manager.services.values() {
        if let Some(vs) = svc.vars.get(&html_sym) {
            if let Value::Html(h) = &vs.value {
                return h.handlers().to_vec();
            }
        }
    }
    Vec::new()
}

/// #153: attach DOM click listeners for the current html value's handlers.
///
/// For now this special-cases the single-button counter demo: each `onclick`
/// handler is bound to the first `<button>` in the render target. General
/// element-to-handler binding needs a full tree render on the wasm side and is
/// tracked as a follow-up. On click, the handler's action value (an
/// `ActionClosure`) is run via `remote_action`; because a DOM listener is
/// synchronous but running an action is async, the work is spawned rather than
/// awaited inline, and the manager is borrowed only inside the spawned task so
/// no borrow is held across an await in the listener itself.
fn attach_handlers(
    manager: &Rc<RefCell<Manager>>,
    handlers: Vec<(String, Value)>,
) {
    use wasm_bindgen::JsCast;
    let Some(win) = web_sys::window() else { return };
    let Some(doc) = win.document() else { return };
    let Some(root) = doc.get_element_by_id("render") else {
        return;
    };
    for (event, action) in handlers {
        // Only onclick is wired in this slice.
        if event != "onclick" {
            continue;
        }
        let Ok(Some(btn)) = root.query_selector("button") else {
            continue;
        };
        let manager_cb = Rc::clone(manager);
        let action_cb = action.clone();
        let closure = Closure::<dyn FnMut()>::new(move || {
            // Extract the action's parts; only ActionClosure is runnable.
            if let Value::ActionClosure {
                stmts,
                env,
                service_net_id,
            } = action_cb.clone()
            {
                let manager_task = Rc::clone(&manager_cb);
                wasm_bindgen_futures::spawn_local(async move {
                    // #153: the render loop may hold the manager borrow while it
                    // drains network events. Rather than panic on a double
                    // borrow, wait for it to be free, then run the action. The
                    // borrow is held across remote_action's network await, so
                    // the render loop pauses until the action completes -- fine
                    // for the single-button demo; finer-grained concurrency is a
                    // follow-up.
                    loop {
                        if manager_task.try_borrow_mut().is_ok() {
                            break;
                        }
                        gloo_timers::future::TimeoutFuture::new(20).await;
                    }
                    let mut m = manager_task.borrow_mut();
                    let _ = m
                        .remote_action(&service_net_id, stmts, env, None)
                        .await;
                });
            }
        });
        let _ = btn
            .dyn_ref::<web_sys::EventTarget>()
            .unwrap()
            .add_event_listener_with_callback(
                "click",
                closure.as_ref().unchecked_ref(),
            );
        // Keep the closure alive for the lifetime of the page.
        closure.forget();
    }
}

/// Connect to `server_ws_addr`, fetch and instantiate the `.mkt` at `path`,
/// then run a background loop that re-renders the `html` def as reactive
/// updates arrive. Returns once loading is done; the loop keeps running.
#[wasm_bindgen]
pub async fn load_service(server_ws_addr: String, path: String) -> Result<String, JsValue> {
    console_error_panic_hook::set_once();

    let net = NetworkActor::new(NodeType::Server).await.map_err(js_err)?;
    let peer_id = net.local_peer_id();

    let mut manager = Manager::new(Interner::new());
    manager.network = Some(net);
    manager.set_local_address(format!("/p2p/{}", peer_id));

    // Browsers can only dial WebSocket transports. Reject a non-ws multiaddr
    // early (e.g. the TCP "Server listening at" address) with a clear message.
    if !server_ws_addr.contains("/ws") {
        return Err(js_err(
            "Expected a WebSocket address containing /ws, e.g. /ip4/127.0.0.1/tcp/9001/ws/p2p/<peer_id>",
        ));
    }
    let server_addr = Address::new(server_ws_addr);

    // 1. Fetch source.
    let source = manager
        .fetch_service_source(&path, server_addr.clone())
        .await
        .map_err(js_err)?;

    // 2. Parse.
    let stmts = parser::parse_string(&source, &mut manager.interner).map_err(js_err)?;

    // 3. Load: imports -> remote, services -> instantiate (auto-subscribes).
    let mut summary = String::new();
    for stmt in stmts {
        match stmt {
            Stmt::Import {
                path: _,
                service_name,
            } => {
                manager
                    .remote_services
                    .insert(service_name, server_addr.clone());
                summary.push_str(&format!(
                    "import {} (remote)\n",
                    manager.interner.get(service_name)
                ));
            }
            Stmt::Service { name, decls } => {
                manager.create_service(name, decls).await.map_err(js_err)?;
                summary.push_str(&format!(
                    "service {} instantiated\n",
                    manager.interner.get(name)
                ));
            }
            _ => {}
        }
    }
    if summary.is_empty() {
        summary.push_str("(no services or imports found)");
    }
    status(&summary);

    let html_sym = manager.interner.insert("html");

    // #153: share the manager between the render loop and (soon) click
    // listeners. Wasm is single-threaded, so Rc<RefCell<_>> is sufficient and
    // correct; no Arc/Mutex needed. Setup above kept sole ownership because it
    // runs before any listener exists.
    let manager = Rc::new(RefCell::new(manager));

    // Initial render, then attach click listeners for any handlers.
    if let Some(html) = current_html(&manager.borrow(), html_sym) {
        render_to_dom(&html);
        let handlers = current_handlers(&manager.borrow(), html_sym);
        attach_handlers(&manager, handlers);
    }

    // 4. Background render loop: pump network events (which apply reactive
    //    Update messages, recomputing dependent defs) and re-render the html
    //    def whenever it changes. Runs on spawn_local; no tokio runtime needed.
    let manager_loop = Rc::clone(&manager);
    wasm_bindgen_futures::spawn_local(async move {
        let mut last = current_html(&manager_loop.borrow(), html_sym);
        loop {
            // #153: hold the mutable borrow only for the dispatch call, never
            // across the timer await below, so a click listener (which also
            // borrows the manager) cannot collide with an outstanding borrow.
            manager_loop.borrow_mut().dispatch_network_events().await;
            let now = current_html(&manager_loop.borrow(), html_sym);
            if now != last {
                if let Some(html) = &now {
                    render_to_dom(html);
                    // #153: re-attach listeners; set_inner_html replaced the
                    // DOM nodes, so previous listeners are gone with them.
                    let handlers = current_handlers(&manager_loop.borrow(), html_sym);
                    attach_handlers(&manager_loop, handlers);
                }
                last = now;
            }
            // Wasm-safe yield (no tokio timer in the browser).
            gloo_timers::future::TimeoutFuture::new(100).await;
        }
    });

    Ok(summary)
}
