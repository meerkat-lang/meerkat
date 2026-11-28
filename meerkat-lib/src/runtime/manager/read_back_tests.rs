//! Real managers and real TCP dispatch, including recursive abort forwarding.
use super::*;
use crate::net::NodeType;
use crate::runtime::txn::VarLock;
use crate::runtime::{parser::parse_string, Node};
use futures::future::join_all;

async fn node(name: &str) -> (Manager, Address) {
    let mut net = NetworkActor::new(NodeType::Server).await.unwrap();
    let NetworkReply::ListenSuccess { addr } = net
        .handle_command(NetworkCommand::Listen {
            addr: Address::new("/ip4/127.0.0.1/tcp/0"),
        })
        .await
    else {
        panic!("listen failed")
    };
    let addr = Address::new(format!("{}/p2p/{}", addr.0, net.local_peer_id()));
    let mut node = Node::new();
    node.unified_ast = parse_string(
        &format!("service {name} {{ var x = 0; var y = 0; }}"),
        &mut node.interner,
    )
    .unwrap();
    node.static_checks().unwrap();
    let ast = node.unified_ast.clone();
    let manager = node
        .on_manager_startup(true, Some(net), HashMap::new(), &ast)
        .await
        .unwrap();
    (manager, addr)
}

struct Cluster {
    origin: Manager,
    peers: Vec<Manager>,
    addresses: Vec<Address>,
}

impl Cluster {
    async fn new() -> Self {
        let (origin, _) = node("s").await;
        let mut peers = Vec::new();
        let mut addresses = Vec::new();
        for name in ["a", "b", "c", "d"] {
            let (peer, addr) = node(name).await;
            peers.push(peer);
            addresses.push(addr);
        }
        // Alias a,b,c,d to the one local service on the corresponding node.
        for peer in &mut peers {
            for (alias, addr) in ["a", "b", "c", "d"].into_iter().zip(&addresses) {
                let symbol = peer.interner.insert(alias);
                if peer.services.contains_key(&symbol) {
                    continue;
                }
                peer.remote_services
                    .insert(symbol, Address::new(format!("{}/{alias}", addr.0)));
            }
        }
        Self {
            origin,
            peers,
            addresses,
        }
    }

    fn call(&self, index: usize, stmts: Vec<ActionStmt>) -> ActionStmt {
        ActionStmt::Do(Expr::Literal {
            val: Value::ActionClosure {
                stmts,
                env: vec![],
                service_net_id: ServiceNetId(format!(
                    "{}/{}",
                    self.addresses[index].0,
                    ["a", "b", "c", "d"][index]
                )),
            },
        })
    }

    fn write(&mut self, n: i32) -> ActionStmt {
        ActionStmt::Assign {
            name: self.origin.interner.insert("x"),
            expr: Expr::Literal {
                val: Value::Int { val: n },
            },
        }
    }

    fn read_a(&mut self) -> ActionStmt {
        ActionStmt::Expr(Expr::ServiceAccess {
            service: self.origin.interner.insert("a"),
            member: self.origin.interner.insert("y"),
        })
    }

    async fn run(&mut self, stmts: &[ActionStmt]) -> Result<(), EvalError> {
        let s = self.origin.interner.insert("s");
        tokio::select! {
            result = tokio::time::timeout(Duration::from_secs(5), self.origin.execute_action(s, stmts)) => result.expect("whole operation exceeded 5s (reply timeout is 15s)"),
            _ = join_all(self.peers.iter_mut().map(serve)) => unreachable!(),
        }
    }

    async fn assert_clean(&mut self) {
        for manager in std::iter::once(&mut self.origin).chain(&mut self.peers) {
            assert!(
                manager.pending_txns.is_empty(),
                "pending transaction leaked"
            );
            let s = *manager.services.keys().next().unwrap();
            for value in manager.services[&s].vars.values() {
                assert!(
                    matches!(value.lock, VarLock::Unlocked),
                    "lock leaked: {:?}",
                    value.lock
                );
                assert_eq!(
                    value.value,
                    Value::Int { val: 0 },
                    "failed transaction published a write"
                );
            }
            let x = manager.interner.insert("x");
            manager
                .execute_action(
                    s,
                    &[ActionStmt::Assign {
                        name: x,
                        expr: Expr::Literal {
                            val: Value::Int { val: 7 },
                        },
                    }],
                )
                .await
                .unwrap();
        }
    }
}

