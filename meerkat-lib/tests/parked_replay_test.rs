//! A participant action that parks must not re-run the effects it already had.
//!
//! `WaitOn` parks the whole action and `dispatch_parked` re-dispatches it from
//! its first statement, so every statement that already ran runs again. The
//! local buffers are restored before parking, which makes a replayed local
//! write harmless. A composed action onto another node is not covered by that:
//! it already executed on the other node under the shared transaction id, its
//! writes are buffered there, and this node cannot roll them back. Re-sending
//! it applies a second time on top of the state the first attempt left.
//!
//! The child here is a real `Manager`, driven by a stand-in network peer, so
//! what it holds after each attempt is what a real participant would hold.
//!
//! Not covered here: a park *inside* the `do` statement, after the child has
//! applied. Nothing that runs after a successful `ActionResponse` can park
//! today, so that case cannot be constructed. When something that reads is
//! added there, port `test_a_park_inside_the_do_statement_does_not_re_send_the_child_action`
//! from `cli-test-fixes`; do not approximate it with a park at a later
//! statement, which is the first test below.

use meerkat_lib::net::{
    codec, Address, MeerkatMessage, NetworkActor, NetworkCommand, NetworkEvent, NetworkReply,
    NodeType, ServiceNetId,
};
use meerkat_lib::runtime::ast::{ActionStmt, BinOp, Expr, Value};
use meerkat_lib::runtime::interner::Symbol;
use meerkat_lib::runtime::interpreter::EvalError;
use meerkat_lib::runtime::parser::parse_string;
use meerkat_lib::runtime::txn::{TxnId, VarLock};
use meerkat_lib::runtime::{Interner, Manager, Node};
use std::collections::HashMap;

/// Bring a `NetworkActor` up on an ephemeral loopback port and return it with
/// its dialable address.
async fn listening_node() -> (NetworkActor, Address) {
    let mut net = NetworkActor::new(NodeType::Server)
        .await
        .expect("network actor");
    let reply = net
        .handle_command(NetworkCommand::Listen {
            addr: Address::new("/ip4/127.0.0.1/tcp/0"),
        })
        .await;
    let addr = match reply {
        NetworkReply::ListenSuccess { addr } => addr,
        other => panic!("expected ListenSuccess, got {:?}", other),
    };
    let full = Address::new(format!("{}/p2p/{}", addr.0, net.local_peer_id()));
    (net, full)
}

async fn manager_for(code: &str, net: Option<NetworkActor>) -> Manager {
    let mut interner = Interner::new();
    let ast = parse_string(code, &mut interner).expect("valid syntax");
    let mut node = Node::new();
    node.interner = interner;
    node.unified_ast = ast.clone();
    node.static_checks().expect("static checks must pass");
    let local_ast = node.unified_ast.clone();
    node.on_manager_startup(true, net, HashMap::new(), &local_ast)
        .await
        .expect("service init must succeed")
}

/// The node under test, the child it composes onto, and the stand-in peer's
/// network layer. `guard` is the member the tests contend so the action parks
/// at a chosen point.
struct Setup {
    mid: Manager,
    child: Manager,
    peer_net: NetworkActor,
    peer_addr: Address,
    /// The transaction id of every composed request the child served
    served: Vec<TxnId>,
    mid_sym: Symbol,
    guard: Symbol,
    /// `cv` in `mid`'s interner: statements are built there and decoded into
    /// the child's interner on arrival
    cv: Symbol,
}

impl Setup {
    async fn new() -> Self {
        let (mid_net, _) = listening_node().await;
        let (peer_net, peer_addr) = listening_node().await;
        let mut mid = manager_for("service mid { var guard = 0; }", Some(mid_net)).await;
        // `cv` records how many times a composed increment was applied
        let child = manager_for("service rc { var cv = 1; }", None).await;
        let mid_sym = mid.interner.insert("mid");
        let guard = mid.interner.insert("guard");
        let cv = mid.interner.insert("cv");
        Setup {
            mid,
            child,
            peer_net,
            peer_addr,
            served: Vec::new(),
            mid_sym,
            guard,
            cv,
        }
    }

