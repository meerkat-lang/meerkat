# Design for controlling the location of execution

Issue #20 asks to implement the execution-location proposal on the
Requirements wiki page.  Rules 1 to 4 of that proposal describe what the
runtime already does: execution starts where an action is triggered, stays in
its service by default, moves to the defining service to run an action
(rule 3), and reads a member of another service on its owner (rule 4).  This
document designs rule 5, the part that was not built.

## Motivation

A call through another service downloads code.  Given

```meerkat
service s1 {
    var y = 5;
    def f = fn (x: int) => x + y;
    def a = fn (x: int) => action { y = y * x; };
}
```

a client running `do s1.a(s1.f(3));` sends four requests before it commits:
a lookup of `a`, a lookup of `f`, a lookup of `y` (because `f`'s body runs on
the client and reads `y`), and an action request that carries `a`'s body back
to `s1`.  In general, a function of another service costs one round trip to
fetch plus one per member its body reads.  Rule 5 lets the author of `s1` say
that a member is called where it is declared:

```meerkat
service s1 {
    var y = 5;
    def f(x: int) = x + y;
    action a(x: int) {
        y = y * x;
    }
}
```

The same client line then takes two requests: `s1` runs `f(3)` and returns
`8`, then `s1` runs `a(8)`.

## What done means

The wiki's text (Requirements, "Execution location", lines 123 to 150) makes
seven claims.  Each is a test.

