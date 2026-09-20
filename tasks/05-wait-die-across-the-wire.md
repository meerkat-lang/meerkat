# 05 — Preserve a wait-die outcome across the network

**Depends on:** nothing. Land before 06; together they make the wait-die story
coherent.
**Reference:** `cli-test-fixes` —
`meerkat-lib/src/runtime/interpreter/evaluator.rs` (`WAIT_DIE_DISPLAY_PREFIX`),
its re-export in `interpreter/mod.rs`, and `Manager::remote_error`
(`manager/mod.rs` ~line 1576).

## Rationale

Errors cross the network as `Display` text: a participant's `EvalError` is
formatted into the `error: Option<String>` field of a response and reconstructed
on the other side.

A **wait-die abort** is not a failure — it is a retry signal. The originator's
retry loop matches on `EvalError::WaitDieAbort`. But a wait-die that happened on
a participant arrives as an ordinary string and is reconstructed as
`EvalError::LocalDispatchFailed`, which is terminal. Routine lock contention on
a remote node therefore fails the whole transaction instead of being retried.

## Specification

Define the prefix as a constant **next to the `Display` impl that emits it**, so
the producer and the consumer cannot drift apart:

```rust
// meerkat-lib/src/runtime/interpreter/evaluator.rs
pub const WAIT_DIE_DISPLAY_PREFIX: &str = "Wait-die abort: ";

// ... in impl Display for EvalError:
EvalError::WaitDieAbort(s) => write!(f, "{}{}", WAIT_DIE_DISPLAY_PREFIX, s),
```

Re-export it from `interpreter/mod.rs`.

Add the reconstruction helper on `Manager`:

```rust
fn remote_error(err: String) -> EvalError {
    match err.strip_prefix(WAIT_DIE_DISPLAY_PREFIX) {
        Some(reason) => EvalError::WaitDieAbort(reason.to_string()),
        None => EvalError::LocalDispatchFailed(err),
    }
}
```

Route every site that reconstructs an `EvalError` from a remote `error` string
through it.

Two properties are load-bearing and must not be "simplified":

- **Match on the prefix, not the phrase anywhere in the string.** Error text
  quotes user input, and an assertion failure carries its own source text. A
  program containing `assert(note == "Wait-die abort: ...")` would otherwise
  have its assertion failure retried through the entire wait-die budget and then
  reported as contention.
- **Strip the prefix when reconstructing.** `WaitDieAbort`'s own `Display` adds
  it back. Keeping it would nest one copy per hop, so a three-node chain reports
  `Wait-die abort: Wait-die abort: Wait-die abort: <reason>`.

## Tests

Three unit tests in `manager/mod.rs` on the reference branch.

**Illustrating test:** `test_remote_error_matches_the_wait_die_prefix_not_the_phrase`
— it builds its input from a *real* failing `assert` whose message happens to
contain the phrase, rather than a hand-written string, and asserts the result is
`LocalDispatchFailed`, not `WaitDieAbort`. A substring match passes the naive
version of this fix and fails here.

Also:
- `test_remote_wait_die_survives_the_round_trip`
- `test_remote_wait_die_prefix_is_not_repeated_per_hop`

## Notes

- This fix is inert on its own: nothing retries a `WaitDieAbort` at the
  originator until task 06. It lands first because it is small and independently
  verifiable, and because 06's diff is easier to read without it mixed in.
