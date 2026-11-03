use super::ast::{ActionStmt, Decl, Expr, Stmt, Value};
use super::env::Env;
use super::graphs::{analysis::declaration_graphs, free_var::cross_service_deps, ServiceGraphs};
use super::interpreter::{eval, execute_seq, EvalContext, EvalError, WAIT_DIE_DISPLAY_PREFIX};
use super::tt::types::ServiceType;
use crate::net::ast::{NetActionStmt, NetValue};
use crate::net::network_layer::NetworkLayer;
use crate::net::{
    codec, Address, LockGroup, MeerkatMessage, NetworkActor, NetworkCommand, NetworkEvent,
    NetworkReply, ServiceNetId,
};
use crate::runtime::interner::{Interner, Symbol};
use crate::runtime::txn::{ReadEntry, Sources, Transaction, TxnId, VClock, VarState, WaitKey};
use petgraph::{graphmap::DiGraphMap, visit::Dfs};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
#[cfg(not(target_arch = "wasm32"))]
use std::time::Duration;
use tokio::sync::oneshot;

/// How many times an originator re-runs a transaction that died or was told
/// to wait before giving up.
///
/// An originator has no queue to park in, so it waits by retrying (see
/// `execute_action_with_txn`). That makes this budget the originator's wait
/// **timeout** as well: an older transaction -- one wait-die says should win --
/// is killed after `MAX_WAIT_DIE_RETRIES * WAIT_ON_RETRY_BACKOFF_MS` (200ms)
/// of backoff, plus however long the attempts themselves take. That includes
/// aborting each attempt's participants: `send_abort` waits for every
/// acknowledgement, up to `send_and_await_reply`'s 15-second timeout when one
/// is lost. The bound is inherent to retrying instead of parking, not a tuning
/// accident; lifting it needs the background message loop of #28.
pub const MAX_WAIT_DIE_RETRIES: u32 = 10;

/// One cross-service dependency's cached state: the last value delivered for it
/// plus the vector clock that value was stamped with by its writer.
pub type DepEntry = ReadEntry;

/// #24: cached values of each def's cross-service deps:
/// def -> {(source service, member) -> (value, clock)}.
pub type DepCache = HashMap<Symbol, HashMap<(Symbol, Symbol), DepEntry>>;

pub struct Service {
    /// Globally unique identity of this service (address-based when networked).
    pub id: ServiceNetId,
    pub name: Symbol,
    /// Per-variable state: value, lock, and latest write transaction in one place
    pub vars: HashMap<Symbol, VarState>,
    pub defs: HashMap<Symbol, Expr>, // original def expressions for re-evaluation
    pub graphs: ServiceGraphs,       // dependency graph container
    /// #24: who depends on each member: member -> {(listener service id, def)}.
    pub listeners: HashMap<Symbol, HashSet<(ServiceNetId, Symbol)>>,
    /// #24: cached values of each def's cross-service deps:
    /// def -> {(source service, member) -> (value, clock)}.
    pub dep_cache: DepCache,
    /// Defs that must notify listeners after their next successful recomputation
    /// because a committed code update may have changed their Sources.
    pending_source_updates: HashSet<Symbol>,
    /// The transaction building this instance; no other reader may observe it.
    initializing: Option<TxnId>,

    /// Optional lock for whole-service blocking during structural updates
    pub service_lock: Option<TxnId>,
}

/// A remote request parked on a variable's wait queue because the requesting
/// transaction is older than the current lock holder (wait-die wait). It holds
/// everything needed to re-dispatch the request and send its deferred reply
/// once the contended lock frees.
pub enum ParkedRequest {
    Action {
        request_id: u64,
        reply_to: String,
        service: Symbol,
        stmts: Vec<ActionStmt>,
        env: Vec<(Symbol, Value)>,
        tid: TxnId,
    },
    Lookup {
        request_id: u64,
        reply_to: String,
        service: Symbol,
        member: Symbol,
        tid: TxnId,
    },
    Lock {
        request_id: u64,
        reply_to: String,
        txn_id: TxnId,
        services: HashMap<String, LockGroup>,
    },
}

impl ParkedRequest {
    /// The transaction this parked request belongs to. Its age decides serve
    /// order, and identifies it for purging when that transaction aborts.
    pub fn tid(&self) -> &TxnId {
        match self {
            ParkedRequest::Action { tid, .. } => tid,
            ParkedRequest::Lookup { tid, .. } => tid,
            ParkedRequest::Lock { txn_id, .. } => txn_id,
        }
    }
}

/// An applied code update awaiting the first values of its new defs.
type Completing = (u64, String, Vec<(Symbol, Symbol)>, web_time::Instant);

pub struct Manager {
    pub services: HashMap<Symbol, Service>,
    /// Maps service name to remote address (for distributed services)
    pub remote_services: HashMap<Symbol, Address>,
    /// Network actor for distributed communication
    pub network: Option<NetworkActor>,
    /// Pending reply channels keyed by request_id
    pub pending_replies: HashMap<u64, oneshot::Sender<MeerkatMessage>>,
    /// Requests received during an RPC, to serve after its caller finishes.
    deferred_events: VecDeque<NetworkEvent>,
    /// Shared by server dispatch and RPC waits; completing never starts an RPC.
    completing_updates: Vec<Completing>,
    /// (Probabilistically) unique identifier of this node, used in transaction
    /// ids so ids minted on different nodes never collide.
    pub node_id: u64,
    /// Distributed transactions this node is participating in: actions composed
    /// by a remote originator, executed under a shared id and held (locks +
    /// buffered writes) until a Commit or Abort arrives.
    pub pending_txns: HashMap<TxnId, Transaction>,
    /// Requests parked because the requesting transaction is older than a lock
    /// holder (wait-die wait), keyed by the contended WaitKey. Drained
    /// oldest-first when that lock frees on commit or abort.
    pub wait_queue: HashMap<WaitKey, Vec<ParkedRequest>>,
    /// Locks released since the server loop last looked, on which a request is
    /// parked waiting to be woken
    ///
    /// Every release goes through `release_locks`, which fills this, so no
    /// failure path can forget to report its keys. A failure surfaces as an
    /// `EvalError` through call paths with nowhere to put a key set, and
    /// `handle_lock_request` under `dispatch_network_events` has no dispatcher
    /// to hand them to at all. The loop drains it with
    /// `take_freed_awaiting_wake`
    ///
    /// Only keys with a waiter are recorded. That keeps it bounded on a node
    /// with no loop to drain it: a CLI client parks nothing, so it queues
    /// nothing, even though it can reach the discard path
    freed_awaiting_wake: HashSet<WaitKey>,
    /// This node's canonical, dialable address, set once after the network is
    /// listening. Service identities are derived from it, so they are stable for
    /// the life of the process (never empty-then-populated) and match the URL
    /// under which the node advertises its services.
    local_address: Option<String>,
    /// Enable local loopback mode
    pub local: bool,
    /// String interner
    pub interner: Interner,
    /// #24: reply address for each remote listener, keyed by the listener's
    /// ServiceNetId, so the owner can route Updates back to it.
    pub listener_addrs: HashMap<ServiceNetId, String>,
    /// Installed declarations and imports, excluding executable program statements
    unified_ast: Vec<Stmt>,
    /// Graphs for installed declarations without a local service instance
    declaration_graphs: HashMap<Symbol, ServiceGraphs>,
    /// Global local services type environment
    pub local_services: Env<'static, ServiceType>,
    /// Directory whose files a server answers code requests from (#39); None
    /// on a node that serves no files
    pub served_base_dir: Option<std::path::PathBuf>,
}

/// The names of the services a program declares itself.
///
/// These are served locally, so an `-i` flag naming one of them is never
/// honoured: see `Manager::register_remote_services`.
pub fn local_service_names(prog: &[Stmt]) -> HashSet<Symbol> {
    prog.iter()
        .filter_map(|s| match s {
            Stmt::Service { name, .. } => Some(*name),
            _ => None,
        })
        .collect()
}

/// Pointwise max of `clocks`; a dimension a clock omits counts as zero.
pub(super) fn join_clocks<'a>(clocks: impl IntoIterator<Item = &'a VClock>) -> VClock {
    let mut joined = VClock::new();
    for clock in clocks {
        for (&dim, &counter) in clock {
            let entry = joined.entry(dim).or_insert(0);
            *entry = (*entry).max(counter);
        }
    }
    joined
}

/// Whether an input's `clock` has reached `target` on every dimension its
/// value depends on (§5.3). Unknown `sources` depend on all of them
fn caught_up(target: &VClock, clock: &VClock, sources: Option<&Sources>) -> bool {
    target.iter().all(|(dim, &counter)| {
        sources.is_some_and(|sources| !sources.contains(dim))
            || clock.get(dim).copied().unwrap_or(0) >= counter
    })
}

/// §5.2's `V_new`: the joined history `v_base` advanced once in each written var
fn advance(mut v_base: VClock, write_set: &HashSet<(Symbol, Symbol)>) -> VClock {
    for &var in write_set {
        *v_base.entry(var).or_insert(0) += 1;
    }
    v_base
}

/// Let the network run for `ms` milliseconds. As in `send_and_await_reply`,
/// browsers have no tokio timer driver
async fn pause(ms: u32) {
    #[cfg(not(target_arch = "wasm32"))]
    tokio::time::sleep(Duration::from_millis(ms.into())).await;
    #[cfg(target_arch = "wasm32")]
    send_wrapper::SendWrapper::new(gloo_timers::future::TimeoutFuture::new(ms)).await;
}

impl Manager {
    pub fn new(interner: Interner) -> Self {
        Manager {
            services: HashMap::new(),
            remote_services: HashMap::new(),
            network: None,
            pending_replies: HashMap::new(),
            deferred_events: VecDeque::new(),
            completing_updates: Vec::new(),
            node_id: Self::random_node_id(),
            pending_txns: HashMap::new(),
            wait_queue: HashMap::new(),
            freed_awaiting_wake: HashSet::new(),
            local_address: None,
            local: false,
            interner,
            listener_addrs: HashMap::new(),
            unified_ast: Vec::new(),
            declaration_graphs: HashMap::new(),
            local_services: Env::new(None),
            served_base_dir: None,
        }
    }

    /// Install initial or imported declarations and retain their analyzed graphs.
    /// Running services keep their graphs; code updates install those separately.
    pub fn install_unified_ast(&mut self, ast: Vec<Stmt>) {
        let mut graphs = declaration_graphs(&ast, |name, _| !self.services.contains_key(&name));
        self.declaration_graphs.clear();
        self.install_analyzed_ast(ast, &mut graphs);
    }

    /// Install committed declarations, moving only graphs without local instances.
    /// Graphs of declarations left unchanged stay installed.
    /// Leave local candidates for update_service_graphs and listener installation.
    pub(crate) fn install_analyzed_ast(
        &mut self,
        mut ast: Vec<Stmt>,
        graphs: &mut HashMap<Symbol, ServiceGraphs>,
    ) {
        // Node validates top-level statements against the final program state;
        // the caller keeps them for execution. Updates check only current declarations.
        ast.retain(|stmt| {
            matches!(
                stmt,
                Stmt::Service { .. } | Stmt::Import { .. } | Stmt::Connect { .. }
            )
        });
        self.declaration_graphs
            .extend(ast.iter().filter_map(|stmt| match stmt {
                Stmt::Service { name, .. } if !self.services.contains_key(name) => {
                    graphs.remove(name).map(|graph| (*name, graph))
                }
                _ => None,
            }));
        self.unified_ast = ast;
    }

    /// Installed declarations and imports. Change them through
    /// `install_unified_ast` or a code update, which maintain their graphs.
    pub fn unified_ast(&self) -> &[Stmt] {
        &self.unified_ast
    }

    /// Park a request on the wait queue for the contended `WaitKey`
    pub fn park_request_key(&mut self, key: WaitKey, parked: ParkedRequest) {
        self.wait_queue.entry(key).or_default().push(parked);
    }

    /// Take the locks released since the last call, to wake whatever is
    /// parked on them. Empty on a quiet loop iteration
    pub fn take_freed_awaiting_wake(&mut self) -> HashSet<WaitKey> {
        std::mem::take(&mut self.freed_awaiting_wake)
    }

    /// Park a request on the wait queue for the contended `(service, var)`
    /// It receives no reply until that variable's lock frees and it is
    /// re-dispatched
    pub fn park_request(&mut self, service: Symbol, var: Symbol, parked: ParkedRequest) {
        let key = WaitKey::Member(self.service_net_id_for_name(service), var);
        self.park_request_key(key, parked);
    }

    /// After a holder releases locks on commit or abort, return the oldest
    /// parked request waiting on each freed lock key, removing it from the
    /// queue. Serving the oldest first is what keeps an older transaction from
    /// being starved by a stream of younger requests.
    pub fn take_ready_waiters(&mut self, freed: &HashSet<WaitKey>) -> Vec<ParkedRequest> {
        let mut ready = Vec::new();
        for key in freed {
            if let Some(waiters) = self.wait_queue.get_mut(key) {
                if let Some(idx) = waiters
                    .iter()
                    .enumerate()
                    .min_by(|(_, a), (_, b)| a.tid().cmp(b.tid()))
                    .map(|(i, _)| i)
                {
                    ready.push(waiters.remove(idx));
                    if waiters.is_empty() {
                        self.wait_queue.remove(key);
                    }
                }
            }
        }
        ready
    }

    /// Remove and return all parked requests belonging to a transaction, used
    /// when it aborts so its waiters do not later wake and prepare locks for a
    /// transaction the originator has abandoned.
    pub fn purge_parked_txn(&mut self, tid: &TxnId) -> Vec<ParkedRequest> {
        let mut removed = Vec::new();
        for waiters in self.wait_queue.values_mut() {
            let mut i = 0;
            while i < waiters.len() {
                if waiters[i].tid() == tid {
                    removed.push(waiters.remove(i));
                } else {
                    i += 1;
                }
            }
        }
        self.wait_queue.retain(|_, v| !v.is_empty());
        removed
    }

    /// (request_id, reply_to) for every currently parked request, so the owner
    /// can periodically reassure waiting originators that they are still queued
    /// (keepalive), keeping the wait from hitting the reply timeout.
    pub fn parked_keepalive_targets(&self) -> Vec<(u64, String)> {
        let mut out = Vec::new();
        for waiters in self.wait_queue.values() {
            for p in waiters {
                let pair = match p {
                    ParkedRequest::Action {
                        request_id,
                        reply_to,
                        ..
                    } => (*request_id, reply_to.clone()),
                    ParkedRequest::Lookup {
                        request_id,
                        reply_to,
                        ..
                    } => (*request_id, reply_to.clone()),
                    ParkedRequest::Lock {
                        request_id,
                        reply_to,
                        ..
                    } => (*request_id, reply_to.clone()),
                };
                out.push(pair);
            }
        }
        out
    }

    /// Record this node's canonical address once the network is listening,
    /// so service identities are stable and consistent with the
    /// advertised URL
    pub fn set_local_address(&mut self, addr: String) {
        self.local_address = Some(addr);
    }

    /// Compute the global identity of a service owned by this node. When
    /// the node has a network address, the identity is that address plus
    /// the service slug; otherwise it falls back to the bare name for
    /// local-only execution
    fn compute_service_net_id(&self, service_name: Symbol) -> ServiceNetId {
        let name_str = self.interner.get(service_name);
        match &self.local_address {
            Some(addr) if !addr.is_empty() => ServiceNetId::new(format!("{}/{}", addr, name_str)),
            Some(_) | None => {
                // No network address: fall back to the bare name. On a
                // single node names are unambiguous, and because
                // `local_address` is fixed at startup this choice never
                // changes mid-run
                ServiceNetId::new(name_str)
            }
        }
    }

    /// Register the parsed `-i <url>` mappings, then run every locally
    /// resolved import. Steps 1-3 of CLI startup; the caller then creates the
    /// services the program declares itself, in program order.
    ///
    /// Shared by every entry point that starts a node from a parsed program, so
    /// the ordering below is established once rather than re-derived per
    /// caller. `unified_ast` must already be set.
    ///
    /// `imports` holds the statements of the imported files this node runs, in
    /// dependency order (`Node::local_imports`). Their services are built and
    /// their updates applied; their other statements are not run here.
    ///
    /// `remote_url_map` maps a service slug to the address serving it. The CLI
    /// builds it from repeated `-i <url>` flags, taking the slug from each
    /// URL's final path segment (`meerkat/src/main.rs`).
    ///
    /// Errors:
    ///   `EvalError`: If instantiating an imported service or applying an
    ///   imported update fails
    pub async fn register_and_instantiate_imports(
        &mut self,
        imports: &[Stmt],
        prog: &[Stmt],
        remote_url_map: &HashMap<String, String>,
    ) -> Result<(), EvalError> {
        // Services declared by this program itself. An import must not
        // instantiate these: the caller creates them from `prog`.
        let local = local_service_names(prog);

        // Register every configured remote service up front, before any import
        // is processed. Doing it lazily as each root `Stmt::Import` is reached
        // makes instantiation order-dependent: an earlier local import walks
        // the whole unified AST, and a remote service it finds there would be
        // built locally as a phantom copy. A remote service reached only
        // transitively has no root `Stmt::Import` at all, so it would never be
        // registered and every read and action would silently target that local
        // copy instead of the owning node.
        self.register_remote_services(remote_url_map, &local);

        // Instantiate every locally resolved import before any of this
        // program's own services, in dependency order. The grammar allows
        // `import` to appear after the service that uses it, and static checks
        // accept that because the unified AST is reordered, so creating imports
        // only when their `Stmt::Import` is reached would build `app` before
        // the `dep` it reads and fail with `ServiceNotFound`.
        let mut imported = HashMap::new();
        self.unified_ast.retain(|stmt| match stmt {
            Stmt::Service { name, decls }
                if !local.contains(name) && !self.remote_services.contains_key(name) =>
            {
                if imported.contains_key(name) {
                    // The cached graph may belong to this skipped declaration.
                    self.declaration_graphs.remove(name);
                    false
                } else {
                    imported.insert(*name, decls.clone());
                    true
                }
            }
            _ => true,
        });
        let graphs = declaration_graphs(&self.unified_ast, |name, _| {
            !self.services.contains_key(&name) && !self.declaration_graphs.contains_key(&name)
        });
        self.declaration_graphs.extend(graphs);

        // Run each import's statements in file order, as on a node of its own:
        // a service is built from the declaration kept above, and an update is
        // applied where it stands, so whatever follows -- such as a later
        // service whose `var` reads the updated member -- sees it.
        for stmt in imports {
            match stmt {
                Stmt::Service { name, .. } => {
                    let Some(decls) = imported.remove(name) else {
                        continue;
                    };
                    if self.services.contains_key(name) {
                        continue;
                    }
                    self.create_service(*name, decls).await?;
                    println!("Imported service '{}'", self.interner.get(*name));
                }
                Stmt::Update { service_name, .. } => {
                    let mut txn = crate::runtime::update::Transaction::new(vec![stmt.clone()]);
                    txn.poll(self).await?;
                    println!("Service '{}' updated", self.interner.get(*service_name));
                }
                Stmt::Atomic { updates } => {
                    if !updates.is_empty() {
                        let mut txn = crate::runtime::update::Transaction::new(updates.clone());
                        txn.poll(self).await?;
                        println!(
                            "Atomic update transaction completed ({} updates)",
                            updates.len()
                        );
                    }
                }
                Stmt::Import { .. }
                | Stmt::Connect { .. }
                | Stmt::Test { .. }
                | Stmt::ActionStmt(_)
                | Stmt::Watch { .. } => {}
            }
        }
        Ok(())
    }

    /// Record each entry of `remote_url_map` -- service slug to the address
    /// serving it -- in `remote_services`.
    ///
    /// The CLI builds that map from repeated `-i <url>` flags, taking the slug
    /// from each URL's final path segment (`meerkat/src/main.rs`); a slug is
    /// therefore whatever the URL happens to end with, not something the user
    /// states outright.
    ///
    /// A service this program declares itself is never registered as remote,
    /// whatever `-i` says. `lookup` consults `remote_services` before anything
    /// local, so a slug collision would otherwise route a locally declared
    /// service's reads and writes to a peer and leave the local copy
    /// unreachable. Since the slug is implicit, such a collision is easy to
    /// hit by accident, so it is reported rather than quietly dropped.
    pub fn register_remote_services(
        &mut self,
        remote_url_map: &HashMap<String, String>,
        local: &HashSet<Symbol>,
    ) {
        for (svc, url) in remote_url_map {
            let sym = self.interner.insert(svc);
            if local.contains(&sym) {
                println!(
                    "Warning: ignoring '-i {}': '{}' is declared by this program \
                     and is served locally",
                    url, svc
                );
                continue;
            }
            self.remote_services.insert(sym, Address::new(url.as_str()));
            println!("Remote service '{}' registered at {}", svc, url);
        }
    }

    pub async fn create_service(
        &mut self,
        name: Symbol,
        decls: Vec<Decl>,
    ) -> Result<(), EvalError> {
        self.ensure_service_visible(name, None)?;
        let id = self.compute_service_net_id(name);
        let mut txn = Transaction::new(TxnId::new(self.node_id));
        txn.service_locked.insert(id.clone());
        // Register the service (with its real `ServiceNetId`) before
        // evaluating any declarations, so action closures built during
        // initialization are stamped with the correct `ServiceNetId`
        // instead of `service_net_id_for_name`'s bare-name fallback
        let exists = self.unified_ast.iter().any(|s| {
            if let Stmt::Service { name: existing, .. } = s {
                *existing == name
            } else {
                false
            }
        });
        let inserted = !exists;
        if inserted {
            self.unified_ast.push(Stmt::Service {
                name,
                decls: decls.clone(),
            });
        }

        let graphs = self
            .declaration_graphs
            .remove(&name)
            .or_else(|| {
                declaration_graphs(&self.unified_ast, |service, _| service == name).remove(&name)
            })
            .unwrap_or_default();

        // Earlier services may already depend on this not-yet-created owner.
        // Reuse initialization's deferred publication to complete those inputs.
        let mut listeners: HashMap<_, HashSet<_>> = HashMap::new();
        if !self.remote_services.contains_key(&name) {
            for service in self.services.values() {
                for (&def, inputs) in &service.graphs.cross_deps {
                    for &(owner, member) in inputs {
                        if owner == name {
                            listeners
                                .entry(member)
                                .or_default()
                                .insert((service.id.clone(), def));
                        }
                    }
                }
            }
        }

        self.services.insert(
            name,
            Service {
                id,
                name,
                vars: HashMap::new(),
                defs: HashMap::new(),
                graphs,
                listeners,
                dep_cache: HashMap::new(),
                pending_source_updates: HashSet::new(),
                initializing: Some(txn.id.clone()),
                service_lock: Some(txn.id.clone()),
            },
        );

        let mut env: Vec<(Symbol, Value)> = vec![];
        let svc_name = name;
        // Inputs a def read beyond its static inputs, through a call
        let mut activated = Vec::new();

        let result = async {
            for decl in decls {
                let is_def = matches!(decl, Decl::DefDecl { .. });
                let (name, val) = match decl {
                    Decl::VarDecl { name, val, .. } | Decl::DefDecl { name, val, .. } => {
                        (name, val)
                    }
                    Decl::TableDecl { .. } => {
                        return Err(EvalError::NotImplemented);
                    }
                };
                // Defs resolve service members through their transactional snapshots;
                // variable initializers retain their existing declaration environment.
                txn.observed_reads = is_def.then(HashSet::new);
                let result = eval(
                    &val,
                    if is_def { &[] } else { &env },
                    &mut EvalContext {
                        manager: self,
                        service_name: svc_name,
                        txn: Some(&mut txn),
                    },
                )
                .await;
                let reads = txn.observed_reads.take();
                let value = result?;
                let mut var_value = VarState::new(value);
                if let Some(reads) = reads {
                    let Some(mut inputs) = reads
                        .iter()
                        .map(|&input| {
                            let key = (self.service_net_id_for_name(input.0), input.1);
                            let (_, clock, sources) = txn.read_cache.get(&key)?;
                            Some((clock.as_ref()?, sources.as_ref().map(Cow::Borrowed)))
                        })
                        .collect::<Option<Vec<_>>>()
                    else {
                        return Err(EvalError::RuntimeError(format!(
                            "Cannot initialize '{}.{}': input histories are missing",
                            self.interner.get(svc_name),
                            self.interner.get(name),
                        )));
                    };
                    // Keep available local inputs from unselected branches
                    // in step with static Sources. Actual reads retain their
                    // snapshot clocks; deferred remote inputs need no RPC. A
                    // local def referenced before its own initialization has
                    // no state yet, so its inputs stand in for it, as in Sources.
                    let graphs = &self.services[&svc_name].graphs;
                    let static_inputs = |def| {
                        graphs
                            .reactive_graph
                            .neighbors_directed(def, petgraph::Direction::Incoming)
                            .map(move |member| (svc_name, member))
                            .chain(graphs.cross_deps.get(&def).into_iter().flatten().copied())
                    };
                    let mut pending: Vec<_> = static_inputs(name).collect();
                    activated.extend(
                        reads
                            .iter()
                            .filter(|input| !pending.contains(input))
                            .map(|&input| (name, input)),
                    );
                    let mut seen = HashSet::new();
                    while let Some(input) = pending.pop() {
                        if reads.contains(&input)
                            || self.remote_services.contains_key(&input.0)
                            || !seen.insert(input)
                        {
                            continue;
                        }
                        match self
                            .services
                            .get(&input.0)
                            .and_then(|service| service.vars.get(&input.1))
                        {
                            Some(state) => inputs.push((
                                &state.vector_clock,
                                self.sources(input.0, input.1, Some(&txn)),
                            )),
                            None if input.0 == svc_name && graphs.defs.contains(&input.1) => {
                                pending.extend(static_inputs(input.1))
                            }
                            None => {}
                        }
                    }
                    let (vector_clock, gate_ok, _) = self.check_input_clocks(&inputs);
                    if !gate_ok {
                        return Err(EvalError::RuntimeError(format!(
                            "Cannot initialize '{}.{}': input clocks are missing or inconsistent",
                            self.interner.get(svc_name),
                            self.interner.get(name),
                        )));
                    }
                    var_value.vector_clock = vector_clock;
                    var_value.sources =
                        self.sources(svc_name, name, Some(&txn))
                            .and_then(|sources| {
                                let mut sources = sources.into_owned();
                                for &(owner, member) in &reads {
                                    let key = (self.service_net_id_for_name(owner), member);
                                    match txn.read_cache[&key].2.as_ref() {
                                        Some(read_sources) => sources.extend(read_sources),
                                        None if self.remote_services.contains_key(&owner) => {
                                            return None
                                        }
                                        None => {}
                                    }
                                }
                                Some(sources)
                            });
                } else {
                    var_value.sources = Some(Sources::from([(svc_name, name)]));
                }
                env.push((name, var_value.value.clone()));
                var_value.latest_write_txn = Some(txn.id.clone());
                if let Some(service) = self.services.get_mut(&svc_name) {
                    service.vars.insert(name, var_value);
                    if is_def {
                        service.defs.insert(name, val);
                    }
                }
            }

            // The remote pairs initialization read stay each def's incorporated
            // inputs, with their clocks, including those a call reached
            let remote_inputs: Vec<_> = self.services[&svc_name]
                .graphs
                .cross_deps
                .iter()
                .flat_map(|(&def, inputs)| inputs.iter().map(move |&input| (def, input)))
                .chain(activated.iter().copied())
                .filter(|(_, (owner, _))| self.remote_services.contains_key(owner))
                .filter_map(|(def, input)| {
                    let key = (self.service_net_id_for_name(input.0), input.1);
                    txn.read_cache.get(&key).map(|entry| (def, input, entry))
                })
                .collect();
            let service = self.services.get_mut(&svc_name).unwrap();
            for (def, input, entry) in remote_inputs {
                service
                    .dep_cache
                    .entry(def)
                    .or_default()
                    .insert(input, entry.clone());
            }

            // Commit only after every initializer succeeds; participant failures
            // take the same rollback path as failed initialization.
            self.commit_txn(&txn).await.map_or(Ok(()), Err)
        }
        .await;
        if result.is_ok() {
            // Requests received during initialization remain registered, but
            // receive their initial snapshot only after this instance commits.
            let deferred_members: Vec<_> =
                self.services[&svc_name].listeners.keys().copied().collect();
            let graphs = std::mem::take(&mut self.services.get_mut(&svc_name).unwrap().graphs);
            self.update_service_graphs(svc_name, graphs).await;
            // Their later changes must reach the def as its static inputs' do
            for (def, input) in activated {
                self.follow_input((svc_name, def), input).await;
            }
            let affected = self.services[&svc_name]
                .listeners
                .values()
                .flatten()
                .filter_map(|(listener, def)| {
                    self.service_name_for_net_id(listener)
                        .filter(|name| *name != svc_name)
                        .map(|name| (name, *def))
                })
                .collect();
            let pending = std::mem::take(
                &mut self
                    .services
                    .get_mut(&svc_name)
                    .unwrap()
                    .pending_source_updates,
            );
            self.mark_source_updates(affected);
            self.services
                .get_mut(&svc_name)
                .unwrap()
                .pending_source_updates = pending;
            self.services.get_mut(&svc_name).unwrap().initializing = None;
            self.release_all_locks(&txn);
            for member in deferred_members {
                self.propagate(svc_name, member).await;
            }
        } else {
            // Initialization only reads participants. Abort releases their locks.
            for addr in txn.participants.iter().cloned().collect::<Vec<_>>() {
                self.send_abort(addr, &txn.id).await;
            }
            let failed_service = self.services.remove(&svc_name).unwrap();
            if inserted {
                self.unified_ast.retain(
                    |stmt| !matches!(stmt, Stmt::Service { name, .. } if *name == svc_name),
                );
            } else {
                // A failed local instance must not erase an existing import.
                self.declaration_graphs
                    .insert(svc_name, failed_service.graphs);
            }
            self.release_all_locks(&txn);
        }

        // Post-condition: If we return an error, ensure no phantom service was left in the AST
        debug_assert!(
            result.is_ok()
                || !inserted
                || !self.unified_ast.iter().any(|s| {
                    if let Stmt::Service { name: existing, .. } = s {
                        *existing == svc_name
                    } else {
                        false
                    }
                }),
            "Invariant violation: phantom service leaked into unified_ast on failure"
        );

        result
    }

