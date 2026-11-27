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
#294 every `@test` and REPL statement runs in a transaction, so this path now
serves only `watch` evaluations, propagation and subscription replies.

`remote_read_participant` already routes through `lookup`, so a participant
serving a transactional read of one of its defs gets this for free. Confirm with
`test_participant_transactional_read_sees_buffered_def`; do not add a second
path.

#### 1a. The clock and Sources of a computed def

A def has no clock dimension of its own. The entry a recompute stores in
`txn.read_cache` carries the **join of the clocks of the reads it made**, and
the **union of their Sources** (unknown if any input's Sources are unknown). An
input that is this transaction's own buffered write contributes no clock — the
write set covers it — and its own name as a Source. Nested def reads contribute
their own entries' clocks, so the join is the def's whole input history.

Locally this repeats what the input entries already hold. It is needed in two
places:

- **A participant answering a transactional read of a def.**
  `remote_read_participant` returns this pair through `lookup_with_clock`, and
  the originator merges the clock into `txn.participant_history`
  (`remote_lookup`, `manager/mod.rs:3057`). Without it, `x = remote.d + 1`
  would not carry the history of what `d` was computed from, which is the
  guarantee #262 added for statements.
- **Service initialization.** `input_candidates` seeds a def's first batch
  from the pairs in the initializing transaction's `read_cache`. When an
  initializer reads an earlier def, the computed pair must match what
  initialization stored for that def.

Record a recompute's reads in a frame on `Transaction`, pushed on entry and
popped on exit, beside §4's in-progress set.

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
- A recompute failure **aborts** the transaction. This holds even when the def
  has a stored value: a def whose last propagation failed keeps its old value
  outside transactions (propagation logs the failure), but a transaction that
  reads it fails rather than reading a value that does not follow from its
  view. Swallowing the failure would commit a state whose derived member does
  not follow from it, and would drop a `WaitDieAbort` or `WaitOn` from a live
  re-read, defeating task 06's retry loop.

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

That is exactly what `touched_services` reports, so bring that plumbing back —
repurposed from "recompute these defs now" to "invalidate these cached reads":

- `Transaction.remote_writes: HashSet<Symbol>` — a **new** field. `main`'s
  `Transaction` (`runtime/txn.rs`) does not have it; add it and initialise it
  empty in `Transaction::new`. Everything below reads or writes it.
- `ActionResponse.touched_services: Option<Vec<String>>` in
  `meerkat-lib/src/net/types.rs`, with `#[serde(default)]` so older peers still
  decode — as `None`, which step 2 treats differently from `Some(vec![])`.
- `codec::validate_touched_services(&[String]) -> Result<()>`.
- `Manager::touched_services_for_txn(&TxnId) -> Vec<String>` — the **transitive**
  set: the union of `txn.remote_writes` and the services named in `txn.written`.
  Transitivity matters because a node the originator never contacted may have
  been written by a nested action, and nothing else reports it.
- `ComposedCall` (task 10) gains `touched_services: Option<Vec<String>>`, so a
  **replayed** dispatch reports the same set as the original, and an unknown
  stays unknown. A replay that reported a narrower set would invalidate less and
  serve a stale value.
- The **producer**. Nothing on `main` fills the field, and it is easy to ship
  the whole consumer side without noticing: the participant's `ActionRequest`
  reply in `meerkat/src/main.rs` must call `touched_services_for_txn(&tid)` on
  success — while the transaction is still in `pending_txns` — and send
  `Some(result)`. The failure replies send `Some(vec![])`; they are about to
  abort the transaction anyway. A producer that is never wired up looks exactly
  like an old peer on the wire, which is why step 2 makes `None` conservative
  rather than benign.

On a successful `ActionResponse`, in `remote_action`:

1. **Merge the reported set into `txn.remote_writes`.** Nothing else populates
   it, and `touched_services_for_txn` reads it to report transitively upward —
   so without this merge a middle node reports only its own services and a
   write two hops down is invisible to the originator, whose cached def then
   stays stale. Validate with `validate_touched_services` before storing.
2. **"Nothing touched" and "not reported" are different answers.** `None` is
   the peer that did not send the field; `Some(vec![])` is the peer that ran and
   wrote nothing. Collapsing them into an empty vec turns version skew into
   silent staleness.

   On `None`, the reference branch's fallback — record the slug dispatched to,
   as `record_touched_services` does — is not enough. In `client -> mid -> rc`,
   a `mid` that does not report leaves the client believing only `mid` was
   touched, so a write on `rc` one hop further down never invalidates anything.
   Invalidate conservatively instead: every memoised def with any cross-service
   dependency, plus §3a's walk from each. That is the widest seed for the same
   primitive, and it is what `refresh_remote_cross_deps_in_txn` did on every
   composed action.
3. **Invalidate**, in two steps. First fix what "owned by a touched service"
   means, because the three names involved are not the same name: a reported
   entry is the *owner node's own* service name, `txn.read_cache` is keyed by
   `(ServiceNetId, Symbol)`, and `graphs.cross_deps` is keyed by in-scope local
   name. Match on the **slug** — `split_service_net_id(sid).1` for a cache key,
   and the same applied to `service_net_id_for_name(owner)` for a cross-dep.
   Comparing interned local names instead silently never fires under an import
   alias, which is exactly the staleness §3b exists to close. Slug matching can
   over-invalidate when two nodes each declare a service of that name; that
   costs a recompute and is the safe direction.

   Then find the *directly* affected entries: every memoised def owned by a
   touched service, plus every memoised def whose `graphs.cross_deps` name one.
   Then **run §3a's listener walk from each of them**, so anything derived from
   a directly affected def is dropped too.

   The second step is not optional. `graphs.cross_deps` records only a def's
   *own* cross-service dependencies, so with `def y = remote.x` and
   `def z = y + 1`, `z` has no cross-service dep at all. Stopping at the direct
   set drops `y` and leaves a stale `z` for the rest of the transaction.

   This is the same primitive as §3a with a different seed set: §3a seeds from
   a written member, §3b seeds from the directly affected defs. Implement
   invalidation once and call it from both.

This transaction's own buffered writes cannot be evicted: since #294 they live
only in `txn.written`, which `lookup` checks before `read_cache`, and
invalidation removes def entries only. Test 7 keeps that true.

The **replay** branch from task 10 must do all of this too, from the recorded
`ComposedCall`. A replay that skipped the merge or the invalidation would leave
the transaction with a narrower touched set and a stale cache than the original
run produced.

Record the `ComposedCall` **first**, ahead of the merge, the validation and the
invalidation. Note what this is and is not: none of those three can park — §3a's
walk is pure removal and §3b reuses it — so this is ordering discipline, not a
fix. It keeps the record ahead of the one fallible step there
(`validate_touched_services`) and ahead of anything fallible added later.

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

⚠️ **It is not sufficient when the writer is the originator, and that case is
out of contract.** The forced re-read of `a.x` arrives at A's
`remote_read_participant`, which builds a **fresh** `Transaction`: the
originator's live one is a local in `execute_transaction`
(`manager/mod.rs:3864`) and never enters `pending_txns`, so the read-back sees
pre-transaction state and returns the same stale value. Do not paper over it —
track the originator's in-flight ids (`Manager::active_txns: HashSet<TxnId>`)
and have `remote_read_participant` return an `EvalError` naming the transaction
when a read arrives for one, so this fails loudly instead of silently reading
around the buffered writes. Making it work means the originator's transaction
becoming addressable while it runs, which is #28's event-loop work.

Since #294 every `@test` and REPL statement is an originator transaction, so
this is reachable from any `for` loop or action that writes a local var and
then reads a remote def over it. Record the boundary in
`meerkat/tests/README.md` next to task 11's scenarios: a def on the originator
over a remote var is in contract; the inverse is not yet.

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

This is a new failure mode, not a ported one; give it its own test.

### 5. Do not reintroduce

- `propagate_in_txn`
- `refresh_remote_cross_deps_in_txn`
- the `propagate_in_txn` call in `assign` — `assign` buffers the write into
  `txn.written`, then runs §3a's invalidation walk, and nothing more. No
  recompute, no lock beyond the write lock, no network read happens in
  `assign`
- `txn.remote_writes` **as a recompute trigger**. It survives, populated and
  consumed as described in §3b, but only to drive invalidation and transitive
  reporting — never to decide what to recompute.
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
   transitive form (`def y = remote.x`, `def z = y + 1`, read `z` first):
   `z` has no cross-service dep of its own, so it passes only if §3b runs the
   listener walk rather than stopping at the directly affected defs.
7. **New — a composed action does not evict this transaction's own writes.**
   Write a local member, compose an action, then read that member back; it must
   still be the buffered value. Since #294 this holds by construction; the test
   keeps it so.
8. **New — transitive touched-services reporting.** In a `client -> mid -> rc`
   chain where only `rc` is written, a `client` def over `rc` must see the new
   value. Fails if §3b's merge into `txn.remote_writes` is missing — or if the
   participant reply never fills the field. Run the same chain a second time
   with the field **omitted from the wire** (the old-peer case): the client must
   still see the new value, via step 2's conservative path.
9. **New — §3c, both halves.** A participant's def over a *second participant's*
   var, read before and after a composed action that writes it, sees the new
   value. And a participant's def over the **originator's** var, read after the
   originator writes it, fails with the named error rather than returning the
   stale value.
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
    initializes `b` with the same clock and Sources as before this task.
16. **New — tables.** A def that folds over a table sees rows inserted earlier
    in the same transaction (`BufferedWrite::Append`), including in a `for`
    loop in a `@test` block.
17. **New — a def that fails is not read stale.** A def whose last propagation
    failed keeps its old stored value; a transaction that reads it aborts with
    the evaluation error instead of returning that value.

## Notes

- Expect this PR to be large. It is the one place where that is the right
  answer: the mechanism has to change in one step, and everything it leans on
  has already landed.
- Performance to state in the PR description: first read of a def with N remote
  dependencies costs N round trips; subsequent reads in the same transaction
  cost nothing until a composed action touches one of those services. Reading a
  remote def read-locks the def and every input it reaches, on every node
  involved, which since #294 includes every `@test` assertion that reads one.
  That is the price of fully transactional semantics (the draft semantics
  accepts it in §3.3.1) and it was accepted deliberately.
- Update `meerkat/tests/README.md`: remove #294's limitation that a local def
  keeps its cached value until a loop commits, record §3c's boundary, and
  state that a transaction reads a def's logically latest value while a `watch`
  shows the propagated one.
