# Sequential code updates

Status: proposed design for pre-implementation review, revised 2026-11-25.
This describes intended behavior, not a claim that the runtime implements it.

Related work: [#265](https://github.com/meerkat-lang/meerkat/issues/265),
[PR #266](https://github.com/meerkat-lang/meerkat/pull/266),
[#267](https://github.com/meerkat-lang/meerkat/issues/267),
[#180](https://github.com/meerkat-lang/meerkat/issues/180),
[#293](https://github.com/meerkat-lang/meerkat/issues/293),
[#86](https://github.com/meerkat-lang/meerkat/issues/86),
[#295](https://github.com/meerkat-lang/meerkat/issues/295),
[#298](https://github.com/meerkat-lang/meerkat/issues/298),
[#297](https://github.com/meerkat-lang/meerkat/issues/297), and
[#213](https://github.com/meerkat-lang/meerkat/issues/213).
The broader [code-update design](code-updates.md) covers syntax, migration,
and coordination. The draft formal semantics,
[Semantics for Meerkat (draft)](../meerkat-semantics.pdf), gives the static
effect of a code update (§2.3, §3.3.3) and sequential composition (§3.3.4),
and models code installation with versioned names (§5–6). This document
specifies how the implementation executes an update and what it does to
stored values. Where it differs from §5–6, and what is still open, is under
"Relation to the formal semantics". The order of work is in
the [implementation plan](../plan/sequential-code-updates-plan.md).

## Summary

A code update is a sequence of steps. Each declaration in it is one step,
executed in source order against the service's existing state and the steps
before it. A var step evaluates its initializer and stores the value. A def
step installs new code. The update commits all its steps together, or none.

Service initialization shares the availability principle, starting with no
initialized local members: an eager read must refer to a member already
available. This work retains today's eager evaluation of service declarations
and existing cross-service startup rules. Def evaluation during an update,
described below, is a separate choice. Making service initialization lazy too
would require a separate compatibility review.

The order in which the resulting declarations are stored has no meaning. The
implementation keeps a replaced declaration in its place and appends new ones.
It does not reorder declarations to satisfy dependencies.

This replaces the dependency-based reordering proposed in #265 and #266.

## Decisions for review

The design rests on these choices. Each is argued in the section named.

1. **Steps, not reordering.** An update executes its declarations in source
   order against the existing state, and the stored order of declarations has
   no meaning ("Why not reorder").
2. **Defs are computed when read.** A def step installs code. Reads during the
   update compute the def from the current view, and each def the update
   declares is evaluated against the final view before commit ("A def step").
3. **One declaration per member per transaction.** An update, or all the
   updates of an `atomic` block, declares each member at most once. A later
   transaction can replace it again ("One declaration per member").
4. **Temporarily incompatible code is allowed** if the update repairs it
   before commit and nothing runs it in between. Stored function values are
   checked through an effect system: a var's type bounds the reads and writes
   of every function it may hold ("Temporarily incompatible code").
5. **Types and kinds may change,** validated as interface changes. A var may
   become a def once writes to defs are rejected (#293) ("Kind changes").
6. **Updates can be checked without running anything.** Static success
   promises neither successful evaluation nor runtime support that has not
   landed ("Checking without execution").
7. **Closure capture is lexical** (#86), so closure creation does not depend
   on which members exist ("Reuse").
8. **The distributed contract is fixed here; the protocol is #180's**
   ("Remote participants").
9. **A table is a var that holds a list of records** (#298), so table steps,
   re-declarations and schema changes follow the var rules ("Tables").

## Why not reorder

#266 reordered the merged declarations so that each followed the members it
reads. The discussion on #265 also proposed initializing services in
topological order. Both treat the final declaration list as a program to
initialize, and neither can express migration:

- `update s { var before = x; var x = 2; }` needs `before` to read the old
  `x`. Reordering has to move the new `x` first.
- `update s { var x = x + 1; }` reads the old `x`. As a final declaration it
  reads itself, which a dependency order rejects as a cycle.
- A kind change such as `update s { var x0 = x; def x = x0 + y; }` looks like
  a cycle between `x0` and `x` in the final declarations. Main rejects it
  today for this reason.

Sequential steps express all three directly.

## Steps

### The view

Each step sees a *view* of every service the transaction touches:

- **Bindings:** each member's kind (var or def), type, and code (a def's
  expression).
- **Values:** each var's value.

A def has no separate value in the view: reading a def evaluates its current
code against the current view.

The view starts as the state before the transaction, protected by the
transaction's locks, and each step changes it. Every read in the transaction
uses the current view: unqualified and qualified reads, reads inside called
functions, and reads answered by another node. Other transactions never see
it.

### A var step: `var x = e`

1. Check `e` against the current bindings. Each member `e` reads eagerly,
   directly or through functions and defs it reads, must be available in the
   view and has the type its current binding gives it.
2. Evaluate `e` in the current view.
3. Bind `x` as a var with `e`'s type, and store the value.

If `x` already exists, `e` reads its old value: `var x = x + 1` increments it,
and creates no dependency of `x` on itself. If `x` is new, `e` cannot read it.

### A def step: `def d = e`

1. Check `e` against the current bindings, as for a var step. `e` must not
   eagerly read `d`, directly or through other defs. That is a cycle,
   whatever `d` was before.
2. Bind `d` as a def with `e`'s type, and install `e` as its code.

A def step does not evaluate `d`. A later step that reads `d` evaluates its
current code against the view at that read, as any read of a def does in a
transaction.

This is a semantic choice. Because every read of `d` evaluates it against the
view at that read, also evaluating `d` at its own step would not change any
value a later read sees, when both succeed. Closure capture is lexical (#86),
so the members that exist when a closure is created do not determine its
captures. The two choices differ in failures and cost: evaluating at the step
does additional work, can acquire locks and add reads to the transaction's
history, and can fail on intermediate inputs. With `x == 0`, the steps
`def r = 10 / x; var x = 2;` would abort if `r` were evaluated at its step.
Under this design they succeed, and `r == 5` after commit.

Before commit, evaluate every def this transaction introduces or replaces
against the final view. The update aborts if a required evaluation fails,
including failure to obtain
its inputs. That changes current behavior: today a new def that lacks an
input waits after commit until propagation delivers it. Under this design, the
transaction reads the def's inputs itself, including remote ones, through the
same view. Where an input depends on this update's changes on another node,
that read needs remote participation (see "Remote participants"). This rule
catches a failure that only the final inputs cause:
`def r = 10 / x;` followed by `var x = 0;`. A def that
would fail only at an intermediate step, where nothing reads it, does not fail
the update, because that value is never observed. An unchanged def that
nothing reads during the update catches up after commit through ordinary
propagation, as after any transaction.

### One declaration per member

A transaction declares each member at most once, as a service declaration
does. The transaction is one update, or all the updates of an `atomic` block,
so two updates in one `atomic` block cannot both declare `s.x`. They can
declare different members of the same service. To replace a member twice, use
two transactions. That exposes an intermediate committed state and loses the
atomicity of a single transaction; it is a deliberate restriction, not an
equivalent way to express repeated replacements atomically. The draft
semantics makes the same restriction: a patch installs one version of each
name (§5).

### Atomic blocks

`atomic { update s1 { … } update s2 { … } }` is one transaction whose steps
are those of its updates, in source order. The order holds across services;
grouping steps by service must not change it. An atomic block contains only
updates: the grammar has no nested atomic blocks.

#297 proposes allowing other statements in an atomic block, such as `let`,
assignments, `insert`, and new services. Those fit the same model: each is a
step, checked and executed against the view at its position, so an assignment
or `insert` is a write step and a `let` binds a value for later steps. Their
reads and writes join the transaction's footprint like an initializer's.

### Examples

Starting with `service s { var x = 1; def d = x * 2; }`:

```meerkat
update s {
    var before = x;
    var x = 2;
    var after = x;
}
```

`before == 1`, `x == 2` and `after == 2`, wherever `x` is stored. If `x` is
instead replaced by a string, `before` keeps the old integer type and value,
and reads after the replacement see the string. That version of the example
must also replace `d = x * 2` with code valid for a string `x`, such as
`def d = x`, before commit.

```meerkat
update s {
    var added = 2;
    var result = added;
}
```

This is valid. Reversed, it is invalid if `added` did not already exist; if
it did, `result` reads its previous value.

```meerkat
update s {
    var before = d;
    var x = 2;
    var after = d;
}
```

`before == 2` and `after == 4`: each read evaluates `d` against the view at
that step. A def the update adds or replaces is read the same way, from its
new code.

The #265 case is valid, although the stored `is_warm` precedes the appended
`is_celsius`:

```meerkat
service weather_station {
    var temperature = 72;
    pub def is_warm = temperature > 65;
}

update weather_station {
    var temperature = 22;
    var is_celsius = true;
    pub def is_warm = if is_celsius then temperature > 18 else temperature > 65;
}
```

## Kind changes

A step may change a member's kind subject to interface validation: a var step
leaves the member a var with the step's value, and a def step leaves it a def
with the step's code. Migration reads the old member before replacing it:

```meerkat
service s { var x = 1; var y = 5; }

// var to def: keep the old value in a new var, and derive x from it
update s { var x0 = x; def x = x0 + y; }    // x == 6

service t { var y = 5; def d = y * 2; }

// def to var: freeze the current value
update t { var d = d; }                     // d == 10, and no longer follows y
```

A member's kind is part of its interface: a var can be read and written, a def
can only be read.

- **def to var** adds writability; readers remain type-compatible if its value
  type is preserved. The member stops reacting to its old inputs, and
  deliveries from subscriptions made for the old def are dropped (#264's tests).
- **var to def** removes writability, like an incompatible type change.
  Retained code that writes the member fails final validation. That includes
  actions in defs, action values stored in vars, and clients on other nodes.
  This requires that writing a def is rejected in the first place, which it
  is not today (#293).

A kind change also changes the member's place in the reactive graph. A var is
a source, and its Sources are itself. A def's Sources come from its inputs.
Defs downstream of the member, on this node or others, see different Sources
after the update. Commit installs the new graph before anything propagates
over it, and propagation uses the new Sources. A value already in flight from
the member's old version must not be accepted by a consumer that now expects
the new one. How to guarantee that is an open requirement, shared with type
changes ("Relation to the formal semantics").

## Tables

A table is a var that holds a list of records (#298). Its type, the table
type, is a list of records with the declared schema, which is how the type
checker already represents it. `table t { n: int, };` declares a var of that
type that starts empty, and `table t { … } = e;` gives it an initializer like
any var's. `insert e into t` is a write that appends a row, `map`, `fold` and
`select` are reads, and a table has Sources like any var.

So a table step in an update is a var step, with no rules of its own:

- **Re-declaring a table** replaces it like any var, and its schema may
  change. Without an initializer the new table is empty; to keep or migrate
  rows, give an initializer that reads the old table, for example
  `table t { n: int, done: bool, } = map(r in t where true) {n: r.n, done: false};`.
  With #297, rows can also be migrated by statements in the same atomic
  block: bind the old rows with a `let` before the re-declaration, then
  `insert` them after it.
- **A schema change is a type change.** Retained code that uses the old
  columns (`fold`, `select`, `map` bodies, field access, and `insert` in
  retained actions, including action values stored in vars) is checked
  against the new schema, and the open requirement on values from old
  versions applies ("Relation to the formal semantics").
- **Kind changes need no table case.** Replacing a table with a var of a
  different type is a type change, and with the same table type it is no
  change at all. Changing between a table and a def is the var-and-def case:
  a table that becomes a def loses its writers, including `insert`.

#298 makes the language match this: today, name resolution treats a table as
a separate kind of member, so `insert`, `map`, `fold` and `select` accept
only a declared table, and a table declaration has no initializer. Table steps
in updates depend on that change.

## Static validation

### Checking without execution

An update can be statically checked against a supplied service environment
without executing initializers or modifying running services. Checking
validates names, types, member kinds, availability, and reactive dependencies.
Success establishes static validity against that environment; it does not
establish successful evaluation or commit. Execution uses the same static
checker before evaluating the update.

The checker maintains a static view containing bindings, availability, and
the code and conservative effect information needed to analyze possible
function values. It checks each declaration, then advances the view without
computing its value. "Available after this step" means available if execution
reaches the next step successfully. A def is available once its code is
installed; reading it evaluates that code. Def and function bodies
are analyzed, not executed, including at the final-state check.

For example:

```meerkat
update s {
    var x = 1 / 0;
    var y = x + true;
}
```

Checking reports the type error in `y` without evaluating `x`. With only the
first declaration, checking succeeds and execution fails with division by
zero. Final pre-commit def evaluation is also execution, not static checking.
Both branches of an expression must be checked as required by ordinary
typing and effects rules; the checker must not execute a condition to select
which code to validate.

Checking from source uses inferred interfaces and conservative code/effect
summaries. Checking against running services uses a consistent description of
their installed state: each member's kind and type, and each def's code. A
var's type includes the latent effect that bounds every function or action
value the var may hold ("Temporarily incompatible code"), so the description
depends on installed code, not on the values stored: writing a new function
value never changes it. Each node keeps the description current whenever it
installs code, so checking never needs to run code to learn what a member
holds. Execution must revalidate under transaction protection if the
environment
could have changed since an earlier check. A checked update is not an
unconditional authorization to commit against a different environment.

The first command-line interface is the existing `--check` mode for files
containing declarations and updates. A separate update can later be checked
against a supplied base program or a running-service description using the
same checker.

### Each step

Check each step against the bindings at that step:

- **Availability.** Every member the step's code reads eagerly, directly or
  through calls and derived reads, is available in the current view. Existing
  members are available from the start. A new member becomes available after
  its own step; checking a var initializer uses the preceding view, so it can
  read an old binding of itself but not its new value.
- **Types.** Existing members have their current types, and a member replaced
  by an earlier step has its new type.
- **Delayed code.** Creating a function or action does not require the
  members its body reads to have values; delayed code keeps the language's
  forward references. Calling it during the update does require them, with
  types compatible at that step.

A def's eager dependencies are checked at its own step, although it is
evaluated later. Reading it at a later step must also check the code that
read can activate against the current bindings. Its earlier check is not a
permanent guarantee: replacing a function it calls can introduce a dependency
that was absent before. For example:

```meerkat
service s {
    var f = fn () => 1;
    def d = f();
}

update s {
    var f = fn () => later;
    var result = d;
    var later = 2;
}
```

Creating `f` permits the delayed reference, but reading `d` calls the new
`f` before `later` is available. Reject that read even though `d` existed
before the update and the final graph is valid. Reuse the effects analysis
to check activated code; do not assume its current handling of cached def
reads establishes this property for compute-on-read.

### Final state

After the last step, validate the state to be installed:

- Every installed def, whether retained, new or replaced, and every function
  and action value stored in a var, is well typed against the final bindings
  and does not write a def. A def declared before a later step changed a
  member's type is checked again here.
- Service interfaces stay compatible with their clients, including clients on
  other nodes, or the update changes those clients too.
- Reactive graphs are complete and acyclic, across services, within the
  analysis limits.

Retained vars keep the types of their stored values. Their initializers are
history: they are not re-run or re-inferred against the final bindings. After
`var x = 1; var saved = x + 1;`, replacing `x` with a string leaves `saved`
an integer. Final validation applies no declaration-position initialization
check: availability follows from the checked step sequence. Establishing
actual values, including the required final def evaluations, is runtime work.

### Temporarily incompatible code

An atomic update may leave retained code incompatible with the current
bindings for some steps, provided it repairs that code before commit and does
not run it in between. With `var x = 1; def d = x + 1;`, an update can replace
`x` with a string and then replace `d`. Reading `d` between those two steps is
an error. Otherwise the update is valid.

For defs, a step that reads `d` checks `d`'s current code against that step's
bindings, including the code activated through its dependencies. Function
values stored in vars need more, because in the implementation a value's type
does not show which members its body reads or writes. The approach is an
effect system: as in the draft semantics, a function's or action's type
records the members its code reads and writes, with their types, as a latent
effect (§2.1). A var that holds functions has such a type, so its latent
effect bounds every function the var may ever hold. Every assignment to the
var, whether from existing code or from new code such as a REPL line, must
fit that type (as in the draft's rule A-Write). A value transaction can then
change which function a var holds, but not what that function may read or
write, so it cannot invalidate an update that was checked against the var's
type. A var whose effect no longer fits the new bindings is ill formed unless
the update repairs it, and calling its function at a step is checked against
that step's bindings like any other code. How to extend the effect system to
do this, including effects inside returned functions and actions, inferring
the bound for a var without an annotation, and the intermediate bindings of
an update, is a semantic design question to solve before type and kind
changes are implemented.

### Top-level statements

Check actions, tests and watches against the bindings at their position in
the file, not the final state: an action written before an update cannot read
a member that update adds.
Their runtime ordering must agree wherever those statements are executed.
This rule does not add execution of statement forms currently skipped by a
CLI mode. A watch that remains active is also a retained client when later
updates are validated.

## Atomicity and failure

No staged value, binding, type or graph becomes visible outside the
transaction before commit. Commit publishes var initializer results and
required final def results together with their bindings. No other transaction
sees a new binding without its corresponding value. Until remote participation
is implemented, one exception remains: a new def whose inputs depend on this
update's changes on another node can be evaluated only after the local commit,
so it gets its first value after commit, as on main (see "Remote
participants"). No initializer may read such a deferred def, directly or
transitively. Failure on this interim first-value path follows current
post-commit error handling and does not undo the committed update. A failed
check or evaluation before commit, including a required final def evaluation,
aborts the update, discards its staged state, and releases its local and
participant locks.

Distributed updates must establish that every participant can commit before
publishing the update. The protocol and its recovery guarantees are #180's;
this document does not choose them.

## Architecture

### Steps and installed state

There are two representations:

- **The ordered steps:** every declaration of the update in source order,
  with its source location. Static checking and runtime execution consume the
  same sequence.
- **The installed state:** member bindings, stored values with their types,
  and dependency graphs. The final declaration list comes from merging the
  update into the service's declarations. It is not the execution order.

A stored var's type is the type established when its value was produced, not
one inferred again from its final declaration.

### Reuse

The checks above use the existing analyses with a different starting state.
They need no second expression checker:

- Name resolution, expression inference, and effects analysis are shared by
  check-only and execution paths, over a static view that evolves step by
  step. Installed vars enter it with their established types. The checker
  does not require an evaluator or an executing `Manager`.
- The effects analysis's initialization-order check becomes an availability
  check. Each member records when it becomes available: its declaration step
  during service initialization, before the first step for existing members
  in an update, and after its step for a new one. Derived reads require
  analysis in the current bindings, not just an availability lookup for the
  named def.
- Closure capture is lexical (#86): a closure captures the locals free in its
  body, whichever members exist when it is created. Unshadowed service-member
  references remain live lookups, including in closures created by startup
  var initializers. The evaluator must distinguish those members from lexical
  locals even where initialization currently puts both in its environment.
  Capture analysis therefore needs no availability information, and the
  effects analysis's separate pass for locals that shadow members goes away.
- A var step's value goes in the transaction's ordinary write buffer, the one
  assignment uses. A def read during the update uses compute-on-read
  ([task 12](../../tasks/12-compute-on-read.md)). It may cache results within
  the transaction, but a cached result must equal what evaluating the def's
  current code against the current view would give. A def step, or a write to
  one of the def's inputs, invalidates it, and the final evaluation before
  commit may reuse a result only if it is still valid for the final view.

### Staged state belongs to the transaction

All staged bindings, types, code and values belong to the update's
transaction or its execution context. A `Manager` may own contexts indexed
by transaction ID; it must not expose one transaction's staged view as ambient
state used by other executions. Once execution is suspendable (#213), other
actions run while an update waits for a lock or a reply, and they must not
see its staged state. #213 moves
`reactive_cache` into each execution's context for the same reason.

Update initializers are expressions and cannot run actions, but their reads
can acquire remote locks and create participant state. Restarting before the
commit decision requires discarding the entire attempt's staged state,
aborting and cleaning up its participants, and following the transaction's
lock-release and retry rules. If remote steps have staged writes or code,
those buffers must be discarded too. Purity alone does not make replay over
retained participant state safe. Under suspendable execution, a waiting
update can instead retain its transaction and locks and resume the same
attempt. A commit decision must be completed under the commit protocol, not
treated as a restartable evaluation failure.

### Locks

Lock requirements are the union over all steps, including steps that revisit
a service. Definition reads are protected as well as value reads, under the
existing transactional read-lock rules.

### Remote participants

The contract is the same across nodes. A read answered by another node during
the update sees the update's staged state for that node's services. A later
step observes every earlier step, wherever it ran.

How a remote owner gets that state is a protocol choice for #180. Owners could
evaluate their own steps as participants, or the coordinator could evaluate
every step and send staged writes and code to each owner before any read that
can observe them. Either way, forwarding final declarations after a local
commit cannot meet the contract. Participant failure during commit (#191)
must be covered by that protocol. Current updates of remote services commit
locally first and forward afterwards; this is an implementation limitation,
not the distributed semantics specified here.

### Startup, imports, and replay

A file containing services and updates initializes the original services and
then executes the updates once each, in order. Validating the final state does
not license initializing services from their final declarations.

A locally instantiated import executes its own declarations and updates. A
service fetched from a running peer supplies its current state and code. The
importer validates that state as installed state, not as a program to
initialize.

Replay needs more than the final declaration list: either a snapshot of state
with current code, or the original declarations and ordered update history.
This design adds no persistence.

## Relation to the formal semantics

The [draft semantics](../meerkat-semantics.pdf) models code installation in
§5–6. This design agrees with it that installing a def does not evaluate it,
that only vars can be written, that a member's kind can change, that a
retained def made ill typed by an update is rejected, that updates compose
sequentially, and that transactions read a committed snapshot under read
locks on both source and definition dependencies. The points below are
differences or open questions, agreed with the draft's author where noted.

- **Expression initializers, and so one event (agreed).** For simplicity,
  the draft gives a var in a code update, and a var's initial code, a
  constant right-hand side (T-Var). The implementation needs expressions,
  because migration reads old values: `var x = x + 1`, `var before = x`, and
  `var x0 = x; def x = x0 + y;`. The draft's constant stands for the value
  such an expression produces.

  The draft abstracts away initializer evaluation by supplying its resulting
  value. The implementation evaluates initializer expressions against the
  update's protected view, including earlier steps, and publishes code and
  values together at one commit ("Atomicity and failure"). Evaluation and
  installation may be separate internal phases, but no intermediate state is
  exposed. The draft's separate installation and initialization events
  (Definition 10, Theorem 8) motivate this correspondence; relating their
  histories to the combined event, including initializer reads and evaluation
  failure, remains formalization work. This follow-up does not gate E2.
- **Initializer read footprint (formalization follow-up).** The draft gives a
  code patch an empty read/write footprint (T-Update, Theorem 8), which its
  commutation argument relies on. With expression initializers the combined
  event reads state. Starting with `x == 1`, the updates
  `update s { var y = x; }` and `update s { var x = 2; }` have disjoint
  destinations, but their order determines whether `y` becomes 1 or 2. The
  implementation treats initializer reads, including reads through functions
  and defs, like a value transaction's: they take locks, enter conflict
  checks, and become part of the event's causal history. Extending the
  draft's patches with that footprint is follow-up work for the
  formalization; it does not change the implementation.
- **One declaration per member per transaction** matches the draft's one
  version per name per patch ("One declaration per member").
- **Open requirement: values from old versions.** The draft tags each
  propagated value with the version that produced it. Its use must agree
  with the versions selected by the consumer's history and the types expected
  by its declaration (§5.1, Definition 12, §6). Historical values can remain
  valid under compatible historical selections; installation does not mean
  every consumer must immediately discard every old value. The implementation
  has no such tags; its current-graph delivery filter (#264) does not itself
  establish this version agreement. A consumer expecting a new version must
  not accept an incompatible old value, whether delayed in transit or retained
  in an input cache. Type equality alone does not establish code provenance,
  so this concerns same-type replacements as well as type and kind changes.
  Main lacks explicit code-version tags; #295 investigates whether delayed
  or retained values can consequently be accepted incorrectly. A failure has
  not yet been reproduced. Same-type replacements, including type-preserving
  def-to-var changes, keep main's existing behavior; this design makes no
  claim that they are safe, and #295's question covers them too. If #295
  confirms a failure, its fix covers them, or they are restricted then. Until
  the question is settled, the design enables no new affected cases: other
  type and kind changes whose old values can reach a consumer (locally or on
  another node) stay unsupported.
  Version tags (the draft's "generation fencing") are one possible mechanism;
  vector clocks alone do not encode code versions (§7).
- **Stored functions.** The draft's latent effects in function types (§2.1)
  are the basis for checking stored function values across type and kind
  changes ("Temporarily incompatible code"): a var's type bounds the effects
  of every function it may hold, and every assignment must fit it, as in
  A-Write. Extending the effect system for this is a semantic design
  question. Summaries must preserve
  expected member types and read/write capabilities, including latent effects
  inside returned functions and actions; dependency names alone are
  insufficient for WF-δ. The higher-order compatibility premise in §6's
  progress theorem remains an obligation to establish.
- **Open question: constant and function-valued defs.** T-Def requires a
  nonempty immediate read effect. This excludes `def d = 5` and defs whose
  expressions only construct closures, which the implementation permits.
  Confirm whether this restriction is intended or should be removed from
  the formalization.
- **Open question: intermediate checking environments.** The expression
  rules assume well-formed environments (§2.2), while this design permits
  retained code to be temporarily incompatible during an update, provided
  it is repaired before commit and not run in between. Clarify how patch
  checking can operate under those intermediate bindings. Final-state
  compatibility alone does not justify those intermediate judgments.
- **Evaluation failure.** The draft assumes evaluation always succeeds. This
  design's abort when a declared def fails before commit goes beyond the
  model rather than conflicting with it.
- **Tables.** The draft models only scalar values. Because a table here is a
  var holding a list of records ("Tables"), it adds a value type rather than
  a new kind of member, and needs no change to the draft's var and def
  kinds.

## Alternatives considered

| Alternative | What it simplifies | Decision |
| --- | --- | --- |
| Reorder declarations, or initialize in topological order | Keeps one initialization check over the final declarations | Rejected: cannot express migration that reads old values (see "Why not reorder") |
| Require every intermediate state to be well typed | No reasoning about code that is incompatible but not run | Rejected: it rejects coordinated migrations, notably across services. Stored functions are handled with an effect system instead |
| Evaluate each def at its own step | A literal reading of "every declaration is a step" | Not adopted: it adds failures on values nothing observes, and still needs the final evaluation |
| Forbid type or kind changes | No old and new interfaces within one update | Rejected: both are important kinds of evolution, and they share one validation |
| Forbid declaring a member twice in one transaction | No repeated steps, no overwritten initializers or defs to account for | Adopted: it matches the draft semantics; splitting replacements into two transactions loses atomicity |
| Let every initializer read the state before the update | Less staging | Rejected: an initializer could not use a member added earlier in the update |

## Boundaries and review questions

This design covers adding and replacing members, including changes of type and
kind, and tables as vars that hold a list of records. It does not add
deletion, renaming, service classes, or new update syntax; the table
initializer comes with #298, and statements other than updates in atomic
blocks with #297. Those should follow the same step model when designed.

Review should establish that:

- checking uses static information alone, reports typing errors without
  running initializers, and leaves runtime state unchanged;
- the availability and type checks accept the migrations above and reject
  reads of unavailable or temporarily incompatible members;
- var types bound the effects of every function value the var may hold, so
  that types and captures stay sound across replacements and no value
  transaction can invalidate a checked update;
- a kind change gives downstream defs, local and remote, the right Sources;
- initializer reads participate in locking, conflict checks, and causal
  history, including indirect reads;
- consumed values agree with the code versions and types the consumer
  expects, including delayed and cached values across updates (the open
  requirement under "Relation to the formal semantics", tracked in #295);
- def evaluation, invalidation and publication preserve the clock and Sources
  contracts, including failures before commit;
- the distributed protocol preserves step order and isolation.

Implementation may land in stages. A stage that limits def reads during an
update, or that runs on one node only, records the limitation until the
verification that lifts it passes.