    /// Update pre-computed dependency graphs for a service and re-wire reactive listeners
    ///
    /// Args:
    ///     `svc_name` (`Symbol`): The target service symbol
    ///     `new_graphs` (`ServiceGraphs`): The new pre-computed service graphs
    ///
    /// Returns:
    ///     `()`
    pub async fn update_service_graphs(&mut self, svc_name: Symbol, new_graphs: ServiceGraphs) {
        let (updated_id, old_edges, edges, defs) = match self.services.get_mut(&svc_name) {
            Some(service) => {
                let old_edges: HashSet<_> = Self::listener_edges(svc_name, service).collect();
                service.graphs = new_graphs;
                let defs = &service.graphs.defs;
                service.dep_cache.retain(|def, _| defs.contains(def));
                service
                    .pending_source_updates
                    .retain(|def| defs.contains(def));
                let edges: Vec<_> = Self::listener_edges(svc_name, service).collect();
                (service.id.clone(), old_edges, edges, defs.clone())
            }
            None => return,
        };

        // Keep runtime input registrations only while their listener is a def.
        for (&owner, service) in self.services.iter_mut() {
            for (&member, listener_set) in service.listeners.iter_mut() {
                listener_set.retain(|(lid, def)| {
                    *lid != updated_id
                        || (defs.contains(def) && !old_edges.contains(&(owner, member, *def)))
                });
            }
        }

        for (owner, member, listener_def) in edges {
            if self.services.contains_key(&owner) {
                if let Some(owner_svc) = self.services.get_mut(&owner) {
                    owner_svc
                        .listeners
                        .entry(member)
                        .or_default()
                        .insert((updated_id.clone(), listener_def));
                }
            } else {
                self.subscribe_remote(owner, member, updated_id.clone(), listener_def)
                    .await;
            }
        }
    }

