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
async fn spike_real_network_readback_cycles() {
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
fn spike_registration_lifecycle() {
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
/// late-abort case it probes the boundary after local commit, which this spike
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
async fn spike_doomed_success_and_commit_boundary() {
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
async fn spike_dropped_future_and_doomed_park() {
    let running = RunningTransactions::default();
    let tid = TxnId::new(1);
    let future = async {
        let registration = running.enter(&tid);
        running.doom(&tid);
        let wait = EvalError::WaitOn(WaitKey::Service(ServiceNetId("s".into())));
        assert!(matches!(
            registration.finish::<()>(Err(wait)),
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
async fn spike_cycle_between_idle_participants_during_abort() {
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
async fn spike_participant_parking_clears_registration() {
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
async fn spike_abort_ack_precedes_cleanup_while_child_is_parked() {
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
