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

What the page actually holds for a handler, and therefore what a click sends
back, is left open below; see the section on the browser trust boundary.

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
structured tree as the anticipated direction.

Existing consumers treat `Html` as text, and that does not change: `as_str` and
the `Display` impl still produce the rendered visible content, and the AST
printer and any text encoding serialize that content. A handler is not part of
that text. It is carried on the tree node rather than spliced into the string,
so a printer or a text-only consumer sees the markup without the handler and
never serializes an action as markup. Within a single node a handler is just the
action (or lambda) value the node holds; how a handler reference travels between
nodes is the transport question the separate trust-boundary issue covers.

A tree alone does not make rendering safe.  If the renderer walks the tree and
concatenates it back into a string, the current injection risk is preserved.
The design therefore requires that rendering go through DOM APIs that treat
values as data: interpolated text is set as text content, ordinary attributes
are set as attribute values, and neither is parsed as markup.  Under that rule a
dynamically computed value cannot introduce elements or event handlers,
regardless of what it contains.

If a service ever needs to emit markup that should be parsed as HTML, that has
to be a distinct and explicitly named construct rather than the default
behaviour of interpolation, so that the trusted case is visible in the source.

## Handlers and the browser trust boundary

Sending an action to another node currently means sending its statements and
captured environment, and every node that receives them is another Meerkat
process.  Binding a handler into a page changes that: the receiving side is a
browser, and whatever it holds can be inspected and altered before it is sent
back.  If a click returns the statements it was given, a client could return
different ones, and the server would execute them.

This is a variant of a problem the system already has -- a peer supplying its
own return route is tracked in #118 -- but the browser makes it sharper, because
the handler is handed out deliberately rather than merely accepted on arrival.
It is also not answered by the earlier decision that a browser client trusts the
server it loaded its code from: that decision runs in the other direction.

An alternative is for the handler in the page to be an opaque reference rather
than the action itself.  The node that renders the markup keeps the closure and
gives the page an identifier for it; a click sends the identifier back, and the
owning node resolves it to the closure it issued and runs that.  Nothing
executable crosses the boundary, and the page cannot name an action it was never
given.  The cost is that identifiers have to be issued, scoped to a particular
page's session, and eventually discarded.

Resolution: this is a real concern, but its architecture is more global than
this feature -- it is really about mobile code in Meerkat as a whole -- so it is
tracked as a separate issue rather than settled here. For now, actions and
lambdas are passed to and from the client over the wire and executed under the
normal execution semantics, with the tampering question deferred to that issue.

## Extending to other inputs

The general rule extends to inputs that carry a value. A checkbox or a text
field wants to hand the action what the user typed or toggled, so the handler
takes that value as an argument:

```meerkat
pub def html = (<input type="text" oninput={(s) => set_name(s)} />);
```

Resolution: the expression for an event attribute is either an action, or a
lambda that takes one argument and returns an action. The lambda's argument
receives the value the event supplies, and its type is checked against what the
event provides. A plain action is used where the event carries no value (a
click); a lambda is used where it does (a text input).

The value-carrying events, and the type each supplies, form the initial
supported subset:

| Event | Payload | Handler expression |
| --- | --- | --- |
| `onclick` | none | an action |
| `oninput` | string | a lambda from string to action |
| `onchange` (text) | string | a lambda from string to action |
| `onchange` (checkbox) | bool | a lambda from bool to action |

Any other attribute beginning with `on` is accepted with a no-argument action.
An attribute in the value-carrying subset requires a lambda whose argument type
matches the payload, and the compiler rejects a mismatch. The lambda form above
is illustrative; the concrete syntax follows Meerkat's existing lambda
expression and is fixed during implementation.

This subsumes the earlier question of how an event value reaches the action: it
arrives as the lambda's argument, needing no change to how actions themselves
are represented.

Radio buttons are a further step.  A group of radio buttons naturally maps to a
choice among named alternatives, which Meerkat cannot currently express.  They
are better revisited once the language has enumerations.

## Resolved questions

Attribute scope: any attribute whose name begins with `on` accepts a
no-argument action. A designated subset of supported event attributes may
additionally accept an action that takes an argument (via the lambda form
above), and the compiler typechecks that the argument has the type the event
supplies. This keeps the common case open-ended while giving the value-carrying
events a checked contract.

Passing an event's value to an action: resolved by the lambda form described
under "Extending to other inputs" above.

In-flight feedback: showing the user something while a remote action is in
flight is out of scope for now; the reactive update arriving later is treated as
sufficient. A future approach might use compound actions -- a non-transactional
action composed of transactional parts, where an early part shows an in-flight
indicator and the rest carries out the command -- but that is left to a separate
issue.

## Reactivity of handler bindings

Actions are often bound in local defs or variables rather than named inline, so
a handler that references one must update inside the rendered HTML when that
definition changes, exactly as any other interpolated value does. The structured
HTML representation should carry handler bindings in a way that preserves this,
so that reactivity extends to the handler an element is bound to, not only to
the text it displays.

## Test case

`meerkat/tests/client_button.mkt` demonstrates the design.  It defines a service
whose `html` renders a value together with a button bound to an action on an
imported service.  It does not work today: event attributes are not recognised,
and an `Html` value cannot carry an action.  It should work once this design is
implemented.
