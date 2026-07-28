use super::ast::{ActionStmt, Decl, Expr, Stmt, Value};
use super::env::Env;
use super::graphs::{analysis::compute_dependencies, free_var::cross_service_deps, ServiceGraphs};
use super::interpreter::{eval, execute, EvalContext, EvalError, ExecuteEffect};
use super::tt::types::ServiceType;
use crate::net::network_layer::NetworkLayer;
use crate::net::{
    codec, Address, LockGroup, MeerkatMessage, NetworkActor, NetworkCommand, NetworkEvent,
    NetworkReply, ServiceNetId,
};
use crate::runtime::interner::{Interner, Symbol};
use crate::runtime::txn::{Transaction, TxnId, VarState, WaitKey};
use std::collections::{HashMap, HashSet};
#[cfg(not(target_arch = "wasm32"))]
use std::time::Duration;
use tokio::sync::oneshot;

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
    /// def -> {(source service, member) -> value}.
    pub dep_cache: HashMap<Symbol, HashMap<(Symbol, Symbol), Value>>,
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

pub struct Manager {
    pub services: HashMap<Symbol, Service>,
    /// Maps service name to remote address (for distributed services)
    pub remote_services: HashMap<Symbol, Address>,
    /// Network actor for distributed communication
    pub network: Option<NetworkActor>,
    /// Pending reply channels keyed by request_id
    pub pending_replies: HashMap<u64, oneshot::Sender<MeerkatMessage>>,
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
    /// This node's canonical, dialable address, set once after the network is
    /// listening. Service identities are derived from it, so they are stable for
    /// the life of the process (never empty-then-populated) and match the URL
    /// under which the node advertises its services.
    local_address: Option<String>,
    /// Enable local loopback mode
    pub local: bool,
    /// String interner
    pub interner: Interner,
    /// #24: transient cache consulted during a reactive recompute. Holds the
    /// (service, member) -> value map for the def currently being recomputed so
    /// MemberAccess resolves from cache instead of a (possibly remote) lookup.
    pub reactive_cache: Option<HashMap<(Symbol, Symbol), Value>>,
    /// #24: reply address for each remote listener, keyed by the listener's
    /// ServiceNetId, so the owner can route Updates back to it.
    pub listener_addrs: HashMap<ServiceNetId, String>,
    /// Global unified AST of all services running on this manager
    pub unified_ast: Vec<Stmt>,
    /// Global local services type environment
    pub local_services: Env<'static, ServiceType>,
}

impl Manager {
    pub fn new(interner: Interner) -> Self {
        Manager {
            services: HashMap::new(),
            remote_services: HashMap::new(),
            network: None,
            pending_replies: HashMap::new(),
            node_id: Self::random_node_id(),
            pending_txns: HashMap::new(),
            wait_queue: HashMap::new(),
            local_address: None,
            local: false,
            interner,
            reactive_cache: None,
            listener_addrs: HashMap::new(),
            unified_ast: Vec::new(),
            local_services: Env::new(None),
        }
    }

