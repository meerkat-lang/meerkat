# Sequential code updates

Status: proposed design for pre-implementation review, 2026-11-24.
This describes intended behavior, not a claim that the runtime implements it.

Related work: [#265](https://github.com/meerkat-lang/meerkat/issues/265),
[PR #266](https://github.com/meerkat-lang/meerkat/pull/266),
[#267](https://github.com/meerkat-lang/meerkat/issues/267), and
[#180](https://github.com/meerkat-lang/meerkat/issues/180).
The broader [code-update design](code-updates.md) covers syntax, migration,
and coordination. Implementation sequencing and verification belong in the
associated issue and implementation plans rather than this durable contract.

## Motivation and decision

A service initializes its fields from top to bottom. An eager initializer
cannot read a field that has not yet been initialized. Constructing a function
or action does not itself perform the reads in its body; calling a function
can perform those reads and is subject to the initialization check.

A code update starts with an existing service state. Its declarations execute
in source order, with each step seeing the existing state and the preceding
steps of the same transaction. Existing fields do not need to be initialized
again merely because an update changes the service's code.

The order used to store the resulting declarations has no language-level
meaning. Keep existing declarations in their positions and append new ones in
order of first introduction. This is an implementation convenience, not an
initialization schedule. Do not reorder declarations to satisfy dependencies.

This replaces the dependency-based AST reordering proposed in #265 and #266.
It retains source-order service initialization; it does not introduce
topological initialization. Reactive dependency graphs still determine
dependencies and detect cycles.

## Execution semantics

An ordinary `update` is one transaction. Within an `atomic` block, traverse
the contained updates and their declarations in source order, including nested
atomic blocks. Nesting does not create an intermediate commit. The order
extends across services; grouping steps by service must not change it.

Before a declaration executes, its view consists of:

- The members, types, code, and values available before the transaction,
  protected by the transaction's consistency and locking rules.
- All additions and replacements performed by earlier declarations in this
  transaction. An earlier replacement takes precedence over the old member.

Evaluate a declaration's initializer in that view. If it succeeds, stage its
value, type, and declaration for subsequent steps. The declaration being
evaluated does not make its new value available to its own initializer. An
existing member of the same name remains readable there; a wholly new member
does not yet have a value.

New and changed `def` declarations receive an initial evaluation at their
step, as they do during service initialization. Their expressions remain
reactive afterward. Existing, unchanged defs are not all eagerly reevaluated
at every step: recompute them when a transaction reads them and their cached
values are no longer valid for its view.

Repeated replacements are execution steps, not redundant declarations.
Execute every initializer, even if a later step replaces its result. An
earlier initializer can supply a value to another step or fail. Only the last
declaration and final value for a member survive commit. This rule does not
relax duplicate-member rejection in a service declaration; repeated update
blocks are sufficient to express successive replacements.

### Examples

Starting with `service s { var x = 1; }`:

```meerkat
update s {
    var before = x;
    var x = 2;
    var after = x;
}
```

The result is `before == 1`, `x == 2`, and `after == 2`, regardless of the
stored position of `x`. If `x` is instead replaced by a string, `before`
still has the old integer type and value. Reads after the replacement see
the string type and value.

```meerkat
update s {
    var x = x + 1;
}
```

This reads the old integer `x` and stages its successor. The initializer is
migration code; it does not create a persistent dependency of `x` on itself.

```meerkat
update s {
    var added = 2;
    var result = added;
}
```

This is valid. Reversing the declarations is invalid if `added` did not
already exist. If it did exist, the earlier read uses its previous version.

For the original #265 case, the following update is valid even though the
stored `is_warm` declaration precedes the newly appended `is_celsius`:

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

### Reactive reads and code versions

All reads within the transaction use the same view: unqualified reads,
qualified reads, reads inside called functions, and reads handled by a remote
participant. Staging a member invalidates cached derived values that depend
on it, transitively. Replacing a definition also invalidates results obtained
from its previous code.

With existing `var x = 1; def d = x * 2;`:

```meerkat
update s {
    var before = d;
    var x = 2;
    var after = d;
}
```

The result is `before == 2` and `after == 4`. The second read must not reuse
the first read's cached value. A newly introduced or replaced def is equally
available to subsequent steps. An unchanged def uses its existing expression
with the transaction's current inputs. Reading a function-valued member uses
the version stored at that step; calling it reads through the same
transactional view, with its actual captured environment.

This is the contract of #267. It applies only to this transaction and its
participants. Independent transactions never observe these staged changes.

### Atomicity and failure

No staged values, declarations, types, or graphs become externally visible
before commit. An initializer failure or a failed static check aborts the
update, discards its staged state, and releases its local and participant
locks. In particular, failure of a new def's initial evaluation must be
reported before publication, not after installing an unvalued def.

Distributed updates must establish that every participant can commit before
publishing the update. The protocol and recovery guarantees are work for
#180; this document does not select a crash-recovery protocol or promise that
an arbitrary network failure can be handled by unilateral rollback.

## Static validation

There are three distinct validation contexts.

### Service initialization

Keep the existing source-order initialization checks. This proposal does not
change the existing rules for cross-service startup dependencies. Reuse the
effects analysis for eager reads, including reads activated by function calls,
rather than introducing a second syntactic dependency scan.

### Update execution

Check each initializer against the types and versions available at its step.
Initially, existing members are available. Add a new member or replace its
binding only after checking its initializer. The check needs more than a set
of names: a field updated later still has its old type and code before that
replacement.

Delayed bodies keep the language's support for forward references. Creating
a closure does not require the members its body will later read to have
values already. Calling it during the update does require those members to
be available, with types compatible with that call. Validate delayed code
that survives the transaction against the final environment as well.

An atomic update may temporarily leave retained code incompatible with the
current types, provided it repairs that code before commit and does not
evaluate the incompatible code in the meantime. The checker must validate
the code activated by each step, not require the entire service to be a
valid final program after every step. This rule needs particular review in
the implementation of higher-order calls and derived reads.

### Final installed state

Validate the resulting service interfaces, retained executable code,
dependent clients, and reactive graphs. Preserve analysis limits and reject
reactive dependency cycles, including cycles across services. For example,
`def x = x + 1` remains an invalid final definition even when an old `x` was
available during its initial evaluation.

Retained vars keep the types of their stored values. Do not rerun or reinfer
their historical initializers against the final environment. For example,
after `var x = 1; var saved = x + 1;`, replacing `x` with a string does not
change `saved`'s integer value or type. Retained defs do need validation
against the new interface because they will execute again. Function and
action values stored in vars also retain executable code and captures whose
compatibility must be checked; treating an initializer as historical does not
exempt those stored values from analysis.

The final declaration list is not a fresh initialization program. In
particular, checking it must not reapply source-position forward-reference
constraints to already available members.

## Architecture

### Separate execution history from installed state

Maintain two representations with different responsibilities:

- An ordered sequence of update steps, preserving every initializer and its
  source location. Static update checking and runtime execution consume this
  sequence in the same order.
- The resulting service state: current member types, declarations, stored
  values, and dependency information. AST merging can retain its simple
  replace-in-place/append policy, but does not determine execution order.

Do not infer a stored var's current type or a closure's captures solely from
the position and initializer of its final AST declaration. Migration may
have used an older environment. Preserve the type and effect information
established when the value was produced, together with the runtime's existing
analysis of actual function values.

### Reuse the effects and typing infrastructure

Provide initialization, update-step, and final-state entry points over the
same name resolution, inference, and effects machinery. These are different
validation contexts, not three implementations of expression checking. The
exact Rust API is an implementation choice.

Effects analysis currently uses declaration positions both for forward-read
checks and for closure-capture analysis and memoization. An update-aware
analysis must represent the available versions and capture context explicitly;
changing only `forward_reference` is insufficient. Cached analysis of a
member or function must not survive a relevant code or environment change.

Do not run ordinary initialization checking on every patched AST prefix.
Besides unnecessary work, that would recheck historical var initializers and
reject temporary interface changes that the atomic update repairs.

### Reuse boundaries and simple defaults

Use the following boundaries to keep the implementation small. Introduce
helpers around existing code as their callers need them; a general compiler
or transaction framework is not a prerequisite.

| Responsibility | Shared implementation | Context that differs |
| --- | --- | --- |
| Enumerate update declarations | One traversal of existing `Stmt`/`Decl` nodes | Consumers collect locks, check steps, evaluate, or merge final declarations |
| Resolve a member and infer an expression | Existing name resolver, `tt::check` expression routines, and member lookup | Current bindings and the declarations available to delayed code |
| Determine eager reads and captures | Existing `Effects` visitor and function analysis | Which members exist at the current step and which code/values they hold |
| Validate reactive dependencies | Existing graph construction and cycle checks | Current executable definitions and stored function values |
| Evaluate an expression | Existing interpreter and transaction-aware lookup | Committed members plus staged values and code |
| Buffer writes and manage read history | Existing lock transaction and task 12's read machinery | Code updates can introduce members and replace definitions as well as values |

The declaration traversal can be an iterator or a small collected sequence
of references. Preserve available source information without introducing a
new intermediate language. Lock collection may aggregate that sequence, but
evaluation must retain every step. Reuse the merge helper for the final AST;
do not recover the execution sequence from its result.

Initialization and update checking share the rule "check/evaluate, then make
available." A newly initializing service begins without initialized local
members; an update begins with its existing members available. Keep
declaration visibility distinct from value availability so delayed forward
references still work. This shared rule does not require rewriting service
creation as a code-update transaction or changing cross-service startup.

One evolving view of current bindings is sufficient for sequential execution.
Before a step, lookup returns the current binding; after that step succeeds,
replace it. Final declarations remain available separately for final checks
and delayed code. An old closure retained as a value keeps its code and
captures under the existing value representation. Do not introduce arbitrary
historical-version lookup, persistent AST snapshots for every prefix, or a
new version identifier on every member merely to express these semantics.

Keep static and runtime drivers small and separate: one works with types and
effect summaries, the other with values and locks. Share traversal and the
underlying expression operations, rather than forcing both through a generic
execution engine with unrelated modes. Member-existence checks used for
capture must agree with the corresponding view, including staged additions.

For static analysis, start with a fresh analysis context for each changed
view, preserving established var types and stored-function summaries as
inputs. This trades repeated analysis for simple cache correctness. It does
not mean invoking whole-program initialization checking on each prefix.
Introduce incremental caches only when measured cost justifies them.

For runtime state, prefer one authoritative buffer for var writes in the
existing transaction. Extract the buffer-and-invalidate operation used by
assignment if needed; do not call the public assignment operation when its
existing-member checks or publication behavior are inappropriate. Structural
changes need additional staged code/type metadata, but should not need a
second competing map of var values. Keep code-update commit responsible for
installing values and structural changes together.

Begin with conservative invalidation: discard all derived memoized results
in the affected transaction view after a staged value or code change. Keep
authoritative var writes, locked base snapshots, and accumulated read history.
Use a clear distinction between those entries and disposable derived results;
do not simply clear today's combined `read_cache`. This avoids maintaining a
second incremental dependency graph solely for transaction caches. If task
12 already supplies selective invalidation, reuse its interface and allow
the update path to invalidate more broadly while definitions are changing.
Participant caches must receive corresponding invalidation information; a
local cache clear alone cannot establish the distributed contract.

Reuse the same compute-on-read operation for ordinary transactions and code
updates. New or changed defs still require an initial evaluation at their
step. Factor the common expression evaluation and result bookkeeping out of
that operation; do not stage the replacement early merely to make an ordinary
lookup evaluate it, since its initializer must still see the previous view.

### Transaction state and reads

Provide a transaction-local view of staged values and code alongside the
locked existing state. Evaluate update initializers through this view, using
the ordinary member lookup path so qualified reads and function calls behave
the same as direct reads. Closure creation must consult this view when
deciding which members exist.

Build on [compute-on-read](../../tasks/12-compute-on-read.md) for derived
values. Distinguish an authoritative staged var value from a cached def value
that can be invalidated. Invalidation must account for dependency and code
changes within the transaction, not only the committed listener graph.
Conservative invalidation of derived caches is acceptable initially if it
preserves authoritative writes and read history.

Reads used for an earlier initializer remain part of the transaction's read
history even if its cached result is invalidated. Commit must preserve the
existing clock and Sources contracts. A def cached during an early step may
be stale after later writes: do not publish it as the final derived value.
Unchanged defs that nobody reads during the update can catch up through
ordinary propagation after commit.

### Locks and remote participants

Union structural and member lock requirements over the complete ordered
sequence, including repeated updates to a service. Protect definition reads
as well as value reads and retain the existing transactional read-lock rules.

Remote owners evaluate their steps as participants in the same transaction.
An owner must have completed an earlier step before a subsequent step on
another owner can observe it. Multiple visits to one service retain their
positions in the global sequence. Participants must serve transactional reads
from their staged code and values. Sending each owner only its final
declarations after a local commit cannot implement this contract.

### Startup, imports, and replay

A file containing service declarations and startup updates must initialize
the original services and then execute the updates exactly once, in order.
Name resolution must preserve that executable sequence; validating the final
state does not authorize substituting final declarations into startup.

Locally instantiated imports execute their own initialization history. A
service fetched from an already-running peer supplies current state and code;
the importer does not replay its initializers or require its folded
declarations to be a valid source-order initialization program. Keep enough
type and dependency information to validate that current interface.

Persisting only the final declaration list is not sufficient for replay.
A future persistence mechanism needs either a state snapshot with current
code or the original initialization and ordered update history. This proposal
does not add persistence.

## Semantic simplifications considered

The choices below are for review; they do not change the contract above.
Implementation shortcuts should preserve observable values, failures, and
transaction boundaries.

| Alternative | What it simplifies | Recommendation |
| --- | --- | --- |
| Require all retained code to remain well typed after every step | Avoids checking whether temporarily incompatible code is activated before repair | Consider if prefix checking proves too complex; it restricts coordinated type migration |
| Make new and changed defs entirely lazy | Removes their mandatory evaluation at the declaration step | Keep eager initial evaluation: it matches service initialization and reports initializer failures before commit |
| Forbid type changes in updates | Avoids old/new type environments for replacements | Do not adopt: type migration is an existing goal, and new-member availability still needs update checking |
| Forbid repeated replacements within one atomic transaction | Removes some execution histories | Little implementation saving once execution uses an ordered sequence; retain successive update blocks |
| Let every initializer read the pre-update snapshot | Reduces staging requirements | Do not adopt: it prevents an initializer from using a new field introduced earlier in the same update |

The first alternative is the substantial semantic tradeoff. For example,
with `var x = 1; def d = x + 1;`, an update can first replace `x` with a
string and then replace `d` with `x`. This design permits that transaction
if it does not read the old `d` after replacing `x`. Requiring every prefix
to be well typed would reject that ordering, even though the final state is
valid. Some migrations can be reordered or use temporary fields; others
become awkward, particularly across services. Even that stricter rule would
still need separate validation of stored vars and historical initializers.

Retain the current semantics unless this restriction is explicitly accepted.
Before building a large analysis for temporarily incompatible code, use small
type-changing and higher-order examples to establish whether existing
on-demand inference and effects analysis can check the code actually
activated at each step. If they cannot, bring that result back for semantic
review instead of silently rejecting migrations or weakening safety.

## Boundaries and review questions

The design covers additions and replacements supported by code updates. It
does not add deletion, renaming, service classes, new update syntax, or table
migration support. Those features should follow the same state-transition
model when designed.

Review should establish that:

- Prefix typing and activated-code checks permit atomic type migration while
  rejecting reads of temporarily incompatible definitions.
- Stored function values and delayed bodies retain sound types and captures
  across replacements, without making final storage order significant.
- Def evaluation, invalidation, and publication preserve the clock and
  Sources contracts, including failures before commit.
- The distributed implementation preserves step order and isolation without
  claiming completion of #180 before its protocol is implemented and tested.

Implementation may land in stages. A limited #265 fix does not establish
#267's read-your-writes contract; local tests alone do not establish the
distributed contract. Track those limitations explicitly until their stated
verification evidence passes.
