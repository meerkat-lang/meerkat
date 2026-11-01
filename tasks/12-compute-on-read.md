# 12 — Compute derived members on read inside a transaction

**Depends on:** 02, 06, 10, 11 (and therefore, transitively, 05 and 07–09).
**Reference:** `cli-test-fixes` — `recompute_def_in_txn` (`manager/mod.rs`
~line 1022) is the piece to **salvage**. `propagate_in_txn` (~line 908),
`refresh_remote_cross_deps_in_txn` (~line 985) and the `propagate_in_txn` call
in `assign` (~line 730) are the pieces **not** to reproduce.

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

Three consequences worth stating, because they are the point:

- Unread defs cost nothing and lock nothing.
- A def spanning a touched service and an unavailable one no longer aborts a
  transaction that never reads it. Note *how*: by not doing the work, not by
  weakening serializability. Do not reintroduce `dep_cache` fallback to achieve
  this.
- Pull evaluates in dependency order by construction, so glitch-freedom stops
  depending on the listener graph and a `changed` flag.

## Specification

### 1. Hook the read path

`Manager::lookup` currently resolves a local member by returning
`service.vars[name]` under a read lock. Defs live in `vars` too, so it cannot
currently tell them apart.

Inside a transaction, when the member is a def (`service.defs.contains_key`):

1. **Take the read lock on the def member itself**, exactly as the current code
   does before returning a stored value — `(service_net_id, name)` recorded in
   `txn.locked`. Do not drop this step on the grounds that §2 locks the
   dependencies: an `update` transaction write-locks **defs as well as vars**
   (`runtime/update.rs:141-147` puts both `Decl::VarDecl` and `Decl::DefDecl`
   into the lock group's write set), so without it a concurrent update can
   replace the def's *expression* while this transaction is evaluating and
   memoising it. Locking dependencies protects the inputs; this protects the
   definition.
2. Serve from `txn.read_cache` if present (see §3a and §3b on what
   invalidates it).
3. Otherwise recompute it (§2), insert the result into `txn.read_cache`, and
   return it.

Outside a transaction, behaviour is unchanged: return the stored value.

`remote_read_participant` already routes through `lookup`, so a participant
serving a transactional read of one of its defs gets this for free. Confirm with
`test_participant_transactional_read_sees_buffered_def`; do not add a second
path.

### 2. The recompute

Reuse the body of `recompute_def_in_txn` from the reference branch. It is most
of the work and each part of it is load-bearing:

- Evaluate with an **empty environment**, so every same-service dependency goes
  through `lookup` and takes a read lock. An earlier version seeded `env` from
  the service's members; `Expr::Variable` then resolves from `env` and never
  reaches `lookup`, which is the only thing that takes a lock. The members a
  derived value rests on are part of the transactional view it commits under, so
  under 2PL they must be held.
- **Install no `reactive_cache` at all** — set it to `None` for the duration,
  saving and restoring the outer value.

  This is where the reference branch's shape must not be copied. `MemberAccess`
  consults `reactive_cache` *before* `lookup`
  (`interpreter/evaluator.rs:301-309`), so anything left in that map is served
  without a read lock and without reaching the owner. The reference branch
  installs the def's `dep_cache` and then computes a `live` set to delete the
  entries it must not serve; under fully transactional semantics every
  cross-service entry is one it must not serve, so the whole mechanism reduces
  to installing nothing. Copying the install without the deletion would quietly
  serve stale pushed values and violate §3 and
  `test_remote_dependency_is_never_served_from_dep_cache`.

  The save/restore still matters, for the reason task 02 gives: evaluation
  awaits remote reads, and an inbound `Update` can re-enter the
  non-transactional `recompute_def` while this one is suspended. Leaving an
  *outer* recompute's cache installed would let it satisfy this def's member
  accesses.

  Memoisation is not lost: `lookup` serves from `txn.read_cache` (§3), which is
  the transactional cache and does carry locks.
- A recompute failure **aborts**. It must not be swallowed: that would commit a
  state whose derived member does not follow from it, and would drop a
  `WaitDieAbort` or `WaitOn` from a live re-read, defeating task 06's retry loop.

Inside a transaction, then, a cross-service dependency is **always** read from
its owner under the shared transaction id. `remote_lookup` pre-registers the
owner in `txn.participants`, so `Commit`/`Abort` releases the read lock it takes.

### 3. Memoisation and invalidation

Without memoisation, every read of a def costs one round trip per remote
dependency. Read locks make a remote read stable against *other* transactions,
so the cache in §1 is sound — with one exception, which is why `remote_lookup`
on the reference branch deliberately never caches: **this transaction's own
composed action** can write, on the owner, a member this transaction already
read.

There is a second, entirely local version of the same problem, and it is the
common case: a transaction reads `def y = x + 1`, then writes `x`, then reads
`y` again. `assign` buffers the write and updates `x`'s own cache entry, so the
memoised `y` from the first read is served unchanged and the transaction sees a
`y` that does not follow from its own `x`. Read-your-own-writes, broken by the
very cache that makes pull affordable.

#### 3a. Invalidate on local write

`assign`, inside a transaction, must **remove from `txn.read_cache` every def
transitively downstream of the member it wrote**, after buffering the write.

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
  `Transaction` (`runtime/txn.rs:248-265`) does not have it; add it and
  initialise it empty in `Transaction::new`. Everything below reads or writes
  it.
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

   Then find the *directly* affected entries: everything in `txn.read_cache`
   owned by a touched service, plus every memoised def whose `graphs.cross_deps`
   name one. Then **run §3a's listener walk from each of them**, so anything
   derived from a directly affected def is dropped too.

   The second step is not optional. `graphs.cross_deps` records only a def's
   *own* cross-service dependencies, so with `def y = remote.x` and
   `def z = y + 1`, `z` has no cross-service dep at all. Stopping at the direct
   set drops `y` and leaves a stale `z` for the rest of the transaction.

   This is the same primitive as §3a with a different seed set: §3a seeds from
   a written member, §3b seeds from the directly affected defs. Implement
   invalidation once and call it from both.

**Never evict an entry that is also in `txn.written`.** Those are this
transaction's own buffered writes, mirrored into `read_cache` by `assign`; they
are authoritative and nothing a participant reports can invalidate them.
Dropping one sends the next read to the pre-commit `service.vars` value and
loses read-your-own-writes. The overlap needs a service to be both written here
and named in a report — which takes a composition cycle (A composes onto B,
which composes back onto A under the same id) — but the guard is one condition
and the failure is silent.

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
originator's live one is a local in `execute_action_with_txn`
(`manager/mod.rs:1981`) and never enters `pending_txns`, so the read-back sees
pre-transaction state and returns the same stale value. Do not paper over it —
track the originator's in-flight ids (`Manager::active_txns: HashSet<TxnId>`)
and have `remote_read_participant` return an `EvalError` naming the transaction
when a read arrives for one, so this fails loudly instead of silently reading
around the buffered writes. Making it work means the originator's transaction
becoming addressable while it runs, which is #28's event-loop work. Record the
boundary in `meerkat/tests/README.md` next to task 11's scenarios: a def on the
originator over a remote var is in contract; the inverse is not yet.

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
`locked`, `written`), and this belongs with it.

