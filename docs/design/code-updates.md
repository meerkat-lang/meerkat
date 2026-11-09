# Design for code updates in Meerkat

## Syntax

The syntax for code updates should distinguish updating existing definitions
from introducing new definitions.  This expresses the intent to update and
ensures that the developer does not update a definition by accident when they
think they are adding a new definition.  The `update` keyword indicates
services and members that are updated.  One possible syntax would look like
this (example borrowed from the Requirements wiki page):

```meerkat
update s1 {
  update def y = x + 5;
}
```

## Service classes

Web clients will probably want to instantiate services defined on the server
that they connect to.  When we update the definition of the service we'd like
all instances of that service to be updated.  That suggests the idea of a
service class.  A service class is a template for instantiating services; the
most common use case is web clients, but there could be others.  Possible
syntax for service class definition and instantiation:

```meerkat
service class MyServiceClass {
  ...
}

service myService = MyServiceClass;
```

When a service class is updated, then all the services that are an instance
of that service class are updated.

The existing `service` declaration can be thought of as declaring a service
class and instantiating it to a `service` that has the same name.  In
contrast, a `service class` is not instantiated until it is done explicitly
with a separate `service` declaration like the one described above.

## Typechecking

A starting point for type checking code updates is found in
[Domingues and Seco 2015](https://docentes.fct.unl.pt/jrcs/files/rebls15.pdf).
The type checker knows about all the dependencies of a service, so if a
code update changes the interface of a service in a non-monotonic way (e.g.
by removing a member, or by changing to an incompatible type, such as from
`int` to `string`), the type checker will verify
that client code is not broken (and/or an update to the client code is
packaged with the service code update).

It should be possible to type check a code update against code in one or more
`.mkt` files, or against one or more running services.  In the latter case,
the code update is guaranteed to succeed unless the code is changed by some
other update in between.  This is a version of the usual type
soundness property, extended to cover code updates.

Note that to make this work in practice, when a variable or table's type is
changed, migration code must be written (as shown in Domingues and Seco) in
order to populate the value of the new variable/table to conform to the
new type.  Typically this migration code reads from the old variable/table
and writes to the new one, but it can read from other sources as well if
need be.  The migration code is executed as part of the code update
transaction, while code update locks are held,
and once the code update is done, the migration code is no longer needed
as all the data has been migrated.  It can be kept around for replay
purposes, if desired.

## Coordination

A design choice we might consider is how to manage code updates in a Meerkat
distributed virtual machine.  First, a simple approach for specifying updates.
Services and service classes live where they were originally defined.  In the
REPL at that node, `update` statements can be executed to update them.  If the
node imports services from other nodes, it can also execute `update` statements
on those imported services.

When Meerkat supports an import statement in the REPL accepting a URL
(Issue #95), then we can easily import any service we want to update.
For now, import by URL means importing a service that is running on some other
Meerkat node and is exposed through that URL.


Eventually, code updates will likely be limited to code that is defined in the
current Meerkat distributed virtual machine (DVM), for security reasons.
Meerkat DVMs may of course choose to extend some level of trust to each other.
We defer the design of this authorization boundary to when we design the DVM.

When an update is executed, we need to acquire a write-lock on any service
being deleted and on any members being deleted or modified.  That write-lock
will prevent any transactions from reading or writing to the locked members.
The type checker is run after locks are acquired but before applying the code
update, and if there are type errors, the locks are released, the update is
canceled, and error messages are reported to the developer.

## Evaluating an update

**Member order.**  A member may only read members declared before it.  An
update keeps a changed member where it was and adds new members at the end, in
the order the update declares them; members are then reordered only as far as
needed for each one to follow the members it reads, keeping the existing
members' order and the update's order.  So an update can add a member and
change an existing def to read it, while a forward reference among the
update's own members is still an error (#265).

**Staged reads.**  The initializers of the vars an update declares, new or
changed, are evaluated in that member order, and each one sees the values the
update has already given its own vars.  With `var a = 1` running,

```meerkat
update s {
    var a = 10;
    var b = a;
}
```

gives `b == 10`.  This holds however a member is read: by its plain name, by a
qualified name such as `s.a`, or inside a function the initializer calls.  The
staged values live in the update's transaction, not in the initializer's local
scope, so a closure doesn't capture them; it reads the member when it runs.
Members the update doesn't declare keep their running values, read under the
update's locks.

**Defs aren't staged.**  A def's first value is computed after the update
commits, so a var initializer that reads a def the same update adds fails, and
one that reads a def the update changes sees the def's value from before the
update.  Staging defs too is tracked in #267.

## Errors due to code update races

A code update is transactional; if it succeeds, it will always preserve
the code in a working state, and if it fails, all changes are rolled back.
However, there
could be races between executing some action and code updates.  Whichever
acquires the appropriate locks will run.  If the code update runs first, some
triggered actions could be invalidated.  We need to design a mechanism to report
an error to the user in such cases, and/or for the programmer to catch any
errors that are triggered by the update and run some other compensating action.
