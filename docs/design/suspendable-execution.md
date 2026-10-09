# Design for suspendable execution

## Motivation

Two goals for Meerkat's runtime cannot be met by the current execution model.

**Wait-die should never kill an older transaction under contention.**  The
point of wait-die is that an older transaction waits for a younger one rather
than dying, so the oldest transaction in the system always makes progress.  If
an older transaction can fail because it waited too long, the scheme loses
that guarantee, and repeated failures and retries raise the possibility of
livelock.

**A node should stay responsive while its transaction runs elsewhere.**  When
an originator starts a transaction and the transaction moves on to execute an
action on another node, the originating node should suspend that execution and
get on with other work: receiving updates from the network, and handling input
from the REPL or a GUI.  Submitting a long-running request to a server should
not freeze the user interface.

Issue #28 asks for a related but smaller thing: a node that listens for input
and network events at the same time.  This document argues that the two goals
above need more than that, namely execution that can be suspended and resumed.
It then proposes an order for that work, for #28, and for task 10 (parked
action replay safety, PR #209), which is in review now.

## How execution works today

A node has one `Manager`, and a running action holds `&mut Manager` for as long
as it runs.  The interpreter reaches it through `EvalContext.manager`, and
every `await` inside an action, including the ones that wait for the network,
happens with that borrow held.  Nothing else can touch the `Manager` until the
action finishes, so nothing else can run.

The runtime works around this in three places.

**Remote calls process events inline.**  `send_and_await_reply` sends a
request, then loops calling `dispatch_network_events` until the reply arrives.
The node therefore keeps handling network messages during a remote call, but
only from inside that call, and it never reads user input.

**Participants park and replay.**  A participant is a node running part of
someone else's transaction.  When it must wait for a lock, the action returns
`EvalError::WaitOn`, the request is parked on `Manager::wait_queue`, and the
call stack unwinds.  When the lock frees, task 09's `wake_freed` re-dispatches
the request from its first statement.  Everything the first run did happens
again, which is the defect task 10 fixes: #209 restores the local buffers when
the action parks, and records each composed action (`do svc.action` on another
node) so the replay skips it rather than sending it twice.