async fn serve(manager: &mut Manager) {
    loop {
        manager.serve_network_events().await;
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

#[tokio::test]
async fn real_network_readback_cycles() {
    // All statements are encoded at the origin and decoded by each actual node.
    for shape in ["direct", "action", "long", "earlier", "forwarded"] {
        let mut c = Cluster::new().await;
        let write = c.write(10);
        let read = c.read_a();
        let back = if shape == "action" {
            c.call(0, vec![write.clone()])
        } else {
            read.clone()
        };
        let mut body = vec![write.clone()];
        if shape == "forwarded" {
            body.push(c.call(3, vec![write.clone()]));
        }
        body.push(if matches!(shape, "long" | "earlier") {
            c.call(2, vec![back])
        } else {
            back
        });
        let call = c.call(0, vec![write.clone(), c.call(1, body)]);
        let mut stmts = vec![];
        if shape == "forwarded" {
            stmts.push(c.call(3, vec![read.clone()]));
        }
        if shape == "earlier" {
            stmts.push(c.call(1, vec![read]));
        }
        stmts.push(call);
        let start = std::time::Instant::now();
        let error = c.run(&stmts).await.unwrap_err();
        assert!(
            matches!(error, EvalError::RunningTransaction(_)),
            "{shape}: {error}"
        );
        assert!(
            error.to_string().contains("read-back"),
            "original diagnostic lost: {error}"
        );
        eprintln!("{shape}: {:?}: {error}", start.elapsed());
        c.assert_clean().await;
    }
}

#[test]
fn registration_lifecycle() {
    let running = RunningTransactions::default();
    let tid = TxnId::new(1);
    let outer = running.enter(&tid);
    {
        let inner = running.enter(&tid);
        assert!(running.doom(&tid));
        assert!(inner.finish(Ok(())).is_err());
    }
    assert!(running.contains(&tid));
    assert!(outer.finish(Ok(())).is_err());
    drop(outer);
    assert!(!running.contains(&tid));
    assert!(running.enter(&tid).finish(Ok(())).is_ok());
}

#[tokio::test]
async fn failed_nested_lock_request_preserves_outer_reservations() {
    for wait in [true, false] {
        for (outer_reads, nested_writes) in
            [(true, true), (true, false), (false, true), (false, false)]
        {
            let (mut manager, _) = node("s").await;
            let declarations = parse_string(
                "service s { var y = 0; def z = y; def x = z; var prior = 0; }
                 service held { var v = 0; } service freshlock { var v = 0; }",
                &mut manager.interner,
            )
            .unwrap();
            // x -> z -> y forces acquisition of the overlapping lock and a
            // new lock before encountering contention, regardless of map order.
            manager.unified_ast = declarations.clone();
            for stmt in declarations {
                let Stmt::Service { name, decls } = stmt else {
                    unreachable!()
                };
                manager.create_service(name, decls).await.unwrap();
            }
            let s = manager.interner.insert("s");
            let x = manager.interner.insert("x");
            let y = manager.interner.insert("y");
            let z = manager.interner.insert("z");
            let prior = manager.interner.insert("prior");
            let held = manager.interner.insert("held");
            let fresh = manager.interner.insert("freshlock");
            let tid = TxnId::new(manager.node_id);
            let foreign = TxnId {
                timestamp: if wait { u128::MAX } else { 0 },
                node_id: 1,
                iteration: 0,
            };
            let mut outer = manager.checkout_transaction(tid.clone());
            if outer_reads {
                manager.acquire_read_lock(s, x, &tid).unwrap();
            } else {
                manager.acquire_write_lock(s, x, &tid, false).unwrap();
            }
            outer.locked.insert((manager.service_net_id_for_name(s), x));
            manager.acquire_service_lock(held, &tid).unwrap();
            outer
                .service_locked
                .insert(manager.service_net_id_for_name(held));
            manager.acquire_write_lock(s, y, &foreign, false).unwrap();
            let member_group = |member: &str| LockGroup {
                service_level_lock: false,
                reads: HashSet::new(),
                writes: HashSet::from([member.into()]),
            };
            // A previous successful nested request has handed this reservation
            // to the outer owner. A subsequent failure must not collect it.
            manager
                .handle_lock_request(
                    tid.clone(),
                    HashMap::from([("s".into(), member_group("prior"))]),
                )
                .await
                .unwrap();
            let service_group = LockGroup {
                service_level_lock: true,
                reads: HashSet::new(),
                writes: HashSet::new(),
            };
            let mut overlapping = member_group("x");
            if !nested_writes {
                overlapping.reads = std::mem::take(&mut overlapping.writes);
            }
            let result = manager
                .handle_lock_request(
                    tid.clone(),
                    HashMap::from([
                        ("s".into(), overlapping),
                        ("held".into(), service_group.clone()),
                        ("freshlock".into(), service_group),
                    ]),
                )
                .await;
            if wait {
                assert!(matches!(result, Err(EvalError::WaitOn(_))), "{result:?}");
            } else {
                assert!(
                    matches!(result, Err(EvalError::WaitDieAbort(_))),
                    "{result:?}"
                );
            }
            if outer_reads && !nested_writes {
                assert!(matches!(&manager.services[&s].vars[&x].lock,
                    VarLock::ReadLocked(owners) if owners.contains(&tid)));
            } else {
                assert!(
                    matches!(&manager.services[&s].vars[&x].lock,
                    VarLock::WriteLocked(owner) if owner == &tid),
                    "nested failure released the outer reservation"
                );
            }
            assert!(matches!(&manager.services[&s].vars[&prior].lock,
                VarLock::WriteLocked(owner) if owner == &tid));
            assert_eq!(manager.services[&held].service_lock.as_ref(), Some(&tid));
            assert!(manager.services[&fresh].service_lock.is_none());
            assert!(matches!(
                manager.services[&s].vars[&z].lock,
                VarLock::Unlocked
            ));
            assert!(matches!(&manager.services[&s].vars[&y].lock,
                VarLock::WriteLocked(owner) if owner == &foreign));
            assert!(manager.pending_txns.is_empty());
            assert!(manager.running.contains(&tid));
            outer.finish(Ok(())).unwrap();
            manager.release_all_locks(&outer);
            drop(outer);
            assert!(!manager.running.contains(&tid));
            for member in [x, prior] {
                assert!(matches!(
                    manager.services[&s].vars[&member].lock,
                    VarLock::Unlocked
                ));
            }
            assert!(manager.services[&held].service_lock.is_none());
        }
    }
}

#[tokio::test]
async fn update_reports_contention_without_exposing_wait_keys() {
    for whole_service in [false, true] {
        let (mut manager, _) = node("s").await;
        let s = manager.interner.insert("s");
        let x = manager.interner.insert("x");
        let tid = TxnId::new(manager.node_id);
        let holder = TxnId {
            timestamp: u128::MAX,
            node_id: 1,
            iteration: 0,
        };
        if whole_service {
            manager.acquire_service_lock(s, &holder).unwrap();
        } else {
            manager.acquire_write_lock(s, x, &holder, false).unwrap();
        }
        let updates = parse_string("update s { var x = 7; }", &mut manager.interner).unwrap();
        let error = crate::runtime::update::Transaction::new_with_id(tid.clone(), updates)
            .apply(&mut manager)
            .await
            .unwrap_err();
        let EvalError::RuntimeError(message) = error else {
            panic!("update exposed a control-flow error: {error:?}")
        };
        assert_eq!(
            message,
            if whole_service {
                "Code update blocked waiting for service 's'"
            } else {
                "Code update blocked waiting for 's.x'"
            }
        );
        assert_eq!(manager.services[&s].vars[&x].value, Value::Int { val: 0 });
        assert!(manager.pending_txns.is_empty());
        assert!(!manager.running.contains(&tid));
        if whole_service {
            assert_eq!(manager.services[&s].service_lock.as_ref(), Some(&holder));
        } else {
            assert!(matches!(&manager.services[&s].vars[&x].lock,
                VarLock::WriteLocked(owner) if owner == &holder));
        }
    }
}

async fn receive(net: &mut NetworkActor) -> MeerkatMessage {
    loop {
        match net.try_recv_event() {
            Some(NetworkEvent::MessageReceived { msg, .. }) => return msg,
            Some(NetworkEvent::SendFailed { error, .. }) => {
                panic!("network send failed: {error:?}")
            }
            _ => (),
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

async fn send(net: &mut NetworkActor, to: &str, msg: MeerkatMessage) {
    let result = net
        .handle_command(NetworkCommand::SendMessage {
            addr: Address::new(to),
            msg,
        })
        .await;
    assert!(!matches!(result, NetworkReply::Failure(_)), "{result:?}");
}

/// A protocol probe deliberately replies success after requesting abort. It
/// tests the doomed check independently of ordinary error propagation. In the
/// late-abort case it probes the boundary after local commit, which this change
/// deliberately leaves under the existing #191 behavior.
async fn abort_then_success(peer: &mut NetworkActor, late: bool) -> String {
    let MeerkatMessage::ActionRequest {
        request_id,
        reply_to,
        txn_id: Some(tid),
        ..
    } = receive(peer).await
    else {
        panic!("expected action")
    };
    let address = peer_address(peer).await;
    if late {
        send(
            peer,
            &reply_to,
            MeerkatMessage::ActionResponse {
                request_id,
                success: true,
                error: None,
                history: vec![],
                writes: vec![],
            },
        )
        .await;
        let MeerkatMessage::Commit { request_id, .. } = receive(peer).await else {
            panic!("expected commit")
        };
        send(
            peer,
            &reply_to,
            MeerkatMessage::Abort {
                request_id: 9002,
                txn_id: tid,
                reply_to: address,
            },
        )
        .await;
        send(
            peer,
            &reply_to,
            MeerkatMessage::CommitResponse {
                request_id,
                success: true,
                error: None,
            },
        )
        .await;
    } else {
        let address = peer_address(peer).await;
        // This unrelated lookup must remain deferred throughout the wait.
        send(
            peer,
            &reply_to,
            MeerkatMessage::LookupRequest {
                request_id: 9001,
                service: "s".into(),
                member: "y".into(),
                reply_to: address.clone(),
                txn_id: Some(TxnId::new(999)),
            },
        )
        .await;
        send(
            peer,
            &reply_to,
            MeerkatMessage::Abort {
                request_id: 9002,
                txn_id: tid,
                reply_to: address,
            },
        )
        .await;
        assert!(
            matches!(
                receive(peer).await,
                MeerkatMessage::AbortResponse { request_id: 9002 }
            ),
            "unrelated request was not deferred"
        );
        send(
            peer,
            &reply_to,
            MeerkatMessage::ActionResponse {
                request_id,
                success: true,
                error: None,
                history: vec![],
                writes: vec![],
            },
        )
        .await;
        let MeerkatMessage::Abort {
            request_id,
            reply_to,
            ..
        } = receive(peer).await
        else {
            panic!("expected originator cleanup")
        };
        send(
            peer,
            &reply_to,
            MeerkatMessage::AbortResponse { request_id },
        )
        .await;
    }
    reply_to
}

async fn peer_address(peer: &mut NetworkActor) -> String {
    let NetworkReply::LocalAddresses { addrs } =
        peer.handle_command(NetworkCommand::GetLocalAddresses).await
    else {
        panic!("local addresses")
    };
    format!("{}/p2p/{}", addrs[0].0, peer.local_peer_id())
}

#[tokio::test]
async fn doomed_success_and_commit_boundary() {
    for late in [false, true] {
        let mut c = Cluster::new().await;
        let mut peer = c.peers.remove(0).network.take().unwrap();
        let write = c.write(10);
        let stmts = vec![write, c.call(0, vec![])];
        let s = c.origin.interner.insert("s");
        let (result, _) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(
                c.origin.execute_action(s, &stmts),
                abort_then_success(&mut peer, late)
            )
        })
        .await
        .expect("protocol probe timed out");
        let x = c.origin.interner.insert("x");
        assert_eq!(result.is_ok(), late, "{result:?}");
        assert_eq!(
            c.origin.services[&s].vars[&x].value,
            Value::Int {
                val: if late { 10 } else { 0 }
            }
        );
        assert!(matches!(
            c.origin.services[&s].vars[&x].lock,
            VarLock::Unlocked
        ));
        // The deferred request becomes serviceable after the execution exits.
        c.origin.serve_network_events().await;
        let reply = tokio::time::timeout(Duration::from_secs(2), receive(&mut peer))
            .await
            .unwrap();
        if late {
            assert!(matches!(
                reply,
                MeerkatMessage::AbortResponse { request_id: 9002 }
            ));
        } else {
            assert!(matches!(
                reply,
                MeerkatMessage::LookupResponse {
                    request_id: 9001,
                    ..
                }
            ));
            // Discard the unrelated read's snapshot as the other originator would.
            let tids: Vec<_> = c.origin.pending_txns.keys().cloned().collect();
            for tid in tids {
                c.origin.abort_participant(&tid).await;
            }
        }
        assert!(c.origin.pending_txns.is_empty());
    }
}

#[tokio::test]
async fn dropped_future_and_doomed_park() {
    let running = RunningTransactions::default();
    let tid = TxnId::new(1);
    let future = async {
        let mut registration = running.checkout(Transaction::new(tid.clone()));
        running.doom(&tid);
        let wait = EvalError::WaitOn(WaitKey::Service(ServiceNetId("s".into())));
        assert!(matches!(
            registration.finish::<()>(Err(wait)),
            Err(EvalError::RunningTransaction(_))
        ));
        assert!(matches!(
            registration.finish::<()>(Err(EvalError::WaitDieAbort("conflict".into()))),
            Err(EvalError::RunningTransaction(_))
        ));
        std::future::pending::<()>().await;
    };
    assert!(tokio::time::timeout(Duration::from_millis(1), future)
        .await
        .is_err());
    assert!(!running.contains(&tid));
    assert!(running.enter(&tid).finish(Ok(())).is_ok());
}

#[tokio::test]
async fn cycle_between_idle_participants_during_abort() {
    let mut c = Cluster::new().await;
    let read_a = c.read_a();
    let read_b = ActionStmt::Expr(Expr::ServiceAccess {
        service: c.origin.interner.insert("b"),
        member: c.origin.interner.insert("y"),
    });
    let stmts = vec![
        c.call(0, vec![read_b]),
        c.call(1, vec![read_a]),
        ActionStmt::Assert(
            Expr::Literal {
                val: Value::Bool { val: false },
            },
            "intentional failure".into(),
        ),
    ];
    assert!(matches!(
        c.run(&stmts).await,
        Err(EvalError::AssertionError(_))
    ));
    c.assert_clean().await;
}

#[tokio::test]
async fn participant_parking_clears_registration() {
    let (mut manager, _) = node("s").await;
    let s = manager.interner.insert("s");
    let x = manager.interner.insert("x");
    let tid = TxnId::new(manager.node_id);
    manager
        .services
        .get_mut(&s)
        .unwrap()
        .vars
        .get_mut(&x)
        .unwrap()
        .lock = VarLock::WriteLocked(TxnId {
        timestamp: u128::MAX,
        node_id: 1,
        iteration: 0,
    });
    let stmt = ActionStmt::Assign {
        name: x,
        expr: Expr::Literal {
            val: Value::Int { val: 1 },
        },
    };
    assert!(matches!(
        manager.remote_read_participant(s, x, tid.clone()).await,
        Err(EvalError::WaitOn(_))
    ));
    assert!(!manager.running.contains(&tid));
    assert!(manager.pending_txns.contains_key(&tid));
    assert!(matches!(
        manager
            .handle_lock_request(
                tid.clone(),
                HashMap::from([(
                    "s".into(),
                    LockGroup {
                        service_level_lock: false,
                        reads: HashSet::from(["x".into()]),
                        writes: HashSet::new(),
                    }
                )])
            )
            .await,
        Err(EvalError::WaitOn(_))
    ));
    assert!(!manager.running.contains(&tid));
    assert!(manager.pending_txns.contains_key(&tid));
    assert!(matches!(
        manager
            .execute_action_participant(s, &[stmt], &[], tid.clone())
            .await,
        Err(EvalError::WaitOn(_))
    ));
    assert!(!manager.running.contains(&tid));
    assert!(manager.pending_txns.contains_key(&tid));
    manager
        .services
        .get_mut(&s)
        .unwrap()
        .vars
        .get_mut(&x)
        .unwrap()
        .lock = VarLock::Unlocked;
    manager
        .remote_read_participant(s, x, tid.clone())
        .await
        .unwrap();
    assert!(!manager.running.contains(&tid));
    manager.abort_participant(&tid).await;
    assert!(!manager.running.contains(&tid));
    assert!(manager.pending_txns.is_empty());
}

/// Observational probe, NOT acceptance of prompt cleanup. Inject Abort through
/// an established peer while a real child is parked. The execution stays doomed
/// and waiting until the child's lock is released; no code failure frees it.
#[tokio::test]
async fn abort_ack_precedes_cleanup_while_child_is_parked() {
    let mut c = Cluster::new().await;
    let write = c.write(10);
    let stmts = vec![c.call(0, vec![write.clone(), c.call(1, vec![write])])];
    let mut a = c.peers.remove(0);
    let mut b = c.peers.remove(0);
    let running = a.running.clone();
    let b_svc = b.interner.insert("b");
    let x = b.interner.insert("x");
    let mut holder = Transaction::new(TxnId {
        timestamp: u128::MAX,
        node_id: 1,
        iteration: 0,
    });
    b.assign(b_svc, x, Value::Int { val: 1 }, &mut holder)
        .unwrap();
    let (parked_tx, parked_rx) = oneshot::channel();
    let (ack_tx, ack_rx) = oneshot::channel();
    let (release_tx, mut release_rx) = oneshot::channel();
    let b_server = async {
        let mut inject = Some((parked_tx, ack_tx));
        loop {
            b.serve_network_events().await;
            if !b.wait_queue.is_empty() {
                if let Some((parked_tx, ack_tx)) = inject.take() {
                    let tid = b.wait_queue.values().next().unwrap()[0].tid().clone();
                    b.keepalive_waiters().await;
                    b.pending_replies.insert(9002, ack_tx);
                    let reply_to = b.local_reply_addr().await;
                    b.send_oneway(
                        c.addresses[0].clone(),
                        MeerkatMessage::Abort {
                            request_id: 9002,
                            txn_id: tid.clone(),
                            reply_to,
                        },
                    )
                    .await;
                    parked_tx.send(tid).unwrap();
                }
            }
            if release_rx.try_recv().is_ok() {
                b.release_all_locks(&holder);
                b.wake_freed().await;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    };
    let observer = async {
        let tid = parked_rx.await.unwrap();
        assert!(matches!(
            ack_rx.await.unwrap(),
            MeerkatMessage::AbortResponse { request_id: 9002 }
        ));
        assert!(running.check(&tid).is_err());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            running.contains(&tid),
            "acknowledgement did not finish the waiting execution"
        );
        release_tx.send(()).unwrap();
        tid
    };
    let s = c.origin.interner.insert("s");
    let driver = async { tokio::join!(c.origin.execute_action(s, &stmts), observer) };
    let (result, tid) = tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(5), driver) => result.unwrap(),
        _ = serve(&mut a) => unreachable!(),
        _ = b_server => unreachable!(),
    };
    assert!(
        matches!(result, Err(EvalError::RunningTransaction(_))),
        "{result:?}"
    );
    assert!(!running.contains(&tid));
    for manager in [&a, &b] {
        assert!(manager.pending_txns.is_empty());
        for service in manager.services.values() {
            for var in service.vars.values() {
                assert!(matches!(var.lock, VarLock::Unlocked));
                assert_eq!(var.value, Value::Int { val: 0 });
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Entry {
    Initialization,
    Replacement,
    Update,
    Locks,
}

#[derive(Clone, Copy, Debug)]
enum Outcome {
    Rejection,
    AbortThenSuccess,
    AbortThenWaitDie,
    Success,
}

/// Probe the extra entry points without needing compute-on-read. The peer
/// attempts a real network callback during a lookup or cascading lock wait.
#[tokio::test]
async fn initialization_update_and_lock_request_lifetimes() {
    for entry in [
        Entry::Initialization,
        Entry::Replacement,
        Entry::Update,
        Entry::Locks,
    ] {
        for outcome in [
            Outcome::Rejection,
            Outcome::AbortThenSuccess,
            Outcome::AbortThenWaitDie,
            Outcome::Success,
        ] {
            let (mut manager, _) = node("s").await;
            let (mut remote, address) = node("r").await;
            let (mut dependency, dependency_address) = node("t").await;
            let t = manager.interner.insert("t");
            manager
                .remote_services
                .insert(t, Address::new(format!("{}/t", dependency_address.0)));
            let held = parse_string("service held { var x = 0; }", &mut manager.interner).unwrap();
            let Stmt::Service { name, decls } = held[0].clone() else {
                unreachable!()
            };
            manager.create_service(name, decls).await.unwrap();
            let mut peer = remote.network.take().unwrap();
            let r = manager.interner.insert("r");
            manager
                .remote_services
                .insert(r, Address::new(format!("{}/r", address.0)));
            // Imported code for static checks; the service instance lives only
            // on the peer, so evaluation and eager locks still use the wire.
            let imported = parse_string("service r { var x = 0; }", &mut manager.interner).unwrap();
            manager.unified_ast.extend(imported);
            let updates = parse_string("update s { var x = r.x; }", &mut manager.interner).unwrap();
            let init =
                parse_string("service fresh { var x = r.x; }", &mut manager.interner).unwrap();
            let Stmt::Service { name: fresh, decls } = init[0].clone() else {
                unreachable!()
            };
            let s = manager.interner.insert("s");
            if matches!(entry, Entry::Replacement) && !matches!(outcome, Outcome::Success) {
                let x = manager.interner.insert("x");
                manager.services.get_mut(&s).unwrap().listeners.insert(
                    x,
                    HashSet::from([(ServiceNetId("existing-subscriber".into()), x)]),
                );
            }
            let old_listeners = manager.services[&s].listeners.clone();
            let tid = TxnId::new(manager.node_id);
            let running = manager.running.clone();
            let running_peer = running.clone();
            let driver = async {
                match entry {
                    Entry::Initialization => manager.create_service(fresh, decls).await,
                    Entry::Replacement => manager.create_service(s, decls).await,
                    Entry::Update => {
                        crate::runtime::update::Transaction::new_with_id(tid.clone(), updates)
                            .apply(&mut manager)
                            .await
                            .map(|_| ())
                    }
                    Entry::Locks => {
                        let group = LockGroup {
                            service_level_lock: false,
                            reads: HashSet::new(),
                            writes: HashSet::from(["x".into()]),
                        };
                        let result = manager
                            .handle_lock_request(
                                tid.clone(),
                                HashMap::from([("s".into(), group.clone()), ("r".into(), group)]),
                            )
                            .await;
                        if result.is_ok() {
                            assert!(!running.contains(&tid));
                            assert!(manager.pending_txns.contains_key(&tid));
                            manager.abort_participant(&tid).await;
                        }
                        result
                    }
                }
            };
            let probe = async {
                let request = receive(&mut peer).await;
                let (request_id, tid, reply_to, lock) = match request {
                    MeerkatMessage::LookupRequest {
                        request_id,
                        txn_id: Some(tid),
                        reply_to,
                        ..
                    } => (request_id, tid, reply_to, false),
                    MeerkatMessage::LockRequest {
                        request_id,
                        txn_id,
                        reply_to,
                        ..
                    } => (request_id, txn_id, reply_to, true),
                    other => panic!("{entry:?}: unexpected request {other:?}"),
                };
                assert!(running_peer.contains(&tid));
                let address = peer_address(&mut peer).await;
                // An acyclic dependency can lock back into the running node.
                // Its reservation must join the outer owner's commit/abort,
                // even though this member was not touched by the outer call.
                send(
                    &mut peer,
                    &reply_to,
                    MeerkatMessage::LockRequest {
                        request_id: 9000,
                        txn_id: tid.clone(),
                        services: HashMap::from([
                            (
                                "s".into(),
                                LockGroup {
                                    service_level_lock: true,
                                    reads: HashSet::new(),
                                    writes: HashSet::new(),
                                },
                            ),
                            (
                                "held".into(),
                                LockGroup {
                                    service_level_lock: false,
                                    reads: HashSet::from(["x".into()]),
                                    writes: HashSet::new(),
                                },
                            ),
                            (
                                "t".into(),
                                LockGroup {
                                    service_level_lock: false,
                                    reads: HashSet::from(["x".into()]),
                                    writes: HashSet::new(),
                                },
                            ),
                        ]),
                        reply_to: address.clone(),
                    },
                )
                .await;
                assert!(matches!(
                    receive(&mut peer).await,
                    MeerkatMessage::LockResponse {
                        request_id: 9000,
                        success: true,
                        ..
                    }
                ));
                send(
                    &mut peer,
                    &reply_to,
                    MeerkatMessage::LookupRequest {
                        request_id: 9001,
                        service: "s".into(),
                        member: "x".into(),
                        reply_to: address.clone(),
                        txn_id: Some(tid.clone()),
                    },
                )
                .await;
                let callback_reply = receive(&mut peer).await;
                let MeerkatMessage::LookupError {
                    request_id: 9001,
                    error,
                } = callback_reply
                else {
                    panic!("{entry:?}/{outcome:?}: callback was not rejected: {callback_reply:?}")
                };
                assert!(matches!(
                    Manager::remote_error(error.clone()),
                    EvalError::RunningTransaction(_)
                ));
                assert!(error.contains("read-back"));
                if matches!(
                    outcome,
                    Outcome::AbortThenSuccess | Outcome::AbortThenWaitDie
                ) {
                    send(
                        &mut peer,
                        &reply_to,
                        MeerkatMessage::Abort {
                            request_id: 9002,
                            txn_id: tid.clone(),
                            reply_to: address.clone(),
                        },
                    )
                    .await;
                    assert!(matches!(
                        receive(&mut peer).await,
                        MeerkatMessage::AbortResponse { request_id: 9002 }
                    ));
                    assert!(running_peer.check(&tid).is_err());
                }
                let error = match outcome {
                    Outcome::Rejection => Some(error),
                    Outcome::AbortThenWaitDie => {
                        Some(EvalError::WaitDieAbort("test contention".into()).to_string())
                    }
                    _ => None,
                };
                let reply = if lock {
                    MeerkatMessage::LockResponse {
                        request_id,
                        txn_id: tid.clone(),
                        success: error.is_none(),
                        error,
                    }
                } else if let Some(error) = error {
                    MeerkatMessage::LookupError { request_id, error }
                } else {
                    MeerkatMessage::LookupResponse {
                        request_id,
                        value: NetValue::Int { val: 10 },
                        clock: Some(vec![]),
                        sources: Some(vec![]),
                    }
                };
                send(&mut peer, &reply_to, reply).await;
                let cleanup = receive(&mut peer).await;
                match cleanup {
                    MeerkatMessage::Abort {
                        request_id,
                        reply_to,
                        ..
                    } => {
                        assert!(
                            running_peer.contains(&tid),
                            "cleanup must remain registered"
                        );
                        send(
                            &mut peer,
                            &reply_to,
                            MeerkatMessage::AbortResponse { request_id },
                        )
                        .await;
                    }
                    MeerkatMessage::Commit {
                        request_id,
                        reply_to,
                        ..
                    } => {
                        assert!(matches!(outcome, Outcome::Success));
                        assert!(
                            !running_peer.contains(&tid),
                            "commit must not remain registered"
                        );
                        // A late abort stays deferred until the commit finishes.
                        send(
                            &mut peer,
                            &reply_to,
                            MeerkatMessage::Abort {
                                request_id: 9003,
                                txn_id: tid.clone(),
                                reply_to: address,
                            },
                        )
                        .await;
                        send(
                            &mut peer,
                            &reply_to,
                            MeerkatMessage::CommitResponse {
                                request_id,
                                success: true,
                                error: None,
                            },
                        )
                        .await;
                    }
                    other => panic!("{entry:?}: expected cleanup, got {other:?}"),
                }
                tid
            };
            let (result, observed_tid) = tokio::time::timeout(Duration::from_secs(5), async {
                tokio::select! {
                    result = async { tokio::join!(driver, probe) } => result,
                    _ = serve(&mut dependency) => unreachable!(),
                }
            })
            .await
            .unwrap_or_else(|_| panic!("{entry:?}/{outcome:?}: timed out"));
            if matches!(outcome, Outcome::Success) {
                result.unwrap();
            } else {
                assert!(
                    matches!(result, Err(EvalError::RunningTransaction(_))),
                    "{entry:?}/{outcome:?}: {result:?}"
                );
            }
            assert!(!running.contains(&observed_tid));
            assert!(manager.pending_txns.is_empty());
            if matches!(entry, Entry::Initialization) {
                assert_eq!(
                    manager.services.contains_key(&fresh),
                    matches!(outcome, Outcome::Success)
                );
            }
            let s = manager.interner.insert("s");
            let x = manager.interner.insert("x");
            let expected = if matches!(
                (entry, outcome),
                (Entry::Update | Entry::Replacement, Outcome::Success)
            ) {
                10
            } else {
                0
            };
            assert_eq!(
                manager.services[&s].vars[&x].value,
                Value::Int { val: expected }
            );
            if matches!(entry, Entry::Replacement) && !matches!(outcome, Outcome::Success) {
                assert_eq!(manager.services[&s].listeners, old_listeners);
                let y = manager.interner.insert("y");
                assert!(manager.services[&s].vars.contains_key(&y));
            }
            assert!(dependency.pending_txns.is_empty());
            for service in manager
                .services
                .values()
                .chain(dependency.services.values())
            {
                assert!(service.service_lock.is_none());
                assert!(service
                    .vars
                    .values()
                    .all(|v| matches!(v.lock, VarLock::Unlocked)));
            }
            if matches!(outcome, Outcome::Success) && !matches!(entry, Entry::Locks) {
                manager.serve_network_events().await;
                assert!(matches!(
                    tokio::time::timeout(Duration::from_secs(2), receive(&mut peer))
                        .await
                        .unwrap(),
                    MeerkatMessage::AbortResponse { request_id: 9003 }
                ));
            }
        }
    }
}

#[tokio::test]
async fn failed_update_cleans_up_existing_and_initializer_participants() {
    for source in [
        "update s { var x: bool = 1; }",
        "update s { var x = a.y; var y = 1 / 0; }",
    ] {
        let mut c = Cluster::new().await;
        let a = c.origin.interner.insert("a");
        c.origin
            .remote_services
            .insert(a, Address::new(format!("{}/a", c.addresses[0].0)));
        let mut imported = parse_string(
            "service a { var x = 0; var y = 0; }",
            &mut c.origin.interner,
        )
        .unwrap();
        imported.append(&mut c.origin.unified_ast);
        c.origin.unified_ast = imported;
        let tid = TxnId::new(c.origin.node_id);
        let update = parse_string(source, &mut c.origin.interner).unwrap();
        let driver = async {
            c.origin
                .handle_lock_request(
                    tid.clone(),
                    HashMap::from([(
                        "a".into(),
                        LockGroup {
                            service_level_lock: false,
                            reads: HashSet::from(["x".into()]),
                            writes: HashSet::new(),
                        },
                    )]),
                )
                .await
                .unwrap();
            assert!(c.origin.pending_txns.contains_key(&tid));
            crate::runtime::update::Transaction::new_with_id(tid.clone(), update)
                .apply(&mut c.origin)
                .await
        };
        let result = tokio::select! {
            result = tokio::time::timeout(Duration::from_secs(5), driver) => result.unwrap(),
            _ = join_all(c.peers.iter_mut().map(serve)) => unreachable!(),
        };
        let error = result.unwrap_err().to_string();
        let expected = if source.contains("bool") {
            "Static check failed"
        } else {
            "Division by zero"
        };
        assert!(error.contains(expected), "{source}: {error}");
        assert!(!c.origin.running.contains(&tid));
        c.assert_clean().await;
    }
}

#[tokio::test]
async fn dropping_execution_unregisters_but_does_not_release_locks() {
    let (mut manager, _) = node("s").await;
    let s = manager.interner.insert("s");
    let x = manager.interner.insert("x");
    let tid = TxnId::new(manager.node_id);
    let mut txn = manager.checkout_transaction(tid.clone());
    manager
        .assign(s, x, Value::Int { val: 10 }, &mut txn)
        .unwrap();
    let future = async move {
        let _txn = txn;
        std::future::pending::<()>().await;
    };
    assert!(tokio::time::timeout(Duration::from_millis(1), future)
        .await
        .is_err());
    assert!(!manager.running.contains(&tid));
    let value = &manager.services[&s].vars[&x];
    assert!(matches!(&value.lock, VarLock::WriteLocked(holder) if holder == &tid));
    assert_eq!(value.value, Value::Int { val: 0 });
}

#[tokio::test]
async fn update_aborts_before_retry_but_not_after_installation() {
    let (mut manager, _) = node("s").await;
    let (mut remote, address) = node("r").await;
    let mut peer = remote.network.take().unwrap();
    let r = manager.interner.insert("r");
    manager
        .remote_services
        .insert(r, Address::new(format!("{}/r", address.0)));
    let imported = parse_string("service r { var x = 0; }", &mut manager.interner).unwrap();
    manager.unified_ast.extend(imported);
    let updates = parse_string(
        "update s { var x = 7; } update r { var x = 1; }",
        &mut manager.interner,
    )
    .unwrap();
    let tid = TxnId::new(manager.node_id);
    let running = manager.running.clone();
    let probe = async {
        for attempt in 0..=1 {
            let MeerkatMessage::LockRequest {
                request_id,
                txn_id,
                reply_to,
                ..
            } = receive(&mut peer).await
            else {
                panic!("expected lock attempt")
            };
            assert_eq!(txn_id.iteration, attempt);
            assert!(running.contains(&txn_id));
            let error = (attempt == 0)
                .then(|| EvalError::WaitDieAbort("test contention".into()).to_string());
            send(
                &mut peer,
                &reply_to,
                MeerkatMessage::LockResponse {
                    request_id,
                    txn_id: txn_id.clone(),
                    success: error.is_none(),
                    error,
                },
            )
            .await;
            if attempt == 0 {
                let MeerkatMessage::Abort {
                    request_id,
                    txn_id: aborted,
                    reply_to,
                } = receive(&mut peer).await
                else {
                    panic!("must abort abandoned attempt before retrying")
                };
                assert_eq!(aborted, txn_id);
                assert!(running.contains(&txn_id));
                // A participant can forward our abort back to us. This old
                // attempt has already been discarded; its callback must not
                // suppress the retry or trigger a second round of aborts.
                let address = peer_address(&mut peer).await;
                send(
                    &mut peer,
                    &reply_to,
                    MeerkatMessage::Abort {
                        request_id: 9010,
                        txn_id: txn_id.clone(),
                        reply_to: address.clone(),
                    },
                )
                .await;
                assert!(matches!(
                    receive(&mut peer).await,
                    MeerkatMessage::AbortResponse { request_id: 9010 }
                ));
                // No new reservations may arrive during abort cleanup.
                send(
                    &mut peer,
                    &reply_to,
                    MeerkatMessage::LockRequest {
                        request_id: 9011,
                        txn_id: txn_id.clone(),
                        services: HashMap::from([(
                            "s".into(),
                            LockGroup {
                                service_level_lock: true,
                                reads: HashSet::new(),
                                writes: HashSet::new(),
                            },
                        )]),
                        reply_to: address,
                    },
                )
                .await;
                assert!(matches!(
                    receive(&mut peer).await,
                    MeerkatMessage::LockResponse {
                        request_id: 9011,
                        success: false,
                        ..
                    }
                ));
                send(
                    &mut peer,
                    &reply_to,
                    MeerkatMessage::AbortResponse { request_id },
                )
                .await;
            }
        }
        let MeerkatMessage::UpdateServiceRequest {
            request_id,
            txn_id: Some(tid),
            reply_to,
            ..
        } = receive(&mut peer).await
        else {
            panic!("expected update forwarding after installation")
        };
        assert_eq!(tid.iteration, 1);
        assert!(!running.contains(&tid));
        send(
            &mut peer,
            &reply_to,
            MeerkatMessage::UpdateServiceResponse {
                request_id,
                error: Some("test remote update failure".into()),
            },
        )
        .await;
        let MeerkatMessage::Commit {
            request_id,
            txn_id,
            reply_to,
            ..
        } = receive(&mut peer).await
        else {
            panic!("post-installation failure must not abort")
        };
        assert_eq!(txn_id, tid);
        assert!(!running.contains(&tid));
        send(
            &mut peer,
            &reply_to,
            MeerkatMessage::CommitResponse {
                request_id,
                success: true,
                error: None,
            },
        )
        .await;
        tid
    };
    let mut update = crate::runtime::update::Transaction::new_with_id(tid.clone(), updates);
    let (result, retried) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(update.apply(&mut manager), probe)
    })
    .await
    .expect("retry/update forwarding timed out");
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("test remote update failure"));
    let s = manager.interner.insert("s");
    let x = manager.interner.insert("x");
    assert_eq!(manager.services[&s].vars[&x].value, Value::Int { val: 7 });
    assert!(matches!(
        manager.services[&s].vars[&x].lock,
        VarLock::Unlocked
    ));
    assert!(!manager.running.contains(&tid));
    assert!(!manager.running.contains(&retried));
    assert!(manager.pending_txns.is_empty());
}
