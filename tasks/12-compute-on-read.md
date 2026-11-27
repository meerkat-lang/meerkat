# 12 — Compute derived members on read inside a transaction

**Depends on:** 02, 06, 10, 11 (and therefore, transitively, 05 and 07–09),
#217 (vector clocks) and #294 (each `@test` and REPL statement in its own
transaction). Land #257 before or with this task: compute-on-read widens the
locks a failed code update leaks.
**Reference:** `cli-test-fixes` — `recompute_def_in_txn` (`manager/mod.rs`
~line 1022) is the piece to **salvage**. `propagate_in_txn` (~line 908),
`refresh_remote_cross_deps_in_txn` (~line 985) and the `propagate_in_txn` call
in `assign` (~line 730) are the pieces **not** to reproduce.

*Revised 2026-11-27 for #217, #294, and the
[sequential code updates design](../docs/design/sequential-code-updates.md).
Line numbers on `main` are as of `60d95e1`.*

## Rationale

A `def` is an eagerly evaluated, cached value, so a plain lookup inside a
transaction returns whatever the last committed propagation stored. Task 11
states the contract this breaks.

PR #189 solved it by **pushing**: `assign` walked the listener graph and
recomputed every dependent def into `txn.read_cache`. That was rejected. Its
entire durable product is cache entries and read locks — `recompute_def_in_txn`
writes to `txn.read_cache` and never to `txn.written`, so no def value is ever
committed by it, and commit (`commit_txn`) stores vars only. If the def is
never read, all of that work is discarded at commit. Worse, it is not cheap
work: a purely local write to `x`, where some `def y = x + remote.z` exists,
forces a network round trip and a remote read lock — per assignment, so once per
iteration of a loop.

**Pull instead.** Compute a def when it is read, under the transaction, from the
transaction's own view. Work and locks then scale with what the transaction
actually observes, and propagation is left to do what it is for: notifying
listeners outside the transaction.

The semantics this implements: inside a transaction, reading a def evaluates
its current code against the transaction's view, the "logically latest value"
of the draft semantics' client-transaction rule (§3.3.1). A cache is allowed
only if it always gives the same answer.

Four consequences worth stating, because they are the point:

- Unread defs cost nothing and lock nothing.
- A def spanning a touched service and an unavailable one no longer aborts a
  transaction that never reads it. Note *how*: by not doing the work, not by
  weakening serializability. Do not reintroduce `dep_cache` fallback to achieve
  this.
- Pull evaluates in dependency order by construction, so glitch-freedom stops
  depending on the listener graph and a `changed` flag.
