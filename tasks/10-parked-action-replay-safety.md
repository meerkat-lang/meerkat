# 10 — A replayed action must not repeat its effects

**Depends on:** 09 (parks must actually resume before replay safety is
observable).
**Reference:** `cli-test-fixes` — `buffered_before` in
`execute_action_participant` (`manager/mod.rs` ~line 2733), `composed_done` /
`composed_seq` / `ComposedCall` in `runtime/txn.rs`, the replay short-circuit in
`remote_action` (~line 2004), commit `c7d61ea`, and
`meerkat-lib/tests/parked_replay_test.rs`.

## Rationale

`EvalError::WaitOn` parks the **whole action** and re-dispatches it from
statement one. So a run that parks must leave behind exactly the state it found,
or the replay observes its own half-finished output. Two halves, both real, and
they belong in one PR because they are one invariant.

**(a) Local buffers.** `x = x + 1` on a service where `x == 0` buffers `1`. If
the action then parks and replays, the replayed `x = x + 1` reads the buffered
`1` from `read_cache` and commits `2`.

**(b) Composed actions.** A composed action (`do svc.action`) shipped to another
node under the shared transaction id has **already run there**, with its writes
buffered on that node. This node cannot roll that back. Re-dispatching applies
it a second time: `do rc.{cv = cv + 1}` against `cv == 1` commits **3**, silently
and with no error anywhere.

## Specification

### (a) Roll back what the parked run buffered

In `execute_action_participant`, snapshot before running:

```rust
let buffered_before = (txn.written.clone(), txn.read_cache.clone());
```

and on the `WaitOn` path only, restore:

```rust
(txn.written, txn.read_cache) = buffered_before;
```

**Restore, do not clear.** An originator may compose two actions onto the same
participant under a single transaction id; the second must not discard the
first's writes.

### (b) Memoise composed dispatches by position

In `runtime/txn.rs`:

```rust
/// A composed action that completed under a transaction: where it was sent.
#[derive(Debug, Clone)]
pub struct ComposedCall {
    pub target: ServiceNetId,
}
```

and on `Transaction`:

```rust
/// Composed actions already executed under this transaction, keyed by their
/// position in this transaction's dispatch order.
pub composed_done: HashMap<u64, ComposedCall>,
/// Position the next composed action dispatched under this transaction takes.
pub composed_seq: u64,
```

In `remote_action`, **after** the existing `participants.insert`
pre-registration, claim the next position and short-circuit if it is already
recorded:

```rust
let replay = match txn.as_deref_mut() {
    Some(t) if shared_tid.is_some() => {
        let seq = t.composed_seq;
        t.composed_seq += 1;
        t.composed_done.get(&seq).cloned().map(|done| (seq, done))
    }
    _ => None,
};
if let Some((seq, done)) = replay {
    if &done.target != service_net_id {
        return Err(EvalError::LocalDispatchFailed(format!(
            "replayed transaction dispatched a different composed action at \
             position {}: recorded '{}', now '{}'",
            seq, done.target.0, service_net_id.0
        )));
    }
    return Ok(());
}
```

On a successful `ActionResponse`, record at `composed_seq - 1`.

In `execute_action_participant`, snapshot `composed_seq` alongside
`buffered_before` and rewind it on the `WaitOn` path, so a replayed dispatch
lands on the slot the first attempt filled.

Three properties this shape gives you, all of which are tested:

- A dispatch occurring **after** a successful run claims a fresh position, so
  two genuinely distinct composed actions stay distinct. This must not become
  "suppress any dispatch to a target we have already contacted".
- The target check fails the transaction rather than guessing. A record whose
  target does not match the dispatch reaching its position means the replay took
  a different branch, and silently substituting the recorded result would be
  wrong.
- Recording happens **before** anything fallible that follows the response. On
  the reference branch that mattered because a cross-service refresh ran there
  and could itself park. That refresh does not exist in this task, but keep the
  record as the first thing done with a successful response — the hazard returns
  in task 12.

## Tests

`meerkat-lib/tests/parked_replay_test.rs` plus two unit tests in
`manager/mod.rs`. The integration tests drive a **real child `Manager`** behind
a stand-in network peer, so the double-apply is observed as a committed value,
not inferred from message counts. Preserve that.

**Illustrating test:**
`test_a_parked_action_applies_its_composed_child_action_exactly_once` — the
child's `cv` starts at 1, the parent action does `do rc.bump` and then parks and
replays. Correct result is `cv == 2`; without the fix the child serves two
requests and commits `3`.

Also:
- `test_a_parked_action_replays_every_composed_action_it_completed`
- `test_a_second_composed_action_under_the_same_transaction_still_dispatches`
  (guards against over-suppression — it must fail if you suppress by target)
- `test_parked_participant_run_rolls_back_what_it_buffered` (half (a))
- `test_second_action_under_one_txn_keeps_the_first_write` (why (a) restores
  rather than clears)

### One test is deferred

⚠️ `test_a_park_inside_the_do_statement_does_not_re_send_the_child_action`
depends on `refresh_remote_cross_deps_in_txn` parking *inside* the `do`
statement, which is eager-propagation machinery we are not taking. **Defer it to
task 12**, where compute-on-read reintroduces a mid-`do` park.

Do not weaken it into a park at the *next* statement in order to land it here.
That is a different case, already covered by the first test, and it would look
like coverage we do not have.

## Notes

- The originator path (`execute_action_with_txn`) needs no change: its wait-die
  retry aborts every participant and mints a fresh `Transaction`, so nothing is
  left buffered downstream when it re-runs. Verify this still holds; do not add
  a second mechanism for it.
