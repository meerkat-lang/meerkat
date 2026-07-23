# Design for triggering actions from the client

## Motivation

A Meerkat service can already render HTML through the `html` definition, and a
browser client renders that markup and re-renders it reactively when the
underlying values change.  What it cannot do is send anything back: the page is
a view, not an interface.  Every state change in the web client demo has to be
triggered by a separate process.

This document proposes a language extension that lets a service bind an action
to a user interface event, so that clicking a button in the browser runs an
action -- on the client, or on the server that the client is connected to.

## What already exists

Three pieces of the mechanism are already in place.

Actions are first class values.  `Value::ActionClosure` holds the statements of
an action together with a captured environment, so an action can be produced by
an expression and invoked later rather than only at its definition site.

An action closure knows where it belongs.  It carries a `ServiceNetId`, which
records the owning node's address.  The comment on that field notes that this
lets the action be executed even if its service is not imported into the scope
where the closure is later used.  That is exactly the browser's situation: the
page holds a handler for an action whose service lives on the server.

Remote invocation already exists.  `remote_action` accepts a `ServiceNetId`, the
action's statements, and an environment -- the same three things an
`ActionClosure` stores.

What is missing is a way to write the binding in source, and a place for the
rendered HTML to carry it.

## Proposed syntax

An event attribute in an HTML template takes an interpolated expression that
evaluates to an action:

```meerkat
service counter {
    var n = 0;
    pub def bump = action { n = n + 1; };
    pub def html = (<button onclick={bump}>increment</button>);
}
```

This reuses the existing `{expr}` interpolation form rather than introducing
new syntax.  The difference is only in what the expression evaluates to: an
action value instead of an integer or a string.

The rule is general.  An attribute whose name begins with `on` is an event
attribute, and its interpolated expression must evaluate to an action.  Any
other attribute keeps its current meaning, where the interpolated value is
rendered as text.

Because an action closure carries its own service identity, the action need not
belong to the service that renders the markup:

```meerkat
import s1

service counter_ui {
    pub def z = s1.y * 2;
    pub def html = (<div><p>z = {z}</p><button onclick={s1.inc_x}>+</button></div>);
}
```

## Semantics

Evaluating an event attribute produces an action value rather than text.  The
value is not rendered into the markup; it is attached to the element.

When the corresponding event fires, the runtime invokes the action.  If the
action's `ServiceNetId` names the local node, it runs locally through the
ordinary action path.  If it names another node, it is sent there through
`remote_action`.  The captured environment and service identity supply the
execution context, so no new capture or scoping rules are required.

Any state the action changes propagates through the existing reactive
machinery.  In the example above, clicking the button increments `x` on the
server, which recomputes `s1.y`, which pushes an update to the subscribed
client, which recomputes `z` and re-renders `html`.  The click is the only new
step; everything after it already works.

## Required change to the HTML representation

An `Html` value is currently backed by a single rendered string.  A string can
carry the text `z = 4`, but it cannot carry a reference to an action closure, so
there is nowhere for an event handler to live.

This proposal therefore requires `Html` to become a structured tree: elements
with attributes, where an attribute value is either text or a Meerkat value.
Event attributes hold action values; everything else renders as it does today.
Rendering to the DOM then walks the tree and attaches real event listeners
instead of splicing strings together.

The `Html` module was written with this in mind.  Its documentation states that
the representation is private precisely so that it can change later, and names a
structured tree as the anticipated direction.  The change also addresses a
concern raised during review of the web client: with a tree, dynamically
computed values are inserted as data rather than as markup, so they cannot be
interpreted as HTML.

## Extending to other inputs

The general rule extends to inputs that carry a value, but those raise a
question this design does not yet settle.  A checkbox or a text field wants to
hand the action what the user typed or toggled:

```meerkat
pub def html = (<input type="text" oninput={set_name} />);
```

An action closure holds statements and a captured environment but no parameter
list, so there is currently no way to pass the event's value in.  Two options
seem plausible: give action closures parameters, in the way ordinary closures
already have them, or bind the event value into the captured environment under
a well known name before invoking the action.  The first is more explicit and
composes better with the rest of the language; the second requires no change to
the value representation.

Radio buttons are a further step.  A group of radio buttons naturally maps to a
choice among named alternatives, which Meerkat cannot currently express.  They
are better revisited once the language has enumerations.

## Open questions

Whether event attributes should be restricted to a known set of event names, or
allowed for any attribute beginning with `on`.  A fixed set catches typos at
check time; an open set needs no change when new events are supported.

How the event's value should reach the action, for inputs that carry one.  See
the section above.

Whether a click that triggers a remote action should give the page any feedback
while the action is in flight, or whether the reactive update arriving later is
sufficient.

## Test case

`meerkat/tests/client_button.mkt` demonstrates the design.  It defines a service
whose `html` renders a value together with a button bound to an action on an
imported service.  It does not work today: event attributes are not recognised,
and an `Html` value cannot carry an action.  It should work once this design is
implemented.