This is a new failure mode, not a ported one; give it its own test.

### 5. Do not reintroduce

- `propagate_in_txn`
- `refresh_remote_cross_deps_in_txn`
- the `propagate_in_txn` call in `assign` — `assign` buffers the write into
  `txn.written` and `txn.read_cache`, then runs §3a's invalidation walk, and
  nothing more. No recompute, no lock, no network read happens in `assign`
- `txn.remote_writes` **as a recompute trigger**. It survives, populated and
  consumed as described in §3b, but only to drive invalidation and transitive
  reporting — never to decide what to recompute.

The non-transactional path is unchanged: defs remain stored values, and
commit-time `propagate` still recomputes and still notifies remote listeners.
Push outside a transaction, pull inside, is a deliberate asymmetry — say so in
the code, because the reference branch's parallel `propagate`/`propagate_in_txn`
naming will otherwise invite someone to restore the symmetry.

## Tests

1. **Un-ignore everything task 11 ignored.** That suite is the acceptance
   criterion. `test_remote_dependency_is_never_served_from_dep_cache` and
   `test_recompute_in_txn_read_locks_same_service_dependencies` are the two that
   most directly constrain this design.
2. **The test task 10 could not land stays unlanded.** §3b is pure cache
   removal and cannot park, so nothing here reintroduces a park *after* a
   successful dispatch within the same `do`. Do not manufacture one to land
   `test_a_park_inside_the_do_statement_does_not_re_send_the_child_action`; say
   in the PR that it is still uncovered.
3. **New — read, write, read again.** With `def y = x + 1`: read `y`, write
   `x`, read `y` again in one transaction; the second read reflects the write.
   This is the test for §3a, and it is the one that fails if memoisation is
   added without local invalidation. Add the transitive form too (`z` over `y`
   over `x`), because a non-transitive walk passes the two-level case.
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
   still be the buffered value. Covers the `txn.written` guard in §3b.
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
10. `meerkat/tests/s1.mkt` passes.

## Notes

- Expect this PR to be large. It is the one place where that is the right
  answer: the mechanism has to change in one step, and everything it leans on
  has already landed.
- Performance to state in the PR description: first read of a def with N remote
  dependencies costs N round trips; subsequent reads in the same transaction
  cost nothing until a composed action touches one of those services. That is
  the price of fully transactional semantics and it was accepted deliberately.