- The reads of one transaction form one snapshot. Today `install_recomputed`
  can overwrite a def's stored value while a transaction waits on a remote
  read, so `a + remote.r + b` can combine an old `a` with a new `b` (found in
  #294's review). A def computed from read-locked inputs cannot change under
  the transaction.

Transactional and non-transactional reads can see different values of the same
def at the same moment: a transaction sees the logically latest value, while a
`watch` or a remote subscriber sees the propagated value, which may lag. That is
the difference between transactional semantics and non-transactional
propagation, and it is intended.

## Specification

### 1. Hook the read path

`Manager::lookup` (`manager/mod.rs:1390`) resolves a local member inside a
transaction in this order: the transaction's own buffered write
(`txn.written`, read through `BufferedWrite::read`), then `txn.read_cache`,
then the stored value under a read lock. Defs live in `vars` too, so it cannot
currently tell them apart.

Inside a transaction, when the member is a def (`service.defs.contains_key`):

1. **Take the read lock on the def member itself**, exactly as the current code
   does before returning a stored value — `(service_net_id, name)` recorded in
   `txn.locked`. Do not drop this step on the grounds that §2 locks the
   dependencies: an `update` transaction write-locks **defs as well as vars**
   (`runtime/update.rs:156-166` puts both `Decl::VarDecl` and `Decl::DefDecl`
   into the lock group's write set), so without it a concurrent update can
   replace the def's *expression* while this transaction is evaluating and
   memoising it. Locking dependencies protects the inputs; this protects the
   definition.
2. Serve from `txn.read_cache` if present (see §3 on what invalidates it).
3. Otherwise recompute it (§2), insert the result into `txn.read_cache` with
   the clock and Sources of §1a, and return it.

Once #293 lands, a def is never in `txn.written`, because an action cannot
write one. Until then, a def an action wrote is served from the buffer like
any written member, which is #293's bug, not this task's.

Read the def's code through one accessor that takes the transaction, for
example `Manager::def_code(service, def, txn)`, returning `service.defs[def]`
today. Do not read `service.defs` directly in the recompute. Unit I of the
[sequential code updates plan](../docs/plan/sequential-code-updates-plan.md)
makes that accessor return code staged by an update in progress (§6).

Outside a transaction, behaviour is unchanged: return the stored value. Since
#294 every `@test` and REPL statement runs in a transaction, but this path
still has other callers: `watch` evaluations, propagation, subscription
replies, and ordinary non-transactional remote reads (`remote_lookup` sends a
`LookupRequest` with `txn_id: None`, which `serve_request` answers through
`lookup_with_clock(…, None)`).

`remote_read_participant` already routes through `lookup`, so a participant
serving a transactional read of one of its defs gets this for free. Confirm with
`test_participant_transactional_read_sees_buffered_def`; do not add a second
path.

#### 1a. The clock and Sources of a computed def

A def has no clock dimension of its own. The entry a recompute stores in
`txn.read_cache` is a receipt for the value it computed, and it must be a pair
that propagation's batch check accepts, because initialization feeds it to
that check (below). So build it with **the rule initialization already uses
for a def's first pair**, over the def's **static inputs**
(`graphs.inputs`), not just the reads this evaluation made:

- an input the evaluation read contributes its receipt (for a var, its
  snapshot pair; for a def, its own receipt; for this transaction's buffered
  write, no clock and its own name as a Source);
- a local input the evaluation did not read, such as one in a branch not
  taken, contributes its current state;
- a remote input the evaluation did not read makes the receipt's Sources
  unknown, without a network read.

The receipt's clock is the join of those clocks and its Sources the union of
those Sources (`join_receipts`); reuse `input_candidates` and `join_receipts`
rather than writing a second version. Do not apply `close_batch`'s rejection
to the receipt itself: the receipt only reports what the value is consistent
with, and the batch check happens where it is consumed.

Why the unread inputs count: with `def a = if s.flag then s.x else s.y;` and
`flag == true`, a receipt with only the reads made would have no clock for
`s.y` while `a`'s Sources include it. If `s.y` had been updated, a downstream
`def b = a + s.y;` would then fail `close_batch`, because `a` would appear
not to have caught up on `s.y`. Including `s.y`'s current state is sound: `a`'s
value is the same for any value of the branch it did not take, so it agrees
with `s.y`'s current value too.

The same receipt serves the transaction's history: `transaction_history`
joins its clock into the commit stamp. The clocks of unread inputs make that
stamp larger than the reads strictly require, which is the conservative
direction: a later gate may wait for history it does not need, but never
accepts an inconsistent batch (the trade-off #262's session clock already
makes). One representation, used for both purposes, keeps the two from
drifting apart.

The receipt is consumed in two places:

- **A participant answering a transactional read of a def.**
  `remote_read_participant` returns the receipt through `lookup_with_clock`,
  and the originator merges its clock into `txn.participant_history`
  (`remote_lookup`, `manager/mod.rs:3057`). Without it, `x = remote.d + 1`
  would not carry the history of what `d` was computed from, which is the
  guarantee #262 added for statements. During initialization the originator
  also keeps the pair to seed the def that read it.
- **Service initialization.** `input_candidates` seeds a def's first batch
  from the pairs in the initializing transaction's `read_cache`. For an input
  that is a def, that pair is now its receipt, built by the same rule, so the
  batch check sees what it would have seen for an installed def.

As with initialization today, the current state of an unread input is sampled
without a read lock, so an input updated between that sample and a later
locked read can still make a downstream batch check fail. That is today's
behavior, not a new failure; locking unread inputs would give up "unread
defs and branches lock nothing".

Record a recompute's reads in a frame on `Transaction`, pushed on entry and
popped on every exit, beside §4's in-progress set, so the rule above knows
which inputs were read.

### 2. The recompute

Reuse the body of `recompute_def_in_txn` from the reference branch. It is most
of the work and each part of it is load-bearing:

- Evaluate with an **empty environment**, so every same-service dependency goes
  through `lookup` and takes a read lock. An earlier version seeded `env` from
  the service's members; `Expr::Variable` then resolves from `env` and never
  reaches `lookup`, which is the only thing that takes a lock. The members a
  derived value rests on are part of the transactional view it commits under, so
  under 2PL they must be held.
- **Pass `inputs: None`** to `eval_with_inputs`
  (`interpreter/evaluator.rs:109`). Since #217, propagation evaluates a def
  against a selected batch passed as `inputs`, and `lookup_input` answers a
  member from that batch before falling back to `lookup`. A batch answers
  without a read lock and from pairs delivered to the def, so a transactional
  recompute must never receive one. With `None`, every member read reaches
  `lookup`.

  This replaces the reference branch's `reactive_cache` handling, which #217
  removed. Because the batch is now a parameter rather than state on the
  `Manager`, an inbound `Update` that re-enters non-transactional propagation
  while this recompute awaits a remote read cannot leak its inputs into it, and
  there is nothing to save and restore.

  Memoisation is not lost: `lookup` serves from `txn.read_cache` (§3), which is
  the transactional cache and does carry locks.
- Distinguish two kinds of error from a recompute:
  - **An evaluation error** (a runtime error in the def's code) **aborts** the
    transaction. This holds even when the def has a stored value: a def whose
    last propagation failed keeps its old value outside transactions
    (propagation logs the failure), but a transaction that reads it fails
    rather than reading a value that does not follow from its view.
    Swallowing it would commit a state whose derived member does not follow
    from it.
  - **A transaction-control error** (`WaitOn`, `WaitDieAbort`) from a read
    inside the recompute **propagates unchanged** into the existing park and
    retry handling (task 06 for originators, parking for participants). It is
    not an evaluation failure and must not be turned into one.

  Either way, §1a's read frame and §4's in-progress marker are popped on the
  way out, including when the error is `WaitOn` and the action parks.

Inside a transaction, then, a cross-service dependency is **always** read from
its owner under the shared transaction id. `remote_lookup` pre-registers the
owner in `txn.participants`, so `Commit`/`Abort` releases the read lock it takes.
`remote_lookup` does not cache the value outside initialization; the def's own
`read_cache` entry is what saves the round trip on a second read.

### 3. Memoisation and invalidation

Without memoisation, every read of a def costs one round trip per remote
dependency. Read locks make a remote read stable against *other* transactions,
so the cache in §1 is sound — with one exception, which is why `remote_lookup`
deliberately never caches: **this transaction's own composed action** can
write, on the owner, a member this transaction already read.

There is a second, entirely local version of the same problem, and it is the
common case: a transaction reads `def y = x + 1`, then writes `x`, then reads
`y` again. The write is buffered in `txn.written`, so a later read of `x` sees
it, but the memoised `y` from the first read is served unchanged and the
transaction sees a `y` that does not follow from its own `x`. Read-your-own-
writes, broken by the very cache that makes pull affordable.

Two rules apply to every invalidation below:

- **Only memoised def entries are removed.** A var's `read_cache` entry is a
  snapshot held under a read lock; removing it would not make a later read more
  current, and buffered writes live in `txn.written`, which invalidation never
  touches.
- **Removing an entry must not forget a read.** `transaction_history`
  (`manager/mod.rs:1539`) builds the transaction's `V_base` from the clocks in
  `read_cache`. Before removing a def entry, merge its clock into the
  transaction's accumulated history (`participant_history`, or a dedicated
  field if that name becomes misleading). Otherwise a read that happened would
  vanish from the commit stamp.

#### 3a. Invalidate on local write

`assign` and `append_table_row`, inside a transaction, must **remove from
`txn.read_cache` every def transitively downstream of the member they wrote**,
after buffering the write. Both are synchronous since #294 and only buffer;
the walk keeps them that way.

Walk `service.listeners` from the written member, transitively, and drop each
def's entry. This is the "mark dirty" half of the pull design, and the
distinction from the eager approach we rejected is the whole point: this walk
**evaluates nothing, takes no locks, issues no network reads and cannot fail**.
It is a `HashMap::remove` per dependent. Do not recompute here; the next read
will, if there is one.

Transitivity is required (`z` derives from `y` derives from `x`), as is
following listeners into other services on this node.

#### 3b. Invalidate on composed action

A composed action can write, on another node, a member that a memoised def
here was computed from, directly or through nested composed actions further
down. Start conservatively: **after a successful composed action, remove every
memoised def entry**, following the two rules above (def entries only, and
their clocks merged into the transaction's history first). Var snapshots and
accumulated history stay. A def read again afterwards is recomputed; unread
defs still cost nothing.

The **replay** branch from task 10 must do the same: when a replayed run
skips a composed action recorded in `composed_done`, it clears memoised def
entries exactly as the original run did after that action. Otherwise the
replay keeps entries the original run had dropped and serves stale values.

This replaces the reference branch's `touched_services` protocol and the new
response field, transaction set, validator and replay record that an earlier
draft of this spec reintroduced. They are not needed:

- Nested writes are already reported. `ActionResponse.writes` carries the
  participant's whole write set (`transaction_history`, which includes its own
  participants' writes), built in `Manager::dispatch_parked`, and
  `remote_action` merges it into `txn.participant_writes`. Every node sends
  it, so there is no "not reported" case to carry upward.
- Clearing every memoised def is correct without any of that information. It
  costs recomputation of defs read again after a composed action, which
  already costs a round trip itself.

If profiling later shows that recomputation matters, a more precise
invalidation can seed §3a's walk from the members in `ActionResponse.writes`:
memoised defs whose `graphs.cross_deps` name a written member, and everything
downstream of them. Do that as a separate change, from the existing field,
not with a new protocol.

This transaction's own buffered writes cannot be evicted: since #294 they live
only in `txn.written`, which `lookup` checks before `read_cache`, and
invalidation removes def entries only. Test 7 keeps that true.

#### 3c. A participant invalidates on its own requests

§3a and §3b both run on the node that writes or composes, and neither can reach
a **participant's** `pending_txns[tid].read_cache`. That cache has the mirror
problem: with `def b.y = a.x` on B and the transaction originating at A, A reads
`b.y`, writes `x`, reads `b.y` again — and B serves the `y` it memoised from the
pre-write `x`.

A participant can only act on the one event it sees, so: **when a request
arrives under a transaction id already in `pending_txns`, invalidate before
running it.** At the top of `execute_action_participant` and
`remote_read_participant`, in the branch where the transaction came out of the
map rather than being freshly created, seed §3a's walk with every memoised def
whose `graphs.cross_deps` are non-empty. Locally derived defs stay: their inputs
are on this node, read-locked by this transaction, and this node's own writes
already invalidate through §3a.

That is sufficient whenever the writer is another *participant* — its buffered
write is in its own `pending_txns` entry and the forced re-read finds it.

⚠️ **It is not sufficient when the writer's transaction is checked out,
and that case is out of contract.** A node's transaction state is out of
`pending_txns` while it runs there: the originator's live transaction is a
local in `execute_transaction` (`manager/mod.rs:3864`) and never enters the
map, and `execute_action_participant` and `remote_read_participant` remove a
participant's transaction from the map for as long as they run. A read-back
that arrives meanwhile builds a **fresh** `Transaction`, sees pre-transaction
state, and returns a stale value. For example, in `client → A → B`, A buffers
`x` and then calls B, whose computation reads A's `x` while A's action is
still running.

Do not paper over it. Track the transaction ids whose state is checked out on
this node (`Manager::active_txns: HashSet<TxnId>`): an originator while
`execute_transaction` runs, and a participant while
`execute_action_participant` or `remote_read_participant` holds its
transaction outside the map. Have `remote_read_participant` return an
`EvalError` naming the transaction when a read arrives for one, so this fails
loudly instead of silently reading around the buffered writes. Clear the mark
on every exit: success, failure, and parking (a parked participant's
transaction goes back into `pending_txns`). Making these read-backs work
means a transaction's state being addressable while it runs, which is #28's
event-loop work.

An action composed back onto a node whose transaction is checked out has the
same problem and is outside this task; check whether any existing test does
that, and record it with the boundary below if one does.

Since #294 every `@test` and REPL statement is an originator transaction, so
this is reachable from any `for` loop or action that writes a local var and
then reads a remote def over it. The implementation PR records the boundary in
`meerkat/tests/README.md` next to task 11's scenarios: a def on a node over a
var whose writer is checked out is not yet in contract.

### 4. Cycle detection

Push terminated cascades with `recompute_def_in_txn`'s `changed` return value.
Pull recurses instead, so a cyclic def is infinite recursion rather than a
terminating fixpoint.

Add an in-progress set keyed by `(ServiceNetId, Symbol)`, entered before
evaluation and left after. Re-entering a def already in progress is an
`EvalError` naming the def.

**Put the set on `Transaction`, not on `Manager`.** A recompute awaits remote
reads, and `send_and_await_reply` pumps network events while it waits, so a
second transaction can begin evaluating the same def on this node while the
first is suspended. A manager-wide set would report that as a cycle — a
spurious, load-dependent failure on a perfectly acyclic program. `Transaction`
already owns the rest of the per-transaction evaluation state (`read_cache`,
`locked`, `written`), and this belongs with it, as does §1a's frame of reads.

Leave the set on every exit from the recompute, including an evaluation
error and a `WaitOn` that parks the action (§2); a marker left behind would
make the next read of that def report a cycle.

This is a new failure mode, not a ported one; give it its own test.

### 5. Do not reintroduce

- `propagate_in_txn`
- `refresh_remote_cross_deps_in_txn`
- the `propagate_in_txn` call in `assign` — `assign` buffers the write into
  `txn.written`, then runs §3a's invalidation walk, and nothing more. No
  recompute, no lock beyond the write lock, no network read happens in
  `assign`
- the `touched_services` protocol (`ActionResponse.touched_services`,
  `Transaction.remote_writes`, `touched_services_for_txn`,
  `validate_touched_services`). §3b clears memoised defs instead, and
  `ActionResponse.writes` already reports nested writes.
- a propagation batch (`inputs`) passed to a transactional recompute (§2).

The non-transactional path is unchanged: defs remain stored values, and
commit-time `propagate` still recomputes and still notifies remote listeners.
Push outside a transaction, pull inside, is a deliberate asymmetry — say so in
the code, because the reference branch's parallel `propagate`/`propagate_in_txn`
naming will otherwise invite someone to restore the symmetry.

### 6. Code updates

Under the sequential code updates design, an update's var steps buffer their
values in the transaction's ordinary write buffer. Compute-on-read therefore
covers an unchanged def read during an update with no update-specific hook:
§3a invalidates it when a step writes one of its inputs. This replaces the
requirement, from #266's review, that compute-on-read treat
`Transaction::staged` as the transaction's writes; `Transaction::staged` exists
only on #266's branch, which was closed.

Defs that an update adds or replaces are unit I of the plan, not this task.
Unit I uses §1's code accessor to read staged code, and the same recompute for
the evaluation of each declared def before commit. This task only has to leave
those two hooks in place.

## Tests

1. **Un-ignore everything task 11 ignored.** That suite is the acceptance
   criterion: `test_local_def_chain_refreshed_within_transaction`,
   `test_aborted_txn_leaves_def_unchanged`,
   `test_def_over_another_service_refreshed_within_transaction`,
   `test_participant_transactional_read_sees_buffered_def`,
   `test_failed_recompute_aborts_the_transaction`,
   `test_remote_dependency_is_never_served_from_dep_cache`,
   `test_uncached_cross_service_dependency_is_read_under_the_transaction` and
   `test_recompute_in_txn_read_locks_same_service_dependencies`, all in
   `meerkat-lib/tests/txn_reactivity_test.rs`. The last two and
   `test_remote_dependency_is_never_served_from_dep_cache` most directly
   constrain this design.
2. **The test task 10 could not land stays unlanded.** §3b is pure cache
   removal and cannot park, so nothing here reintroduces a park *after* a
   successful dispatch within the same `do`. Do not manufacture one to land
   `test_a_park_inside_the_do_statement_does_not_re_send_the_child_action`; say
   in the PR that it is still uncovered.
3. **New — read, write, read again.** With `def y = x + 1`: read `y`, write
   `x`, read `y` again in one transaction; the second read reflects the write.
   This is the test for §3a, and it is the one that fails if memoisation is
   added without local invalidation. Add the transitive form too (`z` over `y`
   over `x`), because a non-transitive walk passes the two-level case. Since
   #294, also cover it from a `@test` block: a `for` loop is one statement and
   one transaction.
4. **New — cycle detection:** a cyclic def read inside a transaction returns an
   error naming the def, and does not hang or overflow the stack.
5. **New — memoisation:** a def with a remote dependency, read twice in one
   transaction, issues one remote lookup.
6. **New — invalidation on a composed action:** the same def, read before and
   after a composed action that writes its remote dependency, returns the
   updated value on the second read. This is the test for §3b. Include the
   transitive form (`def y = remote.x`, `def z = y + 1`, read `z` first), and
   check that var snapshots and the transaction's accumulated history survive
   the clearing. Repeat it with a participant that parks and replays, so the
   replayed run clears memoised defs where the original did.
7. **New — a composed action does not evict this transaction's own writes.**
   Write a local member, compose an action, then read that member back; it must
   still be the buffered value. Since #294 this holds by construction; the test
   keeps it so.
8. **New — nested writes.** In a `client -> mid -> rc` chain where only `rc`
   is written, a `client` def over `rc`, read before and after the composed
   action, sees the new value on the second read.
9. **New — §3c.** A participant's def over a *second participant's* var, read
   before and after a composed action that writes it, sees the new value. A
   read-back to a node whose transaction is checked out fails with the named
   error rather than returning a stale value, in both forms: a participant's
   def over the **originator's** var, read after the originator writes it; and,
   in `client → A → B`, B reading A's `x` that A buffered before calling B,
   while A's action is still running. Check that the mark is cleared after the
   action succeeds, fails, and parks, so a later read for the same transaction
   is served normally.
10. `meerkat/tests/s1.mkt` still passes. It has passed since PR #201 made each
    `do` in an `@test` its own transaction, so it no longer exercises
    read-your-own-writes and is only a regression check. Tests 1 and 3 are
    the ones that show compute-on-read working.
11. **New — a participant's read of a def locks the def's inputs.** A
    transactional read of a def on another node read-locks, on that node, the
    inputs the def was computed from, and `Commit` or `Abort` releases them.
    (Carried over from #266's review.)
12. **New — two nodes, §3b through the transaction's own lookup.** Write through
    a remote action, then read the remote def through the transaction's own
    `lookup` in the same transaction, and see the write. Deferred from #229's
    review, whose participant test calls `remote_read_participant` directly.
13. **New — one snapshot under a concurrent delivery.** A transaction evaluates
    `a + remote.r + b`, where `a` and `b` are local defs over a shared remote
    source, and a delivery for that source arrives while it waits on
    `remote.r`. The result combines `a` and `b` from the same snapshot.
14. **New — history through a def (§1a).** `x = d + 1`, with `d` a local def over
    `u`, gives `x` a clock that includes `u`'s history; the same with a remote
    `d` includes the history of what the owner computed `d` from. And a
    memoised def invalidated by a later write still leaves its inputs' history
    in the commit stamp.
15. **New — initialization.** A service whose def `b` reads an earlier def `a`
    initializes, with the same Sources for `b` as before this task. Cover:
    - an unconditional chain;
    - `a` choosing between local `x` and `y` with a condition, one branch not
      taken: both stay in `b`'s Sources;
    - the case where the untaken input is also read directly downstream and
      has an advanced clock: with an existing service `s` where
      `flag == true` and `s.y` has been updated, a new service declaring
      `def a = if s.flag then s.x else s.y;` and `def b = a + s.y;`
      initializes `b` without a batch error;
    - `a` choosing between a local input and a remote one, with the remote
      branch not taken: `b`'s Sources stay unknown, as today.
16. **New — tables.** A def that folds over a table sees rows inserted earlier
    in the same transaction (`BufferedWrite::Append`), including in a `for`
    loop in a `@test` block.
17. **New — a def that fails is not read stale.** A def whose last propagation
    failed keeps its old stored value; a transaction that reads it aborts with
    the evaluation error instead of returning that value.
18. **New — waiting inside a recompute.** A recompute that meets a held lock
    returns `WaitOn` (or `WaitDieAbort`) unchanged: an originator retries and a
    participant parks, rather than aborting with an evaluation error, and the
    read frame and in-progress marker are gone afterwards, so the retried or
    resumed read succeeds without a spurious cycle error.

## Notes

- Expect this PR to be large. It is the one place where that is the right
  answer: the mechanism has to change in one step, and everything it leans on
  has already landed.
- Performance to state in the PR description: first read of a def with N remote
  dependencies costs N round trips; subsequent reads in the same transaction
  cost nothing until a local write to one of its inputs or a composed action
  (which, for now, clears every memoised def). Reading a
  remote def read-locks the def and every input it reaches, on every node
  involved, which since #294 includes every `@test` assertion that reads one.
  That is the price of fully transactional semantics (the draft semantics
  accepts it in §3.3.1) and it was accepted deliberately.
- `meerkat/tests/README.md` describes current behavior, so this spec revision
  leaves it unchanged: its statement that a local def keeps its cached value
  until a loop commits stays true until task 12 lands. The implementation PR
  updates it: remove that limitation, record §3c's boundary, and state that a
  transaction reads a def's logically latest value while a `watch` shows the
  propagated one.
