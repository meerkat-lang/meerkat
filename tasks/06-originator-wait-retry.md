# 06 — Let the originator wait, by retrying

**Depends on:** 05.
**Reference:** `cli-test-fixes` — the retry branch in
`Manager::execute_action_with_txn` (`manager/mod.rs` ~line 2551),
`describe_wait_key` (~line 2630), `wait_die_backoff` and
`WAIT_DIE_RETRY_BACKOFF_MS` (~line 26–55), and
`meerkat-lib/tests/originator_wait_test.rs`.

## Rationale

Wait-die has two halves. A transaction younger than the lock holder **dies**
(`EvalError::WaitDieAbort`, retry); a transaction older than the holder
**waits** (`EvalError::WaitOn(WaitKey)`, park until the lock frees).

A participant can park: the server loop puts the request on
`Manager::wait_queue` keyed by the contended `WaitKey` and re-runs it when the
lock frees. An **originator has no queue to park in** — it is driving its own
transaction on its own stack. On `main`, `WaitOn` simply escapes
`execute_action_with_txn` and fails the action.

Two visible consequences: a `do` in an `@test` block (as of PR #201 each one runs
as its own transaction) fails outright over contention that was supposed to
resolve itself, and it is reported as a raw `WaitKey` — a `ServiceNetId` plus an
interned `Symbol`, whose `Debug` form means nothing outside the node that
produced it.

This is a latent bug today, since any cross-service read inside a transaction
can produce it. Task 12 makes it a routine path, which is why it lands first.

## Specification

### 1. `WaitOn` joins `WaitDieAbort` in the retry branch

In `execute_action_with_txn`, the branch that currently matches
`Some(EvalError::WaitDieAbort(_))` must also match `Some(EvalError::WaitOn(_))`.
Its existing behaviour is correct for both: abort every participant, release all
locks held by the transaction, and — if `txn_id.iteration < MAX_WAIT_DIE_RETRIES`
— mint `txn_id.retry()` and run the whole transaction again. Retrying the whole
transaction *is* how an originator waits.

### 2. `WaitOn` must never leave the loop as itself

When the retry budget is exhausted, translate rather than propagate:

```rust
return Err(match exec_error.unwrap() {
    EvalError::WaitOn(key) => EvalError::WaitDieAbort(format!(
        "gave up waiting for {} after {} retries",
        self.describe_wait_key(&key), MAX_WAIT_DIE_RETRIES
    )),
    other => other,
});
```

`describe_wait_key(&self, key: &WaitKey) -> String` renders a key in program
terms: `WaitKey::Service(sid)` as `service '<name>'` and
`WaitKey::Member(sid, m)` as `'<service>.<member>'`, resolving through
`service_name_for_net_id` and the interner, falling back to the raw
`ServiceNetId` string when the name is not local.

### 3. Pace the retries

Add `async fn retry_backoff(ms: u64)` and call it before `continue` with the
backoff **the outcome selects**: `WAIT_DIE_RETRY_BACKOFF_MS` (2ms) for
`WaitDieAbort`, `WAIT_ON_RETRY_BACKOFF_MS` (20ms) for `WaitOn`. A single 2ms
helper wired to both compiles, reads fine, and silently keeps the defect below.

This is not cosmetic. A transaction with **no participants** never awaits
anything on the retry path — there is no `send_abort` to make — so without a
yield point all `MAX_WAIT_DIE_RETRIES` attempts execute back-to-back within
microseconds, contending against exactly the state the first attempt saw. The
budget is spent before the lock holder has had any opportunity to commit, and
ordinary contention is reported as an exhausted wait.

Platform-split like the timeout in `send_and_await_reply`: `tokio::time::sleep`
natively, `gloo_timers::future::TimeoutFuture` wrapped in `send_wrapper::SendWrapper`
on `wasm32` (wasm has no tokio timer driver in the browser, and the eval path is
`Send`-bounded).

The two durations differ because the outcomes do: a **die** re-runs promptly,
while a **wait** must give the holder time to commit.

Document the limit honestly, because the shape of §1 has a consequence worth
naming: `MAX_WAIT_DIE_RETRIES` (10) doubles as the originator's wait
**timeout**. An older transaction — one wait-die guarantees should win — is
killed after roughly 200ms of contention, or 20ms if both outcomes share the 2ms
backoff. That bound is inherent to retrying instead of parking, not a tuning
accident; say so where the constant is defined. And this is a yield point, not a
fix for the case where the only thing that could release the lock is a message
this node cannot receive while inside the retry loop. Both need the background
message loop of #28.

## Tests

`meerkat-lib/tests/originator_wait_test.rs` (2 tests) plus the unit test
`test_wait_die_retries_are_paced` — which must pin *which* backoff each outcome
gets, not merely that some sleep happens.

**Illustrating test:** `test_originator_retries_the_transaction_on_a_wait` — a
stand-in network peer counts the `Abort` messages it receives. Each attempt
aborts its participants before retrying, and the originator awaits each
acknowledgement, so the count is deterministic: it must equal
`MAX_WAIT_DIE_RETRIES + 1`. On `main` it is 1.

Also: `test_originator_reports_a_contended_member_not_a_wait_key`, which asserts
the message contains `p.get_n` and contains neither `Symbol(` nor
`ServiceNetId(`.

### The test program must be rewritten

⚠️ On the reference branch, both tests create contention through the eager
propagation we are **not** taking: the program writes `q.w`, which triggers a
recompute of `def mm = w + p.get_n`, which reads across the service boundary and
takes the contended lock. Without eager propagation that write recomputes
nothing and no contention occurs.

Rewrite the action to read `p.get_n` **directly** — a plain cross-service read
inside a transaction takes the same read lock and produces the same `WaitOn`.
The resulting program is smaller and tests the same property. Keep
`hold_get_n_by_a_younger_transaction` as-is.
