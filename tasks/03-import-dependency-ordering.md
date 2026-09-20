# 03 — Emit imported modules in dependency order

**Depends on:** nothing.
**Reference:** `cli-test-fixes`, `meerkat-lib/src/runtime/imports.rs` (+101),
`meerkat-lib/src/runtime/node.rs` (+20), and four new tests in
`meerkat-lib/tests/imports_test.rs`.

## Rationale

`tt::check` walks services **in AST order** against a single flat set of
initialized members. A service is therefore only checkable once everything it
reads has already been declared in that AST.

The `Imports` state machine records modules **as they arrive**, which for a
transitive chain is exactly the reverse of what the checker needs. Resolving
`main -> a -> b` records `[a, b]`, because `a` must be fetched and parsed before
its own `import b` is discovered — but `a` reads `b`, so the checker sees `a`'s
reference to a service it has not met yet and fails.

Separately, the unified AST must place **all** imported statements before the
local program's own statements, for the same reason. On `main` the two entry
points that build the unified AST do this inconsistently.

## Specification

### `imports.rs`

Replace the flat `accumulated_ast: Vec<Stmt>` with a per-file record:

```rust
struct ImportedModule {
    /// Services this file declares.
    declares: Vec<Symbol>,
    /// Services this file imports.
    imports: Vec<Symbol>,
    /// The file's parsed statements.
    stmts: Vec<Stmt>,
}
```

held as `modules: Vec<ImportedModule>` on `Imports`. Keeping each file whole is
what makes ordering possible: a module is the unit that can be placed, and its
statements move together.

`Imports::finalize(self) -> Vec<Stmt>` must emit modules in **post-order over
the import graph**, so a module always precedes every module that imports it:

1. Build `owner: HashMap<Symbol, usize>` mapping each declared service to the
   index of the module declaring it, so an import edge can be followed to the
   module that satisfies it. First declaration wins.
2. Iterative DFS (not recursive — import chains are user-controlled depth) over
   every module as a root, with `done` marking emitted modules and `on_stack`
   marking the current path.
3. An import naming a service with no entry in `owner` is skipped: it is
   declared by the root program, and there is no module here to order against.
4. `on_stack` breaks import cycles rather than looping. **Do not report the
   cycle here.** Emit the modules anyway and let `tt::check` report it — it
   produces a far better diagnostic once it can see the services.

### `node.rs`

Add `fn set_unified_ast(&mut self, local_prog: &[Stmt], imported_ast: Vec<Stmt>)`
which builds `unified_ast` with **imports first**, then the local program.
Route *both* entry points that construct the unified AST through it, so neither
can drift back to appending imports last.

## Tests

All four are in `meerkat-lib/tests/imports_test.rs` on the reference branch.

**Illustrating test:** `test_transitive_imports_are_dependency_ordered` — sets
up `main -> a -> b` where `a` reads `b`, and asserts `b` precedes `a` in the
finalized AST. On `main` the order is `[a, b]` and static checks fail.

Also:
- `test_imports_precede_local_program_in_unified_ast`
- `test_on_node_startup_also_orders_imports_first` (the second entry point)
- `test_network_imports_are_dependency_ordered_regardless_of_arrival` — network
  responses arrive in nondeterministic order; ordering must come from the graph,
  not from arrival.

The nine pre-existing tests in that file must continue to pass unchanged.

## Notes

- This is self-contained within the import subsystem and touches no transaction,
  lock or network code. It should be reviewable on its own terms.
