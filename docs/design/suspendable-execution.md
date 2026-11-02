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
action replay safety, PR #209, since merged).

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
event loop to return to, so it cannot park.  Before task 06 (#202), a `WaitOn`
failed the action outright.  #202 turns it into a retry with backoff, capped at
`MAX_WAIT_DIE_RETRIES`, so an older transaction gives up after about 200ms of
backoff.

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
participants.  Originators can park too once there is an event loop for them
to park in, though they restart rather than replay (see step 3).

The second goal does need it.  In principle replay could do this too: park at
every remote call, and re-run the action from the start when each reply
arrives, skipping recorded results.  But that means recording the value of
every remote read, re-running the action once per remote call, and making
replay the core execution model.  It is not a direction worth taking.

Suspension of any kind requires that a suspended action not hold `&mut
Manager`.  There are two ways to get there.

**Share the `Manager` and borrow it briefly.**  The interpreter keeps its
current recursive `async` structure, but holds a handle to a shared `Manager`
and borrows it only between awaits (see "How actions share the `Manager`"
below).  A suspended action is then an ordinary future, waiting on a channel.

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
state, send, release the borrow, await the reply, and borrow again.  With the
handle described next, that is the only structure that compiles.

### How actions share the `Manager`

**Run on one thread.**  Meerkat's logic runs on tokio's single-threaded
runtime inside a `tokio::task::LocalSet`, and suspended actions are spawned
with `spawn_local`.  Tasks spawned that way need not be `Send`, so the shared
`Manager` is an `Rc<RefCell<Manager>>` on native and wasm alike.

**Reach the `Manager` only through a closure.**  The `Rc<RefCell<_>>` is a
private field of a `ManagerHandle`, whose only access is synchronous:

```rust
impl ManagerHandle {
    pub fn with<R>(&mut self, f: impl FnOnce(&mut Manager) -> R) -> R {
        f(&mut self.0.borrow_mut())
    }
}

// in an action: borrow, release, await, borrow again
let reply = handle.with(|m| m.start_lookup(…));
let value = reply.await;
handle.with(|m| m.finish_lookup(value));
```

The compiler then rules out holding the `Manager` across an `await`.  The
closure is not `async`, so it cannot contain an `.await`, and its result
cannot borrow from its argument, so the `&mut Manager` cannot escape it,
not even inside a returned future.  Because `with` takes `&mut self`, a closure
cannot call `with` on the same handle again; each task gets exactly one
handle.  A nested borrow through a second handle would still panic, but it
panics every time that path runs, so any test of the path finds it.  Holding a
borrow across an `await` is the dangerous mistake: it only fails when another
task happens to run in the gap, and the handle makes it impossible.

Clippy's `await_holding_refcell_ref` lint, on by default and therefore an error
under CI's `-D warnings`, backs this up inside the handle's own module.  It
is not enough by itself: it sees a `Ref` or `RefMut` held directly in a
variable across an `await`, but not one wrapped in another type.

**What this buys.**  This is what meets the second goal: a node keeps
handling network messages and user input while its actions wait for replies.
Once step 4 is in place, a remote call goes like this:

1. The action sends its request and gets back the receiving end of a reply
   channel, `let reply = handle.with(|m| m.start_lookup(…));`.  The borrow
   ends with the closure.
2. The action awaits the reply.  As a local task, awaiting hands the thread
   back to the scheduler.
3. The event loop, another task on the same `LocalSet`, runs meanwhile.  It
   takes the next event from its channel (a request from another node, a REPL
   line, a GUI action) and handles it through its own `with` call.  Nothing
   blocks it, because no action holds the `Manager` while it waits.
4. When the reply arrives, the network layer completes the action's reply
   channel (see step 3 below), and the action resumes on its next turn on the
   thread.

A request from another node that starts an action, such as a participant
running part of a transaction, is spawned the same way, so it too can wait
without blocking anything.  Responsiveness comes at `await` points: on one
thread, a long stretch of pure computation, such as a large `fold` with no
network access, holds the thread until it finishes.  That is inherent to
cooperative scheduling, and sharing the `Manager` across threads would not
avoid it, since other tasks would still wait for the `Manager`.  If it ever
matters, the interpreter can yield periodically with
`tokio::task::yield_now()`.

**Why.**  The design removes the `Send` bounds that `#[async_recursion]` puts
on `eval` and `execute`, the `SendWrapper`s that wasm needs today to pass
browser timers through those bounds (`manager/mod.rs`), and most of the
`#[cfg]` splits for spawning.  Native and wasm then run the same code, so
native tests exercise wasm's paths too.  A single thread rules out data races.
It does not rule out interleaving: another action can run at every `await`,
so an action must re-check anything it relied on before an `await`.  That
holds for any design that releases the `Manager` while waiting.

**Cost.**  Meerkat's logic gets no parallelism, but it has none today: every
action holds `&mut Manager`.  What the multi-threaded runtime does
parallelise is libp2p's own work: connections, encryption and stream sends.
On one thread that competes with interpretation.  For Meerkat's small
messages this should not matter.  If it ever does, run the `LocalSet` with the
`Manager` on a dedicated thread and keep the multi-threaded runtime for
libp2p.  libp2p's tasks are already `Send`, and `.with_tokio()` spawns them
with `tokio::spawn`, which should work unchanged on a single-threaded runtime.

**Alternatives considered.**

- **`Arc<Mutex<Manager>>` on the multi-threaded runtime**, with
  `Rc<RefCell<_>>` on wasm, as this page first proposed.  It keeps the `Send`
  bounds and two code paths.  A `std::sync::Mutex` held across an `await`
  blocks the thread or deadlocks.  A `tokio::sync::Mutex` may legitimately be
  held across an `await`, so nothing enforces releasing it, and an action
  holding it would stall every other action.
- **A bare `RefCell` borrowed by convention, with the Clippy lint as the only
  check.**  The lint misses a borrow wrapped in another type, and a missed one
  fails only when tasks interleave.
- **The state machine** ("exterior" rather than interior mutability): suspended
  actions are data, and the loop lends `&mut Manager` to each step, so no
  `RefCell` is needed.  It costs a rewrite of the evaluator and executor as
  step functions; see Open questions.

The single-threaded runtime and the event-loop shape in step 3 were suggested
by Dustin Juliano; see the discussion on #28 and #213.

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
2. **Tasks 06, 11 and 12.**  Compute-on-read lands, so an action that writes
   a var and then reads a def over it sees the new value.
3. **#28: one event loop.**  Input and network are handled together.
   An originator that must wait parks until the lock frees and then
   restarts, instead of retrying on a timer, so wait-die holds for
   originators too, and #202's retry cap no longer applies to waits.
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
code will later be deleted.  The cost is small, and until task 12 lands, an
action that writes a var and then reads a def over it sees the def's old
value: with `def y = x + 1`, `action { x = 5; assert(y == 6); }` fails.
(`meerkat/tests/s1.mkt`, the regression that opened PR #189, no longer shows
this.  Since PR #201 each `do` in an `@test` is its own transaction, so its
writes commit between statements and it passes.)  #202's retry cap is a
temporary conflict with the first goal, but it should stay until step 3.
Without it, a wait can hang the node.  The lock may be held by a participant
transaction on this node, which releases it only when its originator's
`Commit` or `Abort` arrives, and an originator in the retry loop may process
no network messages at all: with no participants to abort, it awaits nothing
but its backoff.  #202's comment on `retry_backoff` notes this case.  The cap
turns the hang into a failure after about 200ms of backoff.  Processing
messages during the backoff would avoid the hang, but that is waiting in
place, which can deadlock as shown above.  Step 3 removes the cap for waits,
because a parked originator leaves the loop free to process the release.

**Step 3: build the loop so that step 4 fits into it.**  Four choices matter:

- The loop is one channel and a `match`.  Every source of events (the
  network actor, a thread reading stdin, timers, later the GUI) forwards into
  a single channel, and the loop is
  `while let Some(event) = rx.recv().await { match event { … } }`.  Nothing
  is raced, so nothing is cancelled; events are handled in the order they
  arrived; the `match` over one `Event` enum is exhaustive; and tests can drive
  the loop by pushing events into the channel.  Within step 3 an action still
  runs to completion in the loop's body; in step 4 the loop spawns it with
  `spawn_local` instead.

  Alternatives considered: a `tokio::select!` over the sources, as #28
  suggests, works only if every raced future is safe to drop, since the
  branches that lose are cancelled mid-flight, and it is easy to break that
  rule later; one channel cannot break it.  Its advantage, preferring one
  source over another, can be had with a second channel if ever needed.  An
  actor framework (#28's other suggestion) gives the same shape with more
  machinery than this needs.
- Network replies bypass the loop.  The network actor delivers each reply
  directly to its request's `pending_replies` channel, instead of
  `send_and_await_reply` processing events itself.  Those channels already
  are a table of continuations, but today `pending_replies` is a field of the
  `Manager`, which the network actor cannot reach, so the table moves into the
  network actor, or into a small registry shared with it; requests register
  there.  A reply must never queue behind the loop:
  until step 4 an action runs to completion inside the loop, and an action
  waiting for a reply that sits in the loop's own queue would wait forever.
- Originator transactions should live in a table while they run, keyed by id,
  like `pending_txns`.  Task 12 already needs the originator's transaction to
  be reachable by id while it runs, and it names #28 as the fix.
- An originator that must wait should park and restart, not replay.  On
  `WaitOn` it does what #202's retry does: it aborts its participants,
  releases its locks and discards its buffers.  Then, instead of sleeping and
  trying again, it checks the contended key once more and, with no `await` in
  between, either parks on `wait_queue` under it or, if the key freed during
  the cleanup, restarts at once.  The cleanup awaits the participants' abort
  acknowledgements, so the holder can release the key in that window, and
  `release_locks` records a freed key only when something is already parked
  on it: parking after that release would never be woken.  Either way, the
  restart runs the action again from its first statement as a new attempt
  (`TxnId::retry`) with the same timestamp.  A wait counts against no retry
  cap, so an older transaction never fails because it waited.

  Replaying the way a participant does would need more than the table.  A
  waiting originator may already have sent composed actions, whose
  participants hold its writes buffered under the transaction id, and a
  replay would send them again.  Preventing that means carrying #209's
  rollback and position records over to `execute_action_with_txn`, only for
  step 4 to delete them.  Restarting needs none of that, because aborting the
  participants discards what they hold.

  Restarting has two costs.  The new attempt redoes the work of the old one.
  And a parked originator holds no locks, so a younger transaction can take
  one it gave up and make it wait again.  It keeps its timestamp, so it
  still never dies, and step 4 removes both costs: a suspended originator
  keeps its locks and resumes where it stopped.

The loop itself stays the same in step 4; only its handlers change, from
running an action to completion to spawning it.

**Step 4: make execution suspendable.**  The runtime becomes the
single-threaded one described in "How actions share the `Manager`", actions
run as `spawn_local` tasks holding a `ManagerHandle`, and the `Manager`'s
awaiting methods are split around their awaits.  A park becomes a wait on a
lock-grant channel, and a remote call becomes a wait on its reply channel with
the `Manager` released.  #209's rollback and position records, and task 09's
re-dispatch, go away.  This step is tracked in #213.

## Cleanup

Parking, replay and retrying are workarounds for execution that cannot be
suspended.  Each step above makes some of them unnecessary, and removing them
is part of that step, not follow-up work.  Otherwise the runtime ends up with
two mechanisms for the same thing, and the old one keeps constraining changes
to the new.

**After step 3**, when there is one event loop and originators park:

- #202's backoff and retry cap for a wait, which parking replaces.  The
  abort before parking stays until step 4, and so does the restart, which
  the lock freeing now triggers instead of a timer.  The die retry stays:
  that is wait-die itself.
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
- The originator's abort and restart on a wait, from step 3.  A waiting
  originator suspends with its locks held, as a participant does.
- `reactive_cache` as a field on the `Manager`, which moves into each
  execution's context.

The itemised checklists, with the names of functions, constants and tests, are
on #28 for step 3 and on #213 for step 4.  Code that is scaffolding for a later
step should say so in a comment naming the issue that removes it, as #202's
comments on `MAX_WAIT_DIE_RETRIES` already do.

## Open questions

- **Shared `Manager` or state machine.**  This document recommends the shared
  `Manager`, reached only through a `ManagerHandle`; the state machine deserves
  a closer look if a table of continuations turns out to be needed for other
  reasons, such as persisting them.
- **libp2p on one thread.**  To confirm when step 4 is built: that
  `.with_tokio()` needs no change on a single-threaded runtime, and whether
  libp2p's work competes with interpretation enough to need the `LocalSet` on
  a thread of its own.
- **Abort of a suspended action.**  What the action is told, and how its
  partial state is discarded.
- **Results that arrive later.**  Once a REPL command or a GUI action can
  finish after other input has been handled, the interface needs a way to
  report its result asynchronously.
- **Fairness of wakeups.**  Waking oldest first matches wait-die; it should be
  checked that nothing else undoes that.  A single channel at least handles
  events in arrival order, with no random choice between sources.