    /// The `(owner, member, def)` listener registrations `service`'s graphs imply
    fn listener_edges(
        svc_name: Symbol,
        service: &Service,
    ) -> impl Iterator<Item = (Symbol, Symbol, Symbol)> + '_ {
        let local = service
            .graphs
            .reactive_edges()
            .into_iter()
            .map(move |(member, def)| (svc_name, member, def));
        let cross = service.graphs.cross_deps.iter().flat_map(|(&def, inputs)| {
            inputs
                .iter()
                .map(move |&(owner, member)| (owner, member, def))
        });
        local
            .chain(cross)
            .filter(|(_, _, def)| service.graphs.defs.contains(def))
    }

    /// Initialization is private to its transaction, including cache reads.
    /// Ordinary service locks alone cannot protect non-transactional readers.
    pub(crate) fn ensure_service_visible(
        &self,
        service: Symbol,
        txn_id: Option<&TxnId>,
    ) -> Result<(), EvalError> {
        if self
            .services
            .get(&service)
            .and_then(|s| s.initializing.as_ref())
            .is_some_and(|owner| Some(owner) != txn_id)
        {
            return Err(EvalError::RuntimeError(format!(
                "Service '{}' is still initializing",
                self.interner.get(service)
            )));
        }
        Ok(())
    }

    /// Lookup a variable or def's value within a service
    ///
    /// Evaluates stored def expressions to ensure freshness and acquires
    /// appropriate read locks when executed inside a transaction.
    ///
    /// Args:
    ///     var_name (Symbol): The symbol of the variable or definition to look up
    ///     service_name (Symbol): The symbol of the service containing the variable
    ///     txn (Option<&mut Transaction>): An optional active transaction context
    ///
    /// Returns:
    ///     Result<Value, EvalError>: The retrieved runtime value, or an error
    ///
    /// Raises:
    ///     EvalError::VarNotFound: If the variable does not exist in the service
    ///     EvalError::ServiceNotFound: If the service is not found
    pub async fn lookup(
        &mut self,
        var_name: Symbol,
        service_name: Symbol,
        mut txn: Option<&mut Transaction>,
    ) -> Result<Value, EvalError> {
        if let Some(reads) = txn.as_deref_mut().and_then(|t| t.observed_reads.as_mut()) {
            reads.insert((service_name, var_name));
        }
        // Check if service is remote
        if self.remote_services.contains_key(&service_name) {
            return self.remote_lookup(service_name, var_name, txn).await;
        }

        self.ensure_service_visible(service_name, txn.as_deref().map(|t| &t.id))?;
        // Local var read. If inside a transaction, return the cached value if
        // present, otherwise acquire a read lock lazily and cache the value.
        // Transaction state is keyed by (service id, variable) so the same name
        // in different services never collides.
        let key = (self.service_net_id_for_name(service_name), var_name);
        if let Some(t) = txn.as_deref_mut() {
            if let Some(cached) = t.read_cache.get(&key) {
                return Ok(cached.0.clone());
            }
            if !t.locked.contains(&key) {
                self.acquire_read_lock(service_name, var_name, &t.id)?;
                t.locked.insert(key.clone());
            }
        }

        // Return stored var value (and cache it for the transaction)
        if let Some(service) = self.services.get(&service_name) {
            if let Some(var_state) = service.vars.get(&var_name) {
                let value = var_state.value.clone();
                if let Some(t) = txn {
                    t.read_cache.insert(
                        key,
                        (
                            value.clone(),
                            Some(var_state.vector_clock.clone()),
                            var_state.sources.clone(),
                        ),
                    );
                }
                return Ok(value);
            }
        }
        Err(EvalError::VarNotFound(format!(
            "Variable '{}' not found in service '{}'",
            self.interner.get(var_name),
            self.interner.get(service_name)
        )))
    }

    /// Read a value with the clock of that same snapshot, including cached
    /// transactional reads. Used by lookup replies to preserve the pair.
    pub async fn lookup_with_clock(
        &mut self,
        var_name: Symbol,
        service_name: Symbol,
        mut txn: Option<&mut Transaction>,
    ) -> Result<ReadEntry, EvalError> {
        if self.remote_services.contains_key(&service_name) {
            return self
                .remote_lookup_with_clock(service_name, var_name, txn)
                .await;
        }
        let value = self
            .lookup(var_name, service_name, txn.as_deref_mut())
            .await?;
        let (clock, sources) = match txn {
            Some(txn) => {
                let entry =
                    &txn.read_cache[&(self.service_net_id_for_name(service_name), var_name)];
                (entry.1.clone(), entry.2.clone())
            }
            None => {
                let state = &self.services[&service_name].vars[&var_name];
                (Some(state.vector_clock.clone()), state.sources.clone())
            }
        };
        Ok((value, clock, sources))
    }

    /// Assign a value to a service variable
    ///
    /// In a transaction, this acquires a write lock (or upgrades an existing read lock)
    /// and buffers the write. Non-transactional writes are applied immediately and propagated.
    ///
    /// Args:
    ///     service_name (Symbol): The symbol of the service containing the variable
    ///     var_name (Symbol): The symbol of the variable to assign
    ///     value (Value): The value to assign to the variable
    ///     txn (Option<&mut Transaction>): An optional active transaction context
    ///
    /// Returns:
    ///     Result<(), EvalError>: Ok on success, or a lock/validation error
    ///
    /// Raises:
    ///     EvalError::VarNotFound: If the variable does not exist in the service
    ///     EvalError::ServiceNotFound: If the service is not found
    ///     EvalError::WaitDieAbort: If the transaction aborts due to lock contention
    pub async fn assign(
        &mut self,
        service_name: Symbol,
        var_name: Symbol,
        value: Value,
        txn: Option<&mut Transaction>,
    ) -> Result<(), EvalError> {
        self.ensure_service_visible(service_name, txn.as_deref().map(|t| &t.id))?;
        // Inside a transaction: acquire the write lock lazily (upgrading from a
        // read lock for read-then-write patterns like x = x + 1) and buffer the
        // write. The buffered value is applied to the service only at commit, so
        // a transaction that fails partway leaves no partial writes behind.
        if txn.is_some() {
            let key = (self.service_net_id_for_name(service_name), var_name);
            enum LockAction {
                Acquire,
                Upgrade,
            }
            let (txn_id, kind) = {
                let t = txn.as_deref().unwrap();
                let kind = if t.locked.contains(&key) {
                    LockAction::Upgrade
                } else {
                    LockAction::Acquire
                };
                (t.id.clone(), kind)
            };
            match kind {
                LockAction::Upgrade => {
                    self.upgrade_to_write_lock(service_name, var_name, &txn_id)?
                }
                LockAction::Acquire => self.acquire_write_lock(service_name, var_name, &txn_id)?,
            }
            if let Some(t) = txn {
                t.locked.insert(key.clone());
                t.written.insert(key.clone(), value.clone());
                // Reads later in the same transaction see the buffered write
                t.read_cache.insert(key, (value, None, None));
            }
            return Ok(());
        }

        // Non-transactional path: apply the write immediately and propagate.
        if let Some(service) = self.services.get_mut(&service_name) {
            if let Some(var_state) = service.vars.get_mut(&var_name) {
                var_state.value = value;
                self.simultaneous_bump(VClock::new(), &HashSet::from([(service_name, var_name)]));
            } else {
                // in theory we should never encounter this after nameres is fully integrated
                // keep this in mind to guide future development
                return Err(EvalError::VarNotFound(format!(
                    "Variable '{}' not found in service '{}'",
                    self.interner.get(var_name),
                    self.interner.get(service_name)
                )));
            }
        } else {
            return Err(EvalError::ServiceNotFound(format!(
                "Service '{}' not found",
                self.interner.get(service_name)
            )));
        }

        // propagate: re-evaluate defs that depend on this var in topo order
        self.propagate(service_name, var_name).await;
        Ok(())
    }

    // helper function to get the vector clock for a particular var def
    fn get_clock(&self, service_name: &Symbol, var: &Symbol) -> Option<&VClock> {
        let svc = self.services.get(service_name)?;
        let vars = svc.vars.get(var)?;
        Some(&vars.vector_clock)
    }

    /// Traverse the initializing graph; completed inputs retain paired Sources.
    fn sources(
        &self,
        svc: Symbol,
        member: Symbol,
        txn: Option<&Transaction>,
    ) -> Option<Cow<'_, Sources>> {
        let service = self.services.get(&svc)?;
        self.ensure_service_visible(svc, txn.map(|txn| &txn.id))
            .ok()?;
        if !txn.is_some_and(|txn| service.initializing.as_ref() == Some(&txn.id)) {
            return service
                .vars
                .get(&member)?
                .sources
                .as_ref()
                .map(Cow::Borrowed);
        }
        let graphs = &service.graphs;
        let mut sources = Sources::new();
        let mut seen = HashSet::new();
        let mut seen_external = HashSet::new();
        let mut pending = vec![member];
        while let Some(name) = pending.pop() {
            if !seen.insert(name) {
                continue;
            }
            if graphs.vars.contains(&name) {
                sources.insert((svc, name));
            } else if graphs.defs.contains(&name) {
                pending.extend(
                    graphs
                        .reactive_graph
                        .neighbors_directed(name, petgraph::Direction::Incoming),
                );
                for &input in graphs.cross_deps.get(&name).into_iter().flatten() {
                    if self.remote_services.contains_key(&input.0) {
                        let entry = txn
                            .and_then(|txn| {
                                txn.read_cache
                                    .get(&(self.service_net_id_for_name(input.0), input.1))
                            })
                            .or_else(|| service.dep_cache.get(&name)?.get(&input))?;
                        sources.extend(entry.2.as_ref()?);
                    } else if input.0 == svc {
                        pending.push(input.1);
                    } else {
                        if !seen_external.insert(input) {
                            continue;
                        }
                        let owner = self.services.get(&input.0)?;
                        self.ensure_service_visible(input.0, txn.map(|txn| &txn.id))
                            .ok()?;
                        let entry = txn.and_then(|txn| {
                            txn.read_cache
                                .get(&(self.service_net_id_for_name(input.0), input.1))
                        });
                        let snapshot = match entry {
                            Some(entry) => entry.2.as_ref(),
                            None => owner.vars.get(&input.1)?.sources.as_ref(),
                        };
                        sources.extend(snapshot?);
                    }
                }
            } else {
                return None;
            }
        }
        Some(Cow::Owned(sources))
    }

    // helper function to apply a simultaneous bump, as described in section 5.2 of vector clock semantics
    pub(super) fn simultaneous_bump(
        &mut self,
        read_clock: VClock,
        write_set: &HashSet<(Symbol, Symbol)>,
    ) {
        let v_base = join_clocks(
            std::iter::once(&read_clock).chain(
                write_set
                    .iter()
                    .filter_map(|(svc, var)| self.get_clock(svc, var)),
            ),
        );
        let v_new = advance(v_base, write_set);
        for (svc, var) in write_set {
            if let Some(vs) = self.services.get_mut(svc).and_then(|s| s.vars.get_mut(var)) {
                vs.vector_clock = v_new.clone(); // write the new base vector clock back to everything in the write set
            }
        }
    }

    /// §5.2's `V_base` and `W` for everything `txn` has done so far, on this
    /// node and on the participants that replied to it: the clocks of the
    /// snapshots it read and the prior clocks of the vars it overwrote, and
    /// those vars
    ///
    /// Reads are taken from the snapshots the transaction recorded, not the
    /// current state, which may have moved on since
    pub fn transaction_history(&self, txn: &Transaction) -> (VClock, HashSet<(Symbol, Symbol)>) {
        let local_writes: Vec<_> = txn
            .written
            .keys()
            .filter_map(|(sid, var)| self.service_name_for_net_id(sid).map(|n| (n, *var)))
            .collect();
        let v_base = join_clocks(
            txn.read_cache
                .values()
                .filter_map(|(_, clock, _)| clock.as_ref())
                .chain(
                    local_writes
                        .iter()
                        .filter_map(|(svc, var)| self.get_clock(svc, var)),
                )
                .chain([&txn.participant_history]),
        );
        let mut write_set = txn.participant_writes.clone();
        write_set.extend(local_writes);
        (v_base, write_set)
    }

    /// Mark affected defs after all updated graphs are installed. A blocked def
    /// keeps its notification until an input permits successful recomputation.
    pub(crate) fn mark_source_updates(&mut self, members: Vec<(Symbol, Symbol)>) {
        if members.is_empty() {
            return;
        }
        // This temporary graph joins existing dependency edges across services,
        // including imported declarations. It is built only for code updates.
        let mut dependents = DiGraphMap::<(Symbol, Symbol), ()>::new();
        for (&svc, graphs) in self
            .services
            .iter()
            .map(|(name, service)| (name, &service.graphs))
            .chain(self.declaration_graphs.iter())
        {
            for &def in &graphs.defs {
                dependents.add_node((svc, def));
            }
            for (input, def, _) in graphs.reactive_graph.all_edges() {
                // A var's initializer does not create a reactive dependency.
                if graphs.defs.contains(&def) {
                    dependents.add_edge((svc, input), (svc, def), ());
                }
            }
            for (&def, inputs) in &graphs.cross_deps {
                if graphs.defs.contains(&def) {
                    for &input in inputs {
                        dependents.add_edge(input, (svc, def), ());
                    }
                }
            }
        }
        // Share visited state across roots; cycles and shared upstreams need
        // only one pending notification per affected def.
        let mut dfs = Dfs::empty(&dependents);
        for member in members {
            dfs.move_to(member);
            while let Some((svc, def)) = dfs.next(&dependents) {
                if let Some(service) = self.services.get_mut(&svc) {
                    if service.defs.contains_key(&def) {
                        service.pending_source_updates.insert(def);
                    }
                }
            }
        }
    }

    pub(crate) async fn propagate(&mut self, service_name: Symbol, changed_var: Symbol) {
        // #24: event-driven reactivity over the listener graph. A change to a
        // member notifies its listeners. For each listener we resolve its
        // service id to a local service: Some means a local listener, which we
        // recompute from current values (and cached cross-service deps) and, if
        // it needs notification, push back onto the worklist; None means the
        // listener lives on another node, which we notify over the wire via
        // emit_update.
        let mut worklist: Vec<(Symbol, Symbol)> = vec![(service_name, changed_var)];

        while let Some((svc, member)) = worklist.pop() {
            let listeners: Vec<(ServiceNetId, Symbol)> = self
                .services
                .get(&svc)
                .and_then(|s| s.listeners.get(&member))
                .map(|set| set.iter().cloned().collect())
                .unwrap_or_default();

            for (listener_id, listener_def) in listeners {
                let listener_svc = self
                    .services
                    .iter()
                    .find(|(_, s)| s.id == listener_id)
                    .map(|(name, _)| *name);

                match listener_svc {
                    Some(lsvc) => {
                        worklist.extend(self.recompute_def(lsvc, listener_def).await);
                    }
                    None => {
                        self.emit_update(&listener_id, listener_def, svc, member)
                            .await;
                    }
                }
            }
        }
    }

    /// The pair `def` would read for `input` now, if one is complete: its
    /// own cached delivery of a cross-service member, else the state of a
    /// visible local member. A remote member is never read live here
    fn input_receipt(
        &self,
        (svc, def): (Symbol, Symbol),
        (owner, member): (Symbol, Symbol),
    ) -> Option<ReadEntry> {
        self.ensure_service_visible(owner, None).ok()?;
        let cached = (owner != svc)
            .then(|| {
                self.services
                    .get(&svc)?
                    .dep_cache
                    .get(&def)?
                    .get(&(owner, member))
            })
            .flatten();
        match cached {
            Some((value, clock, sources)) => clock
                .as_ref()
                .map(|clock| (value.clone(), Some(clock.clone()), sources.clone())),
            None if self.remote_services.contains_key(&owner) => None,
            None => {
                let state = self.services.get(&owner)?.vars.get(&member)?;
                Some((
                    state.value.clone(),
                    Some(state.vector_clock.clone()),
                    state.sources.clone(),
                ))
            }
        }
    }

    /// Pairs for every static input of `def`, including inputs of branches an
    /// evaluation does not take, or None while one is missing
    fn static_receipts(
        &self,
        service: &Service,
        def: Symbol,
    ) -> Option<HashMap<(Symbol, Symbol), ReadEntry>> {
        if service.initializing.is_some() {
            return None;
        }
        service
            .graphs
            .reactive_graph
            .neighbors_directed(def, petgraph::Direction::Incoming)
            .map(|member| (service.name, member))
            .chain(
                service
                    .graphs
                    .cross_deps
                    .get(&def)
                    .into_iter()
                    .flatten()
                    .copied(),
            )
            .map(|input| Some((input, self.input_receipt((service.name, def), input)?)))
            .collect()
    }

    /// Gate each input using the Sources paired with that value and clock.
    fn check_input_clocks(
        &self,
        inputs: &[(&VClock, Option<Cow<'_, Sources>>)],
    ) -> (VClock, bool, Option<Sources>) {
        let v_target = join_clocks(inputs.iter().map(|(clock, _)| *clock));
        let gate_ok = inputs
            .iter()
            .all(|(clock, sources)| caught_up(&v_target, clock, sources.as_deref()));
        let sources = gate_ok
            .then(|| {
                inputs
                    .iter()
                    .try_fold(Sources::new(), |mut result, (_, sources)| {
                        result.extend(sources.as_ref()?.iter().copied());
                        Some(result)
                    })
            })
            .flatten();
        (v_target, gate_ok, sources)
    }

    /// `check_input_clocks` over read pairs; one without a clock fails it
    fn check_receipts<'a>(
        &self,
        receipts: impl IntoIterator<Item = &'a ReadEntry>,
    ) -> (VClock, bool, Option<Sources>) {
        let Some(inputs) = receipts
            .into_iter()
            .map(|(_, clock, sources)| Some((clock.as_ref()?, sources.as_ref().map(Cow::Borrowed))))
            .collect::<Option<Vec<_>>>()
        else {
            return (VClock::new(), false, None);
        };
        self.check_input_clocks(&inputs)
    }

    /// Make changes of `input` reach `def`: a listener on a local owner,
    /// otherwise a subscription, whose first Update carries the input's pair
    async fn follow_input(
        &mut self,
        (svc, def): (Symbol, Symbol),
        (owner, member): (Symbol, Symbol),
    ) {
        let Some(listener) = self.services.get(&svc).map(|service| service.id.clone()) else {
            return;
        };
        match self.services.get_mut(&owner) {
            Some(service) => {
                service
                    .listeners
                    .entry(member)
                    .or_default()
                    .insert((listener, def));
            }
            None => self.subscribe_remote(owner, member, listener, def).await,
        }
    }

    async fn recompute_component(
        &mut self,
        root: (Symbol, Symbol),
    ) -> Option<Vec<(Symbol, Symbol)>> {
        let mut graph = DiGraphMap::<(Symbol, Symbol), ()>::new();
        graph.add_node(root);
        let mut pending = vec![root];
        while let Some(node) = pending.pop() {
            let Some(service) = self.services.get(&node.0) else {
                continue;
            };
            if self.remote_services.contains_key(&node.0)
                || service.initializing.is_some()
                || !service.defs.contains_key(&node.1)
            {
                continue;
            }
            let inputs = service
                .graphs
                .reactive_graph
                .neighbors_directed(node.1, petgraph::Direction::Incoming)
                .map(|member| (node.0, member))
                .chain(
                    service
                        .graphs
                        .cross_deps
                        .get(&node.1)
                        .into_iter()
                        .flatten()
                        .copied(),
                );
            for input in inputs {
                if !graph.contains_node(input) {
                    pending.push(input);
                }
                graph.add_edge(input, node, ());
            }
        }
        let component = petgraph::algo::tarjan_scc(&graph)
            .into_iter()
            .find(|component| component.contains(&root))?;
        if component.len() < 2 && !graph.contains_edge(root, root) {
            return None;
        }

        // Each member's code and the pairs of its inputs outside the component
        let mut snapshots = HashMap::new();
        for &node in &component {
            let mut receipts = HashMap::new();
            for input in graph.neighbors_directed(node, petgraph::Direction::Incoming) {
                if !component.contains(&input) {
                    receipts.insert(input, self.input_receipt(node, input)?);
                }
            }
            snapshots.insert(
                node,
                (self.services[&node.0].defs[&node.1].clone(), receipts),
            );
        }
        'observe: loop {
            // Every member of a strongly connected component reaches the same
            // boundary roots; its completed clock and Sources therefore agree.
            let (clock, gate_ok, sources) = self.check_receipts(
                snapshots
                    .values()
                    .flat_map(|(_, receipts)| receipts.values()),
            );
            if !gate_ok {
                return None;
            }
            let mut staged = HashMap::new();
            while staged.len() < component.len() {
                let previous_len = staged.len();
                for &node in &component {
                    if staged.contains_key(&node) {
                        continue;
                    }
                    let (expr, receipts) = &snapshots[&node];
                    let inputs = receipts
                        .iter()
                        .map(|(&input, entry)| (input, Some(entry.0.clone())))
                        .chain(
                            component
                                .iter()
                                .map(|&input| (input, staged.get(&input).cloned())),
                        )
                        .collect();
                    let result = super::interpreter::evaluator::eval_with_inputs(
                        expr,
                        &[],
                        &mut EvalContext {
                            manager: self,
                            service_name: node.0,
                            txn: None,
                        },
                        Some(&inputs),
                    )
                    .await;
                    let input = match result {
                        Ok(value) => {
                            staged.insert(node, value);
                            continue;
                        }
                        // Not staged yet: evaluated again in the next round
                        Err(EvalError::InputNotReady(input)) if component.contains(&input) => {
                            continue
                        }
                        Err(EvalError::InputNotReady(input)) => input,
                        Err(_) => return None,
                    };
                    // A member a call reads outside the component joins its
                    // boundary, and the whole component is observed again
                    let receipt = self.input_receipt(node, input);
                    if receipt.is_none() || self.services.contains_key(&input.0) {
                        self.follow_input(node, input).await;
                    }
                    let Some(receipt) = receipt else {
                        return Some(Vec::new());
                    };
                    if let Some((_, receipts)) = snapshots.get_mut(&node) {
                        receipts.insert(input, receipt);
                    }
                    continue 'observe;
                }
                if staged.len() == previous_len {
                    return None;
                }
            }
            return Some(
                staged
                    .into_iter()
                    .filter_map(|(node, value)| {
                        self.install_recomputed(node, value, clock.clone(), sources.clone())
                            .then_some(node)
                    })
                    .collect(),
            );
        }
    }

    fn install_recomputed(
        &mut self,
        (svc, def): (Symbol, Symbol),
        value: Value,
        clock: VClock,
        sources: Option<Sources>,
    ) -> bool {
        let Some(service) = self.services.get_mut(&svc) else {
            log::warn!(
                "recompute_def: service '{}' disappeared after recompute",
                self.interner.get(svc)
            );
            return false;
        };
        let pending = service.pending_source_updates.remove(&def);
        if let Some(state) = service.vars.get_mut(&def) {
            let differs =
                state.value != value || state.vector_clock != clock || state.sources != sources;
            state.value = value;
            state.vector_clock = clock;
            state.sources = sources;
            differs || pending
        } else {
            let mut state = VarState::new(value);
            state.vector_clock = clock;
            state.sources = sources;
            service.vars.insert(def, state);
            true
        }
    }

    /// Evaluate `expr`, the code of `def`, against one fixed set of paired
    /// inputs: `receipts`, which hold its static inputs, plus the pair of each
    /// member a call or branch reads, added before evaluating again from the
    /// start. Evaluation reads nothing else, so it cannot suspend and its
    /// value agrees with the clock and Sources it is gated with. Local inputs
    /// reached this way are followed so their changes reach `def`
    ///
    /// Returns:
    ///     The value with that clock and Sources, None while the pairs
    ///     disagree, `EvalError::InputNotReady` for an input without a pair
    async fn evaluate_checked(
        &mut self,
        (svc, def): (Symbol, Symbol),
        expr: &Expr,
        mut receipts: HashMap<(Symbol, Symbol), ReadEntry>,
    ) -> Result<Option<(Value, VClock, Option<Sources>)>, EvalError> {
        loop {
            let (clock, gate_ok, sources) = self.check_receipts(receipts.values());
            if !gate_ok {
                return Ok(None);
            }
            let inputs = receipts
                .iter()
                .map(|(&input, entry)| (input, Some(entry.0.clone())))
                .collect();
            let result = super::interpreter::evaluator::eval_with_inputs(
                expr,
                &[],
                &mut EvalContext {
                    manager: self,
                    service_name: svc,
                    txn: None,
                },
                Some(&inputs),
            )
            .await;
            let input = match result {
                Err(EvalError::InputNotReady(input)) => input,
                result => return result.map(|value| Some((value, clock, sources))),
            };
            let receipt = self.input_receipt((svc, def), input);
            if self.services.contains_key(&input.0) {
                self.follow_input((svc, def), input).await;
            }
            receipts.insert(input, receipt.ok_or(EvalError::InputNotReady(input))?);
        }
    }

    /// Ordinary propagation uses completed input snapshots. Recursive Sources
    /// changes, and changes a recursive member's own gate cannot accept before
    /// the rest of its cycle catches up, are installed together only after the
    /// component can be evaluated and gated against one private set of input
    /// snapshots.
    pub(crate) async fn recompute_def(
        &mut self,
        svc: Symbol,
        def: Symbol,
    ) -> Vec<(Symbol, Symbol)> {
        let receipts = self
            .services
            .get(&svc)
            .and_then(|service| self.static_receipts(service, def));
        let ready = self.services.get(&svc).is_some_and(|service| {
            service
                .vars
                .get(&def)
                .is_some_and(|state| state.sources.is_some())
                && !service.pending_source_updates.contains(&def)
        }) && receipts
            .as_ref()
            .is_some_and(|receipts| self.check_receipts(receipts.values()).1);
        if !ready {
            if let Some(changed) = self.recompute_component((svc, def)).await {
                return changed;
            }
        }
        let Some(expr) = self
            .services
            .get(&svc)
            .and_then(|service| service.defs.get(&def))
            .cloned()
        else {
            log::warn!(
                "recompute_def: def '{}' not found in service '{}'",
                self.interner.get(def),
                self.interner.get(svc)
            );
            return Vec::new();
        };
        let Some(receipts) = receipts else {
            return Vec::new();
        };
        match self.evaluate_checked((svc, def), &expr, receipts).await {
            Ok(Some((value, clock, sources))) => self
                .install_recomputed((svc, def), value, clock, sources)
                .then_some((svc, def))
                .into_iter()
                .collect(),
            Ok(None) => Vec::new(),
            Err(EvalError::InputNotReady(input)) => {
                // A local input was followed already
                if !self.services.contains_key(&input.0) {
                    self.follow_input((svc, def), input).await;
                }
                Vec::new()
            }
            Err(e) => {
                log::warn!(
                    "propagation of def '{}' failed: {}",
                    self.interner.get(def),
                    e
                );
                Vec::new()
            }
        }
    }

    /// The first of `defs`, introduced by a committed code update, that has no
    /// value yet. Each is evaluated against its paired inputs as propagation
    /// would, so one whose inputs are all paired installs its first value or
    /// reports why its evaluation fails; one that lacks an input keeps
    /// waiting for propagation to deliver it
    pub async fn first_values(
        &mut self,
        defs: &[(Symbol, Symbol)],
    ) -> Result<Option<(Symbol, Symbol)>, EvalError> {
        for &(svc, def) in defs {
            let Some(service) = self.services.get(&svc) else {
                return Ok(Some((svc, def)));
            };
            if service.vars.contains_key(&def) {
                continue;
            }
            let (Some(expr), Some(receipts)) = (
                service.defs.get(&def).cloned(),
                self.static_receipts(service, def),
            ) else {
                return Ok(Some((svc, def)));
            };
            match self.evaluate_checked((svc, def), &expr, receipts).await {
                Ok(Some((value, clock, sources))) => {
                    self.install_recomputed((svc, def), value, clock, sources);
                    self.propagate(svc, def).await;
                }
                Ok(None) | Err(EvalError::InputNotReady(_)) => return Ok(Some((svc, def))),
                Err(e) => {
                    return Err(EvalError::RuntimeError(format!(
                        "Update committed, but '{}.{}' cannot be initialized: {}",
                        self.interner.get(svc),
                        self.interner.get(def),
                        e
                    )))
                }
            }
        }
        Ok(None)
    }

    /// Apply an incoming code update without waiting inside the request handler.
    /// Its reply is sent by complete_code_updates after its new defs have values.
    pub async fn apply_service_update(
        &mut self,
        request_id: u64,
        txn_id: Option<TxnId>,
        source: &str,
        reply_to: String,
    ) {
        let result = match crate::runtime::parser::parse_string(source, &mut self.interner) {
            Ok(stmts) => {
                let mut txn = match txn_id {
                    Some(tid) => crate::runtime::update::Transaction::new_with_id(tid, stmts),
                    None => crate::runtime::update::Transaction::new(stmts),
                };
                Box::pin(txn.apply(self)).await.map_err(|e| e.to_string())
            }
            Err(e) => Err(e),
        };
        match result {
            Ok(introduced) => self.completing_updates.push((
                request_id,
                reply_to,
                introduced,
                web_time::Instant::now() + std::time::Duration::from_secs(15),
            )),
            Err(error) => {
                if !reply_to.is_empty() {
                    self.send_oneway(
                        Address::new(&reply_to),
                        MeerkatMessage::UpdateServiceResponse {
                            request_id,
                            error: Some(error),
                        },
                    )
                    .await;
                }
            }
        }
    }

    /// Advance applied updates using paired inputs already available locally.
    /// This does not wait for a first value or enter another network dispatcher.
    pub async fn complete_code_updates(&mut self) {
        for (request_id, reply_to, defs, deadline) in std::mem::take(&mut self.completing_updates) {
            let error = match self.first_values(&defs).await {
                Ok(None) => None,
                Ok(Some(_)) if web_time::Instant::now() < deadline => {
                    self.completing_updates
                        .push((request_id, reply_to, defs, deadline));
                    continue;
                }
                Ok(Some(waiting)) => Some(self.uninitialized(waiting).to_string()),
                Err(e) => Some(e.to_string()),
            };
            if !reply_to.is_empty() {
                self.send_oneway(
                    Address::new(&reply_to),
                    MeerkatMessage::UpdateServiceResponse { request_id, error },
                )
                .await;
            }
        }
    }

    /// Reassure requests waiting on locks or the first values of a code update.
    pub async fn keepalive_waiters(&mut self) {
        let mut targets = self.parked_keepalive_targets();
        targets.extend(
            self.completing_updates
                .iter()
                .map(|(request_id, reply_to, _, _)| (*request_id, reply_to.clone())),
        );
        for (request_id, reply_to) in targets {
            self.send_oneway(
                Address::new(&reply_to),
                MeerkatMessage::WaitParked { request_id },
            )
            .await;
        }
    }

    /// Drive a top-level code update until its new defs have first values.
    /// Only pauses count against this nominal reply-timeout budget.
    pub async fn await_first_values(&mut self, defs: &[(Symbol, Symbol)]) -> Result<(), EvalError> {
        let mut remaining_ms: i64 = 15_000;
        loop {
            self.serve_network_events().await;
            let Some(waiting) = self.first_values(defs).await? else {
                return Ok(());
            };
            if remaining_ms <= 0 {
                return Err(self.uninitialized(waiting));
            }
            if remaining_ms % 5_000 == 0 {
                self.keepalive_waiters().await;
            }
            pause(10).await;
            remaining_ms -= 10;
        }
    }

    /// The failure of a committed code update whose new `def` has no value yet
    pub fn uninitialized(&self, (svc, def): (Symbol, Symbol)) -> EvalError {
        EvalError::RuntimeError(format!(
            "Update committed, but '{}.{}' has not received consistent inputs yet",
            self.interner.get(svc),
            self.interner.get(def)
        ))
    }

    /// #24: fire-and-forget send (no reply awaited).
    async fn send_oneway(&mut self, addr: Address, msg: MeerkatMessage) {
        if let Some(net) = self.network.as_mut() {
            let reply = net
                .handle_command(NetworkCommand::SendMessage {
                    addr: addr.clone(),
                    msg,
                })
                .await;
            if let NetworkReply::Failure(e) = reply {
                log::warn!("send_oneway to {} failed: {}", addr.0, e);
            }
        }
    }

    /// #24: send the current value of `svc.member` to a remote listener.
    async fn emit_update(
        &mut self,
        listener_id: &ServiceNetId,
        listener_def: Symbol,
        svc: Symbol,
        member: Symbol,
    ) {
        if self.ensure_service_visible(svc, None).is_err() {
            return;
        }
        let reply_to = match self.listener_addrs.get(listener_id) {
            Some(a) => a.clone(),
            None => {
                log::warn!(
                    "emit_update: no reply address for listener '{}'",
                    listener_id.0
                );
                return;
            }
        };
        let state = match self.services.get(&svc).and_then(|s| s.vars.get(&member)) {
            Some(state) => state,
            None => {
                log::warn!(
                    "emit_update: member '{}' not found in service '{}'",
                    self.interner.get(member),
                    self.interner.get(svc)
                );
                return;
            }
        };
        let net_val = match codec::encode_value(&state.value, &self.interner) {
            Ok(nv) => nv,
            Err(e) => {
                log::warn!(
                    "emit_update: failed to encode value for '{}.{}': {}",
                    self.interner.get(svc),
                    self.interner.get(member),
                    e
                );
                return;
            }
        };
        let msg = MeerkatMessage::Update {
            listener_service: listener_id.0.clone(),
            listener_def: self.interner.get(listener_def).to_string(),
            source_service: self.interner.get(svc).to_string(),
            member: self.interner.get(member).to_string(),
            value: net_val,
            clock: codec::encode_clock(&state.vector_clock, &self.interner),
            sources: state
                .sources
                .as_ref()
                .map(|sources| codec::encode_sources(sources, &self.interner)),
        };
        self.send_oneway(Address::new(&reply_to), msg).await;
    }

    /// Subscribe `this_id.listener_def` as a listener on remote `owner.member`
    ///
    /// Args:
    ///     `owner` (`Symbol`): The remote service owner symbol
    ///     `member` (`Symbol`): The member symbol on the remote service
    ///     `this_id` (`ServiceNetId`): The subscriber service identity
    ///     `listener_def` (`Symbol`): The subscriber local definition symbol
    ///
    /// Returns:
    ///     `()`
    pub async fn subscribe_remote(
        &mut self,
        owner: Symbol,
        member: Symbol,
        this_id: ServiceNetId,
        listener_def: Symbol,
    ) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_SUB_ID: AtomicU64 = AtomicU64::new(1);
        let addr = match self.remote_addr(owner) {
            Ok(a) => a,
            Err(_) => return,
        };
        let reply_to = self.local_reply_addr().await;
        let request_id = NEXT_SUB_ID.fetch_add(1, Ordering::SeqCst);
        let msg = MeerkatMessage::RequestUpdates {
            request_id,
            service: self.interner.get(owner).to_string(),
            member: self.interner.get(member).to_string(),
            listener_service: this_id.0,
            listener_def: self.interner.get(listener_def).to_string(),
            reply_to,
        };
        self.send_oneway(addr, msg).await;
    }

    /// #24 owner side: register a remote listener on `service.member` and reply
    /// with the current value as an initial `Update` so it starts in sync.
    pub async fn handle_request_updates(
        &mut self,
        service_sym: Symbol,
        member_sym: Symbol,
        listener_id: ServiceNetId,
        listener_def_sym: Symbol,
        reply_to: String,
    ) {
        // Only register a subscription for a member that actually exists on this
        // service. Without this guard, an unknown member from untrusted network
        // input would permanently grow listeners, listener_addrs, and the
        // interner with no-op subscriptions.
        let member_exists = self
            .services
            .get(&service_sym)
            .map(|s| {
                s.vars.contains_key(&member_sym)
                    || (s.initializing.is_some()
                        && (s.graphs.vars.contains(&member_sym)
                            || s.graphs.defs.contains(&member_sym)))
            })
            .unwrap_or(false);
        if !member_exists {
            return;
        }
        if let Some(svc) = self.services.get_mut(&service_sym) {
            svc.listeners
                .entry(member_sym)
                .or_default()
                .insert((listener_id.clone(), listener_def_sym));
        } else {
            return;
        }
        self.listener_addrs
            .insert(listener_id.clone(), reply_to.clone());

        self.emit_update(&listener_id, listener_def_sym, service_sym, member_sym)
            .await;
    }

    /// #24 listener side: a remote member changed (or its initial value). Cache
    /// it, recompute the dependent def from cache, and cascade to its listeners.
    pub async fn handle_update(
        &mut self,
        listener_id: ServiceNetId,
        listener_def_sym: Symbol,
        source: (Symbol, Symbol),
        value: crate::net::ast::NetValue,
        clock: VClock,
        sources: Option<Sources>,
    ) {
        let value = match codec::decode_value(value, &mut self.interner) {
            Ok(v) => v,
            Err(_) => return,
        };

        let listener_svc = self
            .services
            .iter()
            .find(|(_, s)| s.id == listener_id && s.defs.contains_key(&listener_def_sym))
            .map(|(name, _)| *name);
        let listener_svc = match listener_svc {
            Some(n) => n,
            None => return,
        };

        let sources_changed = self
            .services
            .get(&listener_svc)
            .and_then(|service| service.dep_cache.get(&listener_def_sym))
            .and_then(|cache| cache.get(&source))
            .map(|entry| &entry.2)
            != Some(&sources);
        if let Some(svc) = self.services.get_mut(&listener_svc) {
            svc.dep_cache
                .entry(listener_def_sym)
                .or_default()
                .insert(source, (value, Some(clock), sources));
        }

        if sources_changed {
            self.mark_source_updates(vec![(listener_svc, listener_def_sym)]);
        }

        for (svc, member) in self.recompute_def(listener_svc, listener_def_sym).await {
            self.propagate(svc, member).await;
        }
    }

    /// Next event at a request boundary, including requests deferred by an RPC.
    /// Never use this while a request is still executing.
    pub fn try_recv_event(&mut self) -> Option<NetworkEvent> {
        self.deferred_events
            .pop_front()
            .or_else(|| self.network.as_mut().and_then(|n| n.try_recv_event()))
    }

    /// Dispatch at a request boundary, where participant transactions are stored
    /// and their requests have already produced their replies.
    pub async fn serve_network_events(&mut self) {
        self.dispatch_events(true).await;
        self.wake_freed().await;
    }

    /// Pump replies and reactive traffic during an RPC. Requests that need the
    /// suspended caller's state remain queued for the next request boundary.
    pub async fn dispatch_network_events(&mut self) {
        self.dispatch_events(false).await
    }

    async fn dispatch_events(&mut self, serve: bool) {
        while let Some(event) = if serve {
            self.try_recv_event()
        } else {
            self.network.as_mut().and_then(|n| n.try_recv_event())
        } {
            match event {
                NetworkEvent::MessageReceived { msg, peer } => match msg {
                    // #24: reactive messages are not replies; handle them inline
                    // here in async context rather than buffering them.
                    MeerkatMessage::RequestUpdates {
                        service,
                        member,
                        listener_service,
                        listener_def,
                        reply_to,
                        ..
                    } => {
                        // #24: validate + intern wire names through codec (the
                        // sole interning authority for network data); skip the
                        // message if any identifier fails validation.
                        let (service_sym, member_sym, listener_def_sym) =
                            match codec::decode_request_updates(
                                &service,
                                &member,
                                &listener_def,
                                &mut self.interner,
                            ) {
                                Ok(syms) => syms,
                                Err(_) => continue,
                            };
                        self.handle_request_updates(
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
                        clock,
                        sources,
                    } => {
                        // #24: validate + intern wire names through codec; skip
                        // the message if any identifier fails validation.
                        let (listener_def_sym, source_sym, member_sym, vclock, sources) =
                            match codec::decode_update(
                                &listener_def,
                                &source_service,
                                &member,
                                &mut self.interner,
                                clock,
                                sources,
                            ) {
                                Ok(syms) => syms,
                                Err(_) => continue,
                            };
                        self.handle_update(
                            ServiceNetId(listener_service),
                            listener_def_sym,
                            (source_sym, member_sym),
                            value,
                            vclock,
                            sources,
                        )
                        .await;
                    }
                    MeerkatMessage::UpdateServiceRequest {
                        request_id,
                        txn_id,
                        service_name,
                        source,
                        reply_to,
                    } => {
                        let target_addr = if !peer.is_empty() { peer } else { reply_to };
                        match codec::decode_update_service_request(
                            &service_name,
                            &source,
                            &mut self.interner,
                        ) {
                            Ok((_sym, source)) => {
                                self.apply_service_update(request_id, txn_id, &source, target_addr)
                                    .await;
                            }
                            Err(e) => {
                                if !target_addr.is_empty() {
                                    self.send_oneway(
                                        Address::new(&target_addr),
                                        MeerkatMessage::UpdateServiceResponse {
                                            request_id,
                                            error: Some(e.to_string()),
                                        },
                                    )
                                    .await;
                                }
                            }
                        }
                    }
                    msg @ MeerkatMessage::ServiceCodeRequest { .. } => {
                        Box::pin(self.serve_request(msg)).await;
                    }
                    msg @ (MeerkatMessage::LookupRequest { .. }
                    | MeerkatMessage::ActionRequest { .. }
                    | MeerkatMessage::Commit { .. }
                    | MeerkatMessage::Abort { .. }) => {
                        if serve {
                            Box::pin(self.serve_request(msg)).await;
                        } else {
                            self.deferred_events
                                .push_back(NetworkEvent::MessageReceived { msg, peer });
                        }
                    }
                    msg @ MeerkatMessage::LockRequest { .. } if serve => {
                        Box::pin(self.serve_request(msg)).await;
                    }
                    MeerkatMessage::LockRequest {
                        request_id,
                        txn_id,
                        services,
                        reply_to,
                    } => {
                        let res =
                            Box::pin(self.handle_lock_request(txn_id.clone(), services)).await;
                        let (success, error) = match res {
                            Ok(()) => (true, None),
                            Err(e) => (false, Some(e.to_string())),
                        };
                        let reply_msg = MeerkatMessage::LockResponse {
                            request_id,
                            txn_id: txn_id.clone(),
                            success,
                            error,
                        };
                        let target_addr = if !peer.is_empty() { peer } else { reply_to };
                        if !target_addr.is_empty() {
                            self.send_oneway(Address::new(&target_addr), reply_msg)
                                .await;
                        }
                    }
                    // Everything else is a reply: route it to its waiter.
                    other => {
                        let rid = match &other {
                            MeerkatMessage::LookupResponse { request_id, .. } => Some(*request_id),
                            MeerkatMessage::LookupError { request_id, .. } => Some(*request_id),
                            MeerkatMessage::ActionResponse { request_id, .. } => Some(*request_id),
                            MeerkatMessage::CommitResponse { request_id, .. } => Some(*request_id),
                            MeerkatMessage::AbortResponse { request_id, .. } => Some(*request_id),
                            MeerkatMessage::LockResponse { request_id, .. } => Some(*request_id),
                            MeerkatMessage::WaitParked { request_id, .. } => Some(*request_id),
                            // #39: code responses are replies routed to the waiting client.
                            MeerkatMessage::ServiceCodeResponse { request_id, .. } => {
                                Some(*request_id)
                            }
                            MeerkatMessage::ServiceCodeError { request_id, .. } => {
                                Some(*request_id)
                            }
                            MeerkatMessage::UpdateServiceResponse { request_id, .. } => {
                                Some(*request_id)
                            }
                            MeerkatMessage::Ping { .. }
                            | MeerkatMessage::Pong { .. }
                            | MeerkatMessage::Announce { .. }
                            | MeerkatMessage::Transaction { .. }
                            | MeerkatMessage::Propagation { .. }
                            | MeerkatMessage::LookupRequest { .. }
                            | MeerkatMessage::ActionRequest { .. }
                            | MeerkatMessage::Commit { .. }
                            | MeerkatMessage::Abort { .. }
                            | MeerkatMessage::LockRequest { .. }
                            | MeerkatMessage::RequestUpdates { .. }
                            // #39: an incoming code request is handled server-side, not a reply.
                            | MeerkatMessage::ServiceCodeRequest { .. }
                            | MeerkatMessage::UpdateServiceRequest { .. }
                            | MeerkatMessage::Update { .. } => None,
                        };
                        if let Some(request_id) = rid {
                            if let Some(tx) = self.pending_replies.remove(&request_id) {
                                let _ = tx.send(other);
                            }
                        }
                    }
                },
                NetworkEvent::SendFailed { .. } => {}
                NetworkEvent::PeerConnected { .. } => {}
                NetworkEvent::PeerDisconnected { .. } => {}
            }
        }
        self.complete_code_updates().await;
    }

    /// Rebuild a structured error from a failure a remote node reported.
    ///
    /// Errors cross the wire as their `Display` text, so a wait-die abort comes
    /// back as an ordinary string. Flattening it to `LocalDispatchFailed` turns
    /// ordinary lock contention into a terminal failure, because
    /// `execute_action_with_txn` retries only wait-die outcomes. Every
    /// reply that can carry a lock outcome goes through here so lock requests,
    /// reads and composed actions all behave the same way.
    ///
    /// Matched on the prefix `EvalError`'s `Display` writes, not on the phrase
    /// appearing anywhere. Error text quotes user input -- an assertion carries
    /// its own source text, for one -- so a message that merely mentions
    /// wait-die would otherwise be retried through the whole budget and then
    /// reported as a lock conflict that never happened, hiding the real
    /// failure.
    ///
    /// The prefix is stripped rather than kept, because `WaitDieAbort` writes
    /// it back when it displays itself. Keeping it would nest one copy per node
    /// the failure passed through -- a retry exhausted on the originator, or a
    /// node in the middle forwarding it on -- and the message a `@test` reports
    /// would read `Wait-die abort: Wait-die abort: ...`.
    ///
    /// Args:
    ///     `err` (`String`): The `Display` text a remote node reported
    ///
    /// Returns:
    ///     `EvalError`: `WaitDieAbort` for a lock conflict, otherwise
    ///     `LocalDispatchFailed`
    fn remote_error(err: String) -> EvalError {
        match err.strip_prefix(WAIT_DIE_DISPLAY_PREFIX) {
            Some(reason) => EvalError::WaitDieAbort(reason.to_string()),
            None => EvalError::LocalDispatchFailed(err),
        }
    }

    /// Identify a composed action by what its dispatch ships, so a replay can
    /// tell whether it reached the same action as the recorded one
    fn fingerprint(stmts: &[NetActionStmt], env: &[(String, NetValue)]) -> Result<u64, EvalError> {
        use std::hash::{DefaultHasher, Hash, Hasher};
        let bytes = serde_json::to_vec(&(stmts, env))
            .map_err(|e| EvalError::LocalDispatchFailed(e.to_string()))?;
        let mut hasher = DefaultHasher::new();
        bytes.hash(&mut hasher);
        Ok(hasher.finish())
    }

    /// shared by remote_lookup and remote_action.
    pub async fn send_and_await_reply(
        &mut self,
        addr: Address,
        msg: MeerkatMessage,
        request_id: u64,
        timeout_msg: String,
    ) -> Result<MeerkatMessage, EvalError> {
        // Send the message
        let net = self.network.as_mut().ok_or_else(|| {
            EvalError::LocalDispatchFailed("No network layer available".to_string())
        })?;
        let reply = net
            .handle_command(NetworkCommand::SendMessage {
                addr: addr.clone(),
                msg,
            })
            .await;
        if let NetworkReply::Failure(e) = reply {
            log::warn!("Failed to dispatch request to {}: {}", addr.0, e);
            return Err(EvalError::LocalDispatchFailed(e));
        }

        // Register oneshot channel for this request
        let (tx, mut rx) = oneshot::channel::<MeerkatMessage>();
        self.pending_replies.insert(request_id, tx);
        let mut last_keepalive = web_time::Instant::now();

        // Loop dispatching pending network events then checking for reply,
        // timeout, or a short yield. The loop is required until the background
        // message-loop architecture is implemented as a follow-up.
        //
        // #39: the timer is platform-split. Native uses tokio's timer; wasm has
        // no tokio timer driver in the browser, so it uses gloo-timers, the same
        // way spawn_event_loop is split between tokio and wasm_bindgen_futures.
        #[cfg(not(target_arch = "wasm32"))]
        {
            let timeout = tokio::time::sleep(Duration::from_secs(15));
            tokio::pin!(timeout);

            loop {
                self.dispatch_network_events().await;
                if last_keepalive.elapsed() >= std::time::Duration::from_secs(5) {
                    self.keepalive_waiters().await;
                    last_keepalive = web_time::Instant::now();
                }
                tokio::select! {
                    biased;
                    result = &mut rx => {
                        match result {
                            // Owner parked our request (wait-die wait): it is
                            // alive and still queued, so reset the timeout,
                            // re-register a fresh reply channel, and keep waiting.
                            Ok(MeerkatMessage::WaitParked { .. }) => {
                                let (ntx, nrx) = oneshot::channel::<MeerkatMessage>();
                                self.pending_replies.insert(request_id, ntx);
                                rx = nrx;
                                timeout
                                    .as_mut()
                                    .reset(tokio::time::Instant::now() + Duration::from_secs(15));
                            }
                            Ok(msg) => return Ok(msg),
                            Err(_) => {
                                return Err(EvalError::LocalDispatchFailed(
                                    "Reply channel closed".to_string(),
                                ))
                            }
                        }
                    }
                    _ = &mut timeout => {
                        self.pending_replies.remove(&request_id);
                        return Err(EvalError::LocalDispatchFailed(timeout_msg));
                    }
                    _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                }
            }
        }

        #[cfg(target_arch = "wasm32")]
        {
            use futures::FutureExt;

            // Deadline tracked with performance.now() via instant/web-time-free
            // arithmetic: recreate the timeout each iteration rather than reset.
            let mut remaining_ms: i64 = 15_000;
            loop {
                self.dispatch_network_events().await;
                if last_keepalive.elapsed() >= std::time::Duration::from_secs(5) {
                    self.keepalive_waiters().await;
                    last_keepalive = web_time::Instant::now();
                }

                // #39: SendWrapper makes the (browser-thread-only) timer
                // futures Send so callers on the Send-bounded eval path still
                // compile. Safe: wasm is single-threaded, so the future is only
                // ever polled on its origin thread.
                let mut timeout = send_wrapper::SendWrapper::new(
                    gloo_timers::future::TimeoutFuture::new(remaining_ms.max(0) as u32),
                )
                .fuse();
                let mut yield_tick =
                    send_wrapper::SendWrapper::new(gloo_timers::future::TimeoutFuture::new(10))
                        .fuse();

                futures::select! {
                    result = (&mut rx).fuse() => {
                        match result {
                            Ok(MeerkatMessage::WaitParked { .. }) => {
                                let (ntx, nrx) = oneshot::channel::<MeerkatMessage>();
                                self.pending_replies.insert(request_id, ntx);
                                rx = nrx;
                                remaining_ms = 15_000;
                            }
                            Ok(msg) => return Ok(msg),
                            Err(_) => {
                                return Err(EvalError::LocalDispatchFailed(
                                    "Reply channel closed".to_string(),
                                ))
                            }
                        }
                    }
                    _ = timeout => {
                        self.pending_replies.remove(&request_id);
                        return Err(EvalError::LocalDispatchFailed(timeout_msg));
                    }
                    _ = yield_tick => {
                        remaining_ms -= 10;
                    }
                }
            }
        }
    }

    /// Retrieve the network address associated with a remote service symbol
    ///
    /// Strips the trailing service slug from the registered service URL.
    ///
    /// Args:
    ///     service (Symbol): The remote service symbol to look up
    ///
    /// Returns:
    ///     Result<Address, EvalError>: The target remote network address
    ///
    /// Raises:
    ///     EvalError::ServiceNotFound: If the remote service is not registered
    pub fn remote_addr(&self, service: Symbol) -> Result<Address, EvalError> {
        let full_url = self.remote_services.get(&service).ok_or_else(|| {
            EvalError::ServiceNotFound(format!(
                "Remote service '{}' not found",
                self.interner.get(service)
            ))
        })?;
        Ok(full_url.node_address(self.interner.get(service)))
    }

    /// Get our local address with peer ID for use as reply_to
    /// Replaces loopback/unspecified with the actual outbound IP
    pub async fn local_reply_addr(&mut self) -> String {
        if let Some(addr) = &self.local_address {
            return addr.clone();
        }
        let net = match self.network.as_mut() {
            Some(n) => n,
            None => return String::new(),
        };
        let peer_id = net.local_peer_id();
        let reply = net.handle_command(NetworkCommand::GetLocalAddresses).await;
        let node_ip = self.get_node_ip();
        match reply {
            NetworkReply::LocalAddresses { addrs } => {
                if let Some(addr) = addrs.first() {
                    let addr_str = if self.local {
                        addr.0.clone()
                    } else {
                        addr.0
                            .replace("0.0.0.0", &node_ip)
                            .replace("127.0.0.1", &node_ip)
                    };
                    format!("{}/p2p/{}", addr_str, peer_id)
                } else {
                    String::new()
                }
            }
            NetworkReply::MessageSent { .. }
            | NetworkReply::ListenSuccess { .. }
            | NetworkReply::Failure(_) => String::new(),
        }
    }

    fn random_node_id() -> u64 {
        use std::collections::hash_map::RandomState;
        use std::hash::{BuildHasher, Hasher};
        // #39: web_time provides a wasm-compatible clock; std::time::SystemTime
        // panics ("time not implemented") on wasm32.
        use web_time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut h = RandomState::new().build_hasher();
        h.write_u128(nanos);
        h.finish()
    }

    /// Get the local machine's outbound IP address (non-loopback) or loopback fallback
    pub fn get_node_ip(&self) -> String {
        if self.local {
            return "127.0.0.1".to_string();
        }
        use std::net::UdpSocket;
        UdpSocket::bind("0.0.0.0:0")
            .and_then(|s| {
                s.connect("8.8.8.8:80")?;
                s.local_addr()
            })
            .map(|addr| addr.ip().to_string())
            .unwrap_or_else(|_| "127.0.0.1".to_string())
    }

    /// #39: Fetch the source of a `.mkt` file from a remote server by path.
    ///
    /// Sends a `ServiceCodeRequest` and awaits the reply, reusing the same
    /// request/reply machinery as remote lookups. Returns the source text of
    /// the requested `.mkt` file. The caller processes it through the
    /// normal program-loading path (creating services and resolving imports),
    /// rather than a separate loop here, to avoid duplicating that logic. This
    /// is the mechanism a browser client uses to load a file it imports but
    /// cannot read from a local disk.
    pub async fn fetch_service_source(
        &mut self,
        path: &str,
        server_addr: Address,
    ) -> Result<String, EvalError> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);

        let request_id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        let reply_to = self.local_reply_addr().await;

        let msg = MeerkatMessage::ServiceCodeRequest {
            request_id,
            path: path.to_string(),
            reply_to,
        };

        let reply = self
            .send_and_await_reply(
                server_addr,
                msg,
                request_id,
                format!("Timeout waiting for source of file '{}'", path),
            )
            .await?;

        match reply {
            MeerkatMessage::ServiceCodeResponse { source, .. } => Ok(source),
            MeerkatMessage::ServiceCodeError { error, .. } => {
                Err(EvalError::RemoteDispatchFailed(error))
            }
            _ => Err(EvalError::RemoteDispatchFailed(
                "Unexpected reply to service code request".to_string(),
            )),
        }
    }

    /// Perform a remote variable lookup over the network
    ///
    /// Sends a lookup query to the node owning the remote service and registers
    /// the local node as a transaction participant.
    ///
    /// Args:
    ///     service (Symbol): The remote service symbol
    ///     member (Symbol): The member/variable symbol within the service
    ///     txn (Option<&mut Transaction>): The active transaction context
    ///
    /// Returns:
    ///     Result<Value, EvalError>: The retrieved value, or a network/timeout error
    ///
    /// Raises:
    ///     EvalError::LocalDispatchFailed: If a timeout or dispatch error occurs
    pub async fn remote_lookup(
        &mut self,
        service: Symbol,
        member: Symbol,
        txn: Option<&mut Transaction>,
    ) -> Result<Value, EvalError> {
        self.remote_lookup_with_clock(service, member, txn)
            .await
            .map(|(value, _, _)| value)
    }

    async fn remote_lookup_with_clock(
        &mut self,
        service: Symbol,
        member: Symbol,
        mut txn: Option<&mut Transaction>,
    ) -> Result<ReadEntry, EvalError> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);

        // Remote reads are always served by the owning node, which holds this
        // transaction's buffered writes and read locks. We deliberately do not
        // reuse a cached result on the requesting side: a def's value can change
        // later in the same transaction when a composed action writes one of
        // its dependencies on the owner, so a cached copy would go stale and
        // the def would "stop updating". Re-fetching keeps reads consistent
        // with the owner's buffered state. (Caching provably-immutable reads to
        // save round-trips could be a later optimization.)
        let addr = self.remote_addr(service)?;
        let request_id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        let reply_to = self.local_reply_addr().await;
        let shared_tid = txn.as_ref().map(|t| t.id.clone());

        // Inside a transaction, the owning node will acquire and hold a read lock
        // under the shared id. Pre-register it as a participant so commit/abort
        // releases that lock even if the reply is lost.
        if shared_tid.is_some() {
            if let Some(t) = txn.as_deref_mut() {
                t.participants.insert(addr.clone());
            }
        }

        let msg = MeerkatMessage::LookupRequest {
            request_id,
            service: self.interner.get(service).to_string(),
            member: self.interner.get(member).to_string(),
            reply_to,
            txn_id: shared_tid,
        };

        let reply = self
            .send_and_await_reply(
                addr,
                msg,
                request_id,
                format!(
                    "Timeout waiting for remote lookup of '{}.{}'",
                    self.interner.get(service),
                    self.interner.get(member)
                ),
            )
            .await?;

        match reply {
            MeerkatMessage::LookupResponse {
                value,
                clock,
                sources,
                ..
            } => {
                let value = codec::decode_value(value, &mut self.interner)
                    .map_err(|e| EvalError::LocalDispatchFailed(e.to_string()))?;
                let clock = clock
                    .map(|clock| codec::clock_entry_to_vclock(clock, &mut self.interner))
                    .transpose()
                    .map_err(|e| EvalError::LocalDispatchFailed(e.to_string()))?;
                let sources = sources
                    .map(|sources| codec::decode_sources(sources, &mut self.interner))
                    .transpose()
                    .map_err(|e| EvalError::LocalDispatchFailed(e.to_string()))?;
                let entry = (value, clock, sources);
                if let Some(txn) = txn {
                    // A node that only serves reads never answers an action,
                    // so what it read reaches the commit stamp only through
                    // here. Only the clock is kept: the value is fetched again
                    // on every read
                    if let Some(clock) = &entry.1 {
                        txn.participant_history = join_clocks([&txn.participant_history, clock]);
                    }
                    if txn.observed_reads.is_some() {
                        let key = (self.service_net_id_for_name(service), member);
                        txn.read_cache.insert(key, entry.clone());
                    }
                }
                Ok(entry)
            }
            MeerkatMessage::LookupError { error, .. } => Err(Self::remote_error(error)),
            MeerkatMessage::Ping { .. }
            | MeerkatMessage::Pong { .. }
            | MeerkatMessage::Announce { .. }
            | MeerkatMessage::Transaction { .. }
            | MeerkatMessage::Propagation { .. }
            | MeerkatMessage::LookupRequest { .. }
            | MeerkatMessage::ActionRequest { .. }
            | MeerkatMessage::ActionResponse { .. }
            | MeerkatMessage::Commit { .. }
            | MeerkatMessage::CommitResponse { .. }
            | MeerkatMessage::Abort { .. }
            | MeerkatMessage::AbortResponse { .. }
            | MeerkatMessage::LockRequest { .. }
            | MeerkatMessage::LockResponse { .. }
            | MeerkatMessage::RequestUpdates { .. }
            | MeerkatMessage::Update { .. }
            | MeerkatMessage::ServiceCodeRequest { .. }
            | MeerkatMessage::ServiceCodeResponse { .. }
            | MeerkatMessage::ServiceCodeError { .. }
            | MeerkatMessage::UpdateServiceRequest { .. }
            | MeerkatMessage::UpdateServiceResponse { .. }
            | MeerkatMessage::WaitParked { .. } => Err(EvalError::LocalDispatchFailed(
                "Unexpected reply to lookup request".to_string(),
            )),
        }
    }

    /// Participant side: serve a transactional remote read by acquiring and
    /// holding a read lock on the member under the shared transaction id (kept
    /// in pending_txns until commit/abort), accumulating into any state this
    /// node already prepared for the same transaction.
    pub async fn remote_read_participant(
        &mut self,
        service: Symbol,
        member: Symbol,
        tid: TxnId,
    ) -> Result<ReadEntry, EvalError> {
        let mut txn = self
            .pending_txns
            .remove(&tid)
            .unwrap_or_else(|| Transaction::new(tid.clone()));
        match self
            .lookup_with_clock(member, service, Some(&mut txn))
            .await
        {
            Ok(v) => {
                self.pending_txns.insert(tid, txn);
                Ok(v)
            }
            Err(e) => {
                // Wait-die wait: preserve the transaction so the parked read can
                // resume on release; any other failure releases and drops it.
                if matches!(e, EvalError::WaitOn(_)) {
                    self.pending_txns.insert(tid, txn);
                    return Err(e);
                }
                // Could not acquire the read lock (e.g. conflict): release any
                // locks taken and do not keep this transaction prepared.
                self.discard_failed_participant_txn(txn).await;
                Err(e)
            }
        }
    }

    pub async fn remote_action(
        &mut self,
        service_net_id: &ServiceNetId,
        stmts: Vec<ActionStmt>,
        env: Vec<(Symbol, Value)>,
        mut txn: Option<&mut Transaction>,
    ) -> Result<(), EvalError> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_ACTION_ID: AtomicU64 = AtomicU64::new(1);

        // Dial the node address embedded in the `ServiceNetId`; send the
        // slug as the service name the remote node uses to find its local
        // service. This works even if the service was never imported
        // into the current scope
        let (addr, slug) = Self::split_service_net_id(service_net_id);
        let request_id = NEXT_ACTION_ID.fetch_add(1, Ordering::SeqCst);
        let reply_to = self.local_reply_addr().await;

        // When part of a transaction, ship its id so the remote node executes
        // under the shared transaction and holds (does not commit) until our
        // commit/abort. Standalone (no txn) keeps the old commit-immediately path.
        let shared_tid = txn.as_ref().map(|t| t.id.clone());

        // Pre-register the participant BEFORE sending. If the request times
        // out or the response is lost after the remote already prepared
        // and grabbed locks, the originator's abort path still iterates
        // `txn.participants` and reaches this node to release them. If
        // the remote never received the request, the `Abort` it gets is a
        // harmless no-op
        if shared_tid.is_some() {
            if let Some(t) = txn.as_deref_mut() {
                t.participants.insert(addr.clone());
            }
        }

        let mut net_stmts = Vec::new();
        for s in &stmts {
            net_stmts.push(
                codec::encode_action_stmt(s, &self.interner)
                    .map_err(|e| EvalError::LocalDispatchFailed(e.to_string()))?,
            );
        }

        let mut net_env = Vec::new();
        for (sym, val) in env {
            let key_str = self.interner.get(sym).to_string();
            let enc_val = codec::encode_value(&val, &self.interner)
                .map_err(|e| EvalError::LocalDispatchFailed(e.to_string()))?;
            net_env.push((key_str, enc_val));
        }

        // Claim this dispatch's position in the transaction's dispatch order,
        // and stop here if a parked run already completed it (`composed_done`)
        let mut claim = None;
        if let Some(t) = txn.as_deref_mut() {
            let seq = t.composed_seq;
            t.composed_seq += 1;
            let call = ComposedCall {
                target: service_net_id.clone(),
                fingerprint: Self::fingerprint(&net_stmts, &net_env)?,
            };
            if let Some(done) = t.composed_done.get(&seq) {
                // Target alone is not identity: a replay can branch into a
                // different action on the same service. The recorded effect is
                // already buffered on the other node and cannot be rolled back
                // from here, so failing is the only honest outcome
                if done.target != call.target {
                    return Err(EvalError::LocalDispatchFailed(format!(
                        "replayed transaction dispatched a composed action to '{}' \
                         in place of the one it had dispatched to '{}'",
                        call.target.0, done.target.0
                    )));
                }
                if done.fingerprint != call.fingerprint {
                    return Err(EvalError::LocalDispatchFailed(format!(
                        "replayed transaction's composed action to '{}' differs \
                         from the one it dispatched there before",
                        call.target.0
                    )));
                }
                return Ok(());
            }
            claim = Some((seq, call));
        }

        let msg = MeerkatMessage::ActionRequest {
            request_id,
            service: slug.clone(),
            stmts: net_stmts,
            env: net_env,
            reply_to,
            txn_id: shared_tid,
        };

        let reply = self
            .send_and_await_reply(
                addr.clone(),
                msg,
                request_id,
                format!("Timeout waiting for remote action on service '{}'", slug),
            )
            .await?;

        match reply {
            MeerkatMessage::ActionResponse {
                success,
                error,
                history,
                writes,
                ..
            } => {
                if success {
                    // Participant already registered above. Keep what it
                    // reports it read and wrote for the commit stamp
                    if let Some(txn) = txn {
                        let history = codec::clock_entry_to_vclock(history, &mut self.interner)
                            .map_err(|e| EvalError::LocalDispatchFailed(e.to_string()))?;
                        let writes = codec::decode_sources(writes, &mut self.interner)
                            .map_err(|e| EvalError::LocalDispatchFailed(e.to_string()))?;
                        txn.participant_history = join_clocks([&txn.participant_history, &history]);
                        txn.participant_writes.extend(writes);
                    }
                    Ok(())
                } else {
                    Err(Self::remote_error(
                        error.unwrap_or_else(|| "Remote action failed".to_string()),
                    ))
                }
            }
            MeerkatMessage::Ping { .. }
            | MeerkatMessage::Pong { .. }
            | MeerkatMessage::Announce { .. }
            | MeerkatMessage::Transaction { .. }
            | MeerkatMessage::Propagation { .. }
            | MeerkatMessage::LookupRequest { .. }
            | MeerkatMessage::LookupResponse { .. }
            | MeerkatMessage::LookupError { .. }
            | MeerkatMessage::ActionRequest { .. }
            | MeerkatMessage::Commit { .. }
            | MeerkatMessage::CommitResponse { .. }
            | MeerkatMessage::Abort { .. }
            | MeerkatMessage::AbortResponse { .. }
            | MeerkatMessage::LockRequest { .. }
            | MeerkatMessage::LockResponse { .. }
            | MeerkatMessage::RequestUpdates { .. }
            | MeerkatMessage::Update { .. }
            | MeerkatMessage::ServiceCodeRequest { .. }
            | MeerkatMessage::ServiceCodeResponse { .. }
            | MeerkatMessage::ServiceCodeError { .. }
            | MeerkatMessage::UpdateServiceRequest { .. }
            | MeerkatMessage::UpdateServiceResponse { .. }
            | MeerkatMessage::WaitParked { .. } => Err(EvalError::LocalDispatchFailed(
                "Unexpected reply to action request".to_string(),
            )),
        }
    }

    /// Run a participant request (initial dispatch or a woken waiter), then wake
    /// whatever is parked on locks it released
    pub async fn run_and_reply_or_park(&mut self, parked: ParkedRequest) {
        self.dispatch_parked(parked).await;
        self.wake_freed().await;
    }

    /// Run a participant request and either send its reply, or, if the requesting
    /// transaction is older than a current lock holder (wait-die), park it on the
    /// contended variable's queue to be re-run when that lock frees.
    async fn dispatch_parked(&mut self, parked: ParkedRequest) {
        match parked {
            ParkedRequest::Action {
                request_id,
                reply_to,
                service,
                stmts,
                env,
                tid,
            } => {
                match self
                    .execute_action_participant(service, &stmts, &env, tid.clone())
                    .await
                {
                    Err(EvalError::WaitOn(key)) => {
                        self.park_request_key(
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
                        // What the transaction has read and written through this
                        // node so far, for the originator's commit stamp
                        let (history, writes) = match &other {
                            Ok(()) => {
                                let (v_base, write_set) =
                                    self.transaction_history(&self.pending_txns[&tid]);
                                (
                                    codec::encode_clock(&v_base, &self.interner),
                                    codec::encode_sources(&write_set, &self.interner),
                                )
                            }
                            Err(_) => (Vec::new(), Vec::new()),
                        };
                        let response = MeerkatMessage::ActionResponse {
                            request_id,
                            success: other.is_ok(),
                            error: other.err().map(|e| e.to_string()),
                            history,
                            writes,
                        };
                        self.send_oneway(Address::new(&reply_to), response).await;
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
                match self
                    .remote_read_participant(service, member, tid.clone())
                    .await
                {
                    Err(EvalError::WaitOn(key)) => {
                        self.park_request_key(
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
                    Ok((val, clock, sources)) => {
                        let response = match codec::encode_value(&val, &self.interner) {
                            Ok(enc_val) => MeerkatMessage::LookupResponse {
                                request_id,
                                value: enc_val,
                                clock: clock
                                    .as_ref()
                                    .map(|clock| codec::encode_clock(clock, &self.interner)),
                                sources: sources
                                    .as_ref()
                                    .map(|sources| codec::encode_sources(sources, &self.interner)),
                            },
                            Err(e) => MeerkatMessage::LookupError {
                                request_id,
                                error: e.to_string(),
                            },
                        };
                        self.send_oneway(Address::new(&reply_to), response).await;
                    }
                    Err(e) => {
                        let response = MeerkatMessage::LookupError {
                            request_id,
                            error: e.to_string(),
                        };
                        self.send_oneway(Address::new(&reply_to), response).await;
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
                match self
                    .handle_lock_request(txn_id.clone(), services.clone())
                    .await
                {
                    // Defensive check: If lock acquisition is blocked
                    // again (e.g., by an older transaction), park the
                    // request in the queue to await future release.
                    Err(EvalError::WaitOn(key)) => {
                        self.park_request_key(
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
                        self.send_oneway(Address::new(&reply_to), response).await;
                    }
                }
            }
        }
    }

    /// Re-dispatch the requests parked on locks released since the last call
    /// (`Manager::take_freed_awaiting_wake`), oldest first
    pub async fn wake_freed(&mut self) {
        let freed = self.take_freed_awaiting_wake();
        for parked in self.take_ready_waiters(&freed) {
            // Boxed: this and `run_and_reply_or_park` are mutually recursive
            Box::pin(self.run_and_reply_or_park(parked)).await;
        }
    }

    /// Serve a request a node answers itself: a lookup, an action, a
    /// participant's commit or abort, a lock request, or a request for a file
    /// in `served_base_dir`. Shared by the CLI server loop and by the wait for
    /// a code update's first values, which must not drop them
    /// (`await_first_values`). Returns any other message
    pub async fn serve_request(&mut self, msg: MeerkatMessage) -> Option<MeerkatMessage> {
        match msg {
            // #39: a client requests a .mkt file by path, resolved safely
            // against the served directory
            MeerkatMessage::ServiceCodeRequest {
                request_id,
                path,
                reply_to,
            } if self.served_base_dir.is_some() => {
                let response = self.served_base_dir.as_deref().map(|base_dir| {
                    codec::serve_service_code(request_id, path, &reply_to, base_dir)
                });
                if let Some(response) = response {
                    self.send_oneway(Address::new(&reply_to), response).await;
                }
            }
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
                    self.send_oneway(Address::new(&reply_to), response).await;
                    return None;
                }
                let svc_sym = self.interner.insert(&service);
                let mem_sym = self.interner.insert(&member);
                match txn_id {
                    // Transactional read: park if older than a holder
                    Some(tid) => {
                        self.run_and_reply_or_park(ParkedRequest::Lookup {
                            request_id,
                            reply_to,
                            service: svc_sym,
                            member: mem_sym,
                            tid,
                        })
                        .await;
                    }
                    // Plain unlocked read: reply immediately
                    None => {
                        let result = self.lookup_with_clock(mem_sym, svc_sym, None).await;
                        let response = match result {
                            Ok((val, clock, sources)) => {
                                match codec::encode_value(&val, &self.interner) {
                                    Ok(enc_val) => MeerkatMessage::LookupResponse {
                                        request_id,
                                        value: enc_val,
                                        clock: clock.as_ref().map(|clock| {
                                            codec::encode_clock(clock, &self.interner)
                                        }),
                                        sources: sources.as_ref().map(|sources| {
                                            codec::encode_sources(sources, &self.interner)
                                        }),
                                    },
                                    Err(e) => MeerkatMessage::LookupError {
                                        request_id,
                                        error: e.to_string(),
                                    },
                                }
                            }
                            Err(e) => MeerkatMessage::LookupError {
                                request_id,
                                error: e.to_string(),
                            },
                        };
                        self.send_oneway(Address::new(&reply_to), response).await;
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
                        history: Vec::new(),
                        writes: Vec::new(),
                    };
                    self.send_oneway(Address::new(&reply_to), response).await;
                    return None;
                }
                let svc_sym = self.interner.insert(&service);
                let mut local_stmts = Vec::new();
                let mut decode_failed = false;
                let mut error_msg = None;
                for s in stmts {
                    match codec::decode_action_stmt(s, &mut self.interner) {
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
                        match codec::decode_value(v, &mut self.interner) {
                            Ok(dv) => local_env.push((self.interner.insert(&k), dv)),
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
                        history: Vec::new(),
                        writes: Vec::new(),
                    };
                    self.send_oneway(Address::new(&reply_to), response).await;
                    return None;
                }
                match txn_id {
                    // Part of a distributed transaction: park if older
                    // than a holder, otherwise reply
                    Some(tid) => {
                        self.run_and_reply_or_park(ParkedRequest::Action {
                            request_id,
                            reply_to,
                            service: svc_sym,
                            stmts: local_stmts,
                            env: local_env,
                            tid,
                        })
                        .await;
                    }
                    // Standalone: commit immediately and reply
                    None => {
                        let result = self
                            .execute_action_with_env(svc_sym, &local_stmts, &local_env)
                            .await;
                        let response = MeerkatMessage::ActionResponse {
                            request_id,
                            success: result.is_ok(),
                            error: result.err().map(|e| e.to_string()),
                            history: Vec::new(),
                            writes: Vec::new(),
                        };
                        self.send_oneway(Address::new(&reply_to), response).await;
                    }
                }
            }
            MeerkatMessage::Commit {
                request_id,
                txn_id,
                reply_to,
                clock,
            } => {
                // An undecodable clock commits nothing: the transaction
                // stays held, as if this `Commit` had never arrived
                let result = match codec::clock_entry_to_vclock(clock, &mut self.interner) {
                    Ok(clock) => self.commit_participant(&txn_id, &clock).await,
                    Err(e) => Err(EvalError::LocalDispatchFailed(e.to_string())),
                };
                let response = MeerkatMessage::CommitResponse {
                    request_id,
                    success: result.is_ok(),
                    error: result.err().map(|e| e.to_string()),
                };
                self.send_oneway(Address::new(&reply_to), response).await;
                // Wake transactions that were waiting on locks this
                // commit just released, even if forwarding it failed.
                self.wake_freed().await;
            }
            MeerkatMessage::Abort {
                request_id,
                txn_id,
                reply_to,
            } => {
                self.abort_participant(&txn_id).await;
                // Drop this transaction's own parked requests so they
                // do not later wake for an abandoned transaction.
                self.purge_parked_txn(&txn_id);
                self.send_oneway(
                    Address::new(&reply_to),
                    MeerkatMessage::AbortResponse { request_id },
                )
                .await;
                // Wake transactions that were waiting on locks this
                // abort just released.
                self.wake_freed().await;
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
                    self.send_oneway(Address::new(&reply_to), response).await;
                    return None;
                }
                self.run_and_reply_or_park(ParkedRequest::Lock {
                    request_id,
                    reply_to,
                    txn_id,
                    services,
                })
                .await;
            }
            other => return Some(other),
        }
        None
    }

    /// Resolve an in-scope service name to its global `ServiceNetId`
    ///
    /// Callers only resolve names of local services here (remote reads
    /// and actions are routed before reaching this), so this returns
    /// the service's stored, stable ID
    ///
    /// The bare-name fallback is a defensive default for an unknown
    /// name and is not used for genuine remote services, whose
    /// identities travel embedded in their `ActionClosure`s
    pub fn service_net_id_for_name(&self, service_name: Symbol) -> ServiceNetId {
        if let Some(service) = self.services.get(&service_name) {
            service.id.clone()
        } else if let Some(addr) = self.remote_services.get(&service_name) {
            ServiceNetId::new(addr.0.clone())
        } else {
            ServiceNetId::new(self.interner.get(service_name))
        }
    }

    /// Find a local service (mutably) by its `ServiceNetId`
    fn service_by_net_id_mut(&mut self, service_net_id: &ServiceNetId) -> Option<&mut Service> {
        self.services.values_mut().find(|s| &s.id == service_net_id)
    }

    /// Find the in-scope name of a local service from its `ServiceNetId`
    pub fn service_name_for_net_id(&self, service_net_id: &ServiceNetId) -> Option<Symbol> {
        self.services
            .iter()
            .find(|(_, s)| &s.id == service_net_id)
            .map(|(n, _)| *n)
    }

    /// Split a service identity into the dialable node address and the
    /// service slug (its trailing name segment)
    ///
    /// Allows `remote_action` to use the address embedded in an
    /// `ActionClosure`'s `ServiceNetId` rather than requiring the
    /// service to be imported into the current scope
    fn split_service_net_id(service_net_id: &ServiceNetId) -> (Address, String) {
        match service_net_id.0.rfind('/') {
            Some(i) => (
                Address::new(&service_net_id.0[..i]),
                service_net_id.0[i + 1..].to_string(),
            ),
            None => (Address::new(String::new()), service_net_id.0.clone()),
        }
    }

    /// Attempt to acquire a write lock on a service variable
    ///
    /// If lock contention occurs, determines whether the transaction
    /// should wait or die according to the wait-die deadlock prevention
    /// scheme
    ///
    /// Args:
    ///     `service_name` (`Symbol`): The symbol of the service
    ///     `var` (`Symbol`): The symbol of the variable to lock
    ///     `txn_id` (`&TxnId`): The ID of the requesting transaction
    ///
    /// Returns:
    ///     `Result<(), EvalError>`: `Ok` on successful lock acquisition, or an error
    ///
    /// Raises:
    ///     `EvalError::VarNotFound`: If the variable does not exist
    ///     `EvalError::ServiceNotFound`: If the service does not exist
    ///     `EvalError::WaitDieAbort`: If the transaction aborts under wait-die
    ///     `EvalError::WaitOn`: If the transaction must wait for the lock
    fn acquire_write_lock(
        &mut self,
        service_name: Symbol,
        var: Symbol,
        txn_id: &TxnId,
    ) -> Result<(), EvalError> {
        let sid = self.service_net_id_for_name(service_name);
        let service = self.services.get_mut(&service_name).ok_or_else(|| {
            EvalError::ServiceNotFound(format!(
                "Service '{}' not found",
                self.interner.get(service_name)
            ))
        })?;
        // Enforce service lock boundary: a whole-service lock blocks all
        // member locks unless held by the same transaction
        if let Some(holder) = &service.service_lock {
            if holder != txn_id {
                let holder_older = holder < txn_id;
                if holder_older {
                    return Err(EvalError::WaitDieAbort(format!(
                        "transaction died contending for service lock on '{}'",
                        self.interner.get(service_name)
                    )));
                } else {
                    return Err(EvalError::WaitOn(WaitKey::Service(sid.clone())));
                }
            }
        }
        let var_state = service.vars.entry(var).or_insert_with(|| VarState {
            value: Value::Int { val: 0 },
            lock: crate::runtime::txn::VarLock::Unlocked,
            latest_write_txn: None,
            vector_clock: HashMap::new(),
            sources: None,
        });
        if var_state.lock.try_write(txn_id) {
            Ok(())
        } else {
            match var_state.lock.wait_die(txn_id) {
                crate::runtime::txn::WaitDie::Die => Err(EvalError::WaitDieAbort(format!(
                    "transaction died contending for write lock on '{}'",
                    self.interner.get(var)
                ))),
                crate::runtime::txn::WaitDie::Wait => {
                    Err(EvalError::WaitOn(WaitKey::Member(sid, var)))
                }
            }
        }
    }

    /// Attempt to acquire a read lock on a service variable
    ///
    /// Multi-readers can share read locks, but will conflict with write locks
    /// Uses wait-die deadlock prevention on contention
    ///
    /// Args:
    ///     `service_name` (`Symbol`): The symbol of the service
    ///     `var` (`Symbol`): The symbol of the variable to lock
    ///     `txn_id` (`&TxnId`): The ID of the requesting transaction
    ///
    /// Returns:
    ///     `Result<(), EvalError>`: `Ok` on successful lock acquisition, or an error
    ///
    /// Raises:
    ///     `EvalError::VarNotFound`: If the variable does not exist
    ///     `EvalError::ServiceNotFound`: If the service does not exist
    ///     `EvalError::WaitDieAbort`: If the transaction aborts under wait-die
    ///     `EvalError::WaitOn`: If the transaction must wait for the lock
    fn acquire_read_lock(
        &mut self,
        service_name: Symbol,
        var: Symbol,
        txn_id: &TxnId,
    ) -> Result<(), EvalError> {
        let sid = self.service_net_id_for_name(service_name);
        let service = self.services.get_mut(&service_name).ok_or_else(|| {
            EvalError::ServiceNotFound(format!(
                "Service '{}' not found",
                self.interner.get(service_name)
            ))
        })?;
        // Enforce service lock boundary: a whole-service lock blocks all
        // member locks unless held by the same transaction
        if let Some(holder) = &service.service_lock {
            if holder != txn_id {
                let holder_older = holder < txn_id;
                if holder_older {
                    return Err(EvalError::WaitDieAbort(format!(
                        "transaction died contending for service lock on '{}'",
                        self.interner.get(service_name)
                    )));
                } else {
                    return Err(EvalError::WaitOn(WaitKey::Service(sid.clone())));
                }
            }
        }
        let var_state = service.vars.get_mut(&var).ok_or_else(|| {
            EvalError::VarNotFound(format!("Variable '{}' not found", self.interner.get(var)))
        })?;
        if var_state.lock.try_read(txn_id) {
            Ok(())
        } else {
            match var_state.lock.wait_die(txn_id) {
                crate::runtime::txn::WaitDie::Die => Err(EvalError::WaitDieAbort(format!(
                    "transaction died contending for read lock on '{}'",
                    self.interner.get(var)
                ))),
                crate::runtime::txn::WaitDie::Wait => {
                    Err(EvalError::WaitOn(WaitKey::Member(sid, var)))
                }
            }
        }
    }

    /// Upgrade an existing read lock to a write lock on a service variable
    ///
    /// Used for read-then-write patterns in transactions to avoid conflicts
    ///
    /// Args:
    ///     `service_name` (`Symbol`): The symbol of the service
    ///     `var` (`Symbol`): The symbol of the variable to lock
    ///     `txn_id` (`&TxnId`): The ID of the requesting transaction
    ///
    /// Returns:
    ///     `Result<(), EvalError>`: `Ok` on successful lock upgrade, or an error
    ///
    /// Raises:
    ///     `EvalError::VarNotFound`: If the variable does not exist
    ///     `EvalError::ServiceNotFound`: If the service does not exist
    ///     `EvalError::WaitDieAbort`: If the transaction aborts under wait-die
    ///     `EvalError::WaitOn`: If the transaction must wait for the lock
    fn upgrade_to_write_lock(
        &mut self,
        service_name: Symbol,
        var: Symbol,
        txn_id: &TxnId,
    ) -> Result<(), EvalError> {
        let sid = self.service_net_id_for_name(service_name);
        let service = self.services.get_mut(&service_name).ok_or_else(|| {
            EvalError::ServiceNotFound(format!(
                "Service '{}' not found",
                self.interner.get(service_name)
            ))
        })?;
        // Enforce service lock boundary: a whole-service lock blocks all
        // member locks unless held by the same transaction
        if let Some(holder) = &service.service_lock {
            if holder != txn_id {
                let holder_older = holder < txn_id;
                if holder_older {
                    return Err(EvalError::WaitDieAbort(format!(
                        "transaction died contending for service lock on '{}'",
                        self.interner.get(service_name)
                    )));
                } else {
                    return Err(EvalError::WaitOn(WaitKey::Service(sid.clone())));
                }
            }
        }
        let var_state = service.vars.get_mut(&var).ok_or_else(|| {
            EvalError::VarNotFound(format!("Variable '{}' not found", self.interner.get(var)))
        })?;
        if var_state.lock.upgrade_to_write(txn_id) {
            Ok(())
        } else {
            match var_state.lock.wait_die(txn_id) {
                crate::runtime::txn::WaitDie::Die => Err(EvalError::WaitDieAbort(format!(
                    "transaction died contending to upgrade lock on '{}'",
                    self.interner.get(var)
                ))),
                crate::runtime::txn::WaitDie::Wait => {
                    Err(EvalError::WaitOn(WaitKey::Member(sid, var)))
                }
            }
        }
    }

    /// Helper to get all locked keys (variables and service locks) for a transaction
    fn all_locked_keys(&self, txn: &Transaction) -> HashSet<WaitKey> {
        let mut keys = HashSet::new();
        for (sid, var) in &txn.locked {
            keys.insert(WaitKey::Member(sid.clone(), *var));
        }
        for sid in &txn.service_locked {
            keys.insert(WaitKey::Service(sid.clone()));
        }
        keys
    }

    /// Release all locks (both member and service level) held by a transaction
    pub fn release_all_locks(&mut self, txn: &Transaction) {
        let keys = self.all_locked_keys(txn);
        self.release_locks(&keys, &txn.id);
    }

    /// Release `locked`, recording in `freed_awaiting_wake` the keys a request
    /// is parked on
    fn release_locks(&mut self, locked: &HashSet<WaitKey>, txn_id: &TxnId) {
        for key in locked {
            match key {
                WaitKey::Service(sid) => {
                    if let Some(service) = self.service_by_net_id_mut(sid) {
                        if service.service_lock.as_ref() == Some(txn_id) {
                            service.service_lock = None;
                        }
                    }
                }
                WaitKey::Member(sid, var) => {
                    if let Some(service) = self.service_by_net_id_mut(sid) {
                        if let Some(var_state) = service.vars.get_mut(var) {
                            var_state.lock.release(txn_id);
                        }
                    }
                }
            }
        }
        let waited_on: Vec<WaitKey> = locked
            .iter()
            .filter(|k| self.wait_queue.get(k).is_some_and(|w| !w.is_empty()))
            .cloned()
            .collect();
        self.freed_awaiting_wake.extend(waited_on);
    }

    /// Execute action statements as a transaction with lazy lock
    /// acquisition
    ///
    /// Locks are acquired on demand as each variable is first read or
    /// written during execution (inside `lookup` and `assign`), rather
    /// than upfront.
    /// This handles actions invoked via function calls and conditional
    /// branches, where the set of accessed variables cannot be
    /// determined statically.
    /// Read values are cached in the transaction to avoid re-fetching
    /// (which also avoids redundant network round-trips for remote
    /// reads)
    ///
    /// On completion, a commit records `latest_write_txn` for written
    /// variables, then all locks are released (always, even on error)
    ///
    /// If a lock cannot be acquired, wait-die deadlock prevention
    /// determines whether the transaction waits or dies
    pub async fn execute_action_with_txn(
        &mut self,
        service_name: Symbol,
        stmts: &[ActionStmt],
        initial_env: &[(Symbol, Value)],
    ) -> Result<(), EvalError> {
        let mut txn_id = TxnId::new(self.node_id);

        loop {
            let mut txn = Transaction::new(txn_id.clone());

            let mut env: Vec<(Symbol, Value)> = initial_env.to_vec();
            let exec_error = execute_seq(stmts, &mut env, self, service_name, Some(&mut txn))
                .await
                .err();

            // Both wait-die outcomes retry the whole transaction. A participant
            // told to wait parks on `wait_queue`, but an originator drives its
            // own transaction and has no queue to park in, so retrying is how
            // it waits.
            if let Some(EvalError::WaitDieAbort(_) | EvalError::WaitOn(_)) = &exec_error {
                for addr in txn.participants.iter().cloned().collect::<Vec<_>>() {
                    self.send_abort(addr, &txn.id).await;
                }
                self.release_all_locks(&txn);
                if txn_id.iteration < MAX_WAIT_DIE_RETRIES {
                    txn_id = txn_id.retry();
                    let backoff = match exec_error {
                        Some(EvalError::WaitOn(_)) => WAIT_ON_RETRY_BACKOFF_MS,
                        _ => WAIT_DIE_RETRY_BACKOFF_MS,
                    };
                    retry_backoff(backoff).await;
                    continue;
                }
                // Out of retries. `WaitOn` never leaves this loop as itself:
                // its `WaitKey` holds ids only this node can read back.
                return Err(match exec_error.unwrap() {
                    EvalError::WaitOn(key) => EvalError::WaitDieAbort(format!(
                        "gave up waiting for {} after {} retries",
                        self.describe_wait_key(&key),
                        MAX_WAIT_DIE_RETRIES
                    )),
                    other => other,
                });
            }

            // A refused commit is reported, not swallowed: on `Ok` the CLI
            // prints `@test(...) passed` for a transaction only part of which
            // committed. There is no rollback to offer -- this node's writes
            // are already stored (#191)
            let error = match exec_error {
                None => self.commit_txn(&txn).await,
                Some(e) => {
                    for addr in txn.participants.iter().cloned().collect::<Vec<_>>() {
                        self.send_abort(addr, &txn.id).await;
                    }
                    Some(e)
                }
            };

            self.release_all_locks(&txn);

            return error.map_or(Ok(()), Err);
        }
    }

    /// Render a contended lock key in program terms: `service '<name>'` or
    /// `'<service>.<member>'`, falling back to the raw `ServiceNetId` when the
    /// service is not local.
    fn describe_wait_key(&self, key: &WaitKey) -> String {
        let name_of = |sid: &ServiceNetId| {
            self.service_name_for_net_id(sid)
                .map(|n| self.interner.get(n).to_string())
                .unwrap_or_else(|| sid.0.clone())
        };
        match key {
            WaitKey::Service(sid) => format!("service '{}'", name_of(sid)),
            WaitKey::Member(sid, member) => {
                format!("'{}.{}'", name_of(sid), self.interner.get(*member))
            }
        }
    }

    /// Commit a transaction on this node and on every participant, returning
    /// the first participant's failure to commit (see `commit_participants`)
    ///
    /// Stores this node's writes, commits the participants, and only then
    /// recomputes what derives from the writes. Propagating first would
    /// recompute a def over a remote member while that member's node is still
    /// holding its write buffered, storing a value that was never true
    ///
    /// Propagation is best-effort and runs even when a participant failed:
    /// the writes are stored, so the transaction has committed here and there
    /// is no way back
    ///
    /// Every write, here and on every participant, is stamped with the one
    /// `V_new` of §5.2, computed from what the participants reported
    async fn commit_txn(&mut self, txn: &Transaction) -> Option<EvalError> {
        let (v_base, write_set) = self.transaction_history(txn);
        self.commit_stamped(txn, &advance(v_base, &write_set)).await
    }

    /// Store `txn`'s writes on this node with `clock`, the transaction's
    /// `V_new`, commit its participants with the same clock, then propagate
    ///
    /// `clock` records what the transaction saw of other owners. A cached
    /// remote input behind it would hold back every def that pairs it with
    /// these writes, so its owner is asked for its current value again. The
    /// owner's own `Update` usually arrives as well, possibly after the
    /// `CommitResponse`; the request covers one that will not, because it was
    /// overtaken by an older one or lost
    async fn commit_stamped(&mut self, txn: &Transaction, clock: &VClock) -> Option<EvalError> {
        for ((sid, var), value) in &txn.written {
            if let Some(service) = self.service_by_net_id_mut(sid) {
                if let Some(var_state) = service.vars.get_mut(var) {
                    var_state.value = value.clone();
                    var_state.latest_write_txn = Some(txn.id.clone());
                    var_state.vector_clock = clock.clone();
                }
            }
        }
        let error = self.commit_participants(txn, clock).await;
        for (listener, def, owner, member) in self.inputs_behind(clock) {
            self.subscribe_remote(owner, member, listener, def).await;
        }
        for (sid, var) in txn.written.keys() {
            if let Some(name) = self.service_name_for_net_id(sid) {
                self.propagate(name, *var).await;
            }
        }
        error
    }

    /// The cached remote inputs, as `(listener, def, owner, member)`, whose
    /// clocks have not reached `clock` on a dimension their value depends on
    fn inputs_behind(&self, clock: &VClock) -> Vec<(ServiceNetId, Symbol, Symbol, Symbol)> {
        let mut behind = Vec::new();
        for service in self.services.values() {
            for (def, cache) in &service.dep_cache {
                for (&(owner, member), (_, cached, sources)) in cache {
                    if cached
                        .as_ref()
                        .is_some_and(|cached| !caught_up(clock, cached, sources.as_ref()))
                    {
                        behind.push((service.id.clone(), *def, owner, member));
                    }
                }
            }
        }
        behind
    }

    /// Send `Commit`, carrying the transaction's `clock`, to every participant
    /// of `txn`, returning the first failure in the set's iteration order,
    /// which is arbitrary
    ///
    /// Does not stop at a failure, or the later participants are left
    /// prepared and holding locks
    pub(crate) async fn commit_participants(
        &mut self,
        txn: &Transaction,
        clock: &VClock,
    ) -> Option<EvalError> {
        let mut first_error = None;
        for addr in txn.participants.iter().cloned().collect::<Vec<_>>() {
            if let Err(e) = self.send_commit(addr.clone(), &txn.id, clock).await {
                log::warn!("participant {} failed to commit: {}", addr.0, e);
                first_error.get_or_insert(e);
            }
        }
        first_error
    }

    /// Participant side: execute a composed action under a shared transaction
    /// ID received from the originator, then hold the transaction (locks and
    /// buffered writes) in `pending_txns` until a `Commit` or `Abort` arrives
    ///
    /// Does not commit
    pub async fn execute_action_participant(
        &mut self,
        service_name: Symbol,
        stmts: &[ActionStmt],
        initial_env: &[(Symbol, Value)],
        tid: TxnId,
    ) -> Result<(), EvalError> {
        let mut txn = self
            .pending_txns
            .remove(&tid)
            .unwrap_or_else(|| Transaction::new(tid.clone()));
        // A `WaitOn` parks the whole action and re-runs it from the first
        // statement, so a run that parks must leave the buffers as it found
        // them, or `x = x + 1` reads its own buffered `1` on replay and
        // commits `2`. Restored rather than cleared, because an earlier action
        // under the same transaction id may have buffered writes here too.
        // Locks and participants are kept: they hold this transaction's place
        // in line and still need committing or aborting with it.
        // `composed_seq` is rewound so a replayed composed action finds the
        // record the parked run left in `composed_done`. All of this goes
        // away once a parked action is suspended instead of re-run (#213)
        let before = (
            txn.written.clone(),
            txn.read_cache.clone(),
            txn.composed_seq,
        );
        let mut env: Vec<(Symbol, Value)> = initial_env.to_vec();
        let mut exec_error = execute_seq(stmts, &mut env, self, service_name, Some(&mut txn))
            .await
            .err();
        // Positions are dense, so a completed run that did not reach every
        // recorded one branched past a composed action the parked run made,
        // whose effect is buffered on the other node all the same. Positions
        // below `start` belong to earlier actions under this transaction id
        let start = before.2 as usize;
        if exec_error.is_none() && (txn.composed_seq as usize) < txn.composed_done.len() {
            exec_error = Some(EvalError::LocalDispatchFailed(format!(
                "replayed action dispatched {} composed actions, but its parked run \
                 had completed {}",
                txn.composed_seq as usize - start,
                txn.composed_done.len() - start
            )));
        }
        if let Some(e) = exec_error {
            if matches!(e, EvalError::WaitOn(_)) {
                (txn.written, txn.read_cache, txn.composed_seq) = before;
                self.pending_txns.insert(tid, txn);
                return Err(e);
            }
            self.discard_failed_participant_txn(txn).await;
            return Err(e);
        }
        self.pending_txns.insert(tid, txn);
        Ok(())
    }

    /// Participant side: apply and release a held transaction on `Commit`,
    /// stamping its writes with the originator's `clock`
    ///
    /// An error is a failure forwarding the commit to a node below. The local
    /// commit is done either way, and the freed keys reach the server loop
    /// through `freed_awaiting_wake`, so they are woken on that path too
    pub async fn commit_participant(
        &mut self,
        tid: &TxnId,
        clock: &VClock,
    ) -> Result<(), EvalError> {
        let Some(txn) = self.pending_txns.remove(tid) else {
            return Ok(());
        };
        let forward_error = self.commit_stamped(&txn, clock).await;
        // Held until here, like the originator holds its own. Today the
        // event loop is blocked for the whole of this call, so an earlier
        // release would be harmless. That stops being true under #28's
        // background message loop, which `send_and_await_reply` is also
        // waiting on: once other work can interleave here, releasing
        // before the nodes below have committed exposes half of a
        // distributed transaction to whoever takes the lock next
        self.release_all_locks(&txn);
        forward_error.map_or(Ok(()), Err)
    }

    /// Centralized cleanup for a participant transaction that encountered a terminal failure
    ///
    /// Releases local locks and aborts all sub-participants before dropping the transaction
    ///
    /// The freed keys reach the server loop through `freed_awaiting_wake`
    ///
    /// Args:
    ///     txn (Transaction): The transaction context
    async fn discard_failed_participant_txn(&mut self, txn: Transaction) {
        self.release_all_locks(&txn);
        for addr in txn.participants {
            self.send_abort(addr, &txn.id).await;
        }
    }

    /// Participant side: discard and release a held transaction on `Abort`, and
    /// forward the abort down the chain to any sub-participants
    ///
    /// The freed keys reach the server loop through `freed_awaiting_wake`
    pub async fn abort_participant(&mut self, tid: &TxnId) {
        if let Some(txn) = self.pending_txns.remove(tid) {
            self.discard_failed_participant_txn(txn).await;
        }
    }

    /// Attempt to acquire a whole-service lock on a service
    pub fn acquire_service_lock(
        &mut self,
        service_name: Symbol,
        txn_id: &TxnId,
    ) -> Result<(), EvalError> {
        let sid = self.service_net_id_for_name(service_name);
        let service = self.services.get_mut(&service_name).ok_or_else(|| {
            EvalError::ServiceNotFound(format!(
                "Service '{}' not found",
                self.interner.get(service_name)
            ))
        })?;

        // Step 1: Check whole-service lock boundary
        // A service-level lock acts as an exclusive write lock on all
        // present and future members. If held by another transaction,
        // younger requesters abort immediately under wait-die, while
        // older requesters yield `WaitOn(WaitKey::Service(...))` to park
        if let Some(holder) = &service.service_lock {
            if holder == txn_id {
                return Ok(());
            }
            let holder_older = holder < txn_id;
            if holder_older {
                return Err(EvalError::WaitDieAbort(format!(
                    "transaction died contending for service lock on '{}'",
                    self.interner.get(service_name)
                )));
            } else {
                return Err(EvalError::WaitOn(WaitKey::Service(sid.clone())));
            }
        }

        // Step 2: Defensive O(N) scan across member variables
        // Before granting the service-level lock, scan all member
        // variables to ensure no other transaction holds an active
        // member lock. If contention exists, apply wait-die against
        // the oldest lock holder on that variable
        for (var_name, var_state) in &service.vars {
            if let Some(other_holder) = var_state.lock.oldest_other_holder(txn_id) {
                let holder_older = &other_holder < txn_id;
                if holder_older {
                    return Err(EvalError::WaitDieAbort(format!(
                        "transaction died contending for service lock on '{}'",
                        self.interner.get(service_name)
                    )));
                } else {
                    return Err(EvalError::WaitOn(WaitKey::Member(sid, *var_name)));
                }
            }
        }

        service.service_lock = Some(txn_id.clone());
        Ok(())
    }

    /// Handle eager LockRequest from the network.
    ///
    /// Invokes lock group acquisition logic under a shared transaction,
    /// storing the updated transaction state on success/wait, or releasing
    /// all acquired locks on failure.
    ///
    /// Args:
    ///     txn_id (TxnId): The ID of the transaction.
    ///     services (HashMap<String, LockGroup>): The lock groups.
    ///
    /// Returns:
    ///     Result<(), EvalError>: Ok on success, or an error.
    ///
    /// Raises:
    ///     EvalError::WaitOn: If the transaction must wait.
    ///     EvalError: On any other evaluation failure.
    pub async fn handle_lock_request(
        &mut self,
        txn_id: TxnId,
        services: HashMap<String, LockGroup>,
    ) -> Result<(), EvalError> {
        codec::validate_lock_request(&services)
            .map_err(|e| EvalError::RuntimeError(e.to_string()))?;

        let mut txn = self
            .pending_txns
            .remove(&txn_id)
            .unwrap_or_else(|| Transaction::new(txn_id.clone()));

        let result = self.acquire_lock_group_internal(&mut txn, &services).await;

        match result {
            Ok(()) => {
                self.pending_txns.insert(txn_id, txn);
                Ok(())
            }
            // CRITICAL: All-or-Nothing Eager Lock Group Release
            // If a lock group acquisition encounters contention and
            // yields `WaitOn`, we MUST NOT hold onto partial locks.
            // Holding partial locks while parked would stall incoming
            // atomic updates that require those same locks.
            // On `WaitOn`, we immediately release all partial locks taken
            // so far and clear the transaction's lock tracking sets,
            // ensuring the transaction yields completely and retries
            // from scratch when unparked
            Err(EvalError::WaitOn(key)) => {
                self.release_all_locks(&txn);
                txn.locked.clear();
                txn.service_locked.clear();

                // Drain and explicitly abort sub-participants to release
                // remote partial locks
                for addr in txn.participants.drain().collect::<Vec<_>>() {
                    self.send_abort(addr, &txn.id).await;
                }
                debug_assert!(txn.participants.is_empty());

                self.pending_txns.insert(txn_id, txn);
                Err(EvalError::WaitOn(key))
            }
            Err(e) => {
                self.discard_failed_participant_txn(txn).await;
                Err(e)
            }
        }
    }

    /// Internal lock group acquisition: direct locks + transitive cascades.
    ///
    /// Iteratively resolves and locks all local dependencies, and forwards
    /// LockRequests to remote participant nodes.
    ///
    /// Args:
    ///     txn (&mut Transaction): The transaction context.
    ///     services (&HashMap<String, LockGroup>): The lock requirements.
    ///
    /// Returns:
    ///     Result<(), EvalError>: Ok on success, or an error.
    ///
    /// Raises:
    ///     EvalError::WaitOn: If a lock must wait.
    ///     EvalError::LocalDispatchFailed: If a remote lock fails/times out.
    pub(crate) async fn acquire_lock_group_internal(
        &mut self,
        txn: &mut Transaction,
        services: &HashMap<String, LockGroup>,
    ) -> Result<(), EvalError> {
        let mut remote_locks: HashMap<Symbol, LockGroup> = HashMap::new();

        // 1. Process service-level locks first
        for (svc_name_str, group) in services {
            let svc_sym = self.interner.insert(svc_name_str);
            if group.service_level_lock {
                if self.services.contains_key(&svc_sym) {
                    let net_id = self.service_net_id_for_name(svc_sym);
                    self.acquire_service_lock(svc_sym, &txn.id)?;
                    txn.service_locked.insert(net_id);
                } else if self.remote_services.contains_key(&svc_sym) {
                    remote_locks.insert(svc_sym, group.clone());
                } else {
                    self.acquire_service_lock(svc_sym, &txn.id)?;
                }
            }
        }

        // 2. Queue for resolving all local locks (same and cross service)
        let mut queue: Vec<(Symbol, Symbol, bool)> = Vec::new();
        let mut visited = HashSet::new();

        for (svc_name_str, group) in services {
            let svc_sym = self.interner.insert(svc_name_str);
            if group.service_level_lock {
                continue;
            }
            for r in &group.reads {
                let var_sym = self.interner.insert(r);
                queue.push((svc_sym, var_sym, false));
            }
            for w in &group.writes {
                let var_sym = self.interner.insert(w);
                queue.push((svc_sym, var_sym, true));
            }
        }

        while let Some((svc_sym, mem_sym, is_write)) = queue.pop() {
            if self.services.contains_key(&svc_sym) {
                // Acquire the local lock (or upgrade it to a write lock
                // if already read-locked) before checking the member's
                // dependencies. This prevents a race condition where a
                // def expression is mutated by another transaction
                // after we check its dependencies but before we lock it
                let key = (self.service_net_id_for_name(svc_sym), mem_sym);

                if !txn.locked.contains(&key) {
                    // Acquire a new lock since the variable is not
                    // yet locked by the current transaction
                    if is_write {
                        self.acquire_write_lock(svc_sym, mem_sym, &txn.id)?;
                    } else {
                        self.acquire_read_lock(svc_sym, mem_sym, &txn.id)?;
                    }
                    txn.locked.insert(key);
                } else if is_write {
                    // Upgrade the existing read lock to a write lock
                    // if a write lock is requested
                    self.upgrade_to_write_lock(svc_sym, mem_sym, &txn.id)?;
                }

                // Traverse dependencies after locking
                if visited.insert((svc_sym, mem_sym)) {
                    if let Some(service) = self.services.get(&svc_sym) {
                        // Transitive local dependencies
                        let deps = service.graphs.transitive_dependencies_of(mem_sym);
                        for dep_sym in &deps {
                            queue.push((svc_sym, *dep_sym, false));
                        }

                        // Cross-service dependencies
                        if let Some(expr) = service.defs.get(&mem_sym) {
                            for (remote_svc, remote_mem) in cross_service_deps(expr) {
                                if self.remote_services.contains_key(&remote_svc) {
                                    // Track remote dependencies for a
                                    // remote service to request them
                                    // as a batch later
                                    let remote_mem_str = self.interner.get(remote_mem).to_string();
                                    let entry =
                                        remote_locks.entry(remote_svc).or_insert_with(|| {
                                            LockGroup {
                                                service_level_lock: false,
                                                reads: HashSet::new(),
                                                writes: HashSet::new(),
                                            }
                                        });
                                    entry.reads.insert(remote_mem_str);
                                } else {
                                    // Queue cross-service dependency
                                    // for local lock and traversal
                                    queue.push((remote_svc, remote_mem, false));
                                }
                            }
                        }
                    }
                }
            } else if self.remote_services.contains_key(&svc_sym) {
                // Accumulate remote lock requirements mapped to their
                // host node address
                let mem_str = self.interner.get(mem_sym).to_string();
                let entry = remote_locks.entry(svc_sym).or_insert_with(|| LockGroup {
                    service_level_lock: false,
                    reads: HashSet::new(),
                    writes: HashSet::new(),
                });
                if is_write {
                    entry.writes.insert(mem_str);
                } else {
                    entry.reads.insert(mem_str);
                }
            }
        }

        // 4. Build and send remote lock requests
        let mut node_requests: HashMap<Address, HashMap<String, LockGroup>> = HashMap::new();
        for (remote_svc, group) in remote_locks {
            if let Ok(addr) = self.remote_addr(remote_svc) {
                let svc_name_str = self.interner.get(remote_svc).to_string();
                node_requests
                    .entry(addr)
                    .or_default()
                    .insert(svc_name_str, group);
            }
        }

        for (addr, remote_services) in node_requests {
            txn.participants.insert(addr.clone());

            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT_LOCK_REQ_ID: AtomicU64 = AtomicU64::new(1);
            let request_id = NEXT_LOCK_REQ_ID.fetch_add(1, Ordering::SeqCst);

            let msg = MeerkatMessage::LockRequest {
                request_id,
                txn_id: txn.id.clone(),
                services: remote_services,
                reply_to: self.local_reply_addr().await,
            };

            let reply = self
                .send_and_await_reply(
                    addr.clone(),
                    msg,
                    request_id,
                    format!("Timeout waiting for lock response from {:?}", addr),
                )
                .await?;

            match reply {
                MeerkatMessage::LockResponse { success, error, .. } => {
                    if !success {
                        let err_str = error
                            .unwrap_or_else(|| "Lock request rejected by remote node".to_string());
                        return Err(Self::remote_error(err_str));
                    }
                }
                _ => {
                    return Err(EvalError::LocalDispatchFailed(
                        "Unexpected reply to lock request".to_string(),
                    ))
                }
            }
        }

        Ok(())
    }

    /// Originator side: ask a participant to commit, awaiting its acknowledgement
    pub(crate) async fn send_commit(
        &mut self,
        addr: Address,
        tid: &TxnId,
        clock: &VClock,
    ) -> Result<(), EvalError> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_COMMIT_ID: AtomicU64 = AtomicU64::new(1);
        let request_id = NEXT_COMMIT_ID.fetch_add(1, Ordering::SeqCst);
        let reply_to = self.local_reply_addr().await;
        let msg = MeerkatMessage::Commit {
            request_id,
            txn_id: tid.clone(),
            reply_to,
            clock: codec::encode_clock(clock, &self.interner),
        };
        let reply = self
            .send_and_await_reply(
                addr,
                msg,
                request_id,
                "Timeout waiting for commit acknowledgement".to_string(),
            )
            .await?;
        match reply {
            MeerkatMessage::CommitResponse { success, error, .. } => {
                if success {
                    Ok(())
                } else {
                    Err(EvalError::LocalDispatchFailed(
                        error.unwrap_or_else(|| "Participant commit failed".to_string()),
                    ))
                }
            }
            MeerkatMessage::Ping { .. }
            | MeerkatMessage::Pong { .. }
            | MeerkatMessage::Announce { .. }
            | MeerkatMessage::Transaction { .. }
            | MeerkatMessage::Propagation { .. }
            | MeerkatMessage::LookupRequest { .. }
            | MeerkatMessage::LookupResponse { .. }
            | MeerkatMessage::LookupError { .. }
            | MeerkatMessage::ActionRequest { .. }
            | MeerkatMessage::ActionResponse { .. }
            | MeerkatMessage::Commit { .. }
            | MeerkatMessage::Abort { .. }
            | MeerkatMessage::AbortResponse { .. }
            | MeerkatMessage::LockRequest { .. }
            | MeerkatMessage::LockResponse { .. }
            | MeerkatMessage::RequestUpdates { .. }
            | MeerkatMessage::Update { .. }
            | MeerkatMessage::ServiceCodeRequest { .. }
            | MeerkatMessage::ServiceCodeResponse { .. }
            | MeerkatMessage::ServiceCodeError { .. }
            | MeerkatMessage::UpdateServiceRequest { .. }
            | MeerkatMessage::UpdateServiceResponse { .. }
            | MeerkatMessage::WaitParked { .. } => Err(EvalError::LocalDispatchFailed(
                "Unexpected reply to commit".to_string(),
            )),
        }
    }

    /// Originator side: tell a participant to abort, awaiting acknowledgement
    /// so its locks are released before we return (and the process may exit).
    async fn send_abort(&mut self, addr: Address, tid: &TxnId) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_ABORT_ID: AtomicU64 = AtomicU64::new(1);
        let request_id = NEXT_ABORT_ID.fetch_add(1, Ordering::SeqCst);
        let reply_to = self.local_reply_addr().await;
        let msg = MeerkatMessage::Abort {
            request_id,
            txn_id: tid.clone(),
            reply_to,
        };
        // We await the ack so that in the normal case the participant's locks
        // are released before we return. If the ack times out the participant
        // may still hold locks; durable abort retries and error reporting are
        // tracked under issue #54.
        let _ = self
            .send_and_await_reply(
                addr,
                msg,
                request_id,
                "Timeout waiting for abort acknowledgement".to_string(),
            )
            .await;
    }

    pub async fn execute_action(
        &mut self,
        service_name: Symbol,
        stmts: &[ActionStmt],
    ) -> Result<(), EvalError> {
        self.execute_action_with_txn(service_name, stmts, &[]).await
    }

    pub async fn execute_action_with_env(
        &mut self,
        service_name: Symbol,
        stmts: &[ActionStmt],
        initial_env: &[(Symbol, Value)],
    ) -> Result<(), EvalError> {
        self.execute_action_with_txn(service_name, stmts, initial_env)
            .await
    }

    /// Execute a `@test` block's statements outside any transaction
    ///
    /// The block as a whole is not a transaction. Each statement runs against
    /// committed state, so reads see the latest values and assignments apply
    /// and propagate immediately rather than being buffered until the end of
    /// the block. Only `do` statements are transactional, and each one opens a
    /// fresh transaction of its own (see `executor::execute`).
    ///
    /// The first statement to fail ends the block. Because there is no
    /// enclosing transaction, the effects of the statements that already ran
    /// -- including any `do` that already committed -- stay in place.
    pub async fn execute_test_block(
        &mut self,
        service_name: Symbol,
        stmts: &[ActionStmt],
    ) -> Result<(), EvalError> {
        let mut env: Vec<(Symbol, Value)> = Vec::new();
        execute_seq(stmts, &mut env, self, service_name, None).await
    }
}

impl Default for Manager {
    fn default() -> Self {
        Self::new(Interner::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Decl, Expr, Value};
    use crate::runtime::interpreter::execute;
    use crate::runtime::{parser::parse_string, update::Transaction as CodeUpdate, Node};

    // #24: cross_service_deps pulls out exactly the (service, member) symbols
    // referenced via MemberAccess, and nothing for a purely local expression.
    #[test]
    fn test_cross_service_deps_extraction() {
        let tc = TestContext::new();
        // z = s1.y + 2  ->  {(s1, y)}
        let z_expr = Expr::Binop {
            op: crate::ast::BinOp::Add,
            expr1: Box::new(Expr::MemberAccess {
                service_name: tc.s1,
                member_name: tc.y,
            }),
            expr2: Box::new(Expr::Literal {
                val: Value::Int { val: 2 },
            }),
        };
        assert_eq!(
            cross_service_deps(&z_expr),
            std::collections::HashSet::from([(tc.s1, tc.y)])
        );
        // y = x + 1  ->  {} (no cross-service references)
        let y_expr = Expr::Binop {
            op: crate::ast::BinOp::Add,
            expr1: Box::new(Expr::Variable { name: tc.x }),
            expr2: Box::new(Expr::Literal {
                val: Value::Int { val: 1 },
            }),
        };
        assert!(cross_service_deps(&y_expr).is_empty());
    }

    // #24: a def in s2 that reads s1.y updates eagerly when s1.x changes,
    // driven by the listener cascade rather than a lazy re-lookup.
    #[tokio::test]
    async fn test_cross_service_def_updates_eagerly() {
        let mut tc = TestContext::new();
        let z = tc.manager.interner.insert("z");

        // service s1 { var x = 1; pub def y = x + 1; }
        let s1_decls = vec![
            Decl::VarDecl {
                name: tc.x,
                ty: None,
                val: Expr::Literal {
                    val: Value::Int { val: 1 },
                },
            },
            Decl::DefDecl {
                name: tc.y,
                ty: None,
                val: Expr::Binop {
                    op: crate::ast::BinOp::Add,
                    expr1: Box::new(Expr::Variable { name: tc.x }),
                    expr2: Box::new(Expr::Literal {
                        val: Value::Int { val: 1 },
                    }),
                },
                is_pub: true,
            },
        ];
        tc.manager.create_service(tc.s1, s1_decls).await.unwrap();

        // service s2 { pub def z = s1.y + 2; }
        let s2_decls = vec![Decl::DefDecl {
            name: z,
            ty: None,
            val: Expr::Binop {
                op: crate::ast::BinOp::Add,
                expr1: Box::new(Expr::MemberAccess {
                    service_name: tc.s1,
                    member_name: tc.y,
                }),
                expr2: Box::new(Expr::Literal {
                    val: Value::Int { val: 2 },
                }),
            },
            is_pub: true,
        }];
        tc.manager.create_service(tc.s2, s2_decls).await.unwrap();

        // initial z = (1 + 1) + 2 = 4
        assert_eq!(
            tc.manager
                .services
                .get(&tc.s2)
                .unwrap()
                .vars
                .get(&z)
                .unwrap()
                .value,
            Value::Int { val: 4 }
        );

        // s2.z is registered as a listener on s1.y
        let on_y = tc
            .manager
            .services
            .get(&tc.s1)
            .unwrap()
            .listeners
            .get(&tc.y)
            .cloned()
            .unwrap_or_default();
        assert!(
            on_y.iter().any(|(_, d)| *d == z),
            "s2.z should be registered as a listener on s1.y"
        );

        // s1.x = 4  ->  s1.y = 5  ->  s2.z = 7, eagerly via the cascade
        tc.manager
            .assign(tc.s1, tc.x, Value::Int { val: 4 }, None)
            .await
            .unwrap();

        assert_eq!(
            tc.manager
                .services
                .get(&tc.s2)
                .unwrap()
                .vars
                .get(&z)
                .unwrap()
                .value,
            Value::Int { val: 7 },
            "s2.z should update eagerly through the cross-service listener cascade"
        );
    }

    // #24: handle_update caches a pushed remote value and recomputes the
    // dependent def FROM THE CACHE, not from a fresh lookup of the local value.
    #[tokio::test]
    async fn test_handle_update_recomputes_from_cache() {
        let mut tc = TestContext::new();
        let z = tc.manager.interner.insert("z");

        let s1_decls = vec![
            Decl::VarDecl {
                name: tc.x,
                ty: None,
                val: Expr::Literal {
                    val: Value::Int { val: 1 },
                },
            },
            Decl::DefDecl {
                name: tc.y,
                ty: None,
                val: Expr::Binop {
                    op: crate::ast::BinOp::Add,
                    expr1: Box::new(Expr::Variable { name: tc.x }),
                    expr2: Box::new(Expr::Literal {
                        val: Value::Int { val: 1 },
                    }),
                },
                is_pub: true,
            },
        ];
        tc.manager.create_service(tc.s1, s1_decls).await.unwrap();

        let s2_decls = vec![Decl::DefDecl {
            name: z,
            ty: None,
            val: Expr::Binop {
                op: crate::ast::BinOp::Add,
                expr1: Box::new(Expr::MemberAccess {
                    service_name: tc.s1,
                    member_name: tc.y,
                }),
                expr2: Box::new(Expr::Literal {
                    val: Value::Int { val: 2 },
                }),
            },
            is_pub: true,
        }];
        tc.manager.create_service(tc.s2, s2_decls).await.unwrap();

        let s2_id = tc.manager.services.get(&tc.s2).unwrap().id.0.clone();
        let net_val = codec::encode_value(&Value::Int { val: 10 }, &tc.manager.interner).unwrap();

        // simulate a remote Update saying s1.y = 10, stamped with s1.y's clock.
        // z's only input is this cross-service member, so the gate joins a single
        // clock and passes trivially regardless of its contents.
        let z_sym = tc.manager.interner.insert("z");
        let clock: VClock = HashMap::from([((tc.s1, tc.y), 1u64)]);
        let sources = tc.manager.services[&tc.s1].vars[&tc.y].sources.clone();
        tc.manager
            .handle_update(
                ServiceNetId(s2_id),
                z_sym,
                (tc.s1, tc.y),
                net_val,
                clock,
                sources,
            )
            .await;

        // recomputed from the cached 10 (not s1's local y of 2): 10 + 2 = 12
        assert_eq!(
            tc.manager
                .services
                .get(&tc.s2)
                .unwrap()
                .vars
                .get(&z)
                .unwrap()
                .value,
            Value::Int { val: 12 }
        );
    }

    // #24: handle_request_updates registers a remote listener and records its
    // reply address (the initial Update send is a no-op without a network).
    #[tokio::test]
    async fn test_handle_request_updates_registers_listener() {
        let mut tc = TestContext::new();
        let s1_decls = vec![
            Decl::VarDecl {
                name: tc.x,
                ty: None,
                val: Expr::Literal {
                    val: Value::Int { val: 1 },
                },
            },
            Decl::DefDecl {
                name: tc.y,
                ty: None,
                val: Expr::Binop {
                    op: crate::ast::BinOp::Add,
                    expr1: Box::new(Expr::Variable { name: tc.x }),
                    expr2: Box::new(Expr::Literal {
                        val: Value::Int { val: 1 },
                    }),
                },
                is_pub: true,
            },
        ];
        tc.manager.create_service(tc.s1, s1_decls).await.unwrap();

        let z = tc.manager.interner.insert("z");
        tc.manager
            .handle_request_updates(
                tc.s1,
                tc.y,
                ServiceNetId("remote-s2-id".to_string()),
                z,
                "/ip4/1.2.3.4/tcp/9".to_string(),
            )
            .await;
        let on_y = tc
            .manager
            .services
            .get(&tc.s1)
            .unwrap()
            .listeners
            .get(&tc.y)
            .cloned()
            .unwrap_or_default();
        assert!(
            on_y.iter().any(|(id, d)| id.0 == "remote-s2-id" && *d == z),
            "remote s2.z should be registered as a listener on s1.y"
        );
        assert_eq!(
            tc.manager
                .listener_addrs
                .get(&ServiceNetId("remote-s2-id".to_string()))
                .map(|a| a.as_str()),
            Some("/ip4/1.2.3.4/tcp/9")
        );
    }
    struct TestContext {
        manager: Manager,
        foo: Symbol,
        x: Symbol,
        y: Symbol,
        f: Symbol,
        s1: Symbol,
        s2: Symbol,
        w: Symbol,
        bump: Symbol,
        nonexistent: Symbol,
    }

    impl TestContext {
        fn new() -> Self {
            let mut manager = Manager::default();
            let foo = manager.interner.insert("foo");
            let x = manager.interner.insert("x");
            let y = manager.interner.insert("y");
            let f = manager.interner.insert("f");
            let s1 = manager.interner.insert("s1");
            let s2 = manager.interner.insert("s2");
            let w = manager.interner.insert("w");
            let bump = manager.interner.insert("bump");
            let nonexistent = manager.interner.insert("nonexistent");
            Self {
                manager,
                foo,
                x,
                y,
                f,
                s1,
                s2,
                w,
                bump,
                nonexistent,
            }
        }
    }

    #[tokio::test]
    async fn test_create_service_with_var() {
        let mut tc = TestContext::new();
        let decls = vec![Decl::VarDecl {
            name: tc.x,
            ty: None,
            val: Expr::Literal {
                val: Value::Int { val: 1 },
            },
        }];
        tc.manager.create_service(tc.foo, decls).await.unwrap();
        let result = tc.manager.lookup(tc.x, tc.foo, None).await.unwrap();
        assert_eq!(result, Value::Int { val: 1 });
    }

    #[tokio::test]
    async fn test_create_service_with_def() {
        let mut tc = TestContext::new();
        let decls = vec![
            Decl::VarDecl {
                name: tc.x,
                ty: None,
                val: Expr::Literal {
                    val: Value::Int { val: 2 },
                },
            },
            Decl::DefDecl {
                name: tc.f,
                ty: None,
                val: Expr::Binop {
                    op: crate::ast::BinOp::Add,
                    expr1: Box::new(Expr::Variable { name: tc.x }),
                    expr2: Box::new(Expr::Literal {
                        val: Value::Int { val: 3 },
                    }),
                },
                is_pub: true,
            },
        ];
        tc.manager.create_service(tc.foo, decls).await.unwrap();
        let result = tc.manager.lookup(tc.f, tc.foo, None).await.unwrap();
        assert_eq!(result, Value::Int { val: 5 });
    }

    /// Recomputing a def must leave the inputs of an evaluation already in
    /// progress as they were.
    ///
    /// A checked evaluation reads members only from the fixed, private map of
    /// paired inputs it was given, so a recompute nested in the middle of an
    /// outer one cannot change what the outer evaluation goes on to read.
    #[tokio::test]
    async fn test_recompute_def_restores_an_active_reactive_cache() {
        let mut tc = TestContext::new();
        let decls = vec![
            Decl::VarDecl {
                name: tc.x,
                ty: None,
                val: Expr::Literal {
                    val: Value::Int { val: 2 },
                },
            },
            Decl::DefDecl {
                name: tc.y,
                ty: None,
                val: Expr::Binop {
                    op: crate::ast::BinOp::Add,
                    expr1: Box::new(Expr::Variable { name: tc.x }),
                    expr2: Box::new(Expr::Literal {
                        val: Value::Int { val: 1 },
                    }),
                },
                is_pub: true,
            },
        ];
        tc.manager.create_service(tc.foo, decls).await.unwrap();

        // Stand in for an outer recompute with its own cross-service dep
        // among its fixed inputs.
        let outer: HashMap<(Symbol, Symbol), Option<Value>> =
            [((tc.s1, tc.w), Some(Value::Int { val: 7 }))]
                .into_iter()
                .collect();
        let read_w = Expr::MemberAccess {
            service_name: tc.s1,
            member_name: tc.w,
        };
        let read = async |manager: &mut Manager| {
            super::super::interpreter::evaluator::eval_with_inputs(
                &read_w,
                &[],
                &mut EvalContext {
                    manager,
                    service_name: tc.foo,
                    txn: None,
                },
                Some(&outer),
            )
            .await
            .unwrap()
        };
        let before = read(&mut tc.manager).await;

        tc.manager.recompute_def(tc.foo, tc.y).await;

        assert_eq!(
            (before, read(&mut tc.manager).await),
            (Value::Int { val: 7 }, Value::Int { val: 7 }),
            "a nested recompute must leave the outer inputs untouched"
        );
    }

    /// Build `service <name> { var x = <val>; }`.
    fn service_stmt(interner: &mut Interner, name: &str, val: i32) -> Stmt {
        Stmt::Service {
            name: interner.insert(name),
            decls: vec![Decl::VarDecl {
                name: interner.insert("x"),
                ty: None,
                val: Expr::Literal {
                    val: Value::Int { val },
                },
            }],
        }
    }

    /// `local_service_names` reports declarations and nothing else.
    #[test]
    fn local_service_names_covers_declarations_only() {
        let mut interner = Interner::new();
        let app = interner.insert("app");
        let dep = interner.insert("dep");
        let prog = vec![
            Stmt::Import {
                path: "dep.mkt".to_string(),
                service_name: dep,
            },
            service_stmt(&mut interner, "app", 0),
        ];

        let names = local_service_names(&prog);

        assert!(names.contains(&app));
        assert!(
            !names.contains(&dep),
            "an `import` is a reference, not a declaration: counting it would \
             suppress the `-i` flag that resolves it"
        );
    }

    /// An `-i` flag naming a service this program declares must be refused.
    ///
    /// `lookup` consults `remote_services` before anything local, so
    /// registering it would route the locally declared service's reads and
    /// writes to a peer and leave the local copy permanently unreachable.
    #[test]
    fn register_remote_services_skips_locally_declared_services() {
        let mut manager = Manager::default();
        let prog = vec![service_stmt(&mut manager.interner, "s2", 0)];
        let local = local_service_names(&prog);
        let mut urls = HashMap::new();
        urls.insert("s2".to_string(), "/ip4/127.0.0.1/tcp/9000".to_string());
        urls.insert("s9".to_string(), "/ip4/127.0.0.1/tcp/9001".to_string());

        manager.register_remote_services(&urls, &local);

        let s2 = manager.interner.insert("s2");
        let s9 = manager.interner.insert("s9");
        assert!(
            !manager.remote_services.contains_key(&s2),
            "a locally declared service must never be registered as remote"
        );
        assert!(
            manager.remote_services.contains_key(&s9),
            "an unrelated -i flag must still be honoured"
        );
    }

    /// Imports are instantiated from `unified_ast`, before the program's own
    /// services and in dependency order.
    #[tokio::test]
    async fn register_and_instantiate_imports_builds_imported_services() {
        let mut manager = Manager::default();
        let dep = service_stmt(&mut manager.interner, "dep", 7);
        let app = service_stmt(&mut manager.interner, "app", 1);
        let prog = vec![app.clone()];
        manager.install_unified_ast(vec![dep.clone(), app]);

        manager
            .register_and_instantiate_imports(&[dep], &prog, &HashMap::new())
            .await
            .expect("imports instantiate");

        let dep_sym = manager.interner.insert("dep");
        let app_sym = manager.interner.insert("app");
        assert!(
            manager.services.contains_key(&dep_sym),
            "a locally resolved import must be instantiated"
        );
        assert!(
            !manager.services.contains_key(&app_sym),
            "the program's own services are created by the caller, in program order"
        );
    }

    /// An unrelated `-i` flag must not stop local imports being instantiated.
    ///
    /// `run_server` used to be handed `prog` instead of the unified AST
    /// whenever any `-i` flag was present, which silently dropped every
    /// locally resolved import in exactly the configuration that mixes local
    /// and remote services.
    #[tokio::test]
    async fn register_and_instantiate_imports_survives_an_unrelated_remote_flag() {
        let mut manager = Manager::default();
        let dep = service_stmt(&mut manager.interner, "dep", 7);
        let app = service_stmt(&mut manager.interner, "app", 1);
        let prog = vec![app.clone()];
        manager.install_unified_ast(vec![dep.clone(), app]);
        let mut urls = HashMap::new();
        urls.insert("far".to_string(), "/ip4/127.0.0.1/tcp/9000".to_string());

        manager
            .register_and_instantiate_imports(&[dep], &prog, &urls)
            .await
            .expect("imports instantiate");

        let dep_sym = manager.interner.insert("dep");
        assert!(
            manager.services.contains_key(&dep_sym),
            "an unrelated -i flag must not suppress local imports"
        );
    }

    /// A service resolved remotely must not also be built locally.
    ///
    /// It appears in `unified_ast` because its source was fetched for type
    /// checking, but the owning node serves it; a local copy would shadow the
    /// peer, since `lookup` prefers `remote_services`.
    #[tokio::test]
    async fn register_and_instantiate_imports_skips_remote_services() {
        let mut manager = Manager::default();
        let dep = service_stmt(&mut manager.interner, "dep", 7);
        let app = service_stmt(&mut manager.interner, "app", 1);
        let prog = vec![app.clone()];
        manager.install_unified_ast(vec![dep.clone(), app]);
        let mut urls = HashMap::new();
        urls.insert("dep".to_string(), "/ip4/127.0.0.1/tcp/9000".to_string());

        manager
            .register_and_instantiate_imports(&[dep], &prog, &urls)
            .await
            .expect("imports instantiate");

        let dep_sym = manager.interner.insert("dep");
        assert!(
            manager.remote_services.contains_key(&dep_sym),
            "the -i flag must be honoured"
        );
        assert!(
            !manager.services.contains_key(&dep_sym),
            "a remotely served service must not be built as a local phantom copy"
        );
    }

    #[tokio::test]
    async fn test_lookup_missing_var_returns_error() {
        let mut tc = TestContext::new();
        tc.manager.create_service(tc.foo, vec![]).await.unwrap();
        let result = tc.manager.lookup(tc.nonexistent, tc.foo, None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_def_updates_after_var_change() {
        let mut tc = TestContext::new();
        let decls = vec![
            Decl::VarDecl {
                name: tc.x,
                ty: None,
                val: Expr::Literal {
                    val: Value::Int { val: 1 },
                },
            },
            Decl::DefDecl {
                name: tc.f,
                ty: None,
                val: Expr::Binop {
                    op: crate::ast::BinOp::Add,
                    expr1: Box::new(Expr::Variable { name: tc.x }),
                    expr2: Box::new(Expr::Literal {
                        val: Value::Int { val: 10 },
                    }),
                },
                is_pub: true,
            },
        ];
        tc.manager.create_service(tc.foo, decls).await.unwrap();

        // f should be 11 initially
        let result = tc.manager.lookup(tc.f, tc.foo, None).await.unwrap();
        assert_eq!(result, Value::Int { val: 11 });

        // update x to 5, f should become 15
        tc.manager
            .assign(tc.foo, tc.x, Value::Int { val: 5 }, None)
            .await
            .unwrap();
        let result = tc.manager.lookup(tc.f, tc.foo, None).await.unwrap();
        assert_eq!(result, Value::Int { val: 15 });
    }

    // ---- vector-clock local-diamond tests ----

    fn vint(n: i32) -> Value {
        Value::Int { val: n }
    }
    fn lit_int(n: i32) -> Expr {
        Expr::Literal { val: vint(n) }
    }
    fn mk_add(e1: Expr, e2: Expr) -> Expr {
        Expr::Binop {
            op: crate::ast::BinOp::Add,
            expr1: Box::new(e1),
            expr2: Box::new(e2),
        }
    }

    // Local diamond in one service:
    //   var x = 1
    //   def a = x + 10
    //   def b = x + 20
    //   def q = a + b
    // A single write to x must reach the sink q as one consistent generation.
    // With the glitch gate, q recomputes only once a and b agree on x's clock,
    // so q never observes a mixed (old a, new b) cut — a glitch would surface as
    // q == 33 (11+22 or 12+21). We check both the final value and that every node
    // on the frontier carries the same vector clock.
    #[tokio::test]
    async fn test_local_diamond_glitch_free() {
        let mut tc = TestContext::new();
        let a = tc.manager.interner.insert("a");
        let b = tc.manager.interner.insert("b");
        let q = tc.manager.interner.insert("q");

        let decls = vec![
            Decl::VarDecl {
                name: tc.x,
                ty: None,
                val: lit_int(1),
            },
            Decl::DefDecl {
                name: a,
                ty: None,
                val: mk_add(Expr::Variable { name: tc.x }, lit_int(10)),
                is_pub: true,
            },
            Decl::DefDecl {
                name: b,
                ty: None,
                val: mk_add(Expr::Variable { name: tc.x }, lit_int(20)),
                is_pub: true,
            },
            Decl::DefDecl {
                name: q,
                ty: None,
                val: mk_add(Expr::Variable { name: a }, Expr::Variable { name: b }),
                is_pub: true,
            },
        ];
        tc.manager.create_service(tc.foo, decls).await.unwrap();

        // initial: a = 11, b = 21, q = 32
        assert_eq!(tc.manager.lookup(q, tc.foo, None).await.unwrap(), vint(32));

        // write x = 2  ->  a = 12, b = 22, q = 34
        tc.manager
            .assign(tc.foo, tc.x, vint(2), None)
            .await
            .unwrap();
        assert_eq!(
            tc.manager.lookup(q, tc.foo, None).await.unwrap(),
            vint(34),
            "sink must reflect the fully-updated generation, never a glitch (33)"
        );

        // one write to x => generation 1 on dimension (foo, x). Every node on the
        // frontier — the two intermediates and the sink — must agree on it.
        let foo = tc.manager.services.get(&tc.foo).unwrap();
        let frontier: VClock = HashMap::from([((tc.foo, tc.x), 1u64)]);
        assert_eq!(
            foo.vars.get(&tc.x).unwrap().vector_clock,
            frontier,
            "x bumped to generation 1"
        );
        assert_eq!(
            foo.vars.get(&a).unwrap().vector_clock,
            frontier,
            "a joined x's clock"
        );
        assert_eq!(
            foo.vars.get(&b).unwrap().vector_clock,
            frontier,
            "b joined x's clock"
        );
        assert_eq!(
            foo.vars.get(&q).unwrap().vector_clock,
            frontier,
            "sink q carries the joined frontier"
        );
    }

    // Overwrite a node's stored value and clock directly, to stage an
    // inconsistent frontier the synchronous cascade would never leave behind.
    fn stage_node(mgr: &mut Manager, svc: Symbol, name: Symbol, value: Value, clock: VClock) {
        let vs = mgr
            .services
            .get_mut(&svc)
            .unwrap()
            .vars
            .get_mut(&name)
            .unwrap();
        vs.value = value;
        vs.vector_clock = clock;
    }

    // Targeted defer-path test. The synchronous cascade always finishes in a
    // consistent state, so to hit the gate's DEFER branch directly we stage the
    // frontier by hand:
    //   var x = 1;  def a = x + 10;  def q = a + x;
    // After settling at generation 1, we bump x to generation 2 with a NEW value
    // but leave a at generation 1 (as if a's recompute hasn't happened yet).
    // q's inputs now disagree on x's dimension, so recompute_def(q) must defer —
    // otherwise q would glitch to a + x = 12 + 5 = 17. Once a catches up to
    // generation 2, the gate is satisfied and q recomputes.
    #[tokio::test]
    async fn test_gate_defers_on_stale_input() {
        let mut tc = TestContext::new();
        let a = tc.manager.interner.insert("a");
        let q = tc.manager.interner.insert("q");

        let decls = vec![
            Decl::VarDecl {
                name: tc.x,
                ty: None,
                val: lit_int(1),
            },
            Decl::DefDecl {
                name: a,
                ty: None,
                val: mk_add(Expr::Variable { name: tc.x }, lit_int(10)),
                is_pub: true,
            },
            Decl::DefDecl {
                name: q,
                ty: None,
                val: mk_add(Expr::Variable { name: a }, Expr::Variable { name: tc.x }),
                is_pub: true,
            },
        ];
        tc.manager.create_service(tc.foo, decls).await.unwrap();

        let gen1: VClock = HashMap::from([((tc.foo, tc.x), 1u64)]);
        let gen2: VClock = HashMap::from([((tc.foo, tc.x), 2u64)]);

        // settle at generation 1: x = 2, a = 12, q = 14
        tc.manager
            .assign(tc.foo, tc.x, vint(2), None)
            .await
            .unwrap();
        assert_eq!(tc.manager.lookup(q, tc.foo, None).await.unwrap(), vint(14));

        // stage the inconsistency: x jumps to generation 2 with a new value,
        // while a is left behind at generation 1 with its old value.
        stage_node(&mut tc.manager, tc.foo, tc.x, vint(5), gen2.clone());

        // gate must DEFER: a (gen 1) lags x (gen 2) on dimension (foo, x).
        let changed = tc.manager.recompute_def(tc.foo, q).await;
        assert!(
            changed.is_empty(),
            "gate should defer while a is stale relative to x"
        );
        {
            let foo = tc.manager.services.get(&tc.foo).unwrap();
            assert_eq!(
                foo.vars.get(&q).unwrap().value,
                vint(14),
                "q must keep its consistent value, not glitch to 17"
            );
            assert_eq!(
                foo.vars.get(&q).unwrap().vector_clock,
                gen1,
                "q's clock is unchanged while deferred"
            );
        }

        // a catches up to generation 2; now the gate passes and q recomputes.
        stage_node(&mut tc.manager, tc.foo, a, vint(15), gen2.clone());
        let changed = tc.manager.recompute_def(tc.foo, q).await;
        assert!(
            !changed.is_empty(),
            "gate should pass once a reaches generation 2"
        );
        {
            let foo = tc.manager.services.get(&tc.foo).unwrap();
            assert_eq!(
                foo.vars.get(&q).unwrap().value,
                vint(20),
                "q = a + x = 15 + 5"
            );
            assert_eq!(
                foo.vars.get(&q).unwrap().vector_clock,
                gen2,
                "q joins the generation-2 frontier"
            );
        }
    }

    // ---- vector-clock remote-diamond tests (the M2 wire path) ----

    // Simulate a remote `Update` for `source.member` arriving at `listener_net_id`
    // with the given value and clock, driving the same code path the network
    // dispatcher uses. Interning is already done (these are runtime `Symbol`s),
    // so this bypasses the wire codec and exercises `handle_update` directly.
    async fn deliver_update(
        mgr: &mut Manager,
        listener_net_id: &str,
        listener_def: Symbol,
        source: Symbol,
        member: Symbol,
        value: Value,
        clock: VClock,
    ) {
        let net_val = codec::encode_value(&value, &mgr.interner).unwrap();
        let sources = mgr.services[&source].vars[&member].sources.clone();
        mgr.handle_update(
            ServiceNetId(listener_net_id.to_string()),
            listener_def,
            (source, member),
            net_val,
            clock,
            sources,
        )
        .await;
    }

    // Build `s2` whose only def is the remote-diamond sink `z = s1.a + s1.b`,
    // and return its network id. `s1` derives `a = w + 10` and `b = w + 20`,
    // so `w` is the source both arms share; their local values (10, 20) are
    // sentinels so that if `z` ever computed from a local fallback instead of
    // its delivered dep_cache, the result would be visibly wrong rather than
    // coincidentally correct.
    async fn setup_remote_diamond(tc: &mut TestContext, a: Symbol, b: Symbol, z: Symbol) -> String {
        let w = || Expr::Variable { name: tc.w };
        let s1_decls = vec![
            Decl::VarDecl {
                name: tc.w,
                ty: None,
                val: lit_int(0),
            },
            Decl::DefDecl {
                name: a,
                ty: None,
                val: mk_add(w(), lit_int(10)),
                is_pub: true,
            },
            Decl::DefDecl {
                name: b,
                ty: None,
                val: mk_add(w(), lit_int(20)),
                is_pub: true,
            },
        ];
        tc.manager.create_service(tc.s1, s1_decls).await.unwrap();

        let s2_decls = vec![Decl::DefDecl {
            name: z,
            ty: None,
            val: mk_add(
                Expr::MemberAccess {
                    service_name: tc.s1,
                    member_name: a,
                },
                Expr::MemberAccess {
                    service_name: tc.s1,
                    member_name: b,
                },
            ),
            is_pub: true,
        }];
        tc.manager.create_service(tc.s2, s2_decls).await.unwrap();
        tc.manager.services.get(&tc.s2).unwrap().id.0.clone()
    }

    // Remote diamond across services, glitch-free (gate SUCCEEDS).
    //   s1 owns source w and two derived members a, b (both functions of w).
    //   s2:  def z = s1.a + s1.b
    // z subscribes to both remote members. When a and b arrive stamped with
    // clocks that AGREE on w's dimension, the gate joins them and z recomputes to
    // the consistent sum. We settle a whole generation (a=11, b=21 => 32), then a
    // second (a=12, b=22 => 34). A consistent frontier must never be spuriously
    // deferred — this is the no-false-positive / no-deadlock direction.
    #[tokio::test]
    async fn test_remote_diamond_glitch_free() {
        let mut tc = TestContext::new();
        let a = tc.manager.interner.insert("a");
        let b = tc.manager.interner.insert("b");
        let z = tc.manager.interner.insert("z");
        let s2_id = setup_remote_diamond(&mut tc, a, b, z).await;

        // generation 1: both arms carry (s1,w):1, plus their own dimension. z
        // cannot compute until both are cached, so it settles on the 2nd arrival.
        let clk_a1: VClock = HashMap::from([((tc.s1, tc.w), 1u64), ((tc.s1, a), 1u64)]);
        let clk_b1: VClock = HashMap::from([((tc.s1, tc.w), 1u64), ((tc.s1, b), 1u64)]);
        deliver_update(&mut tc.manager, &s2_id, z, tc.s1, a, vint(11), clk_a1).await;
        deliver_update(&mut tc.manager, &s2_id, z, tc.s1, b, vint(21), clk_b1).await;

        let z_gen1: VClock = HashMap::from([
            ((tc.s1, tc.w), 1u64),
            ((tc.s1, a), 1u64),
            ((tc.s1, b), 1u64),
        ]);
        {
            let vs = tc
                .manager
                .services
                .get(&tc.s2)
                .unwrap()
                .vars
                .get(&z)
                .unwrap();
            assert_eq!(
                vs.value,
                vint(32),
                "z reflects the consistent gen-1 cut (11 + 21)"
            );
            assert_eq!(
                vs.vector_clock, z_gen1,
                "z carries the joined gen-1 frontier"
            );
        }

        // generation 2: advance both arms consistently on (s1,w):2. The lone
        // a@gen-2 arrival transiently disagrees with the still-cached b@gen-1 and
        // is deferred by the gate; only once b@gen-2 lands does z advance.
        let clk_a2: VClock = HashMap::from([((tc.s1, tc.w), 2u64), ((tc.s1, a), 2u64)]);
        let clk_b2: VClock = HashMap::from([((tc.s1, tc.w), 2u64), ((tc.s1, b), 2u64)]);
        deliver_update(&mut tc.manager, &s2_id, z, tc.s1, a, vint(12), clk_a2).await;
        deliver_update(&mut tc.manager, &s2_id, z, tc.s1, b, vint(22), clk_b2).await;

        let z_gen2: VClock = HashMap::from([
            ((tc.s1, tc.w), 2u64),
            ((tc.s1, a), 2u64),
            ((tc.s1, b), 2u64),
        ]);
        let vs = tc
            .manager
            .services
            .get(&tc.s2)
            .unwrap()
            .vars
            .get(&z)
            .unwrap();
        assert_eq!(
            vs.value,
            vint(34),
            "z reflects the consistent gen-2 cut (12 + 22), never a glitch (33)"
        );
        assert_eq!(
            vs.vector_clock, z_gen2,
            "z carries the joined gen-2 frontier"
        );
    }

    // Remote diamond where one arm runs ahead (gate DEFERS).
    // Same shape: s2.z = s1.a + s1.b. After settling gen-1 (z = 32), a jumps to
    // generation 2 while b is still cached at generation 1. z's inputs now
    // disagree on w's dimension, so recompute_def(z) must defer — otherwise z
    // would glitch to a(gen2) + b(gen1) = 12 + 21 = 33. Once b catches up to
    // generation 2, the gate is satisfied and z recomputes to 34.
    #[tokio::test]
    async fn test_remote_diamond_defers_on_stale_arm() {
        let mut tc = TestContext::new();
        let a = tc.manager.interner.insert("a");
        let b = tc.manager.interner.insert("b");
        let z = tc.manager.interner.insert("z");
        let s2_id = setup_remote_diamond(&mut tc, a, b, z).await;

        // settle a consistent generation-1 frontier: z = 11 + 21 = 32.
        let clk_a1: VClock = HashMap::from([((tc.s1, tc.w), 1u64), ((tc.s1, a), 1u64)]);
        let clk_b1: VClock = HashMap::from([((tc.s1, tc.w), 1u64), ((tc.s1, b), 1u64)]);
        deliver_update(&mut tc.manager, &s2_id, z, tc.s1, a, vint(11), clk_a1).await;
        deliver_update(&mut tc.manager, &s2_id, z, tc.s1, b, vint(21), clk_b1).await;

        let z_gen1: VClock = HashMap::from([
            ((tc.s1, tc.w), 1u64),
            ((tc.s1, a), 1u64),
            ((tc.s1, b), 1u64),
        ]);
        assert_eq!(
            tc.manager
                .services
                .get(&tc.s2)
                .unwrap()
                .vars
                .get(&z)
                .unwrap()
                .value,
            vint(32),
            "baseline: consistent gen-1 cut"
        );

        // a runs ahead to generation 2 (new value + (s1,w):2), b stays at gen-1.
        let clk_a2: VClock = HashMap::from([((tc.s1, tc.w), 2u64), ((tc.s1, a), 2u64)]);
        deliver_update(&mut tc.manager, &s2_id, z, tc.s1, a, vint(12), clk_a2).await;

        // gate must DEFER: b (gen-1) lags a (gen-2) on dimension (s1,w). z keeps
        // its consistent value and clock rather than glitching to 12 + 21 = 33.
        {
            let vs = tc
                .manager
                .services
                .get(&tc.s2)
                .unwrap()
                .vars
                .get(&z)
                .unwrap();
            assert_eq!(
                vs.value,
                vint(32),
                "z must not glitch to 33 while b is stale"
            );
            assert_eq!(
                vs.vector_clock, z_gen1,
                "z's clock is unchanged while deferred"
            );
        }

        // b catches up to generation 2; the gate now passes and z recomputes.
        let clk_b2: VClock = HashMap::from([((tc.s1, tc.w), 2u64), ((tc.s1, b), 2u64)]);
        deliver_update(&mut tc.manager, &s2_id, z, tc.s1, b, vint(22), clk_b2).await;

        let z_gen2: VClock = HashMap::from([
            ((tc.s1, tc.w), 2u64),
            ((tc.s1, a), 2u64),
            ((tc.s1, b), 2u64),
        ]);
        let vs = tc
            .manager
            .services
            .get(&tc.s2)
            .unwrap()
            .vars
            .get(&z)
            .unwrap();
        assert_eq!(
            vs.value,
            vint(34),
            "z = a + b = 12 + 22 once the frontier is consistent"
        );
        assert_eq!(vs.vector_clock, z_gen2, "z joins the generation-2 frontier");
    }

    // ---- vector-clock cross-node test (real codec + serde_json round-trip) ----

    // Ship one remote `Update` from `src` to `dst` the way the network path
    // actually does it: build the message with `src`'s interner (as
    // `emit_update` does), serialize/deserialize it through serde_json (the wire
    // format from `protocol.rs`), then decode + intern it into `dst`'s interner
    // (as the dispatcher does) and hand it to `dst.handle_update`. The symbols
    // are re-derived from strings on the far side, so `src` and `dst` need not
    // share a symbol space.
    // The payload of one such update, entirely in `src`'s symbol space.
    struct WireUpdate {
        listener_def: Symbol,
        source: Symbol,
        member: Symbol,
        value: Value,
        clock: VClock,
        sources: Sources,
    }

    async fn send_update_over_wire(
        src: &Manager,
        dst: &mut Manager,
        listener_net_id: &str,
        update: WireUpdate,
    ) {
        // send side (mirrors emit_update)
        let msg = MeerkatMessage::Update {
            listener_service: listener_net_id.to_string(),
            listener_def: src.interner.get(update.listener_def).to_string(),
            source_service: src.interner.get(update.source).to_string(),
            member: src.interner.get(update.member).to_string(),
            value: codec::encode_value(&update.value, &src.interner).unwrap(),
            clock: codec::encode_clock(&update.clock, &src.interner),
            sources: Some(codec::encode_sources(&update.sources, &src.interner)),
        };

        // real transport: the exact serde_json framing recv_message uses
        let bytes = serde_json::to_vec(&msg).unwrap();
        let msg: MeerkatMessage = serde_json::from_slice(&bytes).unwrap();

        // recv side (mirrors the dispatcher's Update arm)
        let (
            listener_service,
            listener_def,
            source_service,
            member,
            net_val,
            wire_clock,
            wire_sources,
        ) = match msg {
            MeerkatMessage::Update {
                listener_service,
                listener_def,
                source_service,
                member,
                value,
                clock,
                sources,
            } => (
                listener_service,
                listener_def,
                source_service,
                member,
                value,
                clock,
                sources,
            ),
            other => panic!("expected Update, got {:?}", other),
        };
        let (listener_def_sym, source_sym, member_sym, vclock, sources) = codec::decode_update(
            &listener_def,
            &source_service,
            &member,
            &mut dst.interner,
            wire_clock,
            wire_sources,
        )
        .unwrap();
        dst.handle_update(
            ServiceNetId(listener_service),
            listener_def_sym,
            (source_sym, member_sym),
            net_val,
            vclock,
            sources,
        )
        .await;
    }

    // Route B: two independent Managers with SEPARATE interners exchange the
    // remote-diamond updates through the real wire codec + a serde_json
    // round-trip. This is the guarantee the single-interner diamond tests cannot
    // give: a clock's (service, var) dimensions must survive re-interning into a
    // *different* symbol space. We deliberately offset node B's interner so the
    // same name maps to a different Symbol id on each node — only because the
    // wire clock is string-keyed does the clock still land on the right names.
    #[tokio::test]
    async fn test_wire_clock_crosses_interner_boundary() {
        // node A: the source side. It owns no services here; we only need its
        // interner to stamp the outgoing message, exactly as emit_update reads a
        // stored clock in A's symbol space.
        let mut node_a = Manager::default();
        let a_s1 = node_a.interner.insert("s1");
        let a_w = node_a.interner.insert("w");
        let a_a = node_a.interner.insert("a");
        let a_b = node_a.interner.insert("b");
        let a_z = node_a.interner.insert("z");

        // node B: the listener side, owns s2.z = s1.a + s1.b. Pad its interner
        // first so identical names get different ids than on node A.
        let mut node_b = Manager::default();
        for pad in ["pad0", "pad1", "pad2", "pad3", "pad4"] {
            node_b.interner.insert(pad);
        }
        let b_s1 = node_b.interner.insert("s1");
        let b_w = node_b.interner.insert("w");
        let b_a = node_b.interner.insert("a");
        let b_b = node_b.interner.insert("b");
        let b_z = node_b.interner.insert("z");
        let b_s2 = node_b.interner.insert("s2");
        assert_ne!(
            a_s1, b_s1,
            "test setup: the two nodes must not share a symbol space, else the \
             re-interning path isn't exercised"
        );

        // Local sentinel s1 on B: it only lets s2.z evaluate at creation and
        // resolves the not-yet-cached arm during the first delivery. z's clock
        // never depends on it — z's inputs are cross-service, so they come from
        // the wire via dep_cache, not from local s1's (empty) clock.
        node_b
            .create_service(
                b_s1,
                vec![
                    Decl::VarDecl {
                        name: b_a,
                        ty: None,
                        val: lit_int(0),
                    },
                    Decl::VarDecl {
                        name: b_b,
                        ty: None,
                        val: lit_int(0),
                    },
                ],
            )
            .await
            .unwrap();
        node_b
            .create_service(
                b_s2,
                vec![Decl::DefDecl {
                    name: b_z,
                    ty: None,
                    val: mk_add(
                        Expr::MemberAccess {
                            service_name: b_s1,
                            member_name: b_a,
                        },
                        Expr::MemberAccess {
                            service_name: b_s1,
                            member_name: b_b,
                        },
                    ),
                    is_pub: true,
                }],
            )
            .await
            .unwrap();
        let b_s2_id = node_b.services.get(&b_s2).unwrap().id.0.clone();

        // Deliver both arms at a consistent generation 1, each stamped in A's
        // symbol space: a = 11 @ {(s1,w):1,(s1,a):1}, b = 21 @ {(s1,w):1,(s1,b):1}.
        send_update_over_wire(
            &node_a,
            &mut node_b,
            &b_s2_id,
            WireUpdate {
                listener_def: a_z,
                source: a_s1,
                member: a_a,
                value: vint(11),
                sources: Sources::from([(a_s1, a_w)]),
                clock: HashMap::from([((a_s1, a_w), 1u64), ((a_s1, a_a), 1u64)]),
            },
        )
        .await;
        send_update_over_wire(
            &node_a,
            &mut node_b,
            &b_s2_id,
            WireUpdate {
                listener_def: a_z,
                source: a_s1,
                member: a_b,
                value: vint(21),
                sources: Sources::from([(a_s1, a_w)]),
                clock: HashMap::from([((a_s1, a_w), 1u64), ((a_s1, a_b), 1u64)]),
            },
        )
        .await;

        // z recomputed to the consistent sum, and its clock is the join expressed
        // entirely in B's symbol space — proving the dimensions were re-interned,
        // not carried as raw ids.
        let expected: VClock = HashMap::from([
            ((b_s1, b_w), 1u64),
            ((b_s1, b_a), 1u64),
            ((b_s1, b_b), 1u64),
        ]);
        let vs = node_b.services.get(&b_s2).unwrap().vars.get(&b_z).unwrap();
        assert_eq!(
            vs.value,
            vint(32),
            "z = s1.a + s1.b = 11 + 21 across the wire"
        );
        assert_eq!(
            vs.vector_clock, expected,
            "z's clock arrived in B's symbol space with the right dimensions"
        );
    }

    // Simultaneity (operation A): all vars co-written in one transaction end
    // with the SAME vector, not per-var stamps. W = {x, y}, R = ∅.
    #[tokio::test]
    async fn test_simultaneous_bump_identical_stamp() {
        let mut tc = TestContext::new();
        let decls = vec![
            Decl::VarDecl {
                name: tc.x,
                ty: None,
                val: lit_int(1),
            },
            Decl::VarDecl {
                name: tc.y,
                ty: None,
                val: lit_int(2),
            },
        ];
        tc.manager.create_service(tc.foo, decls).await.unwrap();

        let w = HashSet::from([(tc.foo, tc.x), (tc.foo, tc.y)]);
        tc.manager.simultaneous_bump(VClock::new(), &w);

        let foo = tc.manager.services.get(&tc.foo).unwrap();
        let xc = foo.vars.get(&tc.x).unwrap().vector_clock.clone();
        let yc = foo.vars.get(&tc.y).unwrap().vector_clock.clone();
        let expected: VClock = HashMap::from([((tc.foo, tc.x), 1), ((tc.foo, tc.y), 1)]);
        assert_eq!(xc, expected);
        assert_eq!(yc, expected);
        assert_eq!(xc, yc, "co-written vars must share one identical stamp");
    }

    // Causal consistency (operation A): the read set is folded into V_base, so a
    // written var's clock dominates the vars it read. R = {y}, W = {x}.
    #[tokio::test]
    async fn test_bump_absorbs_read_set() {
        let mut tc = TestContext::new();
        let decls = vec![
            Decl::VarDecl {
                name: tc.x,
                ty: None,
                val: lit_int(1),
            },
            Decl::VarDecl {
                name: tc.y,
                ty: None,
                val: lit_int(2),
            },
        ];
        tc.manager.create_service(tc.foo, decls).await.unwrap();

        // advance y to generation 1 first
        tc.manager
            .assign(tc.foo, tc.y, vint(9), None)
            .await
            .unwrap();
        // now write x having READ y
        let read_clock = tc.manager.get_clock(&tc.foo, &tc.y).unwrap().clone();
        tc.manager
            .simultaneous_bump(read_clock, &HashSet::from([(tc.foo, tc.x)]));

        let xc = &tc
            .manager
            .services
            .get(&tc.foo)
            .unwrap()
            .vars
            .get(&tc.x)
            .unwrap()
            .vector_clock;
        assert_eq!(
            xc.get(&(tc.foo, tc.y)).copied(),
            Some(1),
            "x dominates the y it read"
        );
        assert_eq!(
            xc.get(&(tc.foo, tc.x)).copied(),
            Some(1),
            "x still +1 on its own dimension"
        );
    }

    // Monotonicity (operation A): repeated writes increase the clock by exactly
    // one each time and never decrease. A write is an event, so even re-writing
    // the same value bumps.
    #[tokio::test]
    async fn test_clock_monotonic_on_repeated_writes() {
        let mut tc = TestContext::new();
        let decls = vec![Decl::VarDecl {
            name: tc.x,
            ty: None,
            val: lit_int(0),
        }];
        tc.manager.create_service(tc.foo, decls).await.unwrap();

        let key = (tc.foo, tc.x);
        for expected in 1..=3u64 {
            tc.manager
                .assign(tc.foo, tc.x, vint(7), None)
                .await
                .unwrap();
            let c = tc
                .manager
                .services
                .get(&tc.foo)
                .unwrap()
                .vars
                .get(&tc.x)
                .unwrap()
                .vector_clock
                .get(&key)
                .copied()
                .unwrap_or(0);
            assert_eq!(c, expected, "clock must increase by exactly 1 per write");
        }
    }

    // Helper: service with a single var x = 0
    async fn manager_with_x() -> TestContext {
        let mut tc = TestContext::new();
        let decls = vec![Decl::VarDecl {
            name: tc.x,
            ty: None,
            val: Expr::Literal {
                val: Value::Int { val: 0 },
            },
        }];
        tc.manager.create_service(tc.foo, decls).await.unwrap();
        tc
    }

    fn x_state(tc: &TestContext) -> &VarState {
        tc.manager
            .services
            .get(&tc.foo)
            .unwrap()
            .vars
            .get(&tc.x)
            .unwrap()
    }

    fn assert_x_unlocked(tc: &TestContext) {
        assert!(matches!(
            &x_state(tc).lock,
            crate::runtime::txn::VarLock::Unlocked
        ));
    }

    #[tokio::test]
    async fn test_txn_read_then_write_upgrades_lock() {
        // `x = x + 1` reads `x` (read lock) then writes `x` (must upgrade
        // to write lock)
        // This is the read-then-write pattern that the old upfront
        // analysis mishandled
        let mut tc = manager_with_x().await;
        let stmts = vec![ActionStmt::Assign {
            name: tc.x,
            expr: Expr::Binop {
                op: crate::ast::BinOp::Add,
                expr1: Box::new(Expr::Variable { name: tc.x }),
                expr2: Box::new(Expr::Literal {
                    val: Value::Int { val: 1 },
                }),
            },
        }];
        tc.manager.execute_action(tc.foo, &stmts).await.unwrap();
        let result = tc.manager.lookup(tc.x, tc.foo, None).await.unwrap();
        assert_eq!(result, Value::Int { val: 1 });
    }

    #[tokio::test]
    async fn test_txn_locks_released_between_transactions() {
        // Locks must be released after a transaction, so a second
        // transaction can acquire them. Running `x = x + 1` twice
        // should yield `x == 2`
        let mut tc = manager_with_x().await;
        let stmts = vec![ActionStmt::Assign {
            name: tc.x,
            expr: Expr::Binop {
                op: crate::ast::BinOp::Add,
                expr1: Box::new(Expr::Variable { name: tc.x }),
                expr2: Box::new(Expr::Literal {
                    val: Value::Int { val: 1 },
                }),
            },
        }];
        tc.manager.execute_action(tc.foo, &stmts).await.unwrap();
        tc.manager.execute_action(tc.foo, &stmts).await.unwrap();
        let result = tc.manager.lookup(tc.x, tc.foo, None).await.unwrap();
        assert_eq!(result, Value::Int { val: 2 });
    }

    #[tokio::test]
    async fn test_txn_var_unlocked_after_commit() {
        // After a transaction completes, the variable's lock should
        // be `Unlocked`
        let mut tc = manager_with_x().await;
        let stmts = vec![ActionStmt::Assign {
            name: tc.x,
            expr: Expr::Literal {
                val: Value::Int { val: 42 },
            },
        }];
        tc.manager.execute_action(tc.foo, &stmts).await.unwrap();
        assert_x_unlocked(&tc);
    }

    #[tokio::test]
    async fn test_txn_successful_write_updates_value_and_latest_write_txn() {
        // A successful transaction commits its buffered write and records
        // the transaction as the latest writer for that variable
        let mut tc = manager_with_x().await;
        let stmts = vec![ActionStmt::Assign {
            name: tc.x,
            expr: Expr::Literal {
                val: Value::Int { val: 42 },
            },
        }];

        tc.manager.execute_action(tc.foo, &stmts).await.unwrap();

        let state = x_state(&tc);
        assert_eq!(state.value, Value::Int { val: 42 });
        assert!(state.latest_write_txn.is_some());
    }

    #[tokio::test]
    async fn test_txn_nested_do_reuses_transaction() {
        // A nested `do` (an action invoking another action) must reuse
        // the same transaction, not start a fresh one. The inner write
        // to `x` should commit and all locks should be released afterward
        // This guards the bug where nested execution clobbered the outer
        // transaction's lock tracking
        let mut tc = manager_with_x().await;
        // outer action: `do` (action { `x` = `x` + 1 })
        let inner = Expr::Action(vec![ActionStmt::Assign {
            name: tc.x,
            expr: Expr::Binop {
                op: crate::ast::BinOp::Add,
                expr1: Box::new(Expr::Variable { name: tc.x }),
                expr2: Box::new(Expr::Literal {
                    val: Value::Int { val: 1 },
                }),
            },
        }]);
        let stmts = vec![ActionStmt::Do(inner)];
        tc.manager.execute_action(tc.foo, &stmts).await.unwrap();

        // inner write took effect
        let result = tc.manager.lookup(tc.x, tc.foo, None).await.unwrap();
        // and the lock was released
        assert_eq!(result, Value::Int { val: 1 });
        assert_x_unlocked(&tc);
    }

    #[tokio::test]
    async fn test_test_block_commits_each_do_separately() {
        // A `@test` block is not itself a transaction: each `do` opens and
        // commits a transaction of its own, so a failure later in the block
        // cannot roll back a `do` that already committed. Contrast with
        // `test_txn_failed_transaction_leaves_no_partial_writes`, where the
        // whole statement list is one transaction
        let mut tc = manager_with_x().await;
        let bump = Expr::Action(vec![ActionStmt::Assign {
            name: tc.x,
            expr: Expr::Binop {
                op: crate::ast::BinOp::Add,
                expr1: Box::new(Expr::Variable { name: tc.x }),
                expr2: Box::new(Expr::Literal {
                    val: Value::Int { val: 1 },
                }),
            },
        }]);
        let stmts = vec![
            ActionStmt::Do(bump.clone()),
            ActionStmt::Do(bump),
            ActionStmt::Assert(
                Expr::Literal {
                    val: Value::Bool { val: false },
                },
                "forced failure".to_string(),
            ),
        ];

        let result = tc.manager.execute_test_block(tc.foo, &stmts).await;

        assert!(matches!(result, Err(EvalError::AssertionError(_))));
        // Each `do` committed as it ran, and the failing assert left them be
        assert_eq!(x_state(&tc).value, Value::Int { val: 2 });
        assert_x_unlocked(&tc);
    }

    #[tokio::test]
    async fn test_txn_failed_transaction_leaves_no_partial_writes() {
        // A transaction that fails partway must leave no partial writes:
        // writes are buffered and applied only on a successful commit
        // Here the first statement writes `x`, the second fails
        // (asserting `false`), so `x` must stay unchanged
        let mut tc = manager_with_x().await;
        let stmts = vec![
            ActionStmt::Assign {
                name: tc.x,
                expr: Expr::Literal {
                    val: Value::Int { val: 99 },
                },
            },
            ActionStmt::Assert(
                Expr::Literal {
                    val: Value::Bool { val: false },
                },
                "false".to_string(),
            ),
        ];
        let result = tc.manager.execute_action(tc.foo, &stmts).await;
        assert!(result.is_err());
        // `x` must remain 0 — the buffered write to 99 was never committed
        let x = tc.manager.lookup(tc.x, tc.foo, None).await.unwrap();
        // and the lock was released
        assert_eq!(x, Value::Int { val: 0 });
        assert_x_unlocked(&tc);
    }

    #[tokio::test]
    async fn test_txn_failed_transaction_preserves_previous_latest_write_txn() {
        // A failed transaction must not update either committed state
        // field: the value and latest writer should remain from the last
        // successful commit
        let mut tc = manager_with_x().await;
        let successful_write = vec![ActionStmt::Assign {
            name: tc.x,
            expr: Expr::Literal {
                val: Value::Int { val: 1 },
            },
        }];
        tc.manager
            .execute_action(tc.foo, &successful_write)
            .await
            .unwrap();
        let previous_txn = x_state(&tc).latest_write_txn.clone();
        assert!(previous_txn.is_some());

        let failing_write = vec![
            ActionStmt::Assign {
                name: tc.x,
                expr: Expr::Literal {
                    val: Value::Int { val: 99 },
                },
            },
            ActionStmt::Assert(
                Expr::Literal {
                    val: Value::Bool { val: false },
                },
                "false".to_string(),
            ),
        ];

        let result = tc.manager.execute_action(tc.foo, &failing_write).await;

        assert!(result.is_err());
        let state = x_state(&tc);
        assert_eq!(state.value, Value::Int { val: 1 });
        assert_eq!(state.latest_write_txn, previous_txn);
        assert_x_unlocked(&tc);
    }

    #[tokio::test]
    async fn test_txn_read_lock_released_after_failure() {
        // If a transaction fails after a read, its read lock must still
        // be released
        let mut tc = manager_with_x().await;
        let last_txn = x_state(&tc).latest_write_txn.clone();
        let stmts = vec![ActionStmt::Assert(
            Expr::Variable { name: tc.x },
            "x".to_string(),
        )];

        let result = tc.manager.execute_action(tc.foo, &stmts).await;

        assert!(result.is_err());
        assert_eq!(x_state(&tc).value, Value::Int { val: 0 });
        // NOTE: changed this test case to check that the latest_write_txn is the txn that created
        // the service and not none (as it was previously). Since this is changing a test case,
        // please make sure to review it
        assert_eq!(x_state(&tc).latest_write_txn, last_txn);
        assert_x_unlocked(&tc);
    }

    #[tokio::test]
    async fn test_txn_cross_service_composition() {
        // A transaction beginning in `s1` composes an action defined in
        // `s2` (the example from issue #44). Both services' writes must
        // commit under the one transaction, and the `(service_net_id, var)`
        // keying must keep them distinct
        let mut tc = TestContext::new();
        // s2 owns `w` and an action that bumps it
        let bump = Expr::Action(vec![ActionStmt::Assign {
            name: tc.w,
            expr: Expr::Binop {
                op: crate::ast::BinOp::Add,
                expr1: Box::new(Expr::Variable { name: tc.w }),
                expr2: Box::new(Expr::Literal {
                    val: Value::Int { val: 5 },
                }),
            },
        }]);
        tc.manager
            .create_service(
                tc.s2,
                vec![
                    Decl::VarDecl {
                        name: tc.w,
                        ty: None,
                        val: Expr::Literal {
                            val: Value::Int { val: 10 },
                        },
                    },
                    Decl::DefDecl {
                        name: tc.bump,
                        ty: None,
                        val: bump,
                        is_pub: true,
                    },
                ],
            )
            .await
            .unwrap();
        // s1 owns `x`
        tc.manager
            .create_service(
                tc.s1,
                vec![Decl::VarDecl {
                    name: tc.x,
                    ty: None,
                    val: Expr::Literal {
                        val: Value::Int { val: 0 },
                    },
                }],
            )
            .await
            .unwrap();

        // Transaction on `s1`: `x` = `x` + 1; `do` `s2.bump`
        let stmts = vec![
            ActionStmt::Assign {
                name: tc.x,
                expr: Expr::Binop {
                    op: crate::ast::BinOp::Add,
                    expr1: Box::new(Expr::Variable { name: tc.x }),
                    expr2: Box::new(Expr::Literal {
                        val: Value::Int { val: 1 },
                    }),
                },
            },
            ActionStmt::Do(Expr::MemberAccess {
                service_name: tc.s2,
                member_name: tc.bump,
            }),
        ];
        tc.manager.execute_action(tc.s1, &stmts).await.unwrap();

        // Both services' writes committed
        assert_eq!(
            tc.manager.lookup(tc.x, tc.s1, None).await.unwrap(),
            Value::Int { val: 1 }
        );
        assert_eq!(
            tc.manager.lookup(tc.w, tc.s2, None).await.unwrap(),
            Value::Int { val: 15 }
        );
        // Locks released on both services
        assert!(matches!(
            tc.manager
                .services
                .get(&tc.s1)
                .unwrap()
                .vars
                .get(&tc.x)
                .unwrap()
                .lock,
            crate::runtime::txn::VarLock::Unlocked
        ));
        assert!(matches!(
            tc.manager
                .services
                .get(&tc.s2)
                .unwrap()
                .vars
                .get(&tc.w)
                .unwrap()
                .lock,
            crate::runtime::txn::VarLock::Unlocked
        ));
    }

    #[tokio::test]
    async fn test_wait_die_younger_dies_at_acquire() {
        // Wait-die: a younger transaction contending for a lock held by
        // an older transaction dies (abort) rather than acquiring it
        let mut tc = TestContext::new();
        tc.manager
            .create_service(
                tc.s1,
                vec![Decl::VarDecl {
                    name: tc.x,
                    ty: None,
                    val: Expr::Literal {
                        val: Value::Int { val: 0 },
                    },
                }],
            )
            .await
            .unwrap();
        let older = crate::runtime::txn::TxnId {
            timestamp: 1,
            node_id: 1,
            iteration: 0,
        };
        tc.manager
            .services
            .get_mut(&tc.s1)
            .unwrap()
            .vars
            .get_mut(&tc.x)
            .unwrap()
            .lock = crate::runtime::txn::VarLock::WriteLocked(older);
        let younger = crate::runtime::txn::TxnId {
            timestamp: u128::MAX,
            node_id: 1,
            iteration: 0,
        };
        let result = tc.manager.acquire_write_lock(tc.s1, tc.x, &younger);
        assert!(matches!(result, Err(EvalError::WaitDieAbort(_))));
    }

    #[tokio::test]
    async fn test_wait_die_older_takes_wait_path() {
        // Wait-die: an older transaction contending for a lock held by
        // a younger transaction takes the wait path, surfaced as
        // `WaitOn` carrying the contended `WaitKey` so the owner
        // can park the request
        let mut tc = TestContext::new();
        tc.manager
            .create_service(
                tc.s1,
                vec![Decl::VarDecl {
                    name: tc.x,
                    ty: None,
                    val: Expr::Literal {
                        val: Value::Int { val: 0 },
                    },
                }],
            )
            .await
            .unwrap();
        let younger = crate::runtime::txn::TxnId {
            timestamp: u128::MAX,
            node_id: 1,
            iteration: 0,
        };
        tc.manager
            .services
            .get_mut(&tc.s1)
            .unwrap()
            .vars
            .get_mut(&tc.x)
            .unwrap()
            .lock = crate::runtime::txn::VarLock::WriteLocked(younger);
        let older = crate::runtime::txn::TxnId {
            timestamp: 1,
            node_id: 1,
            iteration: 0,
        };
        let result = tc.manager.acquire_write_lock(tc.s1, tc.x, &older);
        assert!(matches!(result, Err(EvalError::WaitOn(_))));
    }

    #[tokio::test]
    async fn test_wait_die_action_dies_and_retries() {
        let mut tc = TestContext::new();
        tc.manager
            .create_service(
                tc.s1,
                vec![Decl::VarDecl {
                    name: tc.x,
                    ty: None,
                    val: Expr::Literal {
                        val: Value::Int { val: 0 },
                    },
                }],
            )
            .await
            .unwrap();
        let older = crate::runtime::txn::TxnId {
            timestamp: 1,
            node_id: 1,
            iteration: 0,
        };
        tc.manager
            .services
            .get_mut(&tc.s1)
            .unwrap()
            .vars
            .get_mut(&tc.x)
            .unwrap()
            .lock = crate::runtime::txn::VarLock::WriteLocked(older);
        let stmts = vec![ActionStmt::Assign {
            name: tc.x,
            expr: Expr::Binop {
                op: crate::ast::BinOp::Add,
                expr1: Box::new(Expr::Variable { name: tc.x }),
                expr2: Box::new(Expr::Literal {
                    val: Value::Int { val: 1 },
                }),
            },
        }];
        let result = tc.manager.execute_action(tc.s1, &stmts).await;
        assert!(matches!(result, Err(EvalError::WaitDieAbort(_))));
        assert!(matches!(
            tc.manager
                .services
                .get(&tc.s1)
                .unwrap()
                .vars
                .get(&tc.x)
                .unwrap()
                .lock,
            crate::runtime::txn::VarLock::WriteLocked(_)
        ));
    }

    /// The retry loop must pause between attempts, and for as long as the
    /// outcome calls for: a die re-runs after `WAIT_DIE_RETRY_BACKOFF_MS`, a
    /// wait after the longer `WAIT_ON_RETRY_BACKOFF_MS`.
    ///
    /// The upper bound on the die run is what catches wiring one backoff to
    /// both outcomes: it would either make the die as slow as a wait or the
    /// wait as quick as a die (and the wait bound catches the latter).
    ///
    /// Runs on paused Tokio time, so the measured durations are exactly the
    /// backoffs slept and scheduler delays on a loaded machine cannot push
    /// the die run past its upper bound.
    #[tokio::test(start_paused = true)]
    async fn test_wait_die_retries_are_paced() {
        async fn run_against_holder(timestamp: u128) -> Duration {
            let mut tc = manager_with_x().await;
            let holder = crate::runtime::txn::TxnId {
                timestamp,
                node_id: tc.manager.node_id,
                iteration: 0,
            };
            tc.manager
                .services
                .get_mut(&tc.foo)
                .unwrap()
                .vars
                .get_mut(&tc.x)
                .unwrap()
                .lock = crate::runtime::txn::VarLock::WriteLocked(holder);
            let stmts = vec![ActionStmt::Assign {
                name: tc.x,
                expr: Expr::Variable { name: tc.x },
            }];
            let started = tokio::time::Instant::now();
            let err = tc
                .manager
                .execute_action(tc.foo, &stmts)
                .await
                .expect_err("`x` is held for the whole run");
            assert!(matches!(err, EvalError::WaitDieAbort(_)), "got {err:?}");
            started.elapsed()
        }
        let retries = u64::from(MAX_WAIT_DIE_RETRIES);

        // Holder is older: every attempt dies.
        let die = run_against_holder(1).await;
        let die_min = Duration::from_millis(WAIT_DIE_RETRY_BACKOFF_MS * retries);
        let wait_min = Duration::from_millis(WAIT_ON_RETRY_BACKOFF_MS * retries);
        assert!(
            die >= die_min,
            "a die must pause before each retry: {die:?}"
        );
        assert!(
            die < wait_min,
            "a die must use the short backoff, not the wait's: {die:?}"
        );

        // Holder is younger: every attempt is told to wait.
        let wait = run_against_holder(u128::MAX).await;
        assert!(
            wait >= wait_min,
            "a wait must use the long backoff before each retry: {wait:?}"
        );
    }

    /// A wait-die abort raised on a participant must still be a wait-die abort
    /// once it has crossed the wire.
    ///
    /// Errors travel as `Display` text, and `execute_action_with_txn` retries
    /// `WaitDieAbort` and nothing else, so flattening the reply to
    /// `LocalDispatchFailed` would turn ordinary lock contention into a
    /// terminal failure. This walks the round trip: the participant's error,
    /// serialized the way the reply does it, then rebuilt on the originator.
    #[tokio::test]
    async fn test_remote_wait_die_survives_the_round_trip() {
        let mut tc = manager_with_x().await;

        // An older transaction holds `x` exclusively.
        let older = TxnId {
            timestamp: 1,
            node_id: 1,
            iteration: 0,
        };
        tc.manager
            .services
            .get_mut(&tc.foo)
            .unwrap()
            .vars
            .get_mut(&tc.x)
            .unwrap()
            .lock = crate::runtime::txn::VarLock::WriteLocked(older);

        // A younger transaction reads it as a participant would: wait-die says die.
        let younger = TxnId::new(tc.manager.node_id);
        let err = tc
            .manager
            .remote_read_participant(tc.foo, tc.x, younger)
            .await
            .expect_err("a younger transaction must die against an older holder");
        assert!(matches!(err, EvalError::WaitDieAbort(_)));

        // This is exactly what the reply carries and what the originator gets.
        let on_the_wire = err.to_string();
        let rebuilt = Manager::remote_error(on_the_wire);
        assert!(
            matches!(rebuilt, EvalError::WaitDieAbort(_)),
            "the originator must see a retryable wait-die abort, not a terminal dispatch failure"
        );
    }

    /// A wait-die abort must read the same after any number of hops.
    ///
    /// `Display` writes `WAIT_DIE_DISPLAY_PREFIX`, and the reply carries that
    /// text, so rebuilding the variant from the whole reply stores the prefix
    /// inside the payload and prints it twice. Every further hop -- a retry
    /// exhausted on the originator, or a node in the middle forwarding the
    /// failure on -- adds another copy, so the message a `@test` reports grows
    /// a prefix per node it passed through.
    #[tokio::test]
    async fn test_remote_wait_die_prefix_is_not_repeated_per_hop() {
        let original = EvalError::WaitDieAbort("transaction died contending for 'x'".to_string());
        let expected = original.to_string();

        let mut text = expected.clone();
        for hop in 1..=3 {
            text = Manager::remote_error(text).to_string();
            assert_eq!(
                text, expected,
                "after {hop} hop(s) the message must still read as one wait-die abort"
            );
        }
    }

    /// A remote failure that merely mentions wait-die must stay a failure.
    ///
    /// The reply carries `Display` text, so the prefix `WaitDieAbort` writes is
    /// the only thing that marks a lock conflict. Error messages quote user
    /// input -- an assertion carries its own source text, so a program that
    /// compares against the phrase produces one -- and accepting the phrase
    /// anywhere would send `execute_action_with_txn` through the whole wait-die
    /// retry budget and then report a lock conflict that never happened,
    /// instead of the assertion that actually failed.
    #[tokio::test]
    async fn test_remote_error_matches_the_wait_die_prefix_not_the_phrase() {
        let mut tc = TestContext::new();

        // `assert` carries the source text of its condition, so this is the
        // message a real program produces, not a hand-built string.
        let stmt = ActionStmt::Assert(
            Expr::Literal {
                val: Value::Bool { val: false },
            },
            "note == \"Wait-die abort: seen in the log\"".to_string(),
        );
        let on_the_wire = match execute(&stmt, &[], &mut tc.manager, tc.foo, None).await {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a false assertion must fail"),
        };
        assert!(
            on_the_wire.contains("Wait-die abort"),
            "this test is only meaningful if the message mentions the phrase, got: {on_the_wire}"
        );
        assert!(
            matches!(
                Manager::remote_error(on_the_wire),
                EvalError::LocalDispatchFailed(_)
            ),
            "a failure that only quotes the phrase must stay terminal, not become retryable"
        );
    }

    #[tokio::test]
    async fn test_wait_die_participant_preserves_partial_txn() {
        // Wait-die: a participant action that conflicts mid-execution
        // parks by preserving its partial transaction (locks already
        // taken stay held) in `pending_txns`, so a later re-dispatch
        // can resume rather than restart
        let mut tc = TestContext::new();
        tc.manager
            .create_service(
                tc.s1,
                vec![
                    Decl::VarDecl {
                        name: tc.y,
                        ty: None,
                        val: Expr::Literal {
                            val: Value::Int { val: 0 },
                        },
                    },
                    Decl::VarDecl {
                        name: tc.x,
                        ty: None,
                        val: Expr::Literal {
                            val: Value::Int { val: 0 },
                        },
                    },
                ],
            )
            .await
            .unwrap();
        // A younger transaction holds a write lock on `x`
        let younger = crate::runtime::txn::TxnId {
            timestamp: u128::MAX,
            node_id: 1,
            iteration: 0,
        };
        tc.manager
            .services
            .get_mut(&tc.s1)
            .unwrap()
            .vars
            .get_mut(&tc.x)
            .unwrap()
            .lock = crate::runtime::txn::VarLock::WriteLocked(younger);
        let older = crate::runtime::txn::TxnId {
            timestamp: 1,
            node_id: 1,
            iteration: 0,
        };
        // Older transaction: write `y` (acquires `y`), then touch `x`
        // (conflict, waits)
        let stmts = vec![
            ActionStmt::Assign {
                name: tc.y,
                expr: Expr::Literal {
                    val: Value::Int { val: 5 },
                },
            },
            ActionStmt::Assign {
                name: tc.x,
                expr: Expr::Binop {
                    op: crate::ast::BinOp::Add,
                    expr1: Box::new(Expr::Variable { name: tc.x }),
                    expr2: Box::new(Expr::Literal {
                        val: Value::Int { val: 1 },
                    }),
                },
            },
        ];
        let result = tc
            .manager
            .execute_action_participant(tc.s1, &stmts, &[], older.clone())
            .await;
        // Parked: returns `WaitOn`, and the partial transaction is preserved
        assert!(matches!(result, Err(EvalError::WaitOn(_))));
        assert!(tc.manager.pending_txns.contains_key(&older));
        // The lock it already took on `y` is still held (not released on park)
        assert!(matches!(
            tc.manager
                .services
                .get(&tc.s1)
                .unwrap()
                .vars
                .get(&tc.y)
                .unwrap()
                .lock,
            crate::runtime::txn::VarLock::WriteLocked(_)
        ));
    }

    #[tokio::test]
    async fn test_wait_queue_oldest_first_and_purge() {
        // Wait-die: parked requests on a variable are served oldest-first
        // when the lock frees, and a transaction's waiters are purged
        // when it aborts
        let mut tc = TestContext::new();
        tc.manager
            .create_service(
                tc.s1,
                vec![Decl::VarDecl {
                    name: tc.x,
                    ty: None,
                    val: Expr::Literal {
                        val: Value::Int { val: 0 },
                    },
                }],
            )
            .await
            .unwrap();
        let make = |rid: u64, tid: crate::runtime::txn::TxnId| ParkedRequest::Action {
            request_id: rid,
            reply_to: String::new(),
            service: tc.s1,
            stmts: vec![],
            env: vec![],
            tid,
        };
        let old = crate::runtime::txn::TxnId {
            timestamp: 1,
            node_id: 1,
            iteration: 0,
        };
        let mid = crate::runtime::txn::TxnId {
            timestamp: 5,
            node_id: 1,
            iteration: 0,
        };
        tc.manager.park_request(tc.s1, tc.x, make(1, mid.clone()));
        tc.manager.park_request(tc.s1, tc.x, make(2, old.clone()));
        // Freeing `x` yields the oldest waiter first; the other stays parked
        let mut freed = std::collections::HashSet::new();
        freed.insert(WaitKey::Member(
            tc.manager.service_net_id_for_name(tc.s1),
            tc.x,
        ));
        let ready = tc.manager.take_ready_waiters(&freed);
        assert_eq!(ready.len(), 1);
        assert!(ready[0].tid() == &old);
        // The remaining `mid` waiter is purged when its transaction aborts
        let removed = tc.manager.purge_parked_txn(&mid);
        assert_eq!(removed.len(), 1);
        assert!(tc.manager.wait_queue.is_empty());
    }

    #[tokio::test]
    async fn test_wait_die_parked_request_resumes_after_release() {
        // Wait-die end to end (single node, no network): an older
        // transaction parks on a variable held by a younger one; when
        // the younger aborts and frees the lock, the parked request is
        // taken oldest-first and its re-run resumes from the preserved
        // transaction and now succeeds
        let mut tc = TestContext::new();
        tc.manager
            .create_service(
                tc.s1,
                vec![Decl::VarDecl {
                    name: tc.x,
                    ty: None,
                    val: Expr::Literal {
                        val: Value::Int { val: 0 },
                    },
                }],
            )
            .await
            .unwrap();

        // A younger transaction holds a write lock on `x`, prepared in
        // `pending_txns`
        let younger = crate::runtime::txn::TxnId {
            timestamp: u128::MAX,
            node_id: 1,
            iteration: 0,
        };
        tc.manager
            .services
            .get_mut(&tc.s1)
            .unwrap()
            .vars
            .get_mut(&tc.x)
            .unwrap()
            .lock = crate::runtime::txn::VarLock::WriteLocked(younger.clone());
        let mut younger_txn = crate::runtime::txn::Transaction::new(younger.clone());
        younger_txn
            .locked
            .insert((tc.manager.service_net_id_for_name(tc.s1), tc.x));
        tc.manager.pending_txns.insert(younger.clone(), younger_txn);

        let older = crate::runtime::txn::TxnId {
            timestamp: 1,
            node_id: 1,
            iteration: 0,
        };
        // Older transaction: `x` = `x` + 1 conflicts -> `WaitOn` -> park it
        let stmts = vec![ActionStmt::Assign {
            name: tc.x,
            expr: Expr::Binop {
                op: crate::ast::BinOp::Add,
                expr1: Box::new(Expr::Variable { name: tc.x }),
                expr2: Box::new(Expr::Literal {
                    val: Value::Int { val: 1 },
                }),
            },
        }];
        let r1 = tc
            .manager
            .execute_action_participant(tc.s1, &stmts, &[], older.clone())
            .await;
        assert!(matches!(r1, Err(EvalError::WaitOn(_))));
        tc.manager.park_request(
            tc.s1,
            tc.x,
            ParkedRequest::Action {
                request_id: 1,
                reply_to: String::new(),
                service: tc.s1,
                stmts: stmts.clone(),
                env: vec![],
                tid: older.clone(),
            },
        );

        // The younger holder aborts, freeing `x`. The released keys reach the
        // caller through `take_freed_awaiting_wake`
        tc.manager.abort_participant(&younger).await;
        let freed = tc.manager.take_freed_awaiting_wake();
        assert!(matches!(
            tc.manager
                .services
                .get(&tc.s1)
                .unwrap()
                .vars
                .get(&tc.x)
                .unwrap()
                .lock,
            crate::runtime::txn::VarLock::Unlocked
        ));

        // Wake: take the oldest waiter and re-run it; it should now succeed
        let ready = tc.manager.take_ready_waiters(&freed);
        assert_eq!(ready.len(), 1);
        if let ParkedRequest::Action {
            service,
            stmts,
            env,
            tid,
            ..
        } = &ready[0]
        {
            let r2 = tc
                .manager
                .execute_action_participant(*service, stmts, env, tid.clone())
                .await;
            assert!(r2.is_ok());
        } else {
            panic!("expected an Action waiter");
        }

        // The older transaction now holds `x`'s write lock and is prepared
        assert!(matches!(
            tc.manager
                .services
                .get(&tc.s1)
                .unwrap()
                .vars
                .get(&tc.x)
                .unwrap()
                .lock,
            crate::runtime::txn::VarLock::WriteLocked(_)
        ));
        assert!(tc.manager.pending_txns.contains_key(&older));
    }
    #[tokio::test]
    async fn test_create_service_uses_single_transaction() {
        let mut tc = TestContext::new();
        let decls = vec![
            Decl::VarDecl {
                name: tc.x,
                ty: None,
                val: Expr::Literal {
                    val: Value::Int { val: 1 },
                },
            },
            Decl::VarDecl {
                name: tc.y,
                ty: None,
                val: Expr::Literal {
                    val: Value::Int { val: 2 },
                },
            },
            Decl::DefDecl {
                name: tc.f,
                ty: None,
                val: Expr::Binop {
                    op: crate::ast::BinOp::Add,
                    expr1: Box::new(Expr::Variable { name: tc.x }),
                    expr2: Box::new(Expr::Variable { name: tc.y }),
                },
                is_pub: true,
            },
        ];
        tc.manager.create_service(tc.foo, decls).await.unwrap();

        let foo = tc.manager.services.get(&tc.foo).unwrap();
        let tid = foo.vars.get(&tc.x).unwrap().latest_write_txn.clone();
        assert!(
            tid.is_some(),
            "init writes must record a writer txn from create_service"
        );
        // every var/def initialized by the same transaction
        assert_eq!(foo.vars.get(&tc.y).unwrap().latest_write_txn, tid);
        assert_eq!(foo.vars.get(&tc.f).unwrap().latest_write_txn, tid);
    }

    #[tokio::test]
    async fn test_create_service_rolls_back_on_partial_failure() {
        let mut tc = TestContext::new();
        let decls = vec![
            Decl::VarDecl {
                name: tc.x,
                ty: None,
                val: Expr::Literal {
                    val: Value::Int { val: 1 },
                },
            },
            // adding a bool and number should be a type error
            Decl::VarDecl {
                name: tc.y,
                ty: None,
                val: Expr::Binop {
                    op: crate::ast::BinOp::Add,
                    expr1: Box::new(Expr::Literal {
                        val: Value::Bool { val: true },
                    }),
                    expr2: Box::new(Expr::Literal {
                        val: Value::Int { val: 1 },
                    }),
                },
            },
        ];
        let result = tc.manager.create_service(tc.foo, decls).await;
        assert!(result.is_err());
        assert!(
            tc.manager.services.is_empty(),
            "no services should've been created"
        );
        assert!(
            !tc.manager.unified_ast.iter().any(|s| {
                if let Stmt::Service { name, .. } = s {
                    *name == tc.foo
                } else {
                    false
                }
            }),
            "phantom service must not remain in unified_ast after rollback"
        );
    }

    // full disclosure: I'm not sure how to test that create_service actually occurs under a single transaction
    // in the sense that a lock is truly acquired, so I had Claude write a test for me
    #[tokio::test]
    async fn test_create_service_read_conflicts_under_one_txn() {
        let mut tc = TestContext::new();
        tc.manager
            .create_service(
                tc.s1,
                vec![Decl::VarDecl {
                    name: tc.x,
                    ty: None,
                    val: Expr::Literal {
                        val: Value::Int { val: 7 },
                    },
                }],
            )
            .await
            .unwrap();

        // Simulate another in-flight transaction holding a write lock on s1.x.
        // #98: use an explicit low timestamp so this transaction is
        // deterministically older than s2's internally-generated one. s2 must
        // be the younger party that dies under wait-die; relying on the wall
        // clock made that ordering non-deterministic.
        let ext = crate::runtime::txn::TxnId {
            timestamp: 1,
            node_id: tc.manager.node_id,
            iteration: 0,
        };
        assert!(tc
            .manager
            .services
            .get_mut(&tc.s1)
            .unwrap()
            .vars
            .get_mut(&tc.x)
            .unwrap()
            .lock
            .try_write(&ext));

        // s2's init reads s1.x; under a real transaction this must fail to read-lock.
        // since s2 is younger, it should die instead of waiting
        let result = tc
            .manager
            .create_service(
                tc.s2,
                vec![Decl::DefDecl {
                    name: tc.f,
                    ty: None,
                    val: Expr::MemberAccess {
                        service_name: tc.s1,
                        member_name: tc.x,
                    },
                    is_pub: true,
                }],
            )
            .await;

        assert!(
            matches!(result, Err(EvalError::WaitDieAbort(_))),
            "init read must respect the lock"
        );
        assert!(
            !tc.manager.services.contains_key(&tc.s2),
            "failed init rolls back"
        );
        // the foreign lock is untouched
        assert!(matches!(
            tc.manager.services.get(&tc.s1).unwrap().vars.get(&tc.x).unwrap().lock,
            crate::runtime::txn::VarLock::WriteLocked(ref t) if *t == ext
        ));
    }

    #[tokio::test]
    async fn test_service_lock_blocks_member_read_and_write() {
        let mut tc = TestContext::new();
        tc.manager
            .create_service(
                tc.s1,
                vec![Decl::VarDecl {
                    name: tc.x,
                    ty: None,
                    val: Expr::Literal {
                        val: Value::Int { val: 42 },
                    },
                }],
            )
            .await
            .unwrap();

        let older = TxnId {
            timestamp: 1,
            node_id: 1,
            iteration: 0,
        };
        let younger = TxnId {
            timestamp: 100,
            node_id: 1,
            iteration: 0,
        };

        // Acquire service lock under older transaction
        assert!(tc.manager.acquire_service_lock(tc.s1, &older).is_ok());

        // Younger transaction trying to read member should die
        let res_read = tc.manager.acquire_read_lock(tc.s1, tc.x, &younger);
        assert!(matches!(res_read, Err(EvalError::WaitDieAbort(_))));

        // Younger transaction trying to write member should die
        let res_write = tc.manager.acquire_write_lock(tc.s1, tc.x, &younger);
        assert!(matches!(res_write, Err(EvalError::WaitDieAbort(_))));
    }

    #[tokio::test]
    async fn test_member_lock_blocks_service_lock() {
        let mut tc = TestContext::new();
        tc.manager
            .create_service(
                tc.s1,
                vec![Decl::VarDecl {
                    name: tc.x,
                    ty: None,
                    val: Expr::Literal {
                        val: Value::Int { val: 42 },
                    },
                }],
            )
            .await
            .unwrap();

        let older = TxnId {
            timestamp: 1,
            node_id: 1,
            iteration: 0,
        };
        let younger = TxnId {
            timestamp: 100,
            node_id: 1,
            iteration: 0,
        };

        // Acquire member write lock under older transaction
        assert!(tc.manager.acquire_write_lock(tc.s1, tc.x, &older).is_ok());

        // Younger transaction trying to acquire service lock should die
        let res_svc = tc.manager.acquire_service_lock(tc.s1, &younger);
        assert!(matches!(res_svc, Err(EvalError::WaitDieAbort(_))));

        // Older transaction should be able to acquire service lock on its own service
        assert!(tc.manager.acquire_service_lock(tc.s1, &older).is_ok());
    }

    #[tokio::test]
    async fn test_handle_lock_request_eager_release_on_wait() {
        let mut tc = TestContext::new();
        tc.manager
            .create_service(
                tc.s1,
                vec![
                    Decl::VarDecl {
                        name: tc.x,
                        ty: None,
                        val: Expr::Literal {
                            val: Value::Int { val: 1 },
                        },
                    },
                    Decl::VarDecl {
                        name: tc.y,
                        ty: None,
                        val: Expr::Literal {
                            val: Value::Int { val: 2 },
                        },
                    },
                ],
            )
            .await
            .unwrap();

        let younger = TxnId {
            timestamp: 100,
            node_id: 1,
            iteration: 0,
        };
        let older = TxnId {
            timestamp: 1,
            node_id: 1,
            iteration: 0,
        };

        // Younger transaction locks member x
        assert!(tc.manager.acquire_write_lock(tc.s1, tc.x, &younger).is_ok());

        // Older transaction requests LockGroup covering y and x
        let mut writes = HashSet::new();
        writes.insert(tc.manager.interner.get(tc.y).to_string());
        writes.insert(tc.manager.interner.get(tc.x).to_string());
        let lg = LockGroup {
            service_level_lock: false,
            reads: HashSet::new(),
            writes,
        };
        let mut services = HashMap::new();
        services.insert(tc.manager.interner.get(tc.s1).to_string(), lg);

        // Seed the older transaction in `pending_txns` with a dummy
        // participant address to verify that `participants` are drained
        let mut pre_txn = crate::runtime::txn::Transaction::new(older.clone());
        pre_txn
            .participants
            .insert(crate::net::types::Address("remote:1234".to_string()));
        tc.manager.pending_txns.insert(older.clone(), pre_txn);

        // `handle_lock_request` yields `WaitOn`
        let res = tc
            .manager
            .handle_lock_request(older.clone(), services)
            .await;
        assert!(matches!(res, Err(EvalError::WaitOn(_))));

        // Assert all-or-nothing: lock on `y` was released upon `WaitOn`
        let y_state = tc
            .manager
            .services
            .get(&tc.s1)
            .unwrap()
            .vars
            .get(&tc.y)
            .unwrap();
        assert!(matches!(
            y_state.lock,
            crate::runtime::txn::VarLock::Unlocked
        ));

        // Assert all-or-nothing: `participants` were drained on `WaitOn`
        let parked_txn = tc.manager.pending_txns.get(&older).unwrap();
        assert!(parked_txn.participants.is_empty());
    }

    /// Verify that handle_lock_request rejects requests with invalid identifiers before interning
    #[tokio::test]
    async fn test_handle_lock_request_invalid_identifier_rejected() {
        let interner = Interner::new();
        let mut manager = Manager::new(interner);

        let txn_id = TxnId::new(1);
        let mut services = HashMap::new();
        services.insert(
            "bad-service-name!".to_string(),
            LockGroup {
                service_level_lock: false,
                reads: HashSet::new(),
                writes: HashSet::new(),
            },
        );

        let res = manager.handle_lock_request(txn_id, services).await;
        assert!(res.is_err());
        assert!(matches!(res, Err(EvalError::RuntimeError(_))));
    }

    /// `x = x + 1`
    fn incr_x(tc: &TestContext) -> ActionStmt {
        ActionStmt::Assign {
            name: tc.x,
            expr: Expr::Binop {
                op: crate::ast::BinOp::Add,
                expr1: Box::new(Expr::Variable { name: tc.x }),
                expr2: Box::new(Expr::Literal {
                    val: Value::Int { val: 1 },
                }),
            },
        }
    }

    /// A read of `foo.member` by some transaction, as it sits on the wait queue
    fn parked_lookup(tc: &TestContext, member: Symbol) -> ParkedRequest {
        ParkedRequest::Lookup {
            request_id: 1,
            reply_to: String::new(),
            service: tc.foo,
            member,
            tid: TxnId::new(tc.manager.node_id),
        }
    }

    /// The freed-key queue holds only keys something is actually parked on.
    ///
    /// A key nothing waits on has nothing to wake, and on a node that never
    /// parks anything it would be an entry that is never drained. A CLI client
    /// is exactly that node: it serves a `LockRequest` inline from
    /// `dispatch_network_events` while awaiting some other reply, so it can
    /// reach the discard path, but it has no loop to drain the queue.
    #[tokio::test]
    async fn test_freed_keys_are_queued_only_when_something_is_parked_on_them() {
        let mut tc = manager_with_x().await;
        let key = WaitKey::Member(tc.manager.service_net_id_for_name(tc.foo), tc.x);

        // Locks `x`, then fails terminally, so `x` is released.
        let stmts = vec![
            incr_x(&tc),
            ActionStmt::Assert(
                Expr::Literal {
                    val: Value::Bool { val: false },
                },
                "always fails".to_string(),
            ),
        ];
        let tid = TxnId::new(tc.manager.node_id);
        tc.manager
            .execute_action_participant(tc.foo, &stmts, &[], tid)
            .await
            .expect_err("the assertion fails the action");
        assert!(
            tc.manager.take_freed_awaiting_wake().is_empty(),
            "nothing is parked on `x`, so its release must not be queued"
        );

        // With a waiter, the same failure queues exactly that key.
        let parked = parked_lookup(&tc, tc.x);
        tc.manager.park_request_key(key.clone(), parked);
        let tid = TxnId::new(tc.manager.node_id);
        tc.manager
            .execute_action_participant(tc.foo, &stmts, &[], tid)
            .await
            .expect_err("the assertion fails the action");
        assert_eq!(
            tc.manager.take_freed_awaiting_wake(),
            HashSet::from([key]),
            "a request is parked on `x`, so its release must be queued for waking"
        );
    }

    /// A commit reports its freed keys through the deferred queue, like every
    /// other release, and each key is delivered once.
    ///
    /// Two deliveries of one key would wake two waiters: the second wake fires
    /// while the first waiter is holding the lock it was just handed, and
    /// wait-die kills the second rather than leaving it parked.
    #[tokio::test]
    async fn test_commit_reports_its_freed_keys_once() {
        let mut tc = manager_with_x().await;
        let key = WaitKey::Member(tc.manager.service_net_id_for_name(tc.foo), tc.x);

        // A prepared participant transaction holding the write lock on `x`.
        let holder = TxnId::new(tc.manager.node_id);
        tc.manager
            .execute_action_participant(tc.foo, &[incr_x(&tc)], &[], holder.clone())
            .await
            .unwrap();

        // Two waiters parked behind it, oldest first.
        let parked_action = |timestamp| ParkedRequest::Action {
            request_id: 1,
            reply_to: String::new(),
            service: tc.foo,
            stmts: vec![incr_x(&tc)],
            env: vec![],
            tid: TxnId {
                timestamp,
                node_id: tc.manager.node_id,
                iteration: 0,
            },
        };
        let (first, second) = (parked_action(1), parked_action(2));
        let first_tid = first.tid().clone();
        tc.manager.park_request_key(key.clone(), first);
        tc.manager.park_request_key(key.clone(), second);

        // The server loop commits, then wakes what the queue reports.
        let clock = HashMap::from([((tc.foo, tc.x), 1)]);
        tc.manager
            .commit_participant(&holder, &clock)
            .await
            .unwrap();
        let freed = tc.manager.take_freed_awaiting_wake();
        assert!(freed.contains(&key), "the commit released `x`");

        // Waking serves the oldest waiter, which takes the lock.
        let ready = tc.manager.take_ready_waiters(&freed);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].tid(), &first_tid);
        tc.manager
            .execute_action_participant(tc.foo, &[incr_x(&tc)], &[], first_tid)
            .await
            .expect("the freed lock is now the first waiter's");
        assert!(
            tc.manager.take_freed_awaiting_wake().is_empty(),
            "the key was taken once already; a second delivery would wake the \
             younger waiter against the lock the first one now holds"
        );
        assert_eq!(
            tc.manager.wait_queue.get(&key).map(|w| w.len()),
            Some(1),
            "the younger waiter must stay parked until the new holder commits"
        );
    }

    /// A service with two independent vars, `a` and `x`
    async fn manager_with_a_and_x() -> TestContext {
        let mut tc = TestContext::new();
        let a = tc.manager.interner.insert("a");
        let zero = || Expr::Literal {
            val: Value::Int { val: 0 },
        };
        let decls = vec![
            Decl::VarDecl {
                name: a,
                ty: None,
                val: zero(),
            },
            Decl::VarDecl {
                name: tc.x,
                ty: None,
                val: zero(),
            },
        ];
        tc.manager.create_service(tc.foo, decls).await.unwrap();
        tc
    }

    /// Every lock an originator transaction releases has to be reported for
    /// waking, on the ordinary path as much as the failing one.
    #[tokio::test]
    async fn test_originator_reports_locks_released_on_success() {
        let mut tc = manager_with_x().await;
        let key = WaitKey::Member(tc.manager.service_net_id_for_name(tc.foo), tc.x);
        let parked = parked_lookup(&tc, tc.x);
        tc.manager.park_request_key(key.clone(), parked);

        tc.manager
            .execute_action(tc.foo, &[incr_x(&tc)])
            .await
            .expect("an uncontended write commits");

        assert!(
            tc.manager.take_freed_awaiting_wake().contains(&key),
            "the committed transaction released its write lock on `x`, so the \
             request parked on it must be reported for waking"
        );
    }

    /// The same when the originator gives up on a contended lock, releasing
    /// what the attempt had already locked.
    #[tokio::test]
    async fn test_originator_reports_locks_released_when_it_gives_up() {
        let mut tc = manager_with_a_and_x().await;
        let a = tc.manager.interner.insert("a");
        let key_a = WaitKey::Member(tc.manager.service_net_id_for_name(tc.foo), a);

        // A younger transaction holds `x` for the whole run.
        tc.manager
            .services
            .get_mut(&tc.foo)
            .unwrap()
            .vars
            .get_mut(&tc.x)
            .unwrap()
            .lock = crate::runtime::txn::VarLock::WriteLocked(TxnId {
            timestamp: u128::MAX,
            node_id: tc.manager.node_id,
            iteration: 0,
        });

        // Someone is parked on `a`, which the transaction locks before it
        // contends on `x`.
        let parked = parked_lookup(&tc, a);
        tc.manager.park_request_key(key_a.clone(), parked);

        let stmts = vec![
            ActionStmt::Assign {
                name: a,
                expr: Expr::Literal {
                    val: Value::Int { val: 1 },
                },
            },
            incr_x(&tc),
        ];
        tc.manager
            .execute_action(tc.foo, &stmts)
            .await
            .expect_err("`x` is held for the whole run");

        assert!(
            tc.manager.take_freed_awaiting_wake().contains(&key_a),
            "the attempt released the lock it had taken on `a`, so the request \
             parked on it must be reported for waking"
        );
    }

    async fn start_local_manager(code: &str) -> Manager {
        let mut node = Node::new();
        node.unified_ast = parse_string(code, &mut node.interner).unwrap();
        node.static_checks().unwrap();
        let ast = node.unified_ast.clone();
        node.on_manager_startup(true, None, HashMap::new(), &ast)
            .await
            .unwrap()
    }

    async fn update_service_code(manager: &mut Manager, code: &str) {
        let ast = parse_string(code, &mut manager.interner).unwrap();
        CodeUpdate::new(ast).poll(manager).await.unwrap();
    }

    #[tokio::test]
    async fn shrinking_sources_removes_old_source_and_listener() {
        let mut manager = start_local_manager(
            "service s {
                var x = 1;
                var y = 2;
                pub def a = x + y;
                pub def b = a + 1;
            }",
        )
        .await;
        let [s, x, y, a, b] = ["s", "x", "y", "a", "b"].map(|name| manager.interner.insert(name));
        let original = HashSet::from([(s, x), (s, y)]);
        assert_eq!(manager.sources(s, a, None).as_deref(), Some(&original));
        assert_eq!(manager.sources(s, b, None).as_deref(), Some(&original));
        let listener = (manager.services[&s].id.clone(), a);
        assert!(manager.services[&s].listeners[&y].contains(&listener));

        update_service_code(&mut manager, "update s { pub def a = x; }").await;

        let expected = HashSet::from([(s, x)]);
        assert_eq!(manager.sources(s, b, None).as_deref(), Some(&expected));
        assert_eq!(manager.sources(s, a, None).as_deref(), Some(&expected));
        assert!(manager.services[&s].listeners[&x].contains(&listener));
        assert!(!manager.services[&s]
            .listeners
            .get(&y)
            .is_some_and(|listeners| listeners.contains(&listener)));
        assert_eq!(manager.services[&s].vars[&a].value, Value::Int { val: 1 });
        assert_eq!(manager.services[&s].vars[&b].value, Value::Int { val: 2 });

        manager
            .assign(s, y, Value::Int { val: 20 }, None)
            .await
            .unwrap();
        assert_eq!(manager.services[&s].vars[&a].value, Value::Int { val: 1 });
        assert!(manager.services[&s].vars[&a].vector_clock.is_empty());
        assert_eq!(manager.services[&s].vars[&b].value, Value::Int { val: 2 });
        assert!(manager.services[&s].vars[&b].vector_clock.is_empty());

        manager
            .assign(s, x, Value::Int { val: 7 }, None)
            .await
            .unwrap();
        let clock = HashMap::from([((s, x), 1)]);
        assert_eq!(manager.services[&s].vars[&a].value, Value::Int { val: 7 });
        assert_eq!(manager.services[&s].vars[&a].vector_clock, clock);
        assert_eq!(manager.services[&s].vars[&b].value, Value::Int { val: 8 });
        assert_eq!(manager.services[&s].vars[&b].vector_clock, clock);
    }

    #[test]
    fn imported_declaration_replacement_refreshes_unchanged_consumer_sources() {
        // Declaration-only queries isolate installation from network delivery.
        // The other tests exercise live services, listeners, and stored results.
        let mut manager = Manager::new(Interner::new());
        let ast = parse_string(
            "service A {
                var x = 1;
                var y = 1;
                pub def a = x + 1;
            }
            service B { pub def z = A.a + 1; }",
            &mut manager.interner,
        )
        .unwrap();
        manager.install_unified_ast(ast);
        let [owner, consumer, x, y, a, z] =
            ["A", "B", "x", "y", "a", "z"].map(|name| manager.interner.insert(name));
        let consumer_decl = manager.unified_ast[1].clone();
        let cross_deps = manager.declaration_graphs[&consumer].cross_deps.clone();
        assert!(manager.services.is_empty());
        assert_eq!(
            manager.declaration_graphs[&consumer].transitive_dependencies_of(z),
            HashSet::new()
        );
        assert_eq!(cross_deps[&z], HashSet::from([(owner, a)]));
        assert_eq!(
            manager.declaration_graphs[&owner].transitive_dependencies_of(a),
            HashSet::from([x])
        );

        let mut declarations = parse_string(
            "service A {
                var x = 1;
                var y = 1;
                pub def a = y + 1;
            }",
            &mut manager.interner,
        )
        .unwrap();
        declarations.push(consumer_decl.clone());
        manager.install_unified_ast(declarations);

        assert_eq!(manager.unified_ast[1], consumer_decl);
        assert_eq!(manager.declaration_graphs[&consumer].cross_deps, cross_deps);
        assert_eq!(
            manager.declaration_graphs[&owner].transitive_dependencies_of(a),
            HashSet::from([y])
        );
    }
}
