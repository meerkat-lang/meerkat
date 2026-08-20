# REBLS Paper Overview

This document serves as a concise, plain-language overview of the operational semantics detailed in "Type safe evolution of live systems" by Miguel Domingues and João Costa Seco.

This document is a first step towards fully understanding the type safe live code updates feature and the internals of the Meerkat system and its semantics.

For convenience, terms we use now, such as `var` and `def` were synthesized into the overview.

Please note that this document does not describe the semantics and statics of Meerkat as it exists today. It is important background reading towards the present system, which extends these semantics. Enhancements in the current Meerkat system include imports and how to handle the dependency cycles they can introduce; how nodes start up that rely on imports, sharing code and so on; locking groups; service level locks; service level updates; services as types; the use of resident graphs for various dependency structures, including service level subscribers, and others.

**AI Disclosure:** This document is a human-AI collaboration. I used Google Gemini Pro 3.1 Extended to analyze the original paper and refined it continuously over many sessions to ensure that it reflected the required form to assist with understanding Meerkat internals. I especially focused on providing references to the page, section, and paragraph, with relevant quotations to provide context to help find and cross-examine this document with the original paper. I guided the entire drafting process and made significant manual edits.

## 1. Core Elements

The paper has three distinct systems to manage data and logic:

* **Ref:** Page 1, Section 2, Paragraph 1: "We introduce three main ingredients to model our target scenario of data-centric applications: state variables, data transformation expressions, and actions."

* **State Variables (`var`):** These represent the persistent data layer (e.g., database tables or session variables). It represents a writable state.

	* **Ref:** Page 1, Section 2, Paragraph 1: "Bound state variables model the persistent data layer."

* **Data Transformation Expressions (`def`):** These represent the application logic, acting as queries or pure functions that process data for the user view. This operates as a read-only pure function, which is akin to a nullary function defined in the type.

	* **Ref:** Page 1, Section 2, Paragraph 1: "Application logic layer is modeled by bound data transformation expressions, representing either a query over persistent data, or code that processes results..."

* **Actions:** These are delayed computations that model event handlers (like button clicks) and contain the imperative commands to insert or update the state variables (`var`).

	* **Ref:** Page 1, Section 2, Paragraph 1: "Actions are delayed computations modeling event handlers, and enclose (imperative) insert or update queries to the data layer."

## 2. Dependencies: Data vs. Types

Before discussing how the runtime manages execution, it is critical to clarify how the paper uses the concept of a "dependency," as it uses this term when referring to **two distinct mechanisms** that operate at different stages of the system.

* **Runtime Data-Flow Dependencies (Reactivity):** When the paper discusses the "dependency graph" or dependencies between code and data, it is describing the runtime observer pattern. If variable `B` is calculated using variable `A`, `B` has a data-flow dependency on `A`. This determines *when* variable `B` needs to be recalculated during live execution.

	* **Ref:** Page 2, Section 2.1, Paragraph 5: "This dependency graph is incrementally built... and kept up-to-date with relation to dependencies between data transformation expressions."

* **Static Type Definitions:** The paper uses the term "typed dependency" to describe a strict verification requirement used before code is allowed to run. To avoid confusion, this document will refer to this concept as a **Type Definition**.
  
	* A Type Definition captures two specific rules about how a piece of code intends to use a name:
	  
		* What data **type** it expects (e.g., expecting an Integer), and
		  
		* Whether it expects that name to be a **writable** state (`var`) or a **read-only** pure function (`def` is essentially a nullary function in Meerkat). It determines *if* a new code update is safe to compile and merge into the system.

	* **Ref:** Page 3, Section 3.1, Paragraph 2: "...typed dependencies sets... capture information about the type of a used name, and whether it denotes a state variable."

## 3. Runtime Configurations

The reactive operational semantics are defined on runtime configurations that represent a complete, executing live system. The runtime is tracked using a configuration consisting of three interconnected components.

* **Ref:** Page 4, Section 3.2, Paragraph 1: "We define a reactive operational semantics by means of two layers of small-step reduction relations... defined on runtime configurations... representing a complete live system."

* **Typing Environment:** The registry that describes the current type specification and Type Definitions of the system.

	* **Ref:** Page 4, Section 3.2, Paragraph 1: "Typing environment... describes the current system type specification..."

* **State:** The memory layer mapping names to tuples containing an expression (the code) and a denotation (which may be a computed value or explicitly marked as undefined).

	* **Ref:** Page 4, Section 3.2, Paragraph 1: "...S is a state mapping names... to tuples... with an expression... and the current denotation... that can be either a computed value... or the special denotation undefined()."

* **Queue:** The event loop that handles the evaluation of related events, which are sorted into interaction operations, names to be refreshed, names currently being refreshed, or construction blocks.

	* **Ref:** Page 4, Section 3.2, Paragraph 2: "The third element of a runtime configuration... denotes a queue of operations... The queue disciplines the evaluation of a set of related events. Queued events have one of the following sorts: interaction operations... names to be refreshed... names currently being refreshed... or construction blocks..."

