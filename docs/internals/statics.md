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
3. **Type Checking:** The core type checker, `tt::check`, validates all expressions and definitions of the unified AST using an augmented bidirectional type checking algorithm, constructing comprehensive `ServiceType` environments.
4. **Dependency Analysis & Graph Construction:** The reads pass in `graphs/effects.rs` computes what each member's code reads when evaluated, attributing the reads latent in a function to its callers. From these reads it rejects a program whose members cannot be initialized in declaration order or whose direct dependency graph has a cycle, and the runtime builds the execution topologies.

## Entry Points

The statics pipeline is initiated when new code enters the system. These entry points reside primarily in the `node.rs` bootstrap and `update.rs` transaction modules. There are two primary pathways:

### Node Initialization

When a Meerkat Node first boots up, it parses its initial configuration or the program passed via the `-f` CLI argument. Before static analysis proceeds, the system resolves remote and local dependencies:

- The import state machine in `imports.rs` parses import statements, iteratively fetching missing services from local disk or network peers, handling retries and timeouts.
- Once the dependencies are resolved, the state machine concatenates them into a single Unified AST.
- This Unified AST is passed to the name resolution module, `nameres::resolve`, to validate variable bindings and scoping rules.
- The system then invokes the core type checker, `tt::check`, on the Unified AST.
- Finally, the reads pass checks the whole program for forward references and dependency cycles, and constructs the execution topologies.

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
2. **LocksAcquired:** Once all locks are secured, the system merges the proposed AST updates into a unified AST. The statics pipeline (name resolution, type checking, and dependency analysis) then scans this unified AST.
3. **Evaluated:** If static checks pass, the initial expressions of the new members are evaluated against the locked old state.
4. **Committed:** If evaluation succeeds without errors or cycles, the transaction is committed. The old state is replaced, locks are released, and the updates are broadcast to the rest of the system.

## Type Inference

The augmented bidirectional type checker in `tt::check.rs` evaluates the AST to construct `ServiceType` representations. Member types are computed on demand, so a member may reference one declared after it; a checking stack makes inference terminate when two members' types depend on each other. A function type carries its parameter and return types only.

## Dependency Analysis & Graph Construction

After the type checker has validated the AST, the dependency pass in `graphs/effects.rs` computes the reads of each member's code. The same analysis decides whether the program can be initialized, whether its dependency graph is acyclic and, at runtime, which members each def listens to and which members a code update locks.

### Reads and Edges

A member's code reads another member when evaluating it does. Constructing a closure or an action reads nothing; applying a function reads what its body reads, in the function's own service; a conditional reads what either branch reads. The pass follows the functions a value may be through locals, parameters, members and results, so a def reads the members latent in the functions it calls, and a def that only holds a closure reads nothing. A function applied to closures is analyzed for those arguments, so higher-order functions need no special treatment.

Each read becomes a reactive edge within the service or a cross-service dependency. Only `def` members listen; the reads of a `var` initializer matter at initialization and for the locks a code update takes.

### Initialization Order

Services are initialized in program order and their members in declaration order, so a member's initializer may only read members that already exist: a member of its own service declared before it, and, through a function it applies, a member of a service declared before its own. A direct read of another service's member is not ordered. A program that breaks this rule is rejected as an illegal eager forward reference; one whose direct dependency graph has a cycle, across services included, is rejected as a dependency cycle.

### Functions the Runtime Holds

What applying a function reads is part of the function's type (§1.1 of the semantics), so the graphs of the running services follow the declarations: a local `var` is analyzed in the functions its initializer may evaluate to, a remote member in each version of its code the owner delivered to the defs here that read it. A committed write that stores a closure into a `var`, or replaces one, is checked against the installed graphs: it is refused, as a type error, when a def applying the var would come to read a member its declarations did not make it read, or could no longer be analyzed within the depth limit; a code update declares the new reads. Graphs are recomputed when a code update commits and when a delivered value brings new function code from a remote owner. A def therefore knows every member it will read before it is evaluated, and its evaluation reads only those inputs.

## Historical Context: The Old `dep_analysis` Module

Initially, dependency analysis was decoupled from the type checker and performed entirely via heuristic AST traversals in a separate `dep_analysis` module. While this pure AST traversal worked for direct, first-order function calls, it was limited when handling the dynamic nature of higher-order functions. Without access to the typing environment, a pure AST traversal cannot trace which closure is actually being invoked when a function parameter is called. 

Because of these limitations, the old `dep_analysis` module was removed and dependency tracking was integrated into the type checker, which tracks environment bindings and parameter flows. That integration was later undone: the reads pass follows the functions a value may be through those same bindings and flows itself, which also lets it analyze the functions the runtime holds, and it names members by service, which the type-level sets did not.
