# 07 — A middle node must commit downward before propagating

**Depends on:** nothing, but land before 08 (both restructure
`commit_participant`, and reviewing them together doubles the diff).
**Reference:** `cli-test-fixes` — `store_committed_writes` and
`propagate_committed_writes` (`manager/mod.rs` ~lines 2662 and 2678), the
rewritten `commit_participant` (~line 2768), and
`meerkat-lib/tests/participant_commit_order_test.rs`.

## Rationale

The **originator** already commits in the right order: store its own writes,
send `Commit` to every participant, and only then recompute what is derived from
those writes.

A node in the **middle** of a chain did not. `commit_participant` called
`apply_committed_writes`, which stores *and* propagates in one step, before
forwarding `Commit` to the nodes below it.

Concretely, with `client -> mid -> rc` and `def mid_view = mv + rc.gc`: on
`Commit`, `mid` stores `mv`, then immediately recomputes `mid_view` — reading
`rc.gc` while `rc` is still holding its write **buffered and uncommitted**. The
recomputed `mid_view` combines the new `mv` with the old `gc`, a pair of values
that were never simultaneously true. That value is stored, and stays stored
until some asynchronous `Update` happens to repair it.

## Specification

Split the existing `apply_committed_writes` into two halves that can be
sequenced independently:

```rust
/// Store a committed transaction's buffered writes into the owning services,
/// without propagating.
fn store_committed_writes(&mut self, txn: &Transaction);

/// Recompute the members derived from a committed transaction's writes.
/// Best-effort: the transaction has committed and there is no way back.
async fn propagate_committed_writes(&mut self, txn: &Transaction);
```

`store_committed_writes` writes each `txn.written` entry into the owning service
and sets `latest_write_txn`. `propagate_committed_writes` calls `propagate` for
each written member.

`apply_committed_writes` remains as the composition of the two, for the one
caller that has nothing below it to commit first.

`commit_participant` must then run, in this order:

1. `store_committed_writes(&txn)`
2. `send_commit` to every address in `txn.participants`
3. `propagate_committed_writes(&txn).await`
4. release locks

Both commit paths that can have participants — `execute_action_with_txn` and
`commit_participant` — call the two halves separately.

Locks stay held through step 3. Today the event loop is blocked for the whole
call, so an earlier release would be harmless; that stops being true under #28's
background message loop, which `send_and_await_reply` is also waiting on. Once
other work can interleave, releasing before the nodes below have committed
exposes half of a distributed transaction to whoever takes the lock next. Keep
the comment explaining this.

## Tests

**Illustrating test** (and the only one):
`test_participant_commits_sub_participants_before_propagating` in
`meerkat-lib/tests/participant_commit_order_test.rs`.

It stands in for `rc` with a bare network peer rather than a real `Manager`, so
the peer can record exactly when it receives `Commit` relative to the read that
`mid`'s propagation issues. That makes the ordering directly observable instead
of timing-dependent — preserve this construction if you rewrite the test.

## Notes

- Propagation remains **best-effort** and must not return a `Result`. Once
  writes are stored the transaction is committed and there is no way back; a
  failed recompute is logged, not raised.
