# 09 — Wake requests parked on locks that a failure released

**Depends on:** 08 (§3 below replaces the `ParticipantCommit` it introduced).
**Reference:** `cli-test-fixes` — `Manager::freed_awaiting_wake` (~line 163),
`take_freed_awaiting_wake` (~line 215), `release_locks` (~line 2466), and in
`meerkat/src/main.rs`: `run_and_reply_or_park` (~line 317), `dispatch_parked`
(~line 332), `wake_ready` (~line 476).

## Rationale

When a transaction fails terminally it releases its locks. But the failure
surfaces as an `EvalError`, propagating through call paths that have **nowhere
to return a set of freed keys** — and one of them, `handle_lock_request` under
`dispatch_network_events`, has no dispatcher to hand them to at all.

So the locks were freed and nobody was told. Any request parked on one of those
keys waited for the remaining life of the process on a lock nobody held.

This was got wrong independently in at least five places: originator retry,
originator completion, participant discard, `handle_lock_request` releasing
partial locks, and service initialisation. Fixing them one at a time does not
work — the next path added gets it wrong too.

Task 12 makes transactional reads routine and therefore makes parks routine,
which is why this hardening lands first.

## Specification

### 1. Record at the single choke point

Add to `Manager`:

```rust
/// Locks a transaction released, on which a request is parked waiting to be
/// woken.
freed_awaiting_wake: HashSet<WaitKey>,
```

Fill it inside `release_locks` — the one function every release already passes
through — after the locks are actually dropped:

```rust
let waited_on: Vec<WaitKey> = locked
    .iter()
    .filter(|k| self.wait_queue.get(k).is_some_and(|w| !w.is_empty()))
    .cloned()
    .collect();
self.freed_awaiting_wake.extend(waited_on);
```

**Only keys with an actual waiter are recorded.** This is what keeps the set
bounded on a node that has no loop to drain it: a CLI client parks nothing, so
it queues nothing, even though it can reach the discard path.

Drain with:

```rust
/// Take the locks released by failed transactions since the last call, to wake
/// whatever is parked on them. Empty on a quiet loop iteration.
pub fn take_freed_awaiting_wake(&mut self) -> HashSet<WaitKey>;
```

### 2. Drain it in the server loop

In `meerkat/src/main.rs`, split the existing dispatch so that every parked run
is followed by a drain:

```rust
async fn run_and_reply_or_park(manager: &mut Manager, parked: ParkedRequest) {
    dispatch_parked(manager, parked).await;
    let freed = manager.take_freed_awaiting_wake();
    if !freed.is_empty() {
        Box::pin(wake_ready(manager, freed)).await;
    }
}

async fn wake_ready(manager: &mut Manager, freed: HashSet<WaitKey>) {
    for parked in manager.take_ready_waiters(&freed) {
        run_and_reply_or_park(manager, parked).await;
    }
}
```

`dispatch_parked` holds the existing per-variant logic (run the action / lookup,
send the reply, or re-park on `EvalError::WaitOn(key)`). The `Box::pin` is
required: `run_and_reply_or_park` and `wake_ready` are mutually recursive
`async fn`s. Waiters are drained **oldest first**.

**The drain must also cover releases that happen with no dispatcher present.**
`Manager::send_and_await_reply` pumps `dispatch_network_events` while this node
waits on an outbound reply, and that pump can serve an inbound `LockRequest`
inline; `handle_lock_request` may release partial locks there. Nothing on that
path can hand the freed keys anywhere — it is running underneath an unrelated
outbound call.

Note that the server loop itself never calls `dispatch_network_events`; it
handles inbound events directly, and within the CLI the pump is reached only
from `send_and_await_reply` (`manager/mod.rs:1211` and `:1252`). So there is no
"after the dispatch call" hook to attach to in `main.rs`. (The wasm render loop
pumps as well — `meerkat-wasm/src/lib.rs:270` — but nothing parks there:
`park_request_key` is called only from `main.rs`. A wasm build that ever parks
needs the same sweep at the top of that loop.)

Instead, **sweep at the top of every server-loop iteration**, before any other
work:

```rust
loop {
    let freed = manager.take_freed_awaiting_wake();
    if !freed.is_empty() {
        wake_ready(&mut manager, freed).await;
    }
    // ... pending updates, then the inbound-event match
}
```

This catches anything released underneath an outbound call on the previous
iteration, and it is one site rather than a list of call sites that must be
kept in sync.

### 3. Do not wake a key twice

Two paths already hand freed keys back to their caller, and once
`release_locks` queues them as well, each key is delivered twice. The failure
is not a lost wake but a spurious one: the second wake reaches the *next*
waiter while the first one still holds the lock it was just given, and wait-die
kills it.

`commit_participant` returns its keys in `ParticipantCommit::freed` (task 08)
and the caller wakes them directly. `abort_participant` returns
`HashSet<WaitKey>` (`manager/mod.rs:2141`, via
`discard_failed_participant_txn`) and on `main` the server loop passes that
straight to `wake_ready` too.

Give each key exactly one owner: **the queue, for both paths.**
`abort_participant` and `discard_failed_participant_txn` stop returning a key
set, and both server-loop arms wake from `take_freed_awaiting_wake()`. That
takes the keys as it reads them, so none can be delivered twice.

`commit_participant` then has nothing to report but a failure forwarding the
commit, so task 08's `ParticipantCommit { freed, forward_error }` becomes
`Result<(), EvalError>`. Task 08 kept the two apart so that a forwarding
failure could not lose the freed keys. They are now not returned at all, so
there is nothing to lose.

An earlier draft of this spec had commit keep its return value and take its
own keys back out of the queue. That was rejected because the only consumer
of the returned keys is the commit arm, which just wakes them. And removing
the keys from the queue took away the loop-head sweep's backstop: a caller
that ignored the returned keys would leave their waiters parked forever. With
the queue as the only channel, a missed wake costs at most one loop iteration.

## Tests

**Illustrating test:** `test_freed_keys_are_queued_only_when_something_is_parked_on_them`
(unit test in `manager/mod.rs`). Releasing a lock with an empty wait queue must
queue nothing; releasing one with a waiter must queue exactly that key. This
pins the boundedness property, which is the part most likely to be "simplified"
away.

Also:
- `test_commit_reports_its_freed_keys_once` — the de-duplication in §3.
- `test_originator_reports_locks_released_on_success` and
  `test_originator_reports_locks_released_when_it_gives_up`.
- In `meerkat/src/main.rs`: `test_terminal_participant_action_wakes_parked_requests`
  and `test_terminal_participant_lookup_wakes_parked_requests` — the two
  end-to-end paths through the server loop.

## Notes

- Resist the temptation to have each failure site return its own key set. That
  is the design that failed five times. The value here is that the recording
  point is unavoidable.
