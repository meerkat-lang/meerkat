# Task 12 read-back / abort spike

2026-11-27. Based on main `f414bb5`; assesses the proposal on
`task12-readback-boundary` through `8cca80c`. PR #301 subsequently adopted
the narrower cleanup guarantee and commit boundary in `127ffe7`.

## Result

The proposed rejection and early-abort-acknowledgement mechanism breaks the
read-back cycles we identified, including indirect abort forwarding through
previous participant relationships. Real-node tests complete the failed
operation and cleanup in approximately 0.13–0.36 seconds, well below the
15-second reply timeout. They verify discarded writes, released locks, empty
participant state, and successful subsequent writes.

The stronger assumption that **every** running execution promptly unwinds
after acknowledging an abort is false. A separate probe injects an abort
while the execution waits on a real child parked on another transaction's
lock. The node acknowledges and becomes doomed, but remains in its RPC wait.
Only after the test releases the child's lock does the wait return and cleanup
finish. The doomed check prevents the child's successful reply from causing a
commit. The test observes delayed cleanup; it is not an acceptance test for
prompt cancellation. Production `WaitParked` keepalives can keep such a wait
alive while the lock remains held.

An upstream timeout is one way an abort can arrive without a failing child
reply to unblock that wait. Thus the spec's assumption that the wait always
returns promptly after an abort needs to be narrowed or replaced. The spike
does not implement cancellation of an outstanding RPC or arbitrary execution.

## Prototype

Branch: `task12-abort-spike`.
Worktree: `/tmp/meerkat-task12-abort-spike`.
This branch preserves the prototype and its tests for reference; it is not
the complete implementation of PR #301. PR #300 and its branch are unchanged.
In particular, service initialization, cascading lock requests, and code updates
still need registration integration and tests before the replacement for
PR #300 can land. The shared checkout helper is also not implemented here.

- `manager/running.rs`: counted registrations with a shared doomed flag and
  RAII removal. Nested registrations preserve the outer state. Dropping a
  future removes its registration, not its transaction locks.
- The dispatcher rejects requests for a registered transaction and immediately
  acknowledges its aborts, setting the doomed flag before acknowledgement.
- Successful remote reads/actions and completed executions check that flag.
  Existing terminal errors retain their diagnostic. A doomed execution cannot
  park or retry as though the abort were ordinary lock contention.
- A typed error survives forwarding through the existing error-string fields;
  no wire messages or transaction fields were added.

Production changes total about 170 added lines, plus the test harness. This
supports keeping the rejection/unwinding implementation a separate reviewable
slice. The harness builds and sends ASTs through real managers and ephemeral
TCP listeners; it does not implement compute-on-read.

## Boundaries established by the spike

1. **Register recursive abort cleanup too.** The three execution entry points
   alone are insufficient. An idle participant removed from `pending_txns`
   can recursively await an abort back to itself. The spike registers
   `abort_participants`, including calls from `abort_participant`, so these
   cycles also receive an immediate acknowledgement. The original execution's
   registration remains alive through its own cleanup and lock release.
2. **End execution registration at the commit boundary.** The originator checks
   its doomed flag and removes its execution registration before entering
   `commit_txn`. There is no await between the check and that transition.
   `commit_stamped` installs writes before awaiting participant replies; a
   later doomed check cannot undo them. The spike retains existing handling
   of aborts received during commit. A protocol probe explicitly verifies that
   a late abort cannot roll back an installed value. This is a boundary test,
   not a resolution of #191. Commit-clock failure still registers its abort
   cleanup through `abort_participants`.

## Evidence

`meerkat-lib/src/runtime/manager/readback_spike_tests.rs` contains seven tests:

- Five real-node read-back scenarios: lookup callback, action callback, longer
  call chain, earlier participant relationship, and the originator→D→A then
  originator→A→B→D / B→A sequence from review.
- Nested registration / doomed-mark lifetime.
- A successful RPC reply after an abort, unrelated-transaction deferral, and
  the separate post-commit boundary probe.
- Future-drop registration cleanup and a doomed park result.
- An abort cycle between idle participants, with no read-back rejection.
- Actual participant parking and subsequent read/abort cleanup.
- The parked-child limitation described above. The abort is injected through
  an established peer connection; both executing and parked nodes are real
  managers. Releasing the child's lock lets the operation fail and clean up.

The initial raw-peer version of the last probe intermittently lost its second
send with a transport "connection is closed" error. It was replaced with the
established-connection probe, which exercises the same manager behavior without
that extra connection setup. No transport fix is claimed.

The original local validation logs are in `/tmp/task12-spike-*.log`; those
temporary files are not part of the archived evidence. The test source and
results summarized here are preserved on this branch. Formatting, Clippy with
warnings denied, the WASM check, the CLI smoke test, and all 37 multi-node
regressions passed. The final workspace run passed 592 tests, with 8 existing ignored
tests and 2 `multiple_message` tests filtered out. All seven spike tests
also passed on a separate repeat run. The existing manifest warning about `inline_colorization`
remains; no unrelated dependency changes were made.

## Reproducing the evidence

Run the focused probes with:

```bash
cargo test --locked -p meerkat-lib --all-features readback_spike_tests -- --nocapture
```

For the full regression checks:

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo check --locked -p meerkat-lib --target wasm32-unknown-unknown --all-features
cargo test --locked --workspace --all-features -- --skip multiple_message
cargo run --locked -p meerkat -- --help
cargo build -p meerkat
python3 scripts/test_mkn.py
```

The focused probes use ephemeral TCP ports. The multi-node regression script
uses fixed ports; ensure no previous test nodes are still running.

## Recommendation

The rejection-cycle implementation appears small enough for the first PR,
subject to validating the additional registration paths. PR #301 adopted the
second of these scope choices in `127ffe7`:

- If prompt cleanup is required for arbitrary aborts of running executions,
  this prototype is insufficient. It needs abort-aware RPC cancellation (and
  its cleanup obligations), or different sequencing with #213. Do not infer
  that capability from the doomed flag or RAII guard.
- If this slice guarantees prompt unwinding of read-back rejection chains,
  state that narrower contract explicitly and retain the existing limitations
  for timeout/cancellation cleanup. An early acknowledgement accepts an abort;
  it does not certify that every lock has already been released.

Do not silently extend the first slice to full suspendable execution or
post-commit rollback. The tests establish the stated cases, not a general
proof for delayed, lost, or duplicated messages.
