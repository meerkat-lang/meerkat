# 09 — Wake requests parked on locks that a failure released

**Depends on:** 08 (the commit de-duplication below builds on
`ParticipantCommit`).
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

### 3. Do not wake a key twice

`commit_participant` returns its freed keys in `ParticipantCommit::freed` and
the caller wakes them directly. Those keys must **not** also land in
`freed_awaiting_wake`.

A key delivered twice is woken twice, and the second wake reaches the *next*
waiter while the first one still holds the lock it was just given — wait-die
then kills it. Deduplicate at the commit site.

## Tests

**Illustrating test:** `test_freed_keys_are_queued_only_when_something_is_parked_on_them`
(unit test in `manager/mod.rs`). Releasing a lock with an empty wait queue must
queue nothing; releasing one with a waiter must queue exactly that key. This
pins the boundedness property, which is the part most likely to be "simplified"
away.

Also:
- `test_commit_does_not_queue_the_keys_it_hands_back` — the de-duplication in §3.
- `test_originator_reports_locks_released_on_success` and
  `test_originator_reports_locks_released_when_it_gives_up`.
- In `meerkat/src/main.rs`: `test_terminal_participant_action_wakes_parked_requests`
  and `test_terminal_participant_lookup_wakes_parked_requests` — the two
  end-to-end paths through the server loop.

## Notes

- Resist the temptation to have each failure site return its own key set. That
  is the design that failed five times. The value here is that the recording
  point is unavoidable.