    /// Set the lock on `mid.guard`. Held by a transaction younger than any this
    /// process is about to mint, wait-die resolves the conflict by waiting,
    /// which is the `WaitOn` that parks the action.
    fn lock_guard(&mut self, lock: VarLock) {
        self.mid
            .services
            .get_mut(&self.mid_sym)
            .unwrap()
            .vars
            .get_mut(&self.guard)
            .unwrap()
            .lock = lock;
    }

    fn contend_guard(&mut self) {
        self.lock_guard(VarLock::WriteLocked(TxnId {
            timestamp: u128::MAX,
            node_id: self.mid.node_id,
            iteration: 0,
        }));
    }

    /// Release the contended lock, as the holder's commit or abort would.
    fn release_guard(&mut self) {
        self.lock_guard(VarLock::Unlocked);
    }

    /// Run one participant action to completion on `mid`, with the stand-in
    /// child serving whatever it composes. Calling this again with the same
    /// statements and id is what the server loop does for a parked request.
    async fn run_action(&mut self, stmts: &[ActionStmt], tid: &TxnId) -> Result<(), EvalError> {
        tokio::select! {
            biased;
            r = self.mid.execute_action_participant(self.mid_sym, stmts, &[], tid.clone()) => r,
            _ = child_peer(&mut self.peer_net, &mut self.child, &mut self.served) => {
                unreachable!("the stand-in child runs until the action under test finishes")
            }
        }
    }

    /// What the child has buffered for `rc.cv` under `tid`, or its committed
    /// value if the transaction buffered nothing.
    fn child_cv(&mut self, tid: &TxnId) -> i32 {
        let rc = self.child.interner.insert("rc");
        let cv = self.child.interner.insert("cv");
        let sid = self.child.service_net_id_for_name(rc);
        let buffered = self
            .child
            .pending_txns
            .get(tid)
            .and_then(|t| t.written.get(&(sid, cv)).cloned());
        let value = buffered.unwrap_or_else(|| self.child.services[&rc].vars[&cv].value.clone());
        match value {
            Value::Int { val } => val,
            other => panic!("rc.cv should be an Int, got {:?}", other),
        }
    }

    /// `do rc.{cv = cv + by}`, dispatched to the child.
    fn bump_by(&self, by: i32) -> ActionStmt {
        self.bump_on("rc", by)
    }

    /// `do <service>.{cv = cv + by}`, dispatched to the child's node.
    fn bump_on(&self, service: &str, by: i32) -> ActionStmt {
        ActionStmt::Do(Expr::Literal {
            val: Value::ActionClosure {
                stmts: vec![ActionStmt::Assign {
                    name: self.cv,
                    expr: Expr::Binop {
                        op: BinOp::Add,
                        expr1: Box::new(Expr::Variable { name: self.cv }),
                        expr2: Box::new(Expr::Literal {
                            val: Value::Int { val: by },
                        }),
                    },
                }],
                env: Vec::new(),
                service_net_id: ServiceNetId::new(format!("{}/{}", self.peer_addr.0, service)),
            },
        })
    }

    fn bump(&self) -> ActionStmt {
        self.bump_by(1)
    }

    /// Write `1` to `mid.guard`, which parks while the lock is contended.
    fn touch_guard(&self) -> ActionStmt {
        ActionStmt::Assign {
            name: self.guard,
            expr: Expr::Literal {
                val: Value::Int { val: 1 },
            },
        }
    }

    /// Run `stmts` against a contended `guard` and check the action parked.
    async fn run_until_parked(&mut self, stmts: &[ActionStmt], tid: &TxnId) {
        self.contend_guard();
        let first = self.run_action(stmts, tid).await;
        assert!(
            matches!(first, Err(EvalError::WaitOn(_))),
            "the contended write must park the action, got: {:?}",
            first
        );
    }
}

