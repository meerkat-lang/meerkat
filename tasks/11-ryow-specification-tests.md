# 11 — Land the read-your-own-writes tests as an executable specification

**Depends on:** nothing. Optional, but it makes task 12's review substantially
easier.
**Reference:** `cli-test-fixes` — `meerkat-lib/tests/txn_reactivity_test.rs`,
`meerkat/tests/ryow_{a,b,c,mid}.mkt`,
`meerkat/tests/dist_ryow_{two_remotes,local_write,nested}.mkt`, and
`meerkat/tests/README.md`.

## Rationale

Task 12 changes both *what* derived members do inside a transaction and *how*
that is implemented. Reviewing those together is hard: a reviewer cannot tell
whether an unfamiliar assertion is the agreed contract or an artifact of the
mechanism.

Landing the tests first, red and explicitly ignored, separates the two
questions. Task 12's review then reduces to "these were the agreed semantics;
here they are passing."

**This is tests and fixtures only. No production code changes in this PR.**

If the team objects to landing failing tests — some reviewers reasonably do —
skip this task and fold these files into task 12.

## Rationale for the semantics themselves

A `def` is an eagerly evaluated, cached terminal value, not a thunk. A plain
lookup of a def returns whatever the last committed propagation stored. So a
transaction that writes `x` and then reads `def y = x + 1` sees the
*pre-transaction* `y`, which breaks the most basic thing an action can do.

`meerkat/tests/s1.mkt` was the regression that opened PR #189, but it no longer
demonstrates the problem. PR #201 made each `do` in an `@test` block its own
transaction, so s1.mkt's writes commit and propagate *between* its statements
and it passes. The contract below is unaffected: a write and a dependent read
within **one** transaction still sees the stale value.

    pub def probe = action { x = 5; assert(y == 6); };   // still fails

Use that shape — write and dependent read inside a single action — when
demonstrating the gap, not s1.mkt.

## Specification

1. Copy `meerkat-lib/tests/txn_reactivity_test.rs` from the reference branch.
2. Copy the seven `.mkt` scenarios. They are designed to run **both**
   single-process and against real servers with identical assertions; preserve
   that property.
3. Copy `meerkat/tests/README.md`, which documents the scenarios. Keep its
   honest note that the older `dist_s*` fixtures do not run as written — the
   import path is derived from the service name, so two files both declaring
   `s2` cannot both be named for it — along with the pointer to where the
   equivalent coverage now lives.
4. **Run each test against `main` first.** Mark only the ones that actually fail
   with `#[ignore = "requires compute-on-read: see tasks/12-compute-on-read.md"]`.
   Do not blanket-ignore the file; at least
   `test_var_initialized_from_another_service_is_not_reactive` is expected to
   pass already, and an ignored passing test hides a regression.
5. Record in the PR description which tests are ignored and why.

## Tests

The suite is the deliverable. Seven tests in `txn_reactivity_test.rs`:

**Illustrating test:** `test_local_def_chain_refreshed_within_transaction` — the
minimal statement of the contract: write a var, read a def derived from it in
the same transaction, see the new value.

The rest pin the edges, and each one exists because an earlier attempt got it
wrong:

- `test_var_initialized_from_another_service_is_not_reactive` — `var`s are
  leaves. A `var` initialised from another service must **not** track it.
- `test_aborted_txn_leaves_def_unchanged` — no trace in `service.vars` after an
  abort.
- `test_failed_recompute_aborts_the_transaction` — a recompute that fails must
  abort, not be swallowed. Committing a state whose derived member does not
  follow from it is worse than failing.
- `test_recompute_in_txn_read_locks_same_service_dependencies` — the members a
  derived value rests on are held until commit, exactly as if the program had
  read them directly.
- `test_uncached_cross_service_dependency_is_read_under_the_transaction`
- `test_remote_dependency_is_never_served_from_dep_cache` — **the most important
  one.** `dep_cache` is a reactive push cache holding whatever the last `Update`
  delivered; it has no transactional meaning and carries no read lock. A value
  taken from it is not stable for the rest of the transaction. This test encodes
  a deliberate choice of serializability over availability. A change that makes
  it fail is a change to the contract, not a bug fix.

## Notes

- `meerkat/tests/s1.mkt` no longer exercises this; it passes as of PR #201. Do
  not mark it ignored, and do not document it as expected to fail. The
  end-to-end cases that still fail are those writing a var and reading a
  dependent def inside a single `do`.
