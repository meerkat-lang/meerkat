# Decomposition of PR #189 (`cli-test-fixes`)

PR #189 is ~4,300 lines across 28 files. It contains a number of independent,
valuable bug fixes plus one large design decision — **eager in-transaction
propagation** as the mechanism for read-your-own-writes on derived members —
that we have decided not to take. We are going with **compute-on-read**
instead, for fully transactional semantics.

These twelve tasks land the valuable parts as small, independently reviewable
PRs, then implement compute-on-read on top of a hardened substrate.

## The reference branch

Every task refers to `cli-test-fixes` for a reference implementation.

**Do not `git cherry-pick`.** The branch is 17 commits and everything after
`5211cb8` is a "address review comments" commit touching several unrelated
fixes at once. Extract from the **final tree** of the branch, by function and
by test file, which are cleanly separated in the end state even though the
commits are not.

```bash
git show cli-test-fixes:meerkat-lib/src/runtime/manager/mod.rs
git diff main...cli-test-fixes -- <path>
```

The branch must be left intact as a read-only reference until task 12 merges.

## Sequence

| # | Task | Depends on | Approx. size |
|---|------|-----------|--------------|
| 01 | [Hygiene: clippy and test fixtures](01-hygiene-clippy-and-fixtures.md) | — | ~100 |
| 02 | [Reactive cache reentrancy](02-reactive-cache-reentrancy.md) | — | ~40 |
| 03 | [Import dependency ordering](03-import-dependency-ordering.md) | — | ~350 |
| 04 | [CLI service instantiation](04-cli-service-instantiation.md) | 03 | ~200 |
| 05 | [Wait-die across the wire](05-wait-die-across-the-wire.md) | — | ~80 |
| 06 | [Originator-side wait retry](06-originator-wait-retry.md) | 05 | ~280 |
| 07 | [Participant commit ordering](07-participant-commit-ordering.md) | — | ~250 |
| 08 | [Commit failure reporting](08-commit-failure-reporting.md) | 07 | ~350 |
| 09 | [Waking parked requests](09-waking-parked-requests.md) | 08 | ~400 |
| 10 | [Parked action replay safety](10-parked-action-replay-safety.md) | 09 | ~700 |
| 11 | [RYOW specification tests](11-ryow-specification-tests.md) | — | ~500 (tests only) |
| 12 | [Compute on read](12-compute-on-read.md) | 02, 06, 10, 11 | large |

**Tasks 01–05 are mutually independent** (except 04 on 03) and can be in review
simultaneously. **Tasks 05–10 are serial**: each one hardens something that
compute-on-read will stress, and several touch the same functions.

Task 11 is optional and can be done at any point; it only makes task 12's
review easier by separating "what the semantics are" from "how they are
implemented".

## The organising principle

Everything that compute-on-read will stress lands before it. Transactional
reads go from occasional to routine under compute-on-read, so parks, wait-die
outcomes and lock release all become common paths. Task 12 should not be
simultaneously introducing a mechanism and fixing the failures that mechanism
exposes.

## Known consequence

`meerkat/tests/s1.mkt` — the read-your-own-writes regression that opened PR
#189 — stays broken on `main` until task 12 lands. This is deliberate. Task 11
exists partly to document why.