/// Stand in for the node that owns `rc`: serve each `ActionRequest` and `Abort`
/// against a real `Manager` under the shared transaction id, exactly as the
/// server loop does. Records the id of every action request it served. Runs
/// until cancelled.
async fn child_peer(net: &mut NetworkActor, child: &mut Manager, served: &mut Vec<TxnId>) {
    loop {
        let (reply_to, response) = match net.event_rx.try_recv() {
            Ok(NetworkEvent::MessageReceived {
                msg:
                    MeerkatMessage::ActionRequest {
                        request_id,
                        service,
                        stmts,
                        reply_to,
                        txn_id,
                        ..
                    },
                ..
            }) => {
                let svc = child.interner.insert(&service);
                let local: Vec<ActionStmt> = stmts
                    .into_iter()
                    .map(|s| codec::decode_action_stmt(s, &mut child.interner).expect("decodable"))
                    .collect();
                let tid = txn_id.expect("a composed action carries the shared transaction id");
                served.push(tid.clone());
                let outcome = child
                    .execute_action_participant(svc, &local, &[], tid)
                    .await;
                let response = MeerkatMessage::ActionResponse {
                    request_id,
                    success: outcome.is_ok(),
                    error: outcome.err().map(|e| e.to_string()),
                };
                (reply_to, response)
            }
            Ok(NetworkEvent::MessageReceived {
                msg:
                    MeerkatMessage::Abort {
                        request_id,
                        txn_id,
                        reply_to,
                    },
                ..
            }) => {
                child.abort_participant(&txn_id).await;
                (reply_to, MeerkatMessage::AbortResponse { request_id })
            }
            Ok(_) => continue,
            Err(_) => {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                continue;
            }
        };
        let _ = net
            .handle_command(NetworkCommand::SendMessage {
                addr: Address::new(&reply_to),
                msg: response,
            })
            .await;
    }
}

/// A participant action that composed onto another node and then parked must
/// apply that composed action exactly once.
///
/// The action is `do rc.{cv = cv + 1}` followed by a write to a contended
/// member. The first attempt runs the composed action (the child buffers
/// `cv = 2`) and then parks. When the lock frees the action is re-dispatched
/// from its first statement. Re-sending the composed action under the same
/// transaction id would have the child resume the same `pending_txns` entry,
/// read its own buffered `2` and buffer `3`, with no error anywhere.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_parked_action_applies_its_composed_child_action_exactly_once() {
    let mut s = Setup::new().await;
    let stmts = vec![s.bump(), s.touch_guard()];
    let tid = TxnId::new(s.mid.node_id);

    s.run_until_parked(&stmts, &tid).await;
    assert_eq!(s.served.len(), 1, "the composed action ran once so far");
    assert_eq!(s.child_cv(&tid), 2, "the child buffered one increment");

    s.release_guard();
    s.run_action(&stmts, &tid)
        .await
        .expect("the re-dispatched action must now succeed");

    assert!(
        s.served.iter().all(|t| *t == tid),
        "every composed request belongs to the same transaction"
    );
    assert_eq!(
        s.served.len(),
        1,
        "the composed action must not be re-sent on replay"
    );
    assert_eq!(
        s.child_cv(&tid),
        2,
        "`do rc.{{cv = cv + 1}}` appears once in the program, so `cv` must end at 2"
    );
}

/// Two composed actions in one parked action are both replayed as completed,
/// and neither is re-sent.
///
/// Guards the position bookkeeping: the records are matched by dispatch order,
/// so a rewind that lands off by one would skip the wrong call or re-send it.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_parked_action_replays_every_composed_action_it_completed() {
    let mut s = Setup::new().await;
    let stmts = vec![s.bump(), s.bump(), s.touch_guard()];
    let tid = TxnId::new(s.mid.node_id);

    s.run_until_parked(&stmts, &tid).await;
    assert_eq!(
        s.served.len(),
        2,
        "both composed actions ran before the park"
    );
    assert_eq!(s.child_cv(&tid), 3, "1 + two increments");

    s.release_guard();
    s.run_action(&stmts, &tid)
        .await
        .expect("the re-dispatched action must now succeed");

    assert_eq!(s.served.len(), 2, "neither may be sent a second time");
    assert_eq!(s.child_cv(&tid), 3, "two increments, so `cv` must end at 3");
}