## 4. Subscribers and Reactive Propagation

The semantics enforce a data-driven model using a built-in observer pattern based on runtime data-flow dependencies.

* **Ref:** Page 4, Section 3.2, Paragraph 4: "We define the subscribers of a name a, as all data transformation expressions (def) that depend directly on name a."

When a state variable (`var`) is updated, the runtime intercepts the change, identifies all dependent data transformation expressions (subscribers), and pushes them to the front of the queue.

* **Ref:** Page 4, Section 3.2, Paragraph 6: "...the subscribers of the updated name... are placed at the beginning of the queue. This ensures that we immediately propagate changes into other names, thus giving the reactive behavior to our semantics."

## 5. Dependency Analysis: Guarded vs. Unguarded Cycles

To ensure the reactive propagation of changes does not diverge into an infinite loop, the system tracks the Type Definitions between names and strictly analyzes them to prevent cyclic definitions.

* **Ref:** Page 3, Section 3.1, Paragraph 1: "To do so, we statically keep track of name dependencies to avoid the creation of unguarded cyclic dependencies."

The semantic model distinguishes between two types of cycles during static analysis:

* **Guarded Cycles:** A dependency path is considered safe (guarded) if the cycle is broken by an action (or closure). Because actions are delayed computations that require a user event to trigger, they safely halt the automatic propagation chain.

	* **Ref:** Page 3, Section 3.1, Paragraph 1: "We say that a dependency cycle is guarded if it crosses an action value, and hence needs an explicit interaction operation to be activated."

* **Unguarded Cycles:** A direct cyclic dependency between data transformation expressions (`def`) without an intervening action. The runtime statically rejects these because automatic reactivity would cause them to continuously trigger one another.

	* **Ref:** Page 3, Section 3.1, Paragraph 1: "Any unguarded cycle would cause the propagation process to diverge."

## 6. System Evolution and Atomic Blocks

To safely interleave system evolution with regular execution, live code updates are applied using an "atomic" construction operation.

* **Ref:** Page 3, Section 2.2, Paragraph 3: "Since the evolution of the system is interleaved with regular execution... we next introduce the construction operation atomic, that allows to apply a set of operations in a transactional style."

The atomic mechanism temporarily suspends interactions and reactive change propagation, preventing the system from exposing transient, inconsistent states to users during a live update.

* **Ref:** Page 3, Section 2.2, Paragraph 3: "With this mechanism, we temporarily disallow interaction operations and propagation of changes, which helps avoiding effects such as the one described above (e.g. transient states)."

Formally, these atomic blocks map to composition construction operations evaluated in a sandbox against a partial typing environment and partial state. The system allows transient inconsistencies (like temporary unguarded cycles) between sub-operations within the block, provided the final combined state of the block is sound.

* **Ref:** Page 3, Section 3, Paragraph 1: "In the example from Section 2.2, this was introduced as the atomic block. These blocks are evaluated in a transactional style, allowing for transient inconsistencies in the system."

* **Ref:** Page 4, Section 3.2, Paragraph 2: "Construction blocks are annotated with a typing environment... and a state... that describe its partial effect."

* **Ref:** Page 5, Section 3.3, Paragraph 9: "...we allow for a composition construction operation to introduce transient inconsistencies in-between sub-operations... while ensuring that in the end the application is sound."

## 7. Overview: The Type and Effect System

The following is a practical breakdown of how Type Definitions, Dependency Analysis, and the Effect System connect within the runtime.

* **What is the Type System?** The type system validates the individual building blocks. When you write new code, its Type Definition describes what it needs (e.g., "I need a writable state, or `var`, named `x` containing an integer").

	* **Ref:** Page 3, Section 3.1, Paragraph 2: "...capture information about the type of a used name, and whether it denotes a state variable."

* **What is the Effect System?** In this paper, an "effect" is a formal description of how an operation alters the global registry (the Typing Environment). For example, the operation `def y = x + 1` produces an *effect* that says: "Add a new data transformation named `y` to the environment, and register that its Type Definition depends on `x`". Note that user interactions (like clicking a button) that only modify data, but do not change a Type Definition, produce *no* effects.

	* **Ref:** Page 3, Section 3.1, Paragraph 6: "The typing based on effects of operations... allows us to capture the incremental effects of each operation..."

	* **Ref:** Page 4, Section 3.1, Paragraph 4: "Interaction operations (do e) do not produce any effects... i.e. the application definition is not modified, only the state is modified at runtime."

* **How do they interact with Dependency Analysis?** When you submit a live code update, the Effect System gathers the *effects* of your new code and temporarily projects them onto the current environment. The Dependency Analysis then scans this projected future state. It expands all the Type Definitions mapped by the Effect System to ensure no unguarded cycles were created.

	* **Ref:** Page 5, Section 3.3, Paragraph 2: "Computing the expansion of the typed dependencies is essential to statically avoid circular dependencies, and ensuring that the propagation of changes does not diverge."

### The Connection to Data-Flow Guarantees

