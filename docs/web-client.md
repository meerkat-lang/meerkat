# The Meerkat web client

A Meerkat node can run inside a web browser.  It connects to a Meerkat server
over a libp2p WebSocket, fetches a service's source by path, instantiates that
service locally, and renders its `html` definition into the page, re-rendering
whenever the underlying values change.

The browser is a Meerkat node in the ordinary sense: it has a manager, a network
actor, and services of its own.  What makes it different is that it starts with
nothing but a server address, and that it cannot be dialed back.

## HTML as a value

A service produces HTML through a template literal, which may interpolate
expressions:

```meerkat
service s2 {
    pub def z = s1.y * 2;
    pub def html = (<p>z = {z}</p>);
}
```

`html` is an ordinary reactive definition whose value happens to be HTML, so it
recomputes when `z` changes, which in turn happens when the remote `s1.y`
changes.  The value is an abstract data type: its representation is private to
its module, so it can change later without affecting the code that produces or
consumes it.

## Fetching service code

A client that holds only a server address needs a way to obtain the code it
should run.  A request names a `.mkt` file by path; the server reads that file
and returns its contents, with a separate error reply for the failure cases.

The requested path arrives over the untrusted network, so resolution is
deliberately restrictive.  Absolute paths are rejected.  The path is joined
against a fixed base directory -- the directory the server was started from --
and the result is canonicalized and required to remain inside it, which blocks
both `..` traversal and symlink escapes.

## The browser client

The WebAssembly entry point performs the whole load sequence: dial the server's
WebSocket address, fetch the named file, parse it, and load what it finds.
Imported services are registered as remote, since they live on the server;
services defined in the file are instantiated locally, which subscribes to their
remote dependencies.  The `html` definition is then rendered into a page
element, and a background task keeps the page current as updates arrive.

A minimal host page provides fields for the server address and the file path and
hosts the rendered output.  It is a static page and can be served by any HTTP
server.

## The server's WebSocket address

Browsers cannot dial a raw TCP multiaddr, so a server listens on a WebSocket
address in addition to its usual TCP one.  The `--ws-port` option sets the port,
defaulting to the TCP port plus one.

The TCP address remains canonical: it is what native peers dial and what service
URLs and reply addresses are built from.  The WebSocket address exists only so
that browsers can connect.

## Running in WebAssembly

The networking and runtime code was written against a native async runtime.  It
compiles for the browser target, but several paths depended on facilities the
browser does not provide and failed at run time rather than at build time.  Each
is now split by target, in the same way the codebase already split task
spawning, leaving the native behaviour unchanged:

- Reply timeouts used the native timer, which has no driver in the browser, so
  the browser branch uses a browser-compatible timer.
- Identifier generation read the system clock, which is not available in
  WebAssembly, so it now reads the browser's time source on that target.
- A background task spawned directly on the native runtime is now routed
  through the existing spawn helper, which picks the right spawner per target.

The practical lesson is that compiling for `wasm32-unknown-unknown` says very
little about whether the code will run in a browser.  Each of these was found by
loading the page and reading the panic.

## Reusing the open connection

A browser has no stable listening address, so it cannot be dialed back.  The
design relies instead on the send path reusing connections that are already
open: replies and reactive updates travel back over the same WebSocket
connection the browser opened.

For reply routing to produce a usable address, the browser sets a synthetic
local address carrying its peer id.  Because the server answers over the
existing connection rather than dialing out, a single browser client needs no
circuit relay.

## End to end

1. The browser loads the static host page and the WebAssembly bundle.
2. The client dials the server's WebSocket address and requests a file by path.
3. It parses the source, registers imports as remote, and instantiates the
   services defined in the file, subscribing to their remote dependencies.
4. It evaluates `html`, resolving remote values over the network, and renders
   the result into the page.
5. When state changes on the server, the server pushes an update over the open
   connection; the client applies it, recomputes the dependent definitions and
   `html`, and re-renders without a reload.

## Running it

Build the client:

    cd meerkat-wasm
    wasm-pack build --target web --out-dir www/pkg

Serve the host page:

    cd www
    python3 -m http.server 8080

Start a server.  Passing `--identity` keeps the peer id stable across restarts,
which means the address pasted into the page stays valid:

    cargo run -- -f meerkat/tests/s1.mkt --server --local --identity node.key

The server prints a line beginning "Browser clients connect at" containing its
WebSocket address.  Open the page, paste that address, enter a file path such as
`html_client.mkt`, and load it.  The rendered value appears on the page.

To see a reactive update, change the server's state from another client:

    cargo run -- -f meerkat/tests/poke_s1.mkt \
      -i <server-tcp-address>/s1 --local

The browser re-renders on its own as the update propagates.  Note that the
browser uses the WebSocket address while this client uses the TCP one.  After
rebuilding the WebAssembly bundle, hard-refresh the browser, or it will keep
running the cached build.

## Limitations and further work

The client polls for network events on a timer rather than waiting to be
notified; making event consumption notification-driven affects the native paths
as well and is tracked in issue #154.

A page is a view and cannot yet send anything back.  A design for triggering
actions from the client is under discussion in issue #153.

Interpolated values are formatted via the runtime `Display` implementation and
inserted directly into the rendered markup (no HTML escaping), so this is safe
only while the server is trusted.  Making the HTML representation a structured tree,
so that computed values are inserted as data, is part of that design.

Reply destinations in network messages are supplied by the sender and are not
yet bound to an authenticated identity; this is tracked in issue #118.