| # | Claim | Test |
|---|---|---|
| 1 | `def f(..)` and `action a(..)` are declared at the top level of a service, with a parameter list that may be empty | parser tests, including `def g() = y;` and `action reset() { .. }` |
| 2 | They can only be used with a call | static errors for every other use |
| 3 | A call made by running code (an action, a `@test` or REPL statement, a code update's `var` initializer) runs at the owner instead of loading the code | a stand-in peer sees one request per call and no lookup of the member or of what it reads |
| 4 | Execution returns to the caller, with the result of a def | the caller uses the value; statements after the call run on the caller |
| 5 | They compute the same results as the `fn` spelling | one program against both spellings ends in the same state |
| 6 | Client code may be the same for both | one unchanged client program against both servers |
| 7 | `s1.a(s1.f(3))` takes two round trips instead of four | requests counted before the commit |

Claims 3 and 5 are scoped, as follows.

**Scope.**  Execution moves to the owner for a call of a special def made by
running code, and for `do` of a special action from a node whose copy of the
owner declares it, when the owner's current declaration agrees.  It stays on
the caller, at today's cost, in these cases, each explained below:

- computing a def from pushed values, and creating a service;
- a def call with a function value among its arguments;
- an argument or a result with no wire form (html);
- a caller whose copy of the owner declares the member plain;
- a by-name `do` that the owner declines, because its current declaration is
  no longer an action written in place, or a function whose body is one (a
  special action or its `fn` spelling);
- `do` of a special action in the REPL and the browser, which hold no copy of
  the owner: the owner returns the action, which then runs at the owner, two
  round trips instead of one.

Results are the `fn` spelling's except where the call's location itself shows:
a call back into the waiting caller, or a wait at a third node longer than the
reply timeout, and a caller's copy that predates a change to a parameter list
(see "Limits").  Whether this scope is what #20 asks for is for the
maintainers to confirm.

## Syntax

```meerkat
service s1 {
    var y = 5;
    def f(x: int) = x + y;              // a def with parameters
    pub def g(x: int, z: int): int = x * z + y;
    def now() = y;                      // an empty parameter list
    action a(x: int) {                  // an action with parameters
        y = y * x;
    }
    pub action reset() { y = 0; }
}
```

- The forms are members, so they appear only directly inside `service` and
  `update` blocks.  `pub` may precede either.
- The parentheses are required, also when the list is empty.  A bare name
  (`def f x = ..`) is not accepted: `fn` allows it, but here it would read as
  an ordinary `def`.
- Parameters need types.  The type checker cannot infer a parameter's type,
  which is already the rule for `fn` (`tt/check.rs`, `infer_function_type`).
  The wiki writes `def f(x)`; Meerkat spells it `def f(x: int)`.
- A parameter may not have the name of a member of its service.  Inside an
  action body such a name refers to the member at run time, because a closure
  never captures a local named like a member of its service
  (`interpreter/evaluator.rs`, the capture filters of `Expr::Func` and
  `Expr::Action`), so the parameter would be silently ignored.
- A def may state its result type after the parameters (`): int =`).  When it
  does not, the type is inferred, as for `fn`.
- The `;` after an action's block is optional, as on the wiki.

A call is written as a call of any function member:

- `s1.f(3)` anywhere an expression may appear, and `f(3)` inside `s1`;
- `do s1.a(3);` runs an action, and `do a(3);` inside `s1`.  `do` stays
  because the wiki says client code may be the same for both spellings
  (line 144), and the `fn` spelling of that client line is
  `do s1.a(s1.f(3));`.  Without `do`, `s1.a(3)` is the action as a value, as
  it is for the `fn` spelling.

## Meaning: the `fn` member, plus a mark

The wiki defines the forms by equivalence (lines 127 to 142).  The parser
desugars them into exactly those members and records which form was written:

| Written | Parsed as |
|---|---|
| `def f(x: T) = e;` | `def f = fn (x: T) => e;` with `form: DefForm::Function` |
| `def f(x: T): R = e;` | the same, with the lambda's `return_ty` set to `R` |
| `action a(x: T) { S }` | `def a = fn (x: T) => action { S };` with `form: DefForm::Action` |
| any other `def` | as today, with `form: DefForm::Plain` |

`DefForm` is a new field of `Decl::DefDecl`.  Nothing else in the AST
changes.  Because a special form is an ordinary function value:

- typing is unchanged: `f` has type `int -> int`, `a` has type `int -> unit`;
- the dependency analysis is unchanged: applying `f` reads what its body
  reads, in `s1` (`docs/internals/statics.md`, "Reads and Edges");
- reactive propagation, glitch freedom (#217) and live code updates are
  unchanged: the member holds a closure, as before;
- the owner still serves the member's value to a node that asks for it.

## Only with a call

Name resolution rejects any use of a special form other than as the function
of a call: `let g = s1.f;`, `do s1.a;`, `onclick={s1.reset}`, passing `s1.f`
as an argument, or `watch s1.f;`.  The error names the member and shows the
call to write instead, for example
``s1.a can only be called: write `do s1.a(...)` ``.

How it is built: members are bound with a binding that carries their form
(`Binding::Member(DefForm)`), while `let`s, parameters and loop variables keep
`Binding::Value`, so a local that shadows a member is resolved as the local.
At run time the local wins too, unless a closure or an action written in place
captures it: the capture filters drop a captured local named like a member of
its service (#86, as on `main`), and the body then reads the member.  When
that member is a special form it is read as a value there, which this rule
does not catch.  A special form's own parameters cannot meet this, because
they may not have a member's name (see "Syntax").  The function of an
`Expr::Call` is resolved on a call-position path that accepts special members;
every other position rejects them.  The fallbacks for forward references and
for `s.m` look up the form of the last matching declaration, because the
declarations of an `update` statement follow those of its service (a node's
static checks resolve the patched program, in which an update has already
replaced them).  The operation and the identity of a `fold` are resolved like
any other expression.  The `where` clause of a `select` and the row of an
`insert` are not resolved yet (name resolution skips both), so a special form
named there without a call is not caught.

The rule is checked wherever static checks run: at the CLI's startup
(`Node::static_checks`) and on every code update, on the node that issues it
and on the owner that applies it (`update::Transaction::try_apply`).  The REPL
and the browser client run no static checks, so there it is not enforced, and
the owner keeps answering lookups of the member as a value.

## Where a call runs

A call has a target `(service, member)`: for `s.m(..)` the service before the
dot, for `m(..)` with `m` not bound locally the service of the code that is
running, which may be the owner's service when the owner's own function runs
on another node.  When the target's service is remote and the call is made by
running code, the call goes to the owner unless the caller's copy declares the
member plain (below).  For a call that is sent, the owner decides, from its own
current declarations, whether it runs the call:

| Call made by | With the `fn` spelling | With a special form |
|---|---|---|
| running code (an action, a `@test` or REPL statement, a code update's `var` initializer): `s1.f(3)` | fetch `f`, run it here, fetch each member it reads | one request; `s1` runs `f(3)` and returns the value (an argument that is a function value keeps today's path) |
| running code: `do s1.a(3)` | fetch `a`, apply it here, ship the action's body | one request; `s1` runs `a(3)` |
| computing a def from pushed values | run `f` here | unchanged |
| creating a service (all its initializers) | fetch | unchanged |
| a service on the same node | run locally | unchanged |

**The owner decides for calls of defs.**  The caller sends a call of a
remote member from running code, as a lookup that carries the arguments, only
when its copy of the owner declares the member as a special form or it has no
copy (the REPL and the browser have none), and no argument is a function value
(see "Arguments and results").  A copy that declares the member as a plain
def, a var or a table keeps today's plain lookup, with no arguments on the
wire, so the calls of defs in a program without special forms send exactly
what they sent before.  For a call that is sent, the owner decides: if the
member is a special form, it runs the call and says so in the reply; otherwise
it answers as for a
plain lookup, with the member's value, and the caller applies it locally,
exactly as today and at the same cost.  So a copy that is out of date does not
matter for a call that is sent, and an owner built before this change, which
ignores the arguments, also gets the caller to apply the member itself.
Arguments of a call that is sent are evaluated before the request is, so they
are now evaluated before the function's member is read; the values are the
same.

**For `do`, the caller proposes and the owner confirms.**  An action request
runs whatever statements it carries, so the caller sends `do s1.a(__arg0, ..)`
only when its copy of the owner declares `a` as a special action, and `do a`
without arguments only when the copy declares `a` as a plain def whose
initializer is an action written in place.  Such a request is marked as sent
by name.  The copy may be out of date: an update at the owner can have made
`a` a forward, such as `pub def a = app.hit;` with `app` on the waiting
caller, and running that by name would make the owner call back into the
caller, which waits until the reply timeout (#213), where the fetch path runs
`app.hit` on the caller and succeeds.  So the owner runs a by-name request only
if its own current declaration of `a` still has the right shape: an action
written in place without arguments, or a function whose body is an action
written in place (a special action, or its `fn` spelling) with them.
Otherwise it declines, before the request touches the transaction, and the
caller takes the fetch path as before calls ran on the owner, at one extra
round trip.  A member that the caller's copy shows holding another service's
action is fetched as before, without asking.  Without a copy (the REPL, the
browser), `do s1.a(3)` evaluates the call as above, so the owner returns
`a(3)`, and the `do` then runs that action at its owner: two round trips
instead of one.

**Why computing a def keeps today's path.**  Rule 5 exists to avoid round
trips (end of line 123), and computing a def from pushed values makes none: a
def's re-evaluation reads only values its owners pushed to it
(`interpreter/evaluator.rs`, `lookup_input`), and the dependency analysis
already subscribes it to the members `f` reads.  Re-evaluation is recognised
by its pushed inputs and never sends a call.

**Why creating a service keeps today's path.**  Creation reads remote
function code through its own reads to compute the new service's graphs
(`remote_functions_read`), and seeds each def's clock, its Sources and its
cached inputs from what the creation transaction read
(`manager/mod.rs`, `create_service`).  A call that ran on the owner would
hide those reads.  The gate is therefore closed for the whole creation
transaction, recognised as a transaction under which some local service is
initializing: the predicate `remote_lookup_with_clock` already uses.  It must
test the transaction, not the service in the evaluation context, because
applying a closure switches that service to the closure's own.

Running a def's calls on the owner would mean the owner keeps `f(3)` up to
date and pushes it: the "To think about" note on the Requirements page
(line 209), outside rules 1 to 5.  It needs a subscription per argument value
and a way to unsubscribe, and is left as a follow-up.  Task 12
(compute-on-read) will compute a def inside a transaction with no pushed
inputs; its calls will then go to the owner, which is consistent with task 12
reading every cross-service input from its owner under the transaction.

## Protocol

No new message type.

**Defs.**  `LookupRequest` gains `args: Option<Vec<NetValue>>`, and
`LookupResponse` gains `called: bool`; both default to absent and false, so
peers built before this change still decode them.  With arguments, an owner
whose declaration of the member is a special form evaluates
`s.m(__arg0, ..)` in its own service and replies with the result and
`called: true`:

- inside a transaction, as a participant: it reads under the shared
  transaction id, holds its read locks until `Commit` or `Abort`, and parks
  under wait-die like any transactional read (`ParkedRequest::Lookup` carries
  the arguments).  An expression cannot write or start a composed action, so
  a parked call needs no rollback before it runs again;
- outside a transaction, as a plain unlocked read.

A call carries at most `MAX_NET_CALL_ARGS` (64) arguments.  The owner counts
them before it decodes any, and only for a call of a special form: a plain
lookup never depends on its arguments.  If the result has no wire form, such
as a string longer than the wire limit or html, the owner answers with the
member instead, as for a plain lookup, and the caller applies it and builds
the value itself, as for the `fn` spelling.  That works for a long string.
For html the call still fails, as it does for the `fn` spelling, because no
html value or template can cross the wire yet.

Inside a transaction the reply's clock is what `transaction_history` gives
for the participant, the join of the clocks of everything the transaction has
read there: a node that only serves reads reaches the originator's commit
stamp only through this clock (`manager/mod.rs`, `send_lookup_request`).
Outside a transaction the clock and Sources are absent.  The caller never
stores a call's reply in the transaction's read cache, which is keyed by
member: the value is a result of `f`, not `f`.  The owner checks the number of
arguments against the function's parameters, accepting no arguments for a
single `unit` parameter as the type checker does, and fails the call
otherwise.

**Actions.**  `do s1.a(e1, e2)` with `a` a special action sends the existing
`ActionRequest` with the single statement `do s1.a(__arg0, __arg1);` and the
argument values in its environment (`net/types.rs`, `ActionRequest.env`).
The owner runs it like any action request: evaluating `a` reads its own member
under the transaction, which takes a read lock on `a`; applying it builds the
action in `s1`; running it writes `s1`'s vars.  The names `__arg0, __arg1, ..`
are valid identifiers, so the codec accepts them.  The callee is a member
access, which never consults the environment, so a member named like an
argument cannot be shadowed, and the action's own body never sees the
arguments.

**By-name requests.**  `ActionRequest` gains `by_name: bool` and
`ActionResponse` gains `declined: bool`, both defaulting to false.  The caller
sets `by_name` on `do a` and `do s1.a(__arg0, ..)` sent by name.  The owner
checks such a request against its current declaration of `a` each time it is
about to run it (`manager/mod.rs`, `by_name_request_is_runnable`): when a
standalone request arrives, and for a transaction's request on every dispatch,
including the one after it parked under wait-die, since an update may have
changed `a` meanwhile (`dispatch_parked`).  Nothing awaits between the check
and the run.  If the shape is wrong, it replies `declined: true` with nothing
run; a decline after a park can leave the locks the earlier attempt took,
which the transaction holds until it ends anyway.  The caller then fetches `a`,
applies it to the argument values it has already evaluated, and runs the
action as for a plain copy, so no argument is evaluated twice.  Inside a
transaction the declined request keeps its place in the dispatch order,
recorded as declined (`txn.rs`, `ComposedCall.declined`): a participant that
parks and runs its action again gets the same decline from the record without
sending it, and the fallback keeps the place after it.  A call of the right
shape whose number of arguments does not fit the declaration's parameters is
neither run nor declined: the owner refuses it with the error a def call gets
(`'s1.a' takes 2 arguments, but the call has 1`).  A decline would not help,
because the fallback applies the member to the same arguments, and applying a
function pairs parameters with arguments without checking their number
(`interpreter/evaluator.rs`, `apply_function`).  An owner built before this
change ignores `by_name` and runs the request as before, and a caller built
before it never sets it.

## Transactions

- **Participants.**  The caller registers the owner as a participant before
  sending, as lookups and actions do, so an abort reaches it even if the reply
  is lost.  Calls the owner makes on further services make those services its
  own participants, and commit and abort follow the chain as they do today.
- **Locks.**  The owner takes a read lock on the special form itself, because
  evaluating the call reads the member, and on whatever the body reads; an
  action also takes write locks on what it writes.  The lock on the member
  keeps a concurrent code update from replacing it during the transaction,
  the same reason task 12 locks a def it recomputes.
- **Replay.**  A call of a special action is a composed action: it claims a
  position and a fingerprint (statements and environment, so the arguments
  count), and a replayed run neither repeats it nor sends a different one
  (`remote_action`).  A call of a def is a read and is sent again on replay,
  like a lookup.
- **Errors.**  A failed call fails the caller's transaction like a failed read
  or action.  Wait-die outcomes keep their meaning across the wire
  (`remote_error`).

## Arguments and results

- Arguments are evaluated on the caller, left to right, before the request is
  sent; results come back as values.
- A function value among the arguments of a def call keeps the call on the
  caller: the caller sends the plain lookup and applies the member itself, as
  for the `fn` spelling (`manager/mod.rs`, `remote_call`).  A closure carries
  its service only as a name (`net/codec.rs`), so on the owner a read of one
  of the caller's members would fail, or read the owner's own service if one
  has the same name.  This also keeps on the caller a closure that reads only
  its parameters and captured values; telling the two apart would need the
  closure's free names.  An action value carries its owner's network identity
  and runs there, so it is sent.
- An action always runs on its owner, so a closure passed to `do s1.a(..)` is
  applied there with either spelling: the by-name request carries it as an
  argument, and the fetch path ships it in the action's environment.  A read
  of one of the caller's members then fails, or reads the wrong service, as
  for any closure shipped in an action's environment today.  Fixing that needs
  closures that carry a network identity.

## Code updates

An update may add a special form, replace one, or turn a plain def into a
special form and back: an update replaces a declaration whole.  The static
checks run again on the patched program, so switching a member to a special
form is rejected while any service on that node still uses it as a value.
Other nodes keep the copy they imported at startup for as long as they run:
code updates made at the owner or at another node do not reach them (a node's
own updates to a remote service patch its copy too), and the served file is not
rewritten.  For a call of a def that is sent, the owner decides, so the copy
does not matter; a copy that declares the member plain keeps the plain lookup
even after the owner turns it into a special form, which is the same result at
the old cost.  For `do`, an out-of-date copy gives the same result too: if the
owner has since turned `a` into a member that holds another service's action,
the owner declines the by-name request and the caller fetches `a`, as before
calls ran on the owner.  A changed parameter list is different: the owner
refuses a call whose number of arguments does not fit, a def call and a `do`
sent by name alike, so the caller gets an error (see "Limits").

The text an update forwards to a remote owner (`update.rs`,
`format_update_source`) prints a special form as it was declared:
`[pub ]def NAME(P: T, ..)[: R] = BODY;` and
`[pub ]action NAME(P: T, ..) { S; .. }`, with parameter names taken from the
interner.  The bodies go through the expression and statement printers that
plain updates use (`format_expr`, `format_action_stmt`).  Those printers did
not always produce text that parses back to the same program, which this
work fixes because the new forms rely on them: an `assert` is printed with
its parentheses and with its condition as the user wrote it (the statement
keeps that text, which the owner reports when the assertion fails and which
is within the parser's length limit for an assertion), a lambda prints as
`fn (x: T) => e`, and every compound operand is parenthesised, so
`(x + 1) * 2` no longer comes back as `x + 1 * 2`.
Tuples, `select`, `fold`, action literals, `for` and `insert` statements and
table declarations print in their grammar form too, with a field's value and
a `fold`'s identity parenthesised when they are compound, because the grammar
reads those two at a tighter level.  The REPL labels a `watch` with the same
printer (`meerkat/src/repl.rs`), so its labels show these parentheses too:
`watch s1.x + 1 > 2;` is labelled `(s1.x + 1) > 2`.  An html template still
has no source form, so an update that contains one cannot be forwarded.
`tests/format_update_test.rs` parses, prints and parses again a corpus of
declarations and compares the trees, and runs printed programs to check that
they compute the same values.

## How this fits Meerkat

- **Programmer control.**  The wiki prefers explicit control to an optimizer
  in the short term (line 113).  The author of a service decides, per member,
  where calls of it run, and the declaration shows it.
- **Equivalence.**  A special form computes what its `fn` spelling computes,
  apart from the timeouts and the out-of-date copies under "Limits"; only where
  the work happens differs.  Modelling the forms as that member plus a mark is
  what keeps the change small: the type system, the dependency analysis,
  propagation, glitch freedom and live updates need no change.
- **The owner's code is the authority.**  For calls of defs that are sent, the
  owner's current declaration decides, so callers without the owner's source
  behave correctly; a copy that declares the member plain keeps today's plain
  lookup.  For a `do` sent by name the owner confirms or declines, so an
  out-of-date copy cannot make it run a forward.
- **Transactions.**  A call joins the caller's transaction with the same
  locks, wait-die parking, replay protection and commit stamp as the reads
  and actions it replaces.  Serializability is unchanged.
- **Rule 3.**  Actions already run where they are defined; special actions
  only skip the download of their body, and now take arguments.
- **Protocol.**  Reusing `ActionRequest` and `LookupRequest` keeps the wire
  format and its validation as they are, apart from four defaulted fields and
  one check: a call carries at most `MAX_NET_CALL_ARGS` (64) arguments,
  counted before any is decoded, and only for a call of a special form.
- **Cost model.**  A call of a special def costs one round trip however many
  members its body reads.

## Limits

- **A call back to a waiting node waits for the reply timeout.**  A node
  waiting for a reply puts incoming requests aside until its own request
  finishes (`manager/mod.rs`, `dispatch_events`), so A calling B, which calls
  A, times out.  This is #213.  It is why a `do` sent by name runs only for
  actions written in place at the owner, and why the owner checks that
  against its current declaration.  A special def whose body reads a member
  of the waiting caller's node still meets it; its `fn` spelling reads that
  member on the caller.
- Computing a def and creating a service keep today's path (above).
- A local captured by a closure or by an action written in place is dropped
  when it has the name of a member of its service, and the body then reads
  the member (#86, as on `main`).  When that member is a special form, it is
  read as a value: the call-only rule is checked on names, and there the name
  is the local's.
- A caller whose copy of the owner predates a change to a special form's
  parameter list sends the old number of arguments.  The owner refuses the
  call with an argument-count error, a def call and a `do` sent by name
  alike.  The `fn` spelling has no such check: there a missing argument fails
  as an unknown name, and an extra one is ignored and the call succeeds, so
  in that case the results differ.
- **A stale copy costs a round trip.**  A caller keeps the copy it imported
  at startup, apart from its own updates.  If the owner has since turned `a`
  into a member that holds another service's action, the owner declines a
  by-name `do s1.a(..)` and the caller fetches `a`: the old path plus one
  round trip (see "Protocol").
- An owner built before this change ignores `by_name` and runs a by-name
  request as it runs any request, so it cannot decline.  With a caller whose
  copy is out of date, it then runs a forward to the caller's own service
  and waits on the caller until the reply timeout (#213), where a caller
  built before this change fetched the member and ran the action itself.
- An argument with no wire form (html) cannot be sent.  For `do s1.a(..)`,
  the caller then fetches `a` and ships what its action captures, as before,
  so it still works when the action does not read that argument.  For a call
  of a def, `s1.f(..)` fetches `f` and runs it on the caller as before: one
  round trip plus one per member its body reads.
- A closure passed to an action is applied on the owner with either spelling,
  where a read of one of the caller's members fails or reads a service of the
  same name (see "Arguments and results").  This predates this work.  A def
  call with a function value among its arguments keeps today's path, at
  today's cost.
- A special def whose result has no wire form is answered with the member,
  as for a plain lookup, after the owner has run the body: the caller then
  builds the value itself, which works for a string longer than the wire
  limit.  For html the call still fails, as for the `fn` spelling, because no
  html value or template can cross the wire yet.
- A special def whose body reads a third node that parks that read under
  wait-die for longer than the reply timeout makes the caller time out,
  because the owner does not keep the caller's request alive while it waits.
  The `fn` spelling waits on the third node directly.
- In the REPL and the browser, `do s1.a(3)` takes two round trips, because
  they hold no copy of the owner to tell that `a` is a special action.
- **Browser handlers.**  The browser runs every handler as a remote action
  and ignores a handler that is not an action (`meerkat-wasm`,
  `attach_handlers`), so a special action cannot be a handler yet, neither as
  `onclick={s1.reset}` nor through a wrapper action.  Handlers keep using
  plain action members.
- Until task 12 lands, a special form's body that reads a def inside a
  transaction sees the def's value from before the transaction, as every def
  read does today.
- Not changed: `pub` is not enforced, and the owner still runs any statements
  an `ActionRequest` carries.

## Tests

Each claim of "What done means" is checked by the tests below.  Names are
those of the functions.  `sf` is `meerkat-lib/tests/special_forms_test.rs`,
`el` is `meerkat-lib/tests/execution_location_test.rs`, `ibn` is
`meerkat-lib/tests/invoke_by_name_test.rs`, `mgr` is the unit tests of
`meerkat-lib/src/runtime/manager/mod.rs`, `fu` is
`meerkat-lib/tests/format_update_test.rs`, `nri` is
`meerkat-lib/tests/nameres_integration_test.rs`, and `mkn` is a multi-node
scenario of `scripts/test_mkn.py`, run on real processes.

| # | Claim | Tests |
|---|---|---|
| 1 | Declared at the top level of a service, parameter list possibly empty | sf: `test_parse_def_forms` (including `def now() = y;`), `test_parse_action_forms` (including `action reset()`), `test_parse_rejects_bare_parameter_name`, `test_parse_desugars_to_the_fn_member`, `test_parameter_named_like_member_is_rejected`, `test_forwarded_update_text_round_trips` (special forms declared in an update keep their marks); nri: `test_update_statement_checks_special_form_parameters` (the resolver's own `update` arm, which a node's static checks do not reach) |
| 2 | Usable only with a call | sf: `test_special_form_without_call_is_rejected`, `test_bare_special_form_as_value_is_rejected`, `test_special_form_calls_are_accepted`, `test_local_shadows_special_form`, `test_update_to_special_form_is_rejected_while_used_as_value`, `test_update_replaces_a_special_form_and_turns_it_back_into_a_plain_def`; the operation and identity of a `fold`: `test_special_form_in_fold_is_rejected` |
| 3 | A call by running code runs at the owner, with no lookup of the member or of what it reads | el: `a_call_in_an_action_is_one_request_with_the_arguments`, `a_call_in_a_test_statement_carries_no_transaction`, `do_of_a_special_action_is_one_action_request`, `a_zero_parameter_action_sends_an_empty_environment`, `the_bare_name_in_a_test_block_is_sent_by_name`, `a_special_def_returning_an_action_goes_through_the_owner`, `a_member_reply_is_applied_by_the_caller` and `an_argument_with_no_wire_form_falls_back_to_a_lookup` (the owner or the arguments do not allow a call), `a_copy_that_declares_the_member_plain_sends_a_plain_lookup` (the gate keeps the plain path for a plain copy), `a_caller_with_no_copy_sends_the_call_with_its_arguments`, `a_special_action_named_like_an_argument_reaches_the_member` (the by-name callee is a member access), `an_html_argument_takes_the_fetch_path`; the scope, computing a def and creating a service: `recomputing_a_def_sends_no_request`, `creating_a_service_sends_plain_lookups`; ibn, the zero-argument precursor (`do s.a` of a plain action-literal member is sent by name): `do_on_a_remote_member_sends_no_lookup`, and where the old path is kept: `do_on_a_bound_remote_member_keeps_the_old_path`, `do_on_a_forwarding_member_keeps_the_old_path`; the owner's side, mgr: `a_call_of_a_special_def_under_a_transaction_is_called`, `a_call_outside_a_transaction_has_no_clock`, `anything_else_is_a_plain_lookup`, `the_owner_checks_the_number_of_arguments`, `the_owner_refuses_bad_arguments_only_for_a_call`, `a_parked_call_keeps_its_arguments`, `the_reply_of_a_call_outside_a_transaction_has_no_clock`, `a_call_of_a_special_action_is_called`, `a_result_with_no_wire_form_is_answered_with_the_member`, `a_plain_member_with_arguments_under_a_transaction_is_a_lookup`, `member_decl_takes_the_last_declaration`, `member_decl_follows_a_code_update`; a by-name `do` the owner declines because the caller's copy is out of date: el: `a_stale_copy_of_an_action_that_became_a_forward_is_fetched`, `a_stale_copy_of_a_special_action_that_became_a_forward_is_fetched`; mgr: `a_by_name_request_for_a_forward_is_declined_and_changes_nothing`, `a_by_name_request_for_an_action_written_in_place_runs`, `a_request_not_by_name_for_a_forward_still_runs`, `by_name_call_requests_run_only_for_actions_written_in_place`, `a_parked_by_name_request_is_checked_when_it_is_dispatched`, `a_replayed_declined_probe_and_its_fallback_are_answered_from_the_records`; el: `a_parked_action_replays_a_declined_by_name_call`, `a_declined_by_name_call_does_not_evaluate_its_arguments_twice`; a by-name `do` whose number of arguments does not fit is refused with an error: el: `a_stale_copy_missing_an_argument_gets_the_argument_count_error`, `a_stale_copy_with_an_extra_argument_gets_the_argument_count_error`; mgr: `a_by_name_call_with_the_wrong_number_of_arguments_is_an_error`; a REPL statement, run as the REPL runs it (`execute`, no service, no transaction, no copy of the owner), against a real owner: el: `a_repl_statement_calls_a_special_def_on_the_owner`, `a_repl_do_of_a_special_action_takes_two_round_trips`; a code update's `var` initializer: el: `a_var_added_by_a_code_update_calls_the_owner`; a special def whose body reads a third node inside a transaction, where the commit and the abort reach that node through the owner: el: `a_special_def_that_reads_a_third_node_commits_through_the_owner`, `a_special_def_that_reads_a_third_node_aborts_through_the_owner` |
| 4 | Execution returns to the caller, with the result of a def | el: `a_call_against_a_real_owner_is_one_round_trip` (the caller stores the result and commits), `the_wiki_example_against_a_real_owner_takes_two_requests`; mkn: `location_special_forms` (the caller passes the result of `f` on to `a`, on real processes) |
| 5 | Same results as the `fn` spelling | sf: `test_special_forms_run_like_their_fn_spelling`, `test_special_forms_have_the_type_of_their_fn_spelling`, `test_special_forms_are_type_checked`; el: `an_owner_with_the_fn_spelling_gives_the_same_result`, and for a closure argument, which keeps the call on the caller, `a_closure_argument_is_applied_on_the_caller`, `a_closure_argument_does_not_read_a_service_of_the_same_name_on_the_owner` and `a_closure_argument_gives_the_fn_spelling_result`; mkn: `location_special_forms` and `location_fn_spelling`, which end in the same state (`s1.y == 40`); forwarded updates print text that parses back to the same declarations and computes the same values: fu: `test_forwarded_text_round_trips`, `test_printed_text_of_known_cases`, `test_forwarded_text_keeps_the_meaning`, `test_forwarded_action_runs`, and an `assert` as long as the parser allows: `test_an_assert_at_the_length_limit_round_trips`; a forwarded update with special forms applied by a real owner, which then runs calls of them: el: `a_forwarded_update_with_special_forms_is_applied_and_called_on_the_owner` |
| 6 | Client code may be the same for both | el: `an_owner_with_the_fn_spelling_gives_the_same_result` (one caller program, both owners); mkn: `location_special_forms` and `location_fn_spelling` use the one file `scripts/tests/integration/location/client.mkt` |
| 7 | `s1.a(s1.f(3))` takes two round trips instead of four | el: `the_wiki_example_takes_two_requests`, `the_wiki_example_against_a_real_owner_takes_two_requests`, `the_fn_spelling_takes_four_requests` |

Not covered by a test: the REPL tests run a statement the way the REPL does,
on an in-process node, not through the REPL program itself, and the browser
is not tested.  The multi-node scenarios check the end state, not the number
of requests, which the in-process tests count.

## Open questions

- **Calls inside defs.**  Should the owner maintain a def's calls (above)?
- **`pub`.**  Should only `pub` special forms be callable from another
  service?  This would be the first enforcement of `pub`.
- **Handlers.**  Should the browser run a zero-parameter special action as a
  handler, which needs `attach_handlers` to apply it first?
- **Source for the REPL and the browser.**  Should their `import` fetch and
  keep the owner's source, so that `do` takes one round trip there too?