    /// Park a request on the wait queue for the contended `WaitKey`
    pub fn park_request_key(&mut self, key: WaitKey, parked: ParkedRequest) {
        self.wait_queue.entry(key).or_default().push(parked);
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

    pub async fn create_service(
        &mut self,
        name: Symbol,
        decls: Vec<Decl>,
    ) -> Result<(), EvalError> {
        let id = self.compute_service_net_id(name);
        // Register the service (with its real `ServiceNetId`) before
        // evaluating any declarations, so action closures built during
        // initialization are stamped with the correct `ServiceNetId`
        // instead of `service_net_id_for_name`'s bare-name fallback
        self.unified_ast.push(Stmt::Service {
            name,
            decls: decls.clone(),
        });

        let all_graphs = compute_dependencies(&self.unified_ast, None)
            .map_err(|e| EvalError::VarNotFound(e.format_with_interner(&self.interner)))?;
        let graphs = all_graphs
            .into_iter()
            .last()
            .unwrap_or_else(ServiceGraphs::new);

        self.services.insert(
            name,
            Service {
                id,
                name,
                vars: HashMap::new(),
                defs: HashMap::new(),
                graphs: ServiceGraphs::new(),
                listeners: HashMap::new(),
                dep_cache: HashMap::new(),
                service_lock: None,
            },
        );

        let mut env: Vec<(Symbol, Value)> = vec![];
        let svc_name = name;

        let mut txn = Transaction::new(TxnId::new(self.node_id));
        let mut init_error = None;

        for decl in decls {
            match decl {
                Decl::VarDecl { name, ty: _, val } => {
                    let value = match eval(
                        &val,
                        &env,
                        &mut EvalContext {
                            manager: self,
                            service_name: svc_name,
                            txn: Some(&mut txn),
                        },
                    )
                    .await
                    {
                        Ok(v) => v,
                        Err(e) => {
                            init_error = Some(e);
                            break;
                        }
                    };
                    env.push((name, value.clone()));
                    if let Some(service) = self.services.get_mut(&svc_name) {
                        let mut var_value = VarState::new(value);
                        var_value.latest_write_txn = Some(txn.id.clone());
                        service.vars.insert(name, var_value);
                    }
                }
                Decl::DefDecl { name, val, .. } => {
                    let value = match eval(
                        &val,
                        &env,
                        &mut EvalContext {
                            manager: self,
                            service_name: svc_name,
                            txn: Some(&mut txn),
                        },
                    )
                    .await
                    {
                        Ok(v) => v,
                        Err(e) => {
                            init_error = Some(e);
                            break;
                        }
                    };
                    env.push((name, value.clone()));
                    if let Some(service) = self.services.get_mut(&svc_name) {
                        let mut var_value = VarState::new(value);
                        var_value.latest_write_txn = Some(txn.id.clone());
                        service.vars.insert(name, var_value);
                        service.defs.insert(name, val); // store original expr
                    }
                }
                Decl::TableDecl { .. } => {
                    // we still need to release locks, so no longer return directly after
                    // encountering a TableDecl
                    init_error = Some(EvalError::NotImplemented);
                    break;
                }
            }
        }

        // #87: commit on success, abort and roll back on failure (pattern from
        // execute_action_with_txn).
        // #98: a participant commit can fail after prepare (timeout, etc.).
        // Apply local writes, then attempt every participant commit -- do not
        // stop at the first failure, or later participants are left prepared --
        // and remember the first error to surface once init otherwise succeeded.
        let mut commit_error = None;
        if init_error.is_none() {
            self.apply_committed_writes(&txn).await;
            for addr in txn.participants.iter().cloned().collect::<Vec<_>>() {
                if let Err(e) = self.send_commit(addr, &txn.id).await {
                    if commit_error.is_none() {
                        commit_error = Some(e);
                    }
                }
            }

            // #24: now that init succeeded, register listener edges so
            // a change to a member notifies the defs that depend on it
            // #98: also skip when a participant commit failed -- that
            // service is about to be rolled back below, so it must not
            // wire up listeners
            if commit_error.is_none() {
                self.update_service_graphs(svc_name, graphs).await;
            }

            // #98: a participant failed to commit after prepare. Roll
            // the local service back rather than leaving it
            // half-committed but live: abort participants and remove the
            // service, mirroring the init-failure path. The captured
            // error is returned below
            if commit_error.is_some() {
                for addr in txn.participants.iter().cloned().collect::<Vec<_>>() {
                    self.send_abort(addr, &txn.id).await;
                }
                self.services.remove(&svc_name);
            }
        } else {
            // Execution failed: discard buffered writes and abort participants.
            for addr in txn.participants.iter().cloned().collect::<Vec<_>>() {
                self.send_abort(addr, &txn.id).await;
            }
            self.services.remove(&svc_name);
        }

        // Release all locks held locally (always, even on error)
        let freed = self.all_locked_keys(&txn);
        self.release_locks(&freed, &txn.id);

        // #98: init failure takes precedence (it already rolled back above).
        // Otherwise, surface a participant commit failure rather than reporting
        // success while a remote participant may hold stranded locks.
        match init_error {
            Some(e) => Err(e),
            None => match commit_error {
                Some(e) => Err(e),
                None => Ok(()),
            },
        }
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
        let (updated_id, edges) = match self.services.get_mut(&svc_name) {
            Some(service) => {
                service.graphs = new_graphs;
                let updated_id = service.id.clone();
                let mut edges: Vec<(Symbol, Symbol, Symbol)> = Vec::new();
                for (dep_member, listener_def) in service.graphs.reactive_edges() {
                    if service.defs.contains_key(&listener_def) {
                        edges.push((svc_name, dep_member, listener_def));
                    }
                }
                for (def_name, cross_set) in &service.graphs.cross_deps {
                    for (owner, member) in cross_set {
                        edges.push((*owner, *member, *def_name));
                    }
                }
                (updated_id, edges)
            }
            None => return,
        };

        for service in self.services.values_mut() {
            for listener_set in service.listeners.values_mut() {
                listener_set.retain(|(lid, _)| *lid != updated_id);
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
        // Check if service is remote
        if self.remote_services.contains_key(&service_name) {
            return self.remote_lookup(service_name, var_name, txn).await;
        }

        // Local var read. If inside a transaction, return the cached value if
        // present, otherwise acquire a read lock lazily and cache the value.
        // Transaction state is keyed by (service id, variable) so the same name
        // in different services never collides.
        let key = (self.service_net_id_for_name(service_name), var_name);
        let mut need_read_lock: Option<TxnId> = None;
        if let Some(t) = txn.as_deref() {
            if let Some(cached) = t.read_cache.get(&key) {
                return Ok(cached.clone());
            }
            if !t.locked.contains(&key) {
                need_read_lock = Some(t.id.clone());
            }
        }
        if let Some(txn_id) = need_read_lock {
            self.acquire_read_lock(service_name, var_name, &txn_id)?;
            if let Some(t) = txn.as_deref_mut() {
                t.locked.insert(key.clone());
            }
        }

        // Return stored var value (and cache it for the transaction)
        if let Some(service) = self.services.get(&service_name) {
            if let Some(var_state) = service.vars.get(&var_name) {
                let value = var_state.value.clone();
                if let Some(t) = txn {
                    t.read_cache.insert(key, value.clone());
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
                t.read_cache.insert(key, value);
            }
            return Ok(());
        }

        // Non-transactional path: apply the write immediately and propagate.
        if let Some(service) = self.services.get_mut(&service_name) {
            if let Some(var_state) = service.vars.get_mut(&var_name) {
                var_state.value = value;
            } else {
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

    pub(crate) async fn propagate(&mut self, service_name: Symbol, changed_var: Symbol) {
        // #24: event-driven reactivity over the listener graph. A change to a
        // member notifies its listeners. For each listener we resolve its
        // service id to a local service: Some means a local listener, which we
        // recompute from current values (and cached cross-service deps) and, if
        // it changes, push back onto the worklist to cascade; None means the
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
                        if self.recompute_def(lsvc, listener_def).await {
                            worklist.push((lsvc, listener_def));
                        }
                    }
                    None => {
                        self.emit_update(&listener_id, listener_def, svc, member)
                            .await;
                    }
                }
            }
        }
    }

    /// #24: recompute `def` in `svc` from current values, seeding the reactive
    /// cache with this def's cached cross-service deps so MemberAccess resolves
    /// from cache instead of a (possibly remote) lookup. Returns whether the
    /// stored value changed.
    pub(crate) async fn recompute_def(&mut self, svc: Symbol, def: Symbol) -> bool {
        let expr = match self
            .services
            .get(&svc)
            .and_then(|s| s.defs.get(&def))
            .cloned()
        {
            Some(e) => e,
            None => {
                log::warn!(
                    "recompute_def: def '{}' not found in service '{}'",
                    self.interner.get(def),
                    self.interner.get(svc)
                );
                return false;
            }
        };
        let env: Vec<(Symbol, Value)> = self
            .services
            .get(&svc)
            .map(|s| s.vars.iter().map(|(k, v)| (*k, v.value.clone())).collect())
            .unwrap_or_default();
        let cache = self
            .services
            .get(&svc)
            .and_then(|s| s.dep_cache.get(&def))
            .cloned()
            .unwrap_or_default();

        self.reactive_cache = Some(cache);
        let result = eval(
            &expr,
            &env,
            &mut EvalContext {
                manager: self,
                service_name: svc,
                txn: None,
            },
        )
        .await;
        // The reactive cache is only valid for the single recompute above (its
        // entries are this def's cached cross-service deps), so clear it before
        // returning to avoid leaking stale entries into later evaluations.
        self.reactive_cache = None;

        let value = match result {
            Ok(v) => v,
            Err(e) => {
                log::warn!(
                    "propagation of def '{}' failed: {}",
                    self.interner.get(def),
                    e
                );
                return false;
            }
        };

        if let Some(service) = self.services.get_mut(&svc) {
            if let Some(var_state) = service.vars.get_mut(&def) {
                let differs = var_state.value != value;
                var_state.value = value;
                differs
            } else {
                service.vars.insert(def, VarState::new(value));
                true
            }
        } else {
            log::warn!(
                "recompute_def: service '{}' missing when recomputing def '{}'",
                self.interner.get(svc),
                self.interner.get(def)
            );
            false
        }
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
        let value = match self
            .services
            .get(&svc)
            .and_then(|s| s.vars.get(&member))
            .map(|vs| vs.value.clone())
        {
            Some(v) => v,
            None => {
                log::warn!(
                    "emit_update: member '{}' not found in service '{}'",
                    self.interner.get(member),
                    self.interner.get(svc)
                );
                return;
            }
        };
        let net_val = match codec::encode_value(&value, &self.interner) {
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
            .map(|s| s.vars.contains_key(&member_sym))
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

        let current = self
            .services
            .get(&service_sym)
            .and_then(|s| s.vars.get(&member_sym))
            .map(|vs| vs.value.clone());
        if let Some(value) = current {
            if let Ok(net_val) = codec::encode_value(&value, &self.interner) {
                let msg = MeerkatMessage::Update {
                    listener_service: listener_id.0.clone(),
                    listener_def: self.interner.get(listener_def_sym).to_string(),
                    source_service: self.interner.get(service_sym).to_string(),
                    member: self.interner.get(member_sym).to_string(),
                    value: net_val,
                };
                self.send_oneway(Address::new(&reply_to), msg).await;
            }
        }
    }

    /// #24 listener side: a remote member changed (or its initial value). Cache
    /// it, recompute the dependent def from cache, and cascade to its listeners.
    pub async fn handle_update(
        &mut self,
        listener_id: ServiceNetId,
        listener_def_sym: Symbol,
        source_sym: Symbol,
        member_sym: Symbol,
        value: crate::net::ast::NetValue,
    ) {
        let value = match codec::decode_value(value, &mut self.interner) {
            Ok(v) => v,
            Err(_) => return,
        };

        let listener_svc = self
            .services
            .iter()
            .find(|(_, s)| s.id == listener_id)
            .map(|(name, _)| *name);
        let listener_svc = match listener_svc {
            Some(n) => n,
            None => return,
        };

        if let Some(svc) = self.services.get_mut(&listener_svc) {
            svc.dep_cache
                .entry(listener_def_sym)
                .or_default()
                .insert((source_sym, member_sym), value);
        }

        if self.recompute_def(listener_svc, listener_def_sym).await {
            self.propagate(listener_svc, listener_def_sym).await;
        }
    }

    /// Drain all pending network events and dispatch each to the matching
    /// oneshot channel in pending_replies. Non-matching events are dropped.
    pub async fn dispatch_network_events(&mut self) {
        // Scope the network borrow to just the receive (via the inner match) so
        // the rest of the loop body can take &mut self for the reactive handlers.
        while let Some(event) = match self.network.as_mut() {
            Some(n) => n.try_recv_event(),
            None => None,
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
                    } => {
                        // #24: validate + intern wire names through codec; skip
                        // the message if any identifier fails validation.
                        let (listener_def_sym, source_sym, member_sym) = match codec::decode_update(
                            &listener_def,
                            &source_service,
                            &member,
                            &mut self.interner,
                        ) {
                            Ok(syms) => syms,
                            Err(_) => continue,
                        };
                        self.handle_update(
                            ServiceNetId(listener_service),
                            listener_def_sym,
                            source_sym,
                            member_sym,
                            value,
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
                        let res = match codec::decode_update_service_request(
                            &service_name,
                            &source,
                            &mut self.interner,
                        ) {
                            Ok((_sym, validated_source)) => {
                                match crate::runtime::parser::parse_string(
                                    &validated_source,
                                    &mut self.interner,
                                ) {
                                    Ok(stmts) => {
                                        let mut txn = match txn_id {
                                            Some(tid) => {
                                                crate::runtime::update::Transaction::new_with_id(
                                                    tid, stmts,
                                                )
                                            }
                                            None => crate::runtime::update::Transaction::new(stmts),
                                        };
                                        Box::pin(txn.poll(self)).await.map_err(|e| e.to_string())
                                    }
                                    Err(e) => Err(e),
                                }
                            }
                            Err(e) => Err(e.to_string()),
                        };
                        let error = res.err();
                        let reply_msg = MeerkatMessage::UpdateServiceResponse { request_id, error };
                        let target_addr = if !reply_to.is_empty() { reply_to } else { peer };
                        if !target_addr.is_empty() {
                            self.send_oneway(Address::new(&target_addr), reply_msg)
                                .await;
                        }
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
                        self.send_oneway(Address::new(&reply_to), reply_msg).await;
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
        let service_str = self.interner.get(service);
        let addr_str = full_url.0.trim_end_matches(&format!("/{}", service_str));
        Ok(Address::new(addr_str))
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
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);

        // Remote reads are always served by the owning node, which holds this
        // transaction's buffered writes and read locks. We deliberately do not
        // cache the result on the requesting side: a def's value can change
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
            if let Some(t) = txn {
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
            MeerkatMessage::LookupResponse { value, .. } => {
                let val = codec::decode_value(value, &mut self.interner)
                    .map_err(|e| EvalError::LocalDispatchFailed(e.to_string()))?;
                Ok(val)
            }
            MeerkatMessage::LookupError { error, .. } => Err(EvalError::LocalDispatchFailed(error)),
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
    ) -> Result<Value, EvalError> {
        let mut txn = self
            .pending_txns
            .remove(&tid)
            .unwrap_or_else(|| Transaction::new(tid.clone()));
        match self.lookup(member, service, Some(&mut txn)).await {
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
        txn: Option<&mut Transaction>,
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
            if let Some(t) = txn {
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
            MeerkatMessage::ActionResponse { success, error, .. } => {
                if success {
                    // Participant already registered above; nothing more to do.
                    Ok(())
                } else {
                    Err(EvalError::LocalDispatchFailed(
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

    /// Release all locks held by `txn_id` on the given variables (and service locks)
    /// Release all locks (both member and service level) held by a transaction
    pub fn release_all_locks(&mut self, txn: &Transaction) {
        let keys = self.all_locked_keys(txn);
        self.release_locks(&keys, &txn.id);
    }

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
        const MAX_WAIT_DIE_RETRIES: u32 = 10;
        let mut txn_id = TxnId::new(self.node_id);

        loop {
            let mut txn = Transaction::new(txn_id.clone());

            let mut env: Vec<(Symbol, Value)> = initial_env.to_vec();
            let mut exec_error: Option<EvalError> = None;
            for stmt in stmts {
                match execute(stmt, &env, self, service_name, Some(&mut txn)).await {
                    Ok(ExecuteEffect::Binding(name, val)) => env.push((name, val)),
                    Ok(_) => {}
                    Err(e) => {
                        exec_error = Some(e);
                        break;
                    }
                }
            }

            if matches!(exec_error, Some(EvalError::WaitDieAbort(_))) {
                for addr in txn.participants.iter().cloned().collect::<Vec<_>>() {
                    self.send_abort(addr, &txn.id).await;
                }
                let freed = self.all_locked_keys(&txn);
                self.release_locks(&freed, &txn.id);
                if txn_id.iteration < MAX_WAIT_DIE_RETRIES {
                    txn_id = txn_id.retry();
                    continue;
                }
                return Err(exec_error.unwrap());
            }

            if exec_error.is_none() {
                self.apply_committed_writes(&txn).await;
                for addr in txn.participants.iter().cloned().collect::<Vec<_>>() {
                    let _ = self.send_commit(addr, &txn.id).await;
                }
            } else {
                for addr in txn.participants.iter().cloned().collect::<Vec<_>>() {
                    self.send_abort(addr, &txn.id).await;
                }
            }

            let freed = self.all_locked_keys(&txn);
            self.release_locks(&freed, &txn.id);

            return match exec_error {
                Some(e) => Err(e),
                None => Ok(()),
            };
        }
    }

    /// Apply a transaction's buffered writes to the owning services, record
    /// the writing transaction, and propagate to dependent definitions
    ///
    /// Shared by local commit and by a participant committing on a remote
    /// `Commit` message
    ///
    /// Infallible: once we are applying writes the transaction is
    /// committed, so there is no going back. Propagation is best-effort
    async fn apply_committed_writes(&mut self, txn: &Transaction) {
        let writes: Vec<((ServiceNetId, Symbol), Value)> = txn
            .written
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let txn_id = txn.id.clone();
        for ((sid, var), value) in &writes {
            if let Some(service) = self.service_by_net_id_mut(sid) {
                if let Some(var_state) = service.vars.get_mut(var) {
                    var_state.value = value.clone();
                    var_state.latest_write_txn = Some(txn_id.clone());
                }
            }
        }
        for ((sid, var), _) in &writes {
            if let Some(name) = self.service_name_for_net_id(sid) {
                self.propagate(name, *var).await;
            }
        }
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
        let mut env: Vec<(Symbol, Value)> = initial_env.to_vec();
        let mut exec_error: Option<EvalError> = None;
        for stmt in stmts {
            match execute(stmt, &env, self, service_name, Some(&mut txn)).await {
                Ok(ExecuteEffect::Binding(name, val)) => env.push((name, val)),
                Ok(_) => {}
                Err(e) => {
                    exec_error = Some(e);
                    break;
                }
            }
        }
        if let Some(e) = exec_error {
            if matches!(e, EvalError::WaitOn(_)) {
                self.pending_txns.insert(tid, txn);
                return Err(e);
            }
            self.discard_failed_participant_txn(txn).await;
            return Err(e);
        }
        self.pending_txns.insert(tid, txn);
        Ok(())
    }

    /// Participant side: apply and release a held transaction on `Commit`
    pub async fn commit_participant(&mut self, tid: &TxnId) -> Result<HashSet<WaitKey>, EvalError> {
        if let Some(txn) = self.pending_txns.remove(tid) {
            let freed = self.all_locked_keys(&txn);
            self.apply_committed_writes(&txn).await;
            self.release_locks(&freed, &txn.id);
            let mut forward_err = None;
            for addr in txn.participants.iter().cloned().collect::<Vec<_>>() {
                if let Err(e) = self.send_commit(addr, tid).await {
                    forward_err = Some(e);
                }
            }
            match forward_err {
                Some(e) => Err(e),
                None => Ok(freed),
            }
        } else {
            Ok(HashSet::new())
        }
    }

    /// Centralized cleanup for a participant transaction that encountered a terminal failure
    ///
    /// Releases local locks and aborts all sub-participants before dropping the transaction
    ///
    /// Args:
    ///     txn (Transaction): The transaction context
    ///
    /// Returns:
    ///     HashSet<WaitKey>: The set of freed wait keys
    async fn discard_failed_participant_txn(&mut self, txn: Transaction) -> HashSet<WaitKey> {
        let freed = self.all_locked_keys(&txn);
        self.release_locks(&freed, &txn.id);
        for addr in txn.participants {
            self.send_abort(addr, &txn.id).await;
        }
        freed
    }

    /// Participant side: discard and release a held transaction on `Abort`, and
    /// forward the abort down the chain to any sub-participants
    pub async fn abort_participant(&mut self, tid: &TxnId) -> HashSet<WaitKey> {
        if let Some(txn) = self.pending_txns.remove(tid) {
            self.discard_failed_participant_txn(txn).await
        } else {
            HashSet::new()
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
                let freed = self.all_locked_keys(&txn);
                self.release_locks(&freed, &txn.id);
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
                        return Err(EvalError::LocalDispatchFailed(error.unwrap_or_else(|| {
                            "Lock request rejected by remote node".to_string()
                        })));
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
    ) -> Result<(), EvalError> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_COMMIT_ID: AtomicU64 = AtomicU64::new(1);
        let request_id = NEXT_COMMIT_ID.fetch_add(1, Ordering::SeqCst);
        let reply_to = self.local_reply_addr().await;
        let msg = MeerkatMessage::Commit {
            request_id,
            txn_id: tid.clone(),
            reply_to,
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

        // simulate a remote Update saying s1.y = 10
        let z_sym = tc.manager.interner.insert("z");
        tc.manager
            .handle_update(ServiceNetId(s2_id), z_sym, tc.s1, tc.y, net_val)
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

        // The younger holder aborts, freeing `x`
        let freed = tc.manager.abort_participant(&younger).await;
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
}
