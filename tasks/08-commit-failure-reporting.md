# 08 — Report commit failures instead of swallowing them

**Depends on:** 07.
**Reference:** `cli-test-fixes` — `struct ParticipantCommit`
(`manager/mod.rs` ~line 122), the commit block in `execute_action_with_txn`
(~line 2595), `commit_participant` (~line 2768), and
`meerkat-lib/tests/commit_failure_test.rs`.

## Rationale

The result of committing a participant was discarded with `let _ = ...`. A
participant that refuses the commit, or never answers, left the originator
returning `Ok(())`.

The user-visible symptom: the CLI prints `@test(...) passed` for a transaction
only part of which is actually committed.

## Specification

Introduce a return type carrying **two independent facts**:

```rust
/// What a participant's `Commit` left behind: the locks it released, and any
/// failure forwarding that commit to the nodes below it.
#[derive(Debug, Default)]
pub struct ParticipantCommit {
    /// Locks released by this commit, to wake anything parked on them.
    pub freed: HashSet<WaitKey>,
    /// Failure forwarding `Commit` to a sub-participant, reported to the
    /// originator once the local commit has finished.
    pub forward_error: Option<EvalError>,
}
```

`commit_participant` returns this instead of `HashSet<WaitKey>`.

**Do not fold these into a `Result`.** The caller needs both on the failing
path. The local commit is irreversible once the writes are stored, so a
forwarding failure is something to report upward — not a reason to skip waking
whatever was parked on the locks this commit just freed. A `Result` forces the
caller to choose one, and it will choose wrong.

The three commit paths (`create_service`, `execute_action_with_txn`,
`commit_participant`) repeat the same store / commit / propagate sequence, and
each handles the commit loop's errors differently. They share one function,
`commit_txn`, into which `store_committed_writes` and
`propagate_committed_writes` are folded. Its commit loop,
`commit_participants`, records the **first** failure and keeps going (every
participant still needs its `Commit`). "First" is in the participant set's
iteration order, which is arbitrary. `commit_txn` then still propagates, and
returns the recorded error, which `execute_action_with_txn` returns in place
of `Ok(())`. The live-update path in `update.rs` also calls
`commit_participants`, but ignores its result (#180).

### The originator's side of a refused commit

`send_commit` reconstructs an `EvalError` from `CommitResponse`'s `error`
string (~line 2643 on `main`). It is the fourth such site in `manager/mod.rs`;
task 05 routed the other three through `Manager::remote_error`, and both that
task's spec and the reference branch missed this one. It only starts to matter
here, because `execute_action_with_txn` discards its result today (`let _ =
self.send_commit(...)`, ~line 2166) — the line this task removes.

Decide explicitly rather than by default, and record the reasoning in the PR:

- **Route it through `remote_error`** and a `WaitDieAbort` raised beneath a
  participant's commit is preserved as that variant rather than flattened.
- **Leave it flattened** to `LocalDispatchFailed` and every refused commit
  arrives as one terminal kind.

Note first what the control flow does *not* do, since the choice is easy to
misread as being about retries. The wait-die retry branch is gated on
`exec_error` — the failure of a *statement*, set before the commit loop is
reached — and it is the only path that loops. A failure recorded in the commit
loop is handed to the unconditional `return` that follows, so it cannot
re-enter the retry budget. Nor can it do so on another node:
`execute_action_with_txn` runs only on the originator, because a participant
serving a remote action goes through `execute_action_participant`, which has no
retry loop. Either choice here changes what the failure is *called*, not
whether the action runs again.

The default is to leave it flattened, and the reason is what the name claims
rather than what the loop does. `remote_error` exists to preserve one
distinction — that a failure is routine contention, safe to retry — and
`WaitDieAbort` is the variant carrying it. A refused commit is not safe to
retry: the writes above it are already durable (#191). Labelling it
`WaitDieAbort` would put that false claim into the one variant whose whole
meaning is retry-safety, where anything keying off it later would believe it —
and task 06 adds more machinery keyed off exactly that variant. If the
reasoning turns out to be wrong it is a one-line change, which is the point of
having `remote_error` in one place.

## Tests

`meerkat-lib/tests/commit_failure_test.rs` (2 tests).

**Illustrating test:** `test_originator_surfaces_a_participant_commit_failure` —
a stand-in participant refuses the commit; the originator's `execute_action`
must return `Err`. On `main` it returns `Ok` and the CLI reports the test as
passing.

Also: `test_participant_reports_a_failed_commit_forward_and_still_propagates`,
which pins the "both facts, independently" property — a middle node whose
downward forward fails must still store, still propagate, still free its locks,
*and* report the error.

## Notes

- This does **not** make the transaction atomic under a refused commit. The
  writes above the failure are already durable. That is tracked separately as
  issue #191. This task makes the failure *visible*, which is a precondition for
  fixing it, not the fix. Say so in the PR description so it is not mistaken for
  a completeness claim.

- **Typed wire errors: considered here, still deferred.** Raised as a nitpick
  on #199. `remote_error` classifies a remote failure by matching the `Display`
  prefix `WaitDieAbort` writes, so a human-readable string is load-bearing as a
  protocol field. This task is the natural place to re-examine that, because it
  is the first to let a remote failure string *change the originator's result*
  rather than only be reported: `execute_action_with_txn` returns `Err` on the
  strength of text a participant sent. What it does not do is add a second
  wire-surviving variant — under the default above a refused commit is
  flattened to `LocalDispatchFailed`, like everything that is not a wait-die
  abort. It is still not worth acting on:
  - All six error-carrying `MeerkatMessage` variants are `String` /
    `Option<String>`, filled by ~19 producers with `e.to_string()`. Typing one
    makes it inconsistent with the other five; typing all six is ~32 sites plus
    a wire-side error enum, since `EvalError` is not `Serialize` and
    `WaitOn(WaitKey)` cannot trivially become so. Neither belongs in the same
    diff as a commit-reporting fix.
  - The version-skew argument for it does not apply. There is no version
    negotiation beyond libp2p's exact-match `/meerkat/1.0.0` and no
    `#[serde(default)]` anywhere, so adding a typed field is itself a breaking
    struct change: it relocates the mixed-version hazard from "misread as
    terminal" to "message fails to decode" rather than removing it.

  Revisit when a **second `EvalError` variant has to survive the wire as
  itself**. Today exactly one does — `WaitDieAbort` — and everything else is
  deliberately flattened to `LocalDispatchFailed`; task 06 does not add one,
  since it converts an escaping `WaitOn` into a `WaitDieAbort` before it
  leaves. The call-site count is not the trigger: `remote_error` gaining a
  fourth call site above changes nothing, because all four discriminate the
  same single variant. Revisit also when protocol versioning arrives.