* **Type Preservation:** Once the Effect System verifies that the Type Definitions are well-formed and acyclic, the code is committed. A Type Definition is well-formed if all specified requirements correctly match the actual types and properties of the referenced names. This ensures that redefining a variable (for example, attempting to redefine a writable state `var` into a read-only `def`) does not unexpectedly break existing code that relies on writing to that variable. Type preservation means that as the queue executes (Data-Flow Propagation), the system guarantees it will never break these Type Definitions.

* **Safety:** Because the Effect System pre-verified the absence of unguarded cycles, the Data-Flow Propagation is mathematically guaranteed to finish its work (converge) without entering an infinite loop of automatic reactive updates.

	* **Ref:** Page 5, Section 4, Paragraph 4: "These results allow us to ensure that the automatic propagation of changes converges, and dynamic reconfiguration of both code and data is always sound."


## 8. Queue Effects and Compatibility Verification

(Note that following is different, though not necessarily incompatible, with the enhancements to Meerkat I made for the implementation of type safe live code updates. Please refer to reference documentation for the specifics; this document is an overview of the REBLS paper.)

In a concurrent system, verifying an update against the *current* state is insufficient because pending events in the queue might change the system before the update runs. To solve this, the runtime calculates "queue effects" to simulate the future typing environment resulting from all unprocessed events.

* **Ref:** Page 5, Section 3.3, Paragraph 5: "...we define the notion of queue effects... that computes the effects of an unprocessed queue, and allows to add new construction operations to the queue, in a verified way."

New operations are strictly evaluated against this simulated future environment for compatibility. Compatibility guarantees that incorporating the update will yield an acyclic environment with well-formed Type Definitions.

* **Ref:** Page 5, Section 3.3, Paragraph 4: "The notion of compatibility ensures that for the combined typing environment... all names are acyclic... and their typed dependencies are well-formed..."
## 9. Type Safety and Convergence

The paper proves that a system adhering to these semantics provides three strict operational guarantees:

1. *Runtime Progress:** The system will never reach a halted state where a name lacks an associated value.

	* **Ref:** Page 5, Section 4, Paragraph 2: "...states that all names in the current state have a value associated..."

2. **Type Preservation:** Evaluating operations continuously maintains a well-typed configuration.

	* **Ref:** Page 5, Section 4, Paragraph 3: "THEOREM 4.2 (RUNTIME TYPE PRESERVATION)."

3. **Convergence:** Because the system rejects unguarded cycles, automatic change propagation will always finish processing. The queue will predictably empty in a finite number of steps.

	* **Ref:** Page 5, Section 4, Paragraph 4: "...all well-typed runtime configurations with a non-empty queue, reach a runtime configuration with an empty queue after a finite number of steps."
## 10. Analysis and Extensibility

### 1. Modularity, Imports, and Distributed Services

The formal model in the original paper focuses on a global namespace, explicitly narrowing its scope to exclude scoped namespaces or modular structures.

* **Ref:** Page 3, Section 3, Paragraph 1: "For the sake of simplicity, names are global in the application domain. A modular structure with nested names can be extrapolated from this language, but with no real immediate benefit to the focus of this work."

While the authors suggest a modular structure can be extrapolated, the current semantics do not explicitly detail mechanisms for `imports`, module boundaries, or the cyclic nature of distributed services.

(I had to design and implement solutions to those extensions.)

* **Global State Mapping:** The foundational memory state maps single, global names directly to expressions and values. The current model does not include specific mechanisms for resolving cross-service scopes.

	* **Ref:** Page 4, Section 3.2, Paragraph 1: "...S is a state mapping names... to tuples... with an expression..."

* **Acyclic Verification Scope:** The static analysis used to prevent infinite propagation loops relies on expanding direct, global name dependencies. Because services in distributed systems often import modules that create cross-module dependency cycles, extending this model to a complex modular architecture would require additional theoretical frameworks for cross-service dependency analysis not covered in this text.

	* **Ref:** Page 5, Section 3.3, Paragraph 2: "Computing the expansion of the typed dependencies is essential to statically avoid circular dependencies..."

### 2. Convergence Guarantees

The paper guarantees that the automatic propagation of reactive updates will converge (empty the queue without looping forever), focusing specifically on the reactive framework rather than the entirety of user-provided code.

* **Ref:** Page 5, Section 4, Paragraph 4: "...all well-typed runtime configurations with a non-empty queue, reach a runtime configuration with an empty queue after a finite number of steps."

The convergence guarantee is parametric and relies on the assumption that the underlying functional core language terminates.

* **Ref:** Page 5, Section 4, Paragraph 5: "Notice that this result is based (and parametric) on the termination of the functional core... Using any expression language free of side-effects, for which we can prove termination... then the result also holds."

Therefore, the dependency analysis protects against reactive loops (e.g., `A` updates `B`, which updates `A`). If user-provided code contains an explicit non-terminating loop or a non-terminating recursive closure inside an action block, the execution will reflect that behavior. The convergence theorem specifically covers the reactive framework's propagation paths, assuming the termination of the parameterized core language.

