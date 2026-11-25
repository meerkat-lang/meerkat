# Sequential code updates

Status: proposed design for pre-implementation review, revised 2026-11-24.
This describes intended behavior, not a claim that the runtime implements it.

Related work: [#265](https://github.com/meerkat-lang/meerkat/issues/265),
[PR #266](https://github.com/meerkat-lang/meerkat/pull/266),
[#267](https://github.com/meerkat-lang/meerkat/issues/267),
[#180](https://github.com/meerkat-lang/meerkat/issues/180),
[#293](https://github.com/meerkat-lang/meerkat/issues/293), and
[#213](https://github.com/meerkat-lang/meerkat/issues/213).
The broader [code-update design](code-updates.md) covers syntax, migration,
and coordination. The [draft semantics](../meerkat-semantics.pdf) gives the
static effect of a code update (§3.3.3) and sequential composition (§3.3.4),
and leaves the update's effect on the store unspecified; this document
describes that effect. Implementation order belongs in the issues and
implementation plans.

## Summary

A code update is a sequence of steps. Each declaration in it is one step,
executed in source order against the service's existing state and the steps
before it. A var step evaluates its initializer and stores the value. A def
step installs new code. The update commits all its steps together, or none.

A service declaration is the same thing applied to an empty service: each
declaration is a step, so a member can eagerly read only members declared
before it. Today's initialization rule is the special case of the update rule
that starts from no members.

The order in which the resulting declarations are stored has no meaning. The
implementation keeps a replaced declaration in its place and appends new ones.
It does not reorder declarations to satisfy dependencies.

This replaces the dependency-based reordering proposed in #265 and #266.

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
- **Values:** each var's value, and cached values of defs.

The view starts as the state before the transaction, protected by the
transaction's locks, and each step changes it. Every read in the transaction
uses the current view: unqualified and qualified reads, reads inside called
functions, and reads answered by another node. Other transactions never see
it.

### A var step: `var x = e`

1. Check `e` against the current bindings. Each member `e` reads eagerly,
   directly or through functions it calls, must have a value in the view, and
   has the type its current binding gives it.
2. Evaluate `e` in the current view.
3. Bind `x` as a var with `e`'s type, and store the value.

If `x` already exists, `e` reads its old value: `var x = x + 1` increments it,
and creates no dependency of `x` on itself. If `x` is new, `e` cannot read it.

### A def step: `def d = e`

1. Check `e` against the current bindings, as for a var step. `e` must not
   read `d`, directly or through other defs. That is a cycle, whatever `d` was
   before.
2. Bind `d` as a def with `e`'s type, install `e` as its code, and discard
   cached derived values that depend on `d`.

A def step does not evaluate `d`. A later step that reads `d` computes it from
the current view, as compute-on-read does in an ordinary transaction. Since
`e` cannot read `d`, installing the code first gives the same values as
evaluating `e` at the step.

Before commit, each def the update declares is evaluated against the final
view, and the update aborts if one fails. This catches a failure that only the
final inputs cause: `def r = 10 / x;` followed by `var x = 0;`. A def that
would fail only at an intermediate step, where nothing reads it, does not fail
the update, because that value is never observed. An unchanged def that
nothing reads during the update catches up after commit through ordinary
propagation, as after any transaction.

### Repeated declarations of a member

Within one update block a member may be declared once, as in a service
declaration. Successive update blocks in one `atomic` block may declare it
again. Each of those steps executes, even if a later one replaces its result:
an earlier initializer can supply a value to another step, or fail. Only the
last binding and value survive commit.

### Atomic blocks

`atomic { update s1 { … } update s2 { … } }` is one transaction whose steps
are those of its updates, in source order. The order holds across services;
grouping steps by service must not change it. An atomic block contains only
updates: the grammar has no nested atomic blocks.

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
and reads after the replacement see the string.

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

`before == 2` and `after == 4`. The second read must not reuse the first
read's cached value. A def the update adds or replaces is read the same way,
from its new code.

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

A step may change a member's kind, with no extra rules: a var step leaves the
member a var with the step's value, and a def step leaves it a def with the
step's code. Migration reads the old member before replacing it:

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

- **def to var** adds writability, so readers are unaffected. The member stops
  reacting to its old inputs, and deliveries from subscriptions made for the
  old def are dropped (#264's tests).
- **var to def** removes writability, like an incompatible type change.
  Retained code that writes the member fails final validation. That includes
  actions in defs, action values stored in vars, and clients on other nodes.
  This requires that writing a def is rejected in the first place, which it
  is not today (#293).

A kind change also changes the member's place in the reactive graph. A var is
a source, and its Sources are itself. A def's Sources come from its inputs.
Defs downstream of the member, on this node or others, see different Sources
after the update. Commit installs the new graph before anything propagates
over it, and propagation uses the new Sources.

Tables are outside this design.

## Static validation

### Each step

Check each step against the bindings at that step:

- **Availability.** Every member the step's code reads eagerly, directly or
  through calls, has a value at that step. A member that existed before the
  update has one from the start, and a new member from its own step. Starting
  from an empty service, this is today's initialization check.
- **Types.** Existing members have their current types, and a member replaced
  by an earlier step has its new type.
- **Delayed code.** Creating a function or action does not require the
  members its body reads to have values; delayed code keeps the language's
  forward references. Calling it during the update does require them, with
  types compatible at that step.

A def's availability is checked at its own step, although it is evaluated
later. A step that reads a def then needs only the def to be available, not
everything the def reads. This is also why a service declaration cannot have
a def read a member declared after it.

### Final state

After the last step, validate the state to be installed:

- Retained defs, and function and action values stored in vars, are well
  typed against the final bindings and do not write a def.
- Service interfaces stay compatible with their clients, including clients on
  other nodes, or the update changes those clients too.
- Reactive graphs are complete and acyclic, across services, within the
  analysis limits.

Retained vars keep the types of their stored values. Their initializers are
history: they are not re-run or re-inferred against the final bindings. After
`var x = 1; var saved = x + 1;`, replacing `x` with a string leaves `saved`
an integer. Final validation applies no initialization-order check, since
every member of the final state has a value.

### Temporarily incompatible code

An atomic update may leave retained code incompatible with the current
bindings for some steps, provided it repairs that code before commit and does
not run it in between. With `var x = 1; def d = x + 1;`, an update can replace
`x` with a string and then replace `d`. Reading `d` between those two steps is
an error. Otherwise the update is valid.

For defs, a step that reads `d` checks `d`'s current code against that step's
bindings, so nothing more is needed. Function values stored in vars are
harder, because a value's type does not show which members its body reads.
Before type and kind changes are implemented, small examples should establish
whether existing analysis can check stored functions. If it cannot, bring
concrete accepted and rejected examples back for review. The stricter rule,
that every intermediate state must be well typed, is under "Alternatives
considered".

### Top-level statements

Actions, tests and watches in a file run in order with the file's updates.
Each is checked against the bindings at its position, not the final state: an
action written before an update cannot read a member that update adds.

## Atomicity and failure

No staged value, binding, type or graph becomes visible outside the
transaction before commit. A failed check or evaluation, including the final
evaluation of a declared def, aborts the update, discards its staged state,
and releases its local and participant locks.

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

- Name resolution and type inference stay as they are, over one view that
  evolves step by step. Installed vars enter it with their stored types.
- The effects analysis's initialization-order check becomes an availability
  check. Each member gets the step from which it has a value: its declaration
  position for a service declaration, 0 for an existing member in an update,
  and its step for a new one.
- A var step's value goes in the transaction's ordinary write buffer, the one
  assignment uses. A def read during the update uses compute-on-read
  ([task 12](../../tasks/12-compute-on-read.md)).

### Staged state belongs to the transaction

All staged bindings, types, code and values belong to the update's
transaction or its execution context, never to shared `Manager` state. Once
execution is suspendable (#213), other actions run while an update waits for
a lock or a reply, and they must not see its staged state. #213 moves
`reactive_cache` into each execution's context for the same reason.

Update initializers are expressions and cannot run actions. An update that
must wait can therefore discard its staged state and restart from its first
step, as an originator does under #28's park-and-restart, with nothing to
replay. Once execution is suspendable, it suspends with its locks held.

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
commit cannot meet the contract. #180 should be designed with #191 (participant
failure during commit) alongside #28, and implemented after #213, so that a
participant running update steps suspends rather than replays. Until then,
updates of remote services commit locally first and forward afterwards, a
documented limitation.

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

## Alternatives considered

| Alternative | What it simplifies | Decision |
| --- | --- | --- |
| Reorder declarations, or initialize in topological order | Keeps one initialization check over the final declarations | Rejected: cannot express migration that reads old values (see "Why not reorder") |
| Require every intermediate state to be well typed | No reasoning about code that is incompatible but not run | Fallback if stored functions cannot otherwise be checked; rejects some coordinated migrations, notably across services |
| Evaluate each def at its own step | A literal reading of "every declaration is a step" | Not adopted: it adds failures on values nothing observes, and still needs the final evaluation |
| Forbid type or kind changes | No old and new interfaces within one update | Rejected: both are important kinds of evolution, and they share one validation |
| Forbid redeclaring a member within one atomic block | Fewer execution histories | Little saving once execution follows the steps; successive update blocks stay allowed |
| Let every initializer read the state before the update | Less staging | Rejected: an initializer could not use a member added earlier in the update |

## Boundaries and review questions

This design covers adding and replacing members, including changes of type and
kind. It does not add deletion, renaming, service classes, new update syntax,
or table migration. Those should follow the same step model when designed.

Review should establish that:

- the availability and type checks accept the migrations above and reject
  reads of unavailable or temporarily incompatible members;
- stored function values keep sound types and captures across replacements,
  or the stricter rule is adopted explicitly;
- a kind change gives downstream defs, local and remote, the right Sources;
- def evaluation, invalidation and publication preserve the clock and Sources
  contracts, including failures before commit;
- the distributed protocol preserves step order and isolation.

Implementation may land in stages. A stage that limits def reads during an
update, or that runs on one node only, records the limitation until the
verification that lifts it passes.