**Originators retry.**  The originator's transaction is a local variable in
`execute_action_with_txn`, called directly from the CLI or the REPL.  It has no
event loop to return to, so it cannot park.  On `main`, a `WaitOn` fails the
action outright.  Task 06 (#202) turns it into a retry with backoff, capped at
`MAX_WAIT_DIE_RETRIES`, so an older transaction gives up after about 200ms.

## Why these fall short

The retry cap works against the first goal by design: it is a timeout on
waiting, applied to exactly the transactions that wait-die says should wait.

The inline event loop works against the second goal.  It handles the network
but not input, and only while a remote call is in flight.

The obvious way to make a node responsive is to wait in place, the way
`send_and_await_reply` does, handling other requests while the action waits.
That breaks wait-die's freedom from deadlock, because waits then nest on the
call stack and can only finish in the reverse order they started:

1. T1 waits in place for a lock held by a younger transaction.
2. While T1 waits, a request from an older T0 arrives.  T0 needs a lock that
   T1 holds, so under wait-die T0 waits, also in place, above T1 on the stack.
3. The younger transaction commits and frees T1's lock.  But T1 cannot resume
   until T0's frame above it returns.
4. T0 is waiting for T1.  Neither can move.

This cycle is not in the wait-for graph that wait-die reasons about; the call
stack creates it.  Parking exists to avoid it, by unwinding the stack instead
of waiting on it.

## What suspension requires

The first goal does not strictly need suspension.  An older transaction only
has to wait without failing, and parking plus replay already does that for
participants.  Originators can use the same mechanism once there is an event
loop for them to park in.

The second goal does need it.  In principle replay could do this too: park at
every remote call, and re-run the action from the start when each reply
arrives, skipping recorded results.  But that means recording the value of
every remote read, re-running the action once per remote call, and making
replay the core execution model.  It is not a direction worth taking.

Suspension of any kind requires that a suspended action not hold `&mut
Manager`.  There are two ways to get there.

**Share the `Manager` and borrow it briefly.**  The interpreter keeps its
current recursive `async` structure, but holds a handle to a shared `Manager`
(`Arc<Mutex<_>>`, or `Rc<RefCell<_>>` on wasm) and borrows it only between
awaits.  A suspended action is then an ordinary future, waiting on a channel.

**Make the interpreter a state machine.**  The interpreter becomes a step
function over an explicit stack of frames, and a suspended action's
continuation is data in a table.  This is closer to #28's description of a
table of continuations, but it means rewriting the evaluator and executor.

We propose the first.  The interpreter's contact with the `Manager` is narrow:
`lookup`, `assign`, a few reads of `services`, the service name and id
conversions, and `reactive_cache`.  Changing that part is mostly mechanical.

The real work is in the `Manager` methods that await while holding `&mut self`:
`send_and_await_reply`, remote `lookup`, `remote_action`,
`commit_participants`, and `recompute_def`.  Each has to become: borrow, change
state, send, release the borrow, await the reply, and borrow again.

State that assumes one action runs at a time also has to change:

- `reactive_cache` is a single field on the `Manager`, saved and restored
  around nested recomputes.  Interleaved actions would overwrite each other's
  caches, so it has to move into each execution's own context.
- `execute_action_participant` takes its transaction out of `pending_txns`
  while it runs.  An `Abort` that arrives while the action is suspended would
  not find it.  The transaction has to stay addressable, with the running
  action told about the abort.
- `wait_queue` holds `ParkedRequest`s to re-dispatch.  It would hold wakers for
  suspended actions instead, woken oldest first as `wake_freed` does now.
- Suspended actions get dropped when their transaction aborts.  The comment in
  `recompute_def` already notes that its cache restore is not
  cancellation-safe; that becomes a normal path to handle.

## Sequencing

Three pieces of work are involved:

- **Fixing task 10**, so a parked action does not repeat its effects when it
  replays.
- **Making execution suspendable**, as described above.
- **Listening to input and the network together**, which is #28 as written.

The dependencies run one way.  Suspension needs something to switch to while
an action is suspended: a loop that owns the dispatch of events, which is what
#28 builds.  And #28 is useful without suspension: it lets the REPL or GUI and
the network share a node, and it gives an originator somewhere to park.

We propose this order:

1. **Merge #209 (task 10).**  Parked participant actions replay correctly,
   and wait-die holds for participants.
2. **Tasks 06, 11 and 12.**  Compute-on-read lands, and
   `meerkat/tests/s1.mkt` passes.
3. **#28: one event loop.**  Input and network are handled together.
   Originators park instead of retrying, so wait-die holds for them too, and
   #202's wait retry goes away.
4. **Suspendable execution.**  Nodes stay responsive during remote calls, and
   a park becomes a wait on a lock grant.  #209's replay and task 09's
   re-dispatch go away.

**Step 1: merge #209, not a simpler alternative.**  A simpler fix was
considered: when a parked action has already sent a composed action, abort the
transaction instead of parking it, and let the originator retry.  It is about
15 lines instead of 200, but it makes an older transaction fail under
contention, which is what the first goal rules out.  #209 keeps wait-die's
guarantee, and its changes are self-contained, so step 4 can delete them
cleanly.

**Step 2: do not hold compute-on-read for the refactor.**  Task 12 extends
#209's replay path a little (`ComposedCall` gains `touched_services`), and that
code will later be deleted.  The cost is small, and `s1.mkt` stays broken until
task 12 lands.  #202's retry cap is a temporary conflict with the first goal.
One option is to remove the cap for the wait case and keep retrying with
backoff: a retry keeps the transaction's timestamp, so the oldest transaction
still makes progress.

**Step 3: build the loop so that step 4 fits into it.**  Two choices matter:

- The loop should deliver network replies to the existing `pending_replies`
  channels, instead of `send_and_await_reply` processing events itself.  Those
  channels already are a table of continuations.
- Originator transactions should live in a table while they wait, keyed by id,
  like `pending_txns`.  That lets an originator park and replay the way a
  participant does.  Task 12 already needs the originator's transaction to be
  reachable by id while it runs, and it names #28 as the fix.

Either design in #28 works for this, as long as the loop owns event dispatch:
the `tokio::select!` loop that owns the `Manager`, or the actor design.  With
the `select!` loop, step 4 changes the loop's handlers from running an action
to completion to spawning it; the loop itself stays.

**Step 4: make execution suspendable.**  A park becomes a wait on a lock-grant
channel, and a remote call becomes a wait on its reply channel with the
`Manager` released.  #209's rollback and position records, and task 09's
re-dispatch, go away.  This step is tracked in #213.

## Cleanup

Parking, replay and retrying are workarounds for execution that cannot be
suspended.  Each step above makes some of them unnecessary, and removing them
is part of that step, not follow-up work.  Otherwise the runtime ends up with
two mechanisms for the same thing, and the old one keeps constraining changes
to the new.

**After step 3**, when there is one event loop and originators park:

- #202's wait retry, which the originator no longer needs.  Its die retry
  stays: that is wait-die itself.
- `send_and_await_reply` processing network events itself.
- The separate places that handle network messages outside the loop: the
  `--watch` loop and the server loop in `main.rs`, and the REPL's lack of
  any.
- Task 12's guard that makes a read of an originator's in-flight transaction
  fail, since that transaction becomes reachable by id.

**After step 4**, when execution is suspendable:

- #209's replay machinery: the rollback of buffers on a park, the records of
  completed composed actions, and the checks that a replay matches them, along
  with their tests.  Task 12's additions to it go too.
- Task 09's re-dispatch of parked requests, which becomes waking suspended
  actions.
- `reactive_cache` as a field on the `Manager`, which moves into each
  execution's context.

The itemised checklists, with the names of functions, constants and tests, are
on #28 for step 3 and on #213 for step 4.  Code that is scaffolding for a later
step should say so in a comment naming the issue that removes it, as #202's
comments on `MAX_WAIT_DIE_RETRIES` already do.

## Open questions

- **Shared `Manager` or state machine.**  This document recommends the shared
  `Manager`; the state machine deserves a closer look if a table of
  continuations turns out to be needed for other reasons, such as persisting
  them.
- **Abort of a suspended action.**  What the action is told, and how its
  partial state is discarded.
- **Results that arrive later.**  Once a REPL command or a GUI action can
  finish after other input has been handled, the interface needs a way to
  report its result asynchronously.
- **Fairness of wakeups.**  Waking oldest first matches wait-die; it should be
  checked that nothing else, such as the order the event loop handles
  messages, undoes that.
