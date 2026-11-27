# Implementation plan for sequential code updates

Revised 2026-11-25 for pre-implementation review. No implementation changes
are included.

The contract is in
[Sequential code updates](../design/sequential-code-updates.md). This
plan records the order of work, how it fits the path to suspendable execution,
implementation guidance, and the evidence for each step. Recheck the relevant
issues and branches before starting a unit.

## Order of work

| When | Units |
| --- | --- |
| Now, independently | #86 (lexical closure capture), A (startup history: #285 item 1), B (one traversal, union locks), C (#293: no writes to defs), D1 (spike: checking without execution) |
| After #86 and D1, alongside task 12 | E1 (pure update checking), then E2 (ordered execution and staged vars); F follows E1, G follows installed-state checking |
| After E1 | D2 (effect-system design and prototype: type and kind changes, stored functions, live metadata) |
| After C, D2 and E1 | H1 (static type/kind migration checks); no initializer execution required |
| Independently of the units above | T1 (#298: tables as vars that hold a list of records, with an optional initializer) |
| After T1 and E2 | T2 (table steps in updates); schema changes join H1/H2 |
| After E2 and F | K (#297: statements other than updates in `atomic` blocks); its table-migration test lands with or after T2 |
| After task 12 and E2 | I (def steps and def reads during an update: #267 on one node), then H2 (full local type/kind migration execution) |
| After local migration and the required peer/protocol support | H3 (distributed client validation and Sources evidence) |
| Designed alongside #28, built after #213 | J (remote participation: #180, and #267 across nodes) |

#265 can close when its agreed availability/ordering evidence passes through
static checks and real execution, not merely when E lands. Include G if peer
imports are in that scope, and record remaining #267 limitations. #267 closes
only when I and J satisfy its local and cross-node verification contract.
Units are review subjects, not a promise that each is a small PR. Split further
if implementation exposes another independent mechanism, and avoid
abstraction-only PRs whose helpers have no caller yet.

## Fit with the path to suspendable execution

The path, from the sequencing in
[suspendable execution](../design/suspendable-execution.md), is: revise task
12's spec, then task 12, then #28 (one event loop), then #213 (suspendable
execution).

- **Task 12's spec revision must change.** Its requirement that
  compute-on-read treat `Transaction::staged` as the transaction's writes, and
  its reference to "Evaluating an update" in `code-updates.md`, come from
  #266. Neither is on main, and #266 will not merge as is. Under the design,
  var steps write to the transaction's ordinary write buffer, so task 12 needs
  no update-specific hook for unchanged defs. Replace that requirement with a
  pointer to the design, and keep test 12 as an ordinary-transaction test plus
  an update test that lands with I.
- **E1, E2, F, G and H1 do not wait for task 12.** E1 and H1 check static
  information without evaluating initializers. E2 enables only the runtime
  subset it can execute correctly, with explicit support diagnostics.
- **I waits for task 12**, and can land before #28.
- **H2 waits for I.** Do not enable full type-changing execution while a def
  read can still return a cached value of its old type. H1's static checks
  can land earlier; they do not enable that execution path.
- **J waits for #213.** If owners run update steps as participants before
  #213, a participant that must wait parks and replays. That would need task
  10's replay records for update steps, which #213 then deletes, against the
  cleanup rule in `docs/design/suspendable-execution.md`. Design #180 together
  with #191 while #28 is designed. Until J, keep today's local-first
  forwarding as a documented limitation, and fix #257's lock leak
  independently.
- **Staged state is transaction-owned.** Every unit keeps staged bindings,
  types, code and values in the update's transaction or execution context.
  `Manager` may hold contexts indexed by transaction ID; it must not install
  a transaction's staged view as ambient state. After #213, other actions
  interleave with a waiting update.
- **Checklist additions.** #28's checklist should include the update path's
  own `MAX_WAIT_DIE_RETRIES` loop (`update.rs:183`). An update can park and
  restart before the commit decision only after the entire attempt is
  discarded and participant abort/cleanup and lock-release rules are followed.
  Pure initializers can still acquire remote locks and participant state.
  #213's list of methods to split around their awaits should include
  `update::Transaction::apply`, which holds `&mut Manager` across lock waits,
  evaluation and `send_and_await_reply`.

## Current implementation evidence

Checked 2026-11-24 on `8448a60`, whose runtime source matches main `4a18906`.
Paths are under `meerkat-lib/src/runtime/`. Rows marked "ran" were run
through the CLI; the rest are from reading the code.

| Area | Current behavior | Unit |
| --- | --- | --- |
| `node.rs` `with_resolved_services_and_tests` | Substitutes each service's final, updated declarations into the startup program, so startup initializes the final declarations and then runs the updates again. `update s { var y = x; var x = 2; }` after `var x = 1` gives `y == 2`, not 1 (ran). Imported services are instantiated from unresolved declarations when the root file has updates (#285 item 1) | A |
| `update.rs:167` | `lock_groups.insert` per update statement: a second update of the same service in one atomic block overwrites the first one's locks | B |
| Grammar (`meerkat.lalrpop:98`) | `atomic` contains only `update` statements; nesting is impossible. `manager::run_update` flattens the one level | B |
| `update.rs` traversals | Lock collection, evaluation, `unvalued_defs` and `declared` each walk `Stmt::Update`/`Stmt::Service` separately | B |
| `tt/check.rs` `ActionStmt::Assign`; `Manager::assign` | Assigning to a def type-checks and succeeds at runtime, and the next recompute overwrites it (ran; #293) | C |
| `node.rs` `static_checks`, `update.rs` `try_apply` | Run whole-program `check_program` on the merged declarations | E |
| `interpreter/evaluator.rs` capture filters for `Expr::Func` and `Expr::Action`; `graphs/effects.rs` capture filter in `visit_expr`, and the `shadowing` second pass in `member_value` and `check` | A closure drops a local that shares its name with an existing member and reads the member instead (#86). The effects analysis mirrors this with positions, so capture depends on which members exist when the closure is created | #86 |
| `graphs/effects.rs` `forward_reference` | Compares declaration positions (`initializing: Option<Position>`) | E1 |
| `graphs/effects.rs` `read` | Discards a def's own reads, as suited to reading an initialized cached value; that does not establish safety of later compute-on-read after dependencies change | E1 (check code activated by derived reads in the current view) |
| `tt/check.rs` `type_of_member` | Fast path returns a member's type from `local_services` before inferring its declaration | E1 (seed with var types established by earlier steps) |
| `update.rs` `try_apply` | Evaluates var initializers per update statement; results go to `self.values`, which later initializers cannot see | E2 |
| `update.rs` `try_apply`, `Manager::first_values` | Defs get values only after commit (`recompute_def`). A new def that lacks an input, such as a remote one that depends on the update's forwarded part, waits for propagation to deliver it (`await_first_values`) | I (local inputs), J (remote inputs) |
| Kind changes | `var x0 = x; def x = x0 + y;` and `var d = d;` both fail with "dependency cycle detected" (ran) | E2 (def to var), H1/H2 (var to def) |
| `node.rs` `static_checks` | Checks top-level actions, tests and watches against the final declarations | F |
| `imports.rs` peer folding | Folds a peer's startup updates into its declarations and checks the result as a program to initialize | G |
| `manager/mod.rs` `DepState::offer`, `accept_delivery` | A delivery with the same clock but another value or Sources is taken as a code update, and nothing records which code version produced a value, so a delayed old-code value can look like a new one on a remote consumer (unconfirmed; #295). Local propagation after a code-only change is correct (ran) | H2/H3 (and #295) |
| `update.rs` `try_apply`; `nameres.rs` `resolve_table`; `graphs.rs` `tables` | A table declaration in an update returns `NotImplemented` (lock collection and evaluation). Name resolution treats a table as its own sort (`ExpectedSort::Table`), so `insert`, `map`, `fold` and `select` accept only a declared table, and `insert {n: 1} into d` for a def fails with "expected table" (ran). A table declaration has no initializer | T1, T2 |
| `update.rs` `format_update_source` | Remote updates can't serialize tables, `fold`, `select`, `map`, actions or lambdas (#286) | J (and T2 for remote tables) |
| PR #266 at `8cba620` | Reorders the merged declarations; stages var values in `Transaction::staged` in merged order | Salvage tests and fixtures in E2; not its order or staging map |

The #267 issue body describes #266's behavior as "today". Correct it when #266
is retired.

## Units

### #86. Lexical closure capture (before E1)

Fix #86 first. A closure captures every local free in its body, including one
named like a service member, whatever members exist when it is created. Fix
both evaluator capture filters (`Expr::Func` and `Expr::Action`), and make the
effects analysis's capture filter lexical too. Then delete the `shadowing`
machinery: the second pass in `Effects::member_value`, `Declared::shadowing`,
and the re-analysis in `effects::check`. The update checker then needs no
availability information for capture, and its view stays smaller.

First separate service-member lookup from lexical bindings in startup's
evaluation environment, or otherwise preserve their origins. `create_service`
currently passes earlier service members in `env` to var initializers, so
merely removing the member-name filters would capture their startup values.
Prefer ordinary member lookup where it preserves initialization's snapshots
and eager checks. With `var x = 1; var f = fn () => x;`, calling `f` after
changing `x` must read the current member; a lexical `let x = ...` inside the
initializer must instead be captured.

Tests: a `let` binding and a closure parameter that shadow a member, captured
in a function and in an action, with the member changed before the call.
Pair these with unshadowed member references in functions and actions stored
by startup var initializers: those must observe the changed member. Also
check that the effects analysis agrees with runtime reads. Because this
changes behavior, check the test suites and `scripts/test_mkn.py` for programs
that relied on the old filter.

### A. Preserve the startup history

This is #285's item 1, a regression from #216, and is related to #280 (the
runtime should execute the AST the static checks produced). Fix both cases
#285 describes:

- **Root services.** Resolve names in the original service statements in
  place, instead of substituting the final declarations. Test with a program
  whose result detects early installation: `service s { var x = 1; }` then
  `update s { var y = x; var x = 2; }` passes static checks on main, and must
  give `y == 1`. Initializing the final declarations first gives `y == 2`.
- **Imported services.** When the root file has updates, `static_checks`
  resolves separate copies and rebuilds only the root program, so imported
  services are instantiated from unresolved declarations. #285's example (an
  imported `helper` whose action reads a record member) fails with "Service
  'r' not found" only because the root file has an unrelated update. Imported
  services must also run their resolved declarations, before any update.

Cover root and imported services, with sequential updates and an `atomic`
block, and apply each update at its original position. Close #285's item 1
with this unit, and check whether #280 can close with it or needs more.

### B. One traversal, union locks

Add one helper that yields an update's steps in order (service and
declaration, flattening the one atomic level), and use it for lock
collection, evaluation, `unvalued_defs` and `declared`. Union lock groups per
service. Test an atomic block whose two updates declare different members of
the same service, and check that both updates' locks are held.

### C. Reject writes to defs (#293)

Reject `ActionStmt::Assign` to a def in the type checker. Also reject it in
`Manager::assign`, since writes arrive from other nodes. This is a
prerequisite for var-to-def changes (H).

### D. Analysis and design investigations

D is split so that #265 waits only on the questions E1 needs. Both
investigations start from existing on-demand inference and effects analysis,
evaluate no initializers, and report concrete accepted and rejected examples.

**D1, before E1: checking without execution.** With source code only (what
`--check` has), establish whether these can be checked:

- an unchanged def whose called function is replaced with one that reads a
  not-yet-introduced member (the design's `f`, `d`, `later` example);
- a function-valued initializer with multiple possible results, analyzed
  conservatively without running its condition or body;
- a whole file of declarations and updates checked without a running
  `Manager`.

**D2, after E1 and before H1: effect-system design and prototype.** Cover
type and kind changes, stored functions, and live metadata:

- a retained def read between a type change and its repair (should be
  rejected);
- the same update with no read in between (should be accepted);
- a closure stored in a var whose body reads a member whose type or kind
  changes, called directly and through a higher-order function;
- a var-to-def change with an action value in a var that writes the member;
- checking against supplied installed-state metadata, for live services,
  without changing that state.

The design's approach is an effect system, as in the draft semantics:
function and action types carry latent effects, the members their bodies read
and write, with their types (§2.1, rule WF-δ). A var that holds functions
has such a type, so its latent effect bounds every function the var may
hold, and every assignment must fit it (rule A-Write). A var whose bound no
longer fits the new bindings, or includes a write to a member that became a
def, is then ill formed.

The central design question is moving from values to types. Today's
analysis is value-based: the effects analysis reads the functions vars
currently hold (`published` values), and `check_function_writes` decides when
a function value may be stored. With a value-based description, a value
transaction that stores a new function changes what an update was checked
against. D2 should design bounds carried in types instead, including how a
var's bound is inferred when it has no annotation (from its initializer and
every assignment in installed code), and how new code that assigns a larger
effect is rejected. The effects analysis already summarizes functions, so
check whether those summaries can supply the bounds. The draft's §6 notes the same
gap for higher-order values, so share D2's findings with its author.
Establish whether summaries retain expected member types and read/write
capabilities, including latent effects inside returned functions and actions;
a set of dependency names is insufficient for WF-δ. Include returned closures
and actions in the examples. Resolve how patch checking handles temporarily
incompatible bindings despite §2.2's well-formed-environment premise, before
H1 enables that behavior. Final-state compatibility alone does not justify
the intermediate checking judgments.

D2's main output is the design of that effect-system extension: what a
var type's latent effect contains, how it is inferred, how assignments are
checked against it, how it is checked at an
intermediate step and against the final bindings, and how they are kept for
live services. It is a semantic design question as well as an implementation
one, so bring the proposal back for design review, with concrete accepted and
rejected examples, before H1 is built on it. Distinguish static metadata that
is required from runtime values that are merely convenient; incomplete
metadata must not trigger evaluation as a fallback.

### E. Update steps (the #265 fix)

Split into a pure checking feature and runtime integration. E1 can land and
serve `--check` independently of E2. Passing the checker establishes static
validity, not runtime support or successful evaluation. Keep execution on its
existing validation path until E2 integrates the shared checker and reports
unsupported execution cases explicitly.

#### E1. Check updates without executing them

- Give the effects analysis an availability map, from member to the step at
  which it becomes available, in place of positions. Initialization uses
  declaration positions, so its behavior and errors stay the same. In an
  update, existing members are available before the first step and a new
  member after its step. Capture needs no availability information once #86
  has landed.
- Check each initializer against the preceding static view before replacing
  its binding. Advance types, code and effect summaries without computing
  values. Use a fresh `Effects` and type context per changed view, rather than
  incremental cache invalidation, until profiling says otherwise.
- Seed the type checker with var types, not def types: the type each var's
  step established (from source, when checking a file), or its installed type
  (for the later live adapter). Historical var initializers are then not
  re-inferred. Recheck code activated by a def read against the current view,
  including calls through replaced functions.
- Run final-state validation: types, graphs, cycles. No initialization-order
  check on final storage positions, and no final def evaluation.
- Reject a member declared twice in one transaction: within one update, or
  across the updates of one `atomic` block.
- Expose a checker taking an update and static service environment. It must
  not require an executing `Manager` or call the evaluator. Reuse it in the
  existing CLI `--check` path for files containing updates; separate-update
  checking against a supplied base or live interface is a later adapter.
- When checking against live services, acquire a consistent description of
  their installed state (kinds, types including the latent effects that bound
  what a var's functions may read and write, and def code). The design
  requires each node to keep that description current as it installs code;
  writing a value never changes it. D2 defines the latent effects and G how
  peers supply them. Until G, checking
  against a remote peer may lack some of it: report that as unsupported,
  never fill it in by running code. Execution revalidates under transaction
  protection if that description could have changed.

Review E1's availability analysis separately from installed-state typing and
final-state integration, as commits or smaller PRs with actual callers.
Resolve D1 first rather than introducing a second effects visitor.
Advanced migration checks can follow as H1 without holding up the #265 cases.

Evidence: checking accepts `var x = 1 / 0` without running it, but diagnoses
`var y = x + true` after it; reports old/new type and unavailable-read errors;
checks function-valued initializers conservatively; and leaves runtime state
unchanged. Test the checker directly without service creation, plus CLI
`--check`. Neither path may evaluate a var initializer or a final def.

#### E2. Execute ordered updates using E1

- Evaluate steps in order from B's traversal. Stage each var's value in the
  transaction's write buffer: extract the buffer-and-cache part of
  `Manager::assign`, without its existence checks. Later initializers see it
  through ordinary lookup, including qualified reads and calls.
- Install bindings and values together at commit. A failure discards staged
  state.
- Invoke E1 against the protected environment before evaluation. Keep
  execution-support checks separate from language typing errors.
- Reuse ordinary transaction locking, conflict checks, and read-history
  recording for initializer reads, including calls and supported def reads.
  The draft's empty value-read/write footprint for code patches does not
  apply to expression initializers committed with their values. The
  implementation accounts for those reads like a value transaction's. Adding
  the footprint to the draft is a formalization follow-up and does not gate
  E2.
- Table declarations in updates stay unsupported until T2, reported before
  any initializer runs; the static check (E1) can accept them once T1 lands.
- Enable no new cases affected by values from old versions (#295). Same-type
  replacements, including type-preserving def-to-var changes, keep main's
  existing behavior, with no claim that they are safe; #295 covers them too,
  and if it confirms a failure, its fix covers them or they are restricted
  then. #295 investigates whether main can incorrectly accept delayed
  or retained old-code values; a failure has not yet been reproduced. E2's
  new initializer forms, such as `var d = d`, do not add new kinds of change.
  Type changes, including def-to-var changes that also change the value's
  type, and var-to-def changes that reach consumers stay with H2 and H3.

Until I, execution cannot promise current-view def reads or final def
evaluation before commit. E2 therefore refuses, as unsupported execution
(#267), any update with a step that reads a def that needs staged code or
recomputation after earlier writes. That includes reads of unchanged defs
whose inputs changed, and reads inside called functions. Decide this from
E1's analysis **before any initializer runs**, not partway through
evaluation, and without a second dependency analysis. Where the analysis
cannot show that a cached result is consistent with the current view, refuse
conservatively. An unaffected old def can still be read, as in `var d = d`
before any change to its inputs or code.

This refuses some updates that main accepts today while silently reading
stale values. That change is intended; say so in E2's PR, with a test. This
#267 restriction concerns reads during an update and remains in place;
factoring out #295 adds no rejection of existing cases solely for its
suspected delayed-delivery problem. The same restriction keeps type-changing
execution safe until H2: with existing
`var x = 1; def d = x;`, the update `var x = "new"; var copy = d;` is refused,
not given an old integer under a string type. Record the remaining
post-commit def-failure behavior until I; do not claim E2 implements the
complete atomicity contract for defs.

Execution evidence:

- `meerkat/tests/test_update.mkt` runs through the CLI, with assertions on
  every changed or new value;
- the design's var examples (before/after, added/result, `var x = x + 1`);
- with initial `x == 1`, separate updates `var y = x` and `var x = 2`
  produce `y == 1` or `y == 2` according to their order; disjoint destinations
  must not cause the runtime to ignore the read conflict. Assert that
  committed values retain initializer read history, including indirect reads;
- def to var with `var d = d`;
- an `atomic` block whose updates declare different members of one service;
- unavailable reads still rejected, and initialization errors unchanged;
- the mkn `local_update` and `remote_update` scenarios salvaged from #266,
  with assertions rewritten to observable values, not member order.

### F. Top-level statements at their position

After E1, check each action, test and watch against the view at its position.
Test an action before an update that reads a member the update adds
(rejected), and one after it (accepted). Use `@test` blocks for executable CLI
coverage: the current client loop skips bare actions and watches. Adding
execution of those forms in that mode is separate work, not a prerequisite
for fixing static checking. Retained watches remain clients of later updates.

### G. Peer interfaces as installed state

Validate a service fetched from a running peer as installed state, with no
initialization order. Test an import of a peer after an update that leaves a
member stored before one it reads. Check whether #269 has the same cause
before treating it separately. Look at the peer metadata available first; a
change to the wire format needs its own review. G is also where a peer starts
supplying the installed-state description the design requires (kinds, types,
def code, with latent effects as part of var types), so updates can be
checked against it.

### H. Type changes and var-to-def (three review units)

**H1: static validation, after C, D2 and E1.** Track old and new types per
step. Validate retained defs and stored functions (as D2 decided) against the
final interface, kind included. Extend check-only coverage; no initializer
evaluation or task 12 dependency. Test:

- the design's type-change example, with and without a read in between;
- a retained var's type survives;
- `var x0 = x; def x = x0 + y;`;
- a retained local writer of a member that becomes a def is rejected,
  including action values stored in vars.

**H2: local execution, after H1 and I.** Enable full type-changing and
var-to-def updates only when derived reads use current code and values.
Test type preservation through intermediate reads, local kind transitions,
and downstream values, clocks and Sources. This removes E2's migration
restriction; it does not introduce another checker or derived-read path.

Old-version safety is a prerequisite for H2 and H3, the stages that enable
type changes and var-to-def changes that can reach consumers. Consumed values
must agree with the versions and types the consumer expects; the draft can
retain historical values under compatible historical selections. Check
delayed messages and retained cached inputs. Establish whether local commit
and propagation order suffice for each case H2 enables. A locally changed
service may have remote consumers, and current-graph delivery filtering
(#264) alone does not establish version agreement. #295 investigates this
unconfirmed failure scenario on main, including same-type replacements; its
tests and any necessary fix should come first or with H2. The draft tags
values with their producing version ("generation fencing", §5). Settle a
mechanism or a safety argument
with the draft's author before enabling each case, or keep it unsupported.
Coordinate distributed support with J.

**H3: distributed validation and publication.** Separate review from local
migration. Identify how relevant remote clients and their interface versions
are obtained, using G's peer metadata work; do not claim all clients were
checked from incomplete information. Test retained remote writers and
downstream values, clocks and Sources across nodes, including a value in
flight or retained in an input cache from an old version when the update
commits. Coordinate with J for
atomic migrations spanning owners. Individual static metadata/checking work
can land earlier, but remote execution claims require protocol evidence.

### Task 12 (separate workstream)

Revise its spec as described above, then follow its own prerequisites and
review breakdown.

### I. Def steps and def reads during an update (#267 on one node)

After task 12 and E2. This establishes current-view derived reads for the
supported types; H2 then enables full type-changing execution on that base.

- A def step installs staged code in the transaction and discards dependent
  cached derived values. Start conservatively: clear every disposable derived
  entry in the transaction after a step that changes code or a value, while
  keeping authoritative writes, locked base snapshots and read history. That
  needs a clear split in `read_cache` between authoritative and derived
  entries; do not clear it wholesale.
- Reads of defs during the update go through task 12's compute-on-read, with
  the staged code.
- Before commit, evaluate every def the transaction introduces or replaces,
  using the final view. Reuse a cached result only if it remains valid there.
  Failure to evaluate or obtain required
  inputs aborts before publication; it is not grounds for silently omitting
  that member from this check.
- Until J, keep today's post-commit first-value path (`first_values`,
  `await_first_values`) for a new def whose inputs depend on this update's
  part on another node: before J, that part is applied only after the local
  commit, so I cannot evaluate the def before commit. Decide which defs those
  are before execution, from E1's analysis, and record the limitation. Do not
  abort them, which would refuse updates that work today. Failure on this
  first-value path follows current post-commit error handling and cannot
  roll back the committed update. No initializer may read a deferred def,
  directly or transitively, as specified below. J removes this path.
- Publish values with clocks and Sources consistent with commit. Do not
  publish a def value cached at an earlier step.
- Lift E2's def-read restriction only for reads I can serve from the current
  view. Until J, an initializer cannot read a def deferred to the post-commit
  first-value path, directly or through another def or called function.
  Reads requiring another owner's staged changes also remain unsupported.
  Use E1's analysis to reject these cases before any initializer runs, keeping
  execution-support diagnostics separate from typing errors. The local case
  of `test_update_var_cannot_read_a_def_the_update_adds` (from #266) becomes
  a positive test.

Evidence: new, changed and unchanged defs read during an update reflect
earlier steps; a read, write, read sequence refreshes transitive dependents;
replacing a def's code invalidates earlier results; a def that fails on the
final view aborts the update and leaves live state unchanged.
Preserve eager service initialization. Test
the design's `def r = 10 / x; var x = 2;` with old `x == 0`: the update
succeeds and `r == 5`. Test a new def with a remote input from the same
update's forwarded part: when no initializer reads it, it gets its first
value after commit, as on main. Pair that with direct and transitive reads
of it from an initializer, rejected as unsupported before execution until J.
Also test that a read requiring another owner's staged changes stays
restricted until J.

Review staged code lookup/evaluation separately from invalidation and
publication bookkeeping if needed, but land the local feature only with
correct failure handling, clocks and Sources. This is a review stack, not a
claim that all of I is a small independent PR.

### T. Tables as vars (#298)

The design treats a table as a var that holds a list of records ("Tables").
Two units:

**T1: the language change (#298).** Independent of the update work, and
worth checking with the author of #216.

- `table t { … };` declares a var of the table type that starts empty, and
  `table t { … } = e;` gives it an initializer. Allow the initializer in
  service declarations and updates alike, and print it in
  `format_update_source`.
- `insert e into t` appends to a writable var of a table type, staying an
  append in the transaction's buffer (#294's `BufferedWrite::Append`).
- `map`, `fold` and `select` read any member of a table type, including a
  def. Remove the separate table sort in name resolution (`resolve_table`,
  `ExpectedSort::Table`) and the separate `tables` set in the graphs.
- Existing table programs and tests keep working; "expected table" errors
  become type errors.

**T2: table steps in updates, after T1 and E2.** A table step is a var step:
check its initializer (or empty table) against the declared type at its
step, stage the value in the transaction's write buffer, and remove the
`NotImplemented` cases in `update.rs` through B's traversal. Re-declaring a
table with the same or a different schema works like re-declaring a var.
Schema changes that retained code depends on are type changes, checked and
executed by H1 and H2. #285's remaining table and `fold` reactivity items
come before T2's tests of defs over a migrated table.

Evidence: a new table, empty and initialized; a table re-declared with
`= t`; a schema migration with `map`; a re-declaration without an
initializer, which leaves the table empty; and, through H1/H2, retained
`fold`, `select`, `map` and `insert` users across a schema change and
table-to-def and table-to-var changes.

### K. Statements in atomic blocks (#297)

After E2 and F. Allow `let`, assignments, `insert` and new services in an
`atomic` block, as further steps of the same transaction. Each is checked
against the view at its position, using F's mechanism, and executed through
the transaction's ordinary write buffer; their reads join the footprint like
an initializer's. `insert` from a top-level block needs a qualified target
(`s.t`), which T1 or K must add. Test `let`, assignments and `insert` on
existing members, including a failure partway that leaves nothing installed.
Migrating table rows with `let old = s.t;`, a re-declaration of `t`, and
`insert` of each old row, all in one block, re-declares a table in an update,
which E2 refuses until T2; land that test with or after T2.

### J. Remote participation (#180, and #267 across nodes)

Designed with #191 alongside #28, built after #213. #286 (serializing tables,
`fold`, `select`, `map`, actions and lambdas in remote updates) is a
prerequisite for updates that carry those forms to another node. Review
subjects, in order:

1. The protocol and failure model as a design change: owner-evaluated steps
   or coordinator-evaluated steps with staged writes and code pushed to
   owners; participant cache invalidation; the commit point; retries.
2. Participant-side staged state, served to transactional reads, using the
   suspendable participant machinery.
3. Coordinator ordering, including a sequence that visits owner A, then B,
   then A again.
4. Commit and abort integration, with failure tests. Activate it only once
   participant and coordinator changes are both in place.

Evidence: cross-node forms of E's and I's tests; reversing cross-service steps
changes reads as specified; other transactions never see staged state; failed
preparation leaves services usable and releases participant locks.

## Implementation guidance

- One traversal (B) serves locks, checking, evaluation and the final merge.
  The merge helper builds the final declarations; nothing recovers the
  execution order from them.
- No second expression checker, effects visitor or syntactic eager-read scan.
  Static and runtime drivers stay small and separate: one works with types
  and effect summaries, the other with values and locks. Check-only and
  execution reuse the same static checker; runtime-support restrictions are
  separate diagnostics, not additions to the language's typing rules.
- One evolving view of current bindings. Sequential evaluation needs no
  per-prefix snapshots or historical-version lookup. Generation identifiers
  used to validate deliveries are a separate possible mechanism; they need
  not require retaining historical code or values. An old closure kept as a
  value keeps its code and captures under the existing value representation.
- One authoritative buffer for var writes: the transaction's. No second map
  of staged values. Code-update commit installs values and structural changes
  together.
- Def evaluation reuses compute-on-read. A def step installs code and reads
  happen through ordinary lookup. This is the chosen update semantics, not
  proof of equivalence to eager declaration-time evaluation. Keep service
  initialization's existing eager evaluation unchanged.
- Closure capture is lexical after #86, so neither the checker nor the
  runtime consults which members exist when a closure is created.

## Regression matrix

Assert observable values and errors, not stored declaration order. Cover
static checks and runtime where both apply.

- `test_update.mkt`, with assertions on every changed and new value.
- Check-only: division by zero does not execute; a later typing error is still
  reported; no initializer or final def is evaluated; supplied live state is
  unchanged; function summaries cover possible values without running code.
- A stale checked environment is revalidated before execution; static
  validity and temporarily unsupported execution have distinct diagnostics.
- Existing later-position member; new earlier member; unavailable later
  member; existing member replaced later; self-migration.
- A member declared twice within one update, or across the updates of one
  atomic block, is rejected; different members of one service in two updates
  of one atomic block are accepted, with both updates' locks held.
- Type changes: retained var type; temporarily incompatible def, read and not
  read; client incompatibility; stored closures (per D2).
- Kind changes in both directions; retained local and remote writers of a
  member that becomes a def; Sources downstream, local and remote; writes to
  a def rejected (#293).
- Direct and qualified reads; called and higher-order functions; delayed
  functions and actions; captures across replacements; an unchanged def calls
  a replaced function that reads a not-yet-introduced member; a local that
  shadows a member is captured (#86), while unshadowed member references in
  closures stored by startup var initializers remain live lookups.
- Updates main accepts today with a stale def read are refused as unsupported
  by E2, before any initializer runs.
- Until J, an unread new def requiring the update's forwarded part retains
  the post-commit first-value path. Initializer reads of it, direct or
  transitive, and reads requiring another owner's staged changes are refused
  as unsupported before execution, including after I.
- New, changed and unchanged defs read during an update; transitive
  invalidation; final reactive cycles; failure on the final view, but not at
  an intermediate step nothing reads (`def r = 10 / x; var x = 2;`).
- Across type and kind changes enabled by H2 and H3, consumed values agree
  with the versions and types the consumer expects, locally and across nodes,
  including delayed messages and retained input caches; cases without that
  evidence are rejected before execution. Same-type replacements are covered
  by #295's tests.
- Top-level statements before and after an update.
- Tables: a new table, empty and initialized; re-declared with `= t`, with a
  `map` migration, and without an initializer; schema changes against
  retained `fold`, `select`, `map` and `insert` users; table-to-def and
  table-to-var changes; and, with #297, rows migrated by `let` and `insert` in
  one atomic block.
- CLI startup (initializers run once), local imports, peer imports, runtime
  updates, and a second update after a final state whose stored order puts a
  member before one it reads.
- Local and cross-node isolation, abort cleanup, and consistent value, clock
  and Sources publication.

For implementation changes, run the focused tests, then the Rust checks from
CI and the pre-commit hooks, and the CLI help smoke test. Before pushing
runtime changes, run `cargo build -p meerkat`, then
`python3 scripts/test_mkn.py`, and check for leftover nodes with
`ps -C meerkat -o pid,args`. AGENTS.md covers the known `cascade_lock_wait`
flake and stopping servers started by hand.

## Related documents to update

- **[Semantics PDF](../meerkat-semantics.pdf):** the design's "Relation to
  the formal semantics" lists where it differs from the draft's
  code-installation model (§5–6), and the open requirement on values from old
  versions. Expression initializers, and therefore installing code and
  values in one event, are agreed with the draft's author (Nov 25). Relating
  the draft's separate event histories to the combined event, including
  initializer reads and evaluation failure, is a formalization follow-up
  that does not gate E2. Establish old-version safety (#295) before H2 and H3 enable
  type and kind changes that reach consumers, or keep those cases unsupported.
  Ask whether T-Def's exclusion of constant and closure-only defs is intended:
  the implementation keeps both, since actions are declared as defs, so this
  is a question for the model and does not gate E1. Resolve the
  well-formed-environment premise for temporarily incompatible bindings as
  part of D2's effect-system design, before H1. Keep the committed PDF current when a new
  draft replaces it.
- **Task 12's spec:** as above, before task 12 starts.
- **#267's body:** describe main, not #266, and point to the design.
- **#28 and #213 checklists:** the update retry loop, and
  `Transaction::apply`.
- **#266:** close it with a pointer to the design. Its tests and mkn fixtures
  move into E2.

## Draft comment for #265

Post after the design has a commit or PR link, and replace the path below
with that link.

> Proposed replacement for the Decision above: an update executes its
> declarations as steps, in source order, against the existing state and the
> steps before it. A var step evaluates its initializer and stores the value;
> a def step installs new code. Service initialization shares the availability
> principle starting from no initialized members, while retaining its existing
> eager evaluation. The stored order of the resulting declarations has no meaning: replaced
> declarations stay in place and new ones are appended, with no
> dependency-based reordering.
>
> Reordering, whether #266's or a topological initialization order, cannot
> express migration that reads an old value: `var before = x; var x = 2;`,
> `var x = x + 1;`, or a kind change such as `var x0 = x; def x = x0 + y;`.
> Steps express all three.
>
> The design is in `docs/design/sequential-code-updates.md`. It supersedes
> #266's ordering; #266's tests and end-to-end fixtures move into the fix.
> E1 makes updates checkable without evaluating initializers; E2 uses that
> checker for ordered execution and staged vars. Static success does not
> promise successful evaluation or currently implemented runtime support.
> Def reads during an update (#267) follow task 12, and full type-changing
> execution follows those reads. Remote participation follows #213 with
> #180's protocol. Close #265 when its agreed static and end-to-end evidence
> passes, with any remaining #267 and peer-import limitations explicit.

Then update the issue's Decision to point to the design, so the old ordering
rule is no longer the apparent acceptance criterion.
