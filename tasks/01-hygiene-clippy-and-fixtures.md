# 01 — Hygiene: clippy lints and test fixtures

**Depends on:** nothing. Land first — it unblocks the pre-commit hook for every
other task.
**Reference:** `cli-test-fixes`, commit `c08c6bb` plus the `.mkt` changes in
`a5375d2` and `7dc0a18`.

## Rationale

Two unrelated mechanical fixes, grouped because both are trivial and both block
other work.

**(a) Clippy blocks all commits.** The pre-commit hook runs
`cargo clippy -- -D warnings`. Clippy 1.92 newly flags `collapsible_else_if` at
two sites in `meerkat-lib/src/runtime/tt/check.rs` (around lines 591 and 815)
that predate this work. Until they are fixed, *no* commit to this repository
passes the hook, whatever it touches.

**(b) Two test fixtures do not parse or type-check.**

- `meerkat/tests/dist_s2_mid.mkt` declares `pub def update = action {...}` and
  `dist_s1_top.mkt` calls `do s2.update`. **`update` is a reserved keyword** —
  see `Token::UPDATE_KW` in `meerkat-lib/src/runtime/parser/lex.rs:185` and the
  `"update" <i:Ident> "{" <ds:Decls> "}"` production in `meerkat.lalrpop:90`.
  The fixture cannot parse.
- `meerkat/tests/test_client.mkt` and `test_distrib.mkt` declare
  `pub def f = fn n => ...`. The type checker now requires a parameter
  annotation in this position.

## Specification

1. Rewrite the two `else { if .. }` blocks in `tt/check.rs` as `else if`.
   Behaviour must not change. `cargo clippy --fix --lib -p meerkat-lib` produces
   the correct result; review the reindentation, as the diff is large (~89
   lines) relative to the semantic change (zero).
2. Rename the `update` def to `bump` in `dist_s2_mid.mkt`, and the call site
   `do s2.update` → `do s2.bump` in `dist_s1_top.mkt`.
3. Add `int` annotations: `fn n =>` → `fn (n:int) =>` in `test_client.mkt`
   (`inc_delegate`) and `test_distrib.mkt` (`mul_y`).

## Tests

No new tests. The verification is that the hooks pass:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Notes

- Do **not** fold any other change into this PR. Its entire value is being
  obviously safe to approve.
- `meerkat/tests/README.md` on the reference branch documents these fixtures,
  but it also documents the `ryow_*` scenarios that do not exist yet. The README
  belongs to task 11, not here.
