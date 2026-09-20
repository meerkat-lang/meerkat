# 02 — Restore the outer reactive cache after a nested recompute

**Depends on:** nothing.
**Reference:** `cli-test-fixes`, `Manager::recompute_def` in
`meerkat-lib/src/runtime/manager/mod.rs` (~line 830), and the unit test
`test_recompute_def_restores_an_active_reactive_cache`.

## Rationale

This is a pre-existing reentrancy bug in ordinary (non-transactional) reactive
propagation. It has nothing to do with transactions or read-your-own-writes; it
was found while working on PR #189 and should not wait for it.

`Manager::reactive_cache` is a transient `Option<HashMap<(Symbol, Symbol), Value>>`
holding the cross-service dependency values for **the def currently being
recomputed**, so that `MemberAccess` resolves from cache instead of issuing a
remote lookup. `recompute_def` installs it on entry and, on `main`, clears it on
exit:

```rust
self.reactive_cache = Some(cache);
let result = eval(...).await;
self.reactive_cache = None;      // <-- bug
```

Evaluation can `await` a remote read. While it waits, `send_and_await_reply`
pumps network events, so an inbound `Update` can re-enter `handle_update` and
therefore `recompute_def` **while an outer recompute is suspended**. The inner
call then clears the cache the outer call installed. When the outer recompute
resumes, its member accesses miss the cache and fall through to `lookup`,
resolving against whatever state exists at that moment rather than the
dependency snapshot the recompute was started with.

## Specification

`recompute_def` must save the previous value of `self.reactive_cache` and
restore it, rather than clearing:

```rust
let outer_cache = self.reactive_cache.take();
self.reactive_cache = Some(cache);
let result = eval(...).await;
self.reactive_cache = outer_cache;
```

The restore must happen on **both** the success and the error path — the
existing code already evaluates into a `result` binding before matching on it,
so restoring immediately after the `eval` is sufficient. Do not introduce an
early `return` between the install and the restore.

## Tests

**Illustrating test:** `test_recompute_def_restores_an_active_reactive_cache`
(unit test in `manager/mod.rs` on the reference branch).

It installs a sentinel cache as if an outer recompute were in progress, runs a
`recompute_def`, and asserts `manager.reactive_cache` still holds the sentinel
afterwards. On `main` it holds `None` and fails.

## Notes

- The reference branch applies the same save/restore to `recompute_def_in_txn`.
  That function is part of the eager-propagation mechanism we are not taking;
  the pattern returns in task 12, which reuses that function's body. Only fix
  `recompute_def` here.
- Extend the doc comment to explain *why* it is a save/restore and not a clear.
  The reference branch's comment is good; reuse it. Without the explanation this
  reads like a pointless complication and will be "simplified" back into a bug.
