## Agentic Workflows

After modifying any `.md` workflow file under `.github/workflows/`, always
recompile and commit the generated workflow files with the source change:

```bash
gh aw compile
apm compile
```

For Goal issues, keep the completion contract evidence-based. A goal is complete
only when the issue's stated verification evidence supports it.

## Testing

CI (`.github/workflows/ci.yml`) and the pre-commit hooks cover the same Rust checks: formatting, `cargo clippy` with `-D warnings`, the wasm32 `cargo check`, and `cargo test --workspace` skipping `multiple_message`. CI checks formatting with `cargo fmt --all -- --check`; the pre-commit hook runs `cargo fmt --all --` and may rewrite files. CI also runs a `meerkat -- --help` smoke test; see both config files for exact flags.

A full regression run also includes `python3 scripts/test_mkn.py`, the
multi-node network tests. It is not in CI and takes about a minute. Run
`cargo build -p meerkat` first: it launches nodes with `cargo run`, and the
hooks build with `--all-features`, so after a commit the first node can spend
its startup time compiling and fail with "Timeout waiting for node ... to
initialize".

## Known Flaky Tests

These fail intermittently for reasons unrelated to most changes. If one fails,
check its issue before investigating. When the issue closes, remove its line
here.

- `action_cross_node` in `scripts/test_mkn.py`: client fails with "Import fetch
  failed for service 'B'". [#204](https://github.com/meerkat-lang/meerkat/issues/204)
- `mkn_client_timeout_slow` in `scripts/test_mkn.py`: runs no Meerkat code,
  only a 2-second `sleep`, yet sometimes times out.
  [#205](https://github.com/meerkat-lang/meerkat/issues/205)

List all known flaky tests with `gh issue list --label flaky`.