/// Suppression is scoped to the replay, not to the transaction.
///
/// An originator can compose two actions onto the same participant under one
/// transaction id. The second is a new dispatch, not a replay of the first, and
/// must go out: treating "already composed onto this node" as the condition
/// would silently drop it.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_second_composed_action_under_the_same_transaction_still_dispatches() {
    let mut s = Setup::new().await;
    let stmts = vec![s.bump()];
    let tid = TxnId::new(s.mid.node_id);

    // Nothing is contended here: both actions run straight through
    for _ in 0..2 {
        s.run_action(&stmts, &tid)
            .await
            .expect("an uncontended composed action must succeed");
    }

    assert_eq!(s.served.len(), 2, "two separate actions are two dispatches");
    assert_eq!(s.child_cv(&tid), 3, "each action increments once");
}

/// A replay that reaches a different composed action on the same target fails
/// the transaction instead of suppressing it.
///
/// Matching on target alone would skip the dispatch, commit the first run's
/// `+1` and never run the `+10` the surviving run asked for. The replay is
/// modelled by re-running under the same id with the branch already taken.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_replay_that_dispatches_a_different_action_to_the_same_target_fails() {
    let mut s = Setup::new().await;
    let tid = TxnId::new(s.mid.node_id);
    s.run_until_parked(&[s.bump(), s.touch_guard()], &tid).await;

    s.release_guard();
    let replay = s.run_action(&[s.bump_by(10), s.touch_guard()], &tid).await;

    match replay {
        Err(EvalError::LocalDispatchFailed(msg)) => assert!(
            msg.contains("differs from the one it dispatched there before"),
            "the error must say the action changed, not just name the target: {msg}"
        ),
        other => panic!("a diverging replay must fail the transaction, got: {other:?}"),
    }
    assert_eq!(s.served.len(), 1, "the diverging action must not be sent");
    assert!(!s.mid.pending_txns.contains_key(&tid));
    assert_eq!(s.child_cv(&tid), 1, "the child was told to abort");
}

/// A replay that reaches a composed action on a different target fails the
/// transaction, and says which targets the two runs dispatched to.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_replay_that_dispatches_to_a_different_target_fails() {
    let mut s = Setup::new().await;
    let tid = TxnId::new(s.mid.node_id);
    s.run_until_parked(&[s.bump(), s.touch_guard()], &tid).await;

    s.release_guard();
    let replay = s
        .run_action(&[s.bump_on("other", 1), s.touch_guard()], &tid)
        .await;

    let rc = format!("'{}/rc'", s.peer_addr.0);
    let other_target = format!("'{}/other'", s.peer_addr.0);
    match replay {
        Err(EvalError::LocalDispatchFailed(msg)) => assert!(
            msg.contains(&format!(
                "to {other_target} in place of the one it had dispatched to {rc}"
            )),
            "the error must name both targets: {msg}"
        ),
        other => panic!("a diverging replay must fail the transaction, got: {other:?}"),
    }
    assert_eq!(s.served.len(), 1, "the diverging action must not be sent");
    assert!(!s.mid.pending_txns.contains_key(&tid));
    assert_eq!(s.child_cv(&tid), 1, "the child was told to abort");
}

/// A replay that completes without reaching a composed action the parked run
/// made fails the transaction: that action's effect is buffered on the other
/// node all the same, and committing it would apply a write the surviving run
/// never asked for.
///
/// An earlier action under the same transaction id already dispatched one, so
/// the error's counts must be this action's, not the transaction's.
#[tokio::test(flavor = "multi_thread")]
async fn test_a_replay_that_skips_a_recorded_dispatch_fails() {
    let mut s = Setup::new().await;
    let tid = TxnId::new(s.mid.node_id);
    s.run_action(&[s.bump()], &tid)
        .await
        .expect("an uncontended composed action must succeed");
    s.run_until_parked(&[s.bump(), s.touch_guard()], &tid).await;

    s.release_guard();
    let replay = s.run_action(&[s.touch_guard()], &tid).await;

    match replay {
        Err(EvalError::LocalDispatchFailed(msg)) => assert!(
            msg.contains("dispatched 0 composed actions, but its parked run had completed 1"),
            "the counts must be this action's: {msg}"
        ),
        other => panic!("a replay that skips a recorded dispatch must fail, got: {other:?}"),
    }
    assert!(!s.mid.pending_txns.contains_key(&tid));
    assert_eq!(s.child_cv(&tid), 1, "the child was told to abort");
}
