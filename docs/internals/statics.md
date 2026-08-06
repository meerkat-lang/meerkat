# Meerkat Static Semantics Overview

The statics in Meerkat support type safety and execution stability in a reactive environment. Because the language supports live code updates, the statics are modularized to be invoked both during initial node startup and incrementally at runtime. 

It is important to distinguish between two distinct forms of runtime updates:

- **Reactive Propagation Updates:** Data-level value changes (e.g., when a user interacts with a variable) that automatically propagate across existing topological graphs.
- **Type-Level Service Code Updates:** Structural updates (e.g., adding a new definition, changing a member's type) that can introduce breaking type changes or dependency cycles. These require static verification before the node's state is mutated and updates are broadcast.

The core purpose of dependency analysis is to track dependencies between service members to avoid cyclic dependencies. Any cycle causes the change propagation process to diverge. Statically ensuring an acyclic dependency graph allows automatic propagation to converge and supports dynamic reconfiguration.

When an update is submitted to modify, rename, or introduce members, the static semantics project the proposed changes into a temporary isolated state. This ensures the global topology remains well-formed and acyclic before any new code is committed. In this topology, state variables, `var`, act as leaf nodes; while their initialization expressions depend on other service members, they do not propagate reactive changes or contribute to cycles the way pure data transformation expressions, `def`, do.

The statics are organized as a multi-pass pipeline in the compiler/interpreter:

1. **Import Resolution & AST Unification:** The entry point dispatch mechanisms resolve local and network imports, concatenating all dependencies into a single, unified abstract syntax tree. This unified AST exists so that subsequent static passes can evaluate the entire program context holistically, without needing to repeatedly traverse file boundaries or manage remote network requests.
2. **Name Resolution:** The `nameres::resolve` module evaluates the AST to validate variable bindings and scoping rules.
3. **Type Checking & Effect Tracking:** The core type checker, `tt::check`, evaluates the unified AST using an augmented bidirectional type checking algorithm. This pass is enhanced with an effect tracking system to handle the complexities of live updates:
   - It validates all expressions and definitions to construct comprehensive `ServiceType` environments.
   - It tracks the dependencies of function types by evaluating their bodies and embedding the accessed variables directly into their type signatures.
   - It expands these projected dependencies to verify the absence of cycles before changes are committed.
4. **Graph Construction:** The graph analysis module, `compute_dependencies`, transforms the validated AST into concrete execution topologies.

## Entry Points

The statics pipeline is initiated when new code enters the system. These entry points reside primarily in the `node.rs` bootstrap and `update.rs` transaction modules. There are two primary pathways:

### Node Initialization

When a Meerkat Node first boots up, it parses its initial configuration or the program passed via the `-f` CLI argument. Before static analysis proceeds, the system resolves remote and local dependencies:

- The import state machine in `imports.rs` parses import statements, iteratively fetching missing services from local disk or network peers, handling retries and timeouts.
- Once the dependencies are resolved, the state machine concatenates them into a single Unified AST.
- This Unified AST is passed to the name resolution module, `nameres::resolve`, to validate variable bindings and scoping rules.
- The system then invokes the core type checker, `tt::check`, on the Unified AST.
- Finally, it passes the validated AST to the graph analysis module, `compute_dependencies`, to construct the execution topologies.

### Atomic Updates & Transaction Semantics

During execution, a user can submit a code update transaction. Because Meerkat allows hot-swapping service definitions dynamically, the runtime processes live updates using an "all-or-nothing" transaction model. This prevents the node from exposing transient, inconsistent states to users.

To interleave system evolution with regular execution, updates are applied via `update` and `atomic` blocks. A single `update` block that is not enclosed within an `atomic` block is implicitly treated as a singular atomic transaction. These blocks construct a temporary isolated state representing a partial future environment. The transaction simulates the combined effects of the update against the current environment to ensure the final state remains well-formed and acyclic.

### Locking & Isolation

Before an update transaction can evaluate its temporary state, it must acquire necessary locks to provide linearizability and prevent write skews. The runtime leverages a Wait-Die deadlock prevention mechanism to coordinate this process:

- The state machine scans the proposed update to identify read and write access on specific members, grouping them into lock requirements.
- If an update introduces new members or targets an entirely new service, it requests a full service-level lock.
- The transaction attempts to acquire all locks in its group. Under Wait-Die semantics, if a lock conflict occurs with an older transaction, the current transaction waits; if it conflicts with a younger transaction, the current transaction aborts (dies) and retries.
- Lock acquisition must complete entirely before static evaluation begins.

### The Live Code Update Algorithm

The atomic update lifecycle is managed by a state machine that proceeds through four high-level phases:

1. **Init:** The runtime computes lock requirements based on the requested update to determine necessary service-level and member-level locks.
2. **LocksAcquired:** Once all locks are secured, the system merges the proposed AST updates into a unified AST. The statics pipeline (name resolution, type checking, and effect tracking) then scans this unified AST.
3. **Evaluated:** If static checks pass, the initial expressions of the new members are evaluated against the locked old state.
4. **Committed:** If evaluation succeeds without errors or cycles, the transaction is committed. The old state is replaced, locks are released, and the updates are broadcast to the rest of the system.

## Type Inference & Dependency Analysis

The core of the statics pipeline resides inside the type checker module, `tt::check.rs`. The augmented bidirectional type checker evaluates the AST to construct `ServiceType` representations while simultaneously tracing dependencies to evaluate the effects of updates. 

It accomplishes this by maintaining a stack of dependency sets. When the type checker enters a function body or action block, it pushes a new empty set onto the stack. As it evaluates expressions, any accessed variable or service member is added to the active set at the top of the stack. 

When it exits a function block, it pops the set and embeds those accessed variables directly into the resulting `Type::Func` signature. By statically baking these dependencies into the type system, the runtime can prevent infinite loops during reactive propagation or recursive evaluation.

### Dependency Sets

Dependency tracking statically bookkeeps the names an expression relies on, its typed dependencies. A dependency set, `DepSet`, is a collection of these topological identities. 

A critical nuance is that dependency sets are only attached to function signatures, `Type::Func`. This boundary distinction arises because service-level declarations, such as `var` and `def`, are unguarded contexts. They evaluate sequentially and eagerly at initialization. Because they execute immediately, the type checker can trace and satisfy their dependencies directly inline while type checking without needing to persist them.

Closures and functions, however, are guarded contexts. They suspend the evaluation of their abstract syntax tree until called. Therefore, `Type::Func` is the sole type representation that requires a `DepSet` to track latent dependencies that will be triggered upon invocation. Instead of modifying the parent execution scope immediately, the dependency set remains dormant inside the type signature until the function is explicitly invoked.

### Reusing Inference for Higher-Order Functions

Higher-order functions, HOFs, accept closures as arguments. Because closures carry dependency sets, passing them opaquely can result in losing the dependency information when the HOF is executed.

To address this, the type checker applies inference contextually at the call-site:

1. When a function call expression encounters a target function, it checks the provided arguments.
2. If any argument is a closure carrying a non-empty dependency set, the type checker creates a temporary, localized typing environment.
3. This environment binds the caller-supplied parameter types, including their active dependency sets, to the formal parameters of the HOF.
4. The type checker applies inference to the HOF body using this new environment.
5. Whenever the HOF invokes the parameter, the environment unwraps and propagates those original dependencies up into the call site's active scope.

### Same-Service vs Cross-Service Edges

**Same-Service Dependencies:** References to members within the local service are accumulated into the block's active dependency set and statically validated against the tracked initialization state. This approach helps identify illegal forward references and dependency cycles.
**Cross-Service Dependencies:** When navigating across a service boundary, the type checker validates the remote interface contract but isolates those remote dependencies from the local dependency set, which avoids false-positive dependency cycles across distributed boundaries.

## Graph Construction

After the type checker finishes validating the AST, the type information, including the dependency sets, is discarded. At this stage, the program has been evaluated for type consistency, uninitialized variable accesses, and topological initialization or reactive update cycles.

Control flow then passes to the graph analysis module `graphs/analysis.rs`. 

### Topology Construction

The `graphs` module is a remnant of older dependency analysis logic. Its original analytical responsibilities were partially shifted into `tt::check` to properly handle complex evaluation cases like higher-order functions. However, the module is retained because various systems, including `manager.rs`, `node.rs`, and `update.rs`, still call out to it, expecting a fully constructed `ServiceGraphs` data structure to determine topological execution ordering.

Because the type checker has already validated the AST, the `graphs` module no longer performs complex dependency validation or attempts to expand HOF closures. Instead, it functions as a simplified graph transformation pass:

- It uses recursive AST traversals, `free_var` and `cross_service_deps`, to extract the baseline topological edges.
- It maps these edges directly into service graph structures, compiling separate sub-graphs for reactive definitions, mutable state, and external dependencies.

This output is returned to the entry point, where the runtime uses it to calculate the execution order and instantiate the program.

## Historical Context: The Old `dep_analysis` Module

Initially, dependency analysis was decoupled from the type checker and performed entirely via heuristic AST traversals in a separate `dep_analysis` module. While this pure AST traversal worked for direct, first-order function calls, it was limited when handling the dynamic nature of higher-order functions. Without access to the typing environment, a pure AST traversal cannot trace which closure is actually being invoked when a function parameter is called. 

Because of these limitations, the old `dep_analysis` module was removed. Integrating dependency tracking directly into the type checker addressed this issue, as the type system inherently tracks environment bindings and parameter flows.
