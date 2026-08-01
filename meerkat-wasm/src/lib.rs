//! #39: WebAssembly browser client for Meerkat. Connects to a Meerkat server
//! over libp2p WebSocket, fetches a `.mkt` file by path, parses it, and
//! instantiates its services. Imported services are registered as remote
//! (they live on the server), so their defs are live network lookups that
//! drive reactive updates. A background loop re-renders the `html` def to the
//! DOM as Update messages arrive.
//!
//! #153: this client shares the Manager via Rc<RefCell> and calls &mut self
//! methods (dispatch_network_events, remote_action) that await internally, so
//! a borrow necessarily spans those awaits. This is sound because wasm is
//! single-threaded and click tasks use try_borrow_mut with backoff, so the
//! borrows never actually overlap. The lint is allowed file-wide for that
//! reason.
#![allow(clippy::await_holding_refcell_ref)]

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

// #153: attach DOM click listeners for the current html value's handlers.
//
// For now this special-cases the single-button counter demo: each `onclick`
// handler is bound to the first `<button>` in the render target. General
// element-to-handler binding needs a full tree render on the wasm side and is
// tracked as a follow-up. On click, the handler's action value (an
// `ActionClosure`) is run via `remote_action`; because a DOM listener is
// synchronous but running an action is async, the work is spawned rather than
// awaited inline, and the manager is borrowed only inside the spawned task so
// no borrow is held across an await in the listener itself.

thread_local! {
    // #153: keep the current render's event closures alive. attach_handlers
    // clears this before each render, dropping the previous render's
    // closures, so repeated re-renders do not leak closures unboundedly
    // (set_inner_html has already discarded the DOM nodes they were on).
    static HANDLER_CLOSURES: RefCell<Vec<Closure<dyn FnMut()>>> =
        const { RefCell::new(Vec::new()) };
}

fn attach_handlers(manager: &Rc<RefCell<Manager>>, handlers: Vec<(String, Value)>) {
    use wasm_bindgen::JsCast;
    let Some(win) = web_sys::window() else { return };
    let Some(doc) = win.document() else { return };
    let Some(root) = doc.get_element_by_id("render") else {
        return;
    };
    // #153: drop the previous render's closures before creating this render's.
    HANDLER_CLOSURES.with(|c| c.borrow_mut().clear());
    for (event, action) in handlers {
        // #153: map an on* attribute to a DOM event name by dropping "on"
        // (onclick -> click, oninput -> input). Bind onclick to a button and
        // other events to an input, if present under the render root.
        let Some(dom_event) = event.strip_prefix("on") else {
            continue;
        };
        let dom_event = dom_event.to_string();
        let selector = if dom_event == "click" {
            "button"
        } else {
            "input"
        };
        let Ok(Some(target)) = root.query_selector(selector) else {
            continue;
        };
        let manager_cb = Rc::clone(manager);
        let action_cb = action.clone();
        let closure = Closure::<dyn FnMut()>::new(move || {
            // Only ActionClosure is runnable in this slice (no-argument
            // actions). Lambda-valued handlers for value-carrying events are a
            // planned follow-up.
            if let Value::ActionClosure {
                stmts,
                env,
                service_net_id,
            } = action_cb.clone()
            {
                let manager_task = Rc::clone(&manager_cb);
                wasm_bindgen_futures::spawn_local(async move {
                    // Wait for the render loop to release the manager borrow
                    // rather than panicking on a double borrow.
                    loop {
                        if manager_task.try_borrow_mut().is_ok() {
                            break;
                        }
                        gloo_timers::future::TimeoutFuture::new(20).await;
                    }
                    // The borrow spans remote_action's await by necessity;
                    // see the module-level note. The try_borrow_mut loop above
                    // ensures no other borrow is live when we take this one.
                    let result = {
                        let mut m = manager_task.borrow_mut();
                        m.remote_action(&service_net_id, stmts, env, None).await
                    };
                    if let Err(e) = result {
                        status(&format!("action failed: {}", e));
                    }
                });
            }
        });
        let _ = target
            .dyn_ref::<web_sys::EventTarget>()
            .unwrap()
            .add_event_listener_with_callback(&dom_event, closure.as_ref().unchecked_ref());
        // #153: retain this render's closure (dropped on the next render's
        // clear) rather than leaking it with forget().
        HANDLER_CLOSURES.with(|c| c.borrow_mut().push(closure));
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
            // Borrow spans dispatch's await by necessity; see module note.
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
