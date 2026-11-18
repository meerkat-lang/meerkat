# Tests that work

* test0.meerkat
* test1.meerkat
* test_func_update.meerkat
* All \*.mkt files

test7 does not work at present (tables were implemented in a previous version but are not working now)





# Distributed transaction tests (issue #44)

These exercise transactions that compose actions across services, including
across separate network nodes. A transaction starting in one service can run
an action defined in another service atomically; commit and abort coordinate
across all participating nodes.

**Transaction boundaries in `@test`.** As of PR #201 an `@test` block is not
itself a transaction. Each `do` in the block runs in its own transaction and
commits before the next statement, while a `do` nested inside an action that is
already in a transaction joins that one. So a composed action is still atomic
end to end, but an assertion written at the `@test` level runs *after* the
preceding `do` has committed and cannot roll it back.

## Local (single process)

test_cross_service_txn.mkt runs two services in one process where an action in
s1 composes s2's action:

    cargo run -- -f meerkat/tests/test_cross_service_txn.mkt

Expected: @test(s1) passed.

## Cross-node (two nodes)

Each server prints a line "Service URL: <addr>/<service>" on startup. Copy the
URL for the service you need and pass it to the client with -i.

Note: the dist_s* scenarios below do not run as written. `import s2` asks the
serving node for `s2.mkt` -- the import path is derived from the service name
(`Imports::resolve_import`) -- but these fixtures are named dist_s2_server.mkt,
dist_s2_mid.mkt and dist_s3_server.mkt, so every import fails to resolve
(`requested file not found: "s2.mkt"`). Making them runnable means naming each
file after the service it declares, which the two files declaring `s2` cannot
both do in one directory; the ryow_* scenarios further down follow that
convention and do run. Until the fixtures are renamed, the equivalent coverage
lives in `cargo test` (see Unit tests below).

1. Start the s2 server (owns w and the bump action):

    cargo run -- -f meerkat/tests/dist_s2_server.mkt -s -p 9100

2. In another terminal, run the client. s1's action does x = x + 1; do s2.bump;
   under one transaction:

    cargo run -- -f meerkat/tests/dist_s1_client.mkt -i <s2 Service URL>

   Expected: @test(s1) passed (local write committed).

3. Confirm the remote write committed by reading it back in a fresh transaction:

    cargo run -- -f meerkat/tests/dist_check.mkt -i <s2 Service URL>

   Expected: @test(chk) passed (reads s2.w_val == 15).

dist_abort.mkt is the same composition but with a failing assertion. As of PR
#201 that assertion no longer aborts the composition: it sits at the `@test`
level, after `do update_both`, so the composed action -- including the remote
write to s2 -- commits before the assertion runs and fails. Nothing is left to
abort, and s2.w stays at 15. To exercise abort and remote lock release, the
failing assertion has to move *inside* the action, into the same transaction as
the write.

## Transitive (three nodes)

Demonstrates s1 -> s2 -> s3, where s2's composed action itself composes s3.
Subject to the same fixture-naming problem as the two-node scenario above.

1. Start s3:

    cargo run -- -f meerkat/tests/dist_s3_server.mkt -s -p 9300

2. Start s2, importing s3 (use s3's printed Service URL):

    cargo run -- -f meerkat/tests/dist_s2_mid.mkt -s -p 9200 -i <s3 Service URL>

3. Run the top-level client, importing s2:

    cargo run -- -f meerkat/tests/dist_s1_top.mkt -i <s2 Service URL>

   Expected: @test(s1) passed.

4. Verify both downstream writes committed:

    cargo run -- -f meerkat/tests/dist_check3.mkt -i <s2 Service URL> -i <s3 Service URL>

   Expected: @test(chk) passed (s2.w_val == 15 and s3.z_val == 107).

## Derived values across nodes

These check that once an action commits, the `def`s derived from what it wrote
reflect it, including when the write happened on another node. Each runs
standalone as a single process (imports resolve from disk) and distributed
against real servers; the assertions are the same either way.

Their assertions sit at the `@test` level, so each runs after the preceding
`do` has committed (see "Transaction boundaries" above). They therefore test
propagation after commit, not read-your-own-writes inside one transaction,
which is what `txn_reactivity_test` (see Unit tests below) specifies.

Two remote services, so that a def over both reflects an action on each, the
second without losing the first:

    cargo run -- -f meerkat/tests/ryow_a.mkt -s -p 9200 --local
    cargo run -- -f meerkat/tests/ryow_b.mkt -s -p 9300 --local
    cargo run -- -f meerkat/tests/dist_ryow_two_remotes.mkt -i <ryow_a URL> -i <ryow_b URL> --local

Expected: @test(two_rem) passed.

A remote action and then a local write, both reflected in a def over the two:

    cargo run -- -f meerkat/tests/ryow_a.mkt -s -p 9200 --local
    cargo run -- -f meerkat/tests/dist_ryow_local_write.mkt -i <ryow_a URL> --local

Expected: @test(loc_write) passed.

A nested action, so that a write made on a node this client never contacted
directly reaches a local def derived from it (client -> ryow_mid -> ryow_c):

    cargo run -- -f meerkat/tests/ryow_c.mkt -s -p 9400 --local
    cargo run -- -f meerkat/tests/ryow_mid.mkt -s -p 9500 --local -i <ryow_c URL>
    cargo run -- -f meerkat/tests/dist_ryow_nested.mkt -i <ryow_mid URL> -i <ryow_c URL>

Expected: @test(nested_cli) passed.

The single-process forms need no server:

    cargo run -- -f meerkat/tests/dist_ryow_two_remotes.mkt
    cargo run -- -f meerkat/tests/dist_ryow_local_write.mkt
    cargo run -- -f meerkat/tests/dist_ryow_nested.mkt

## Unit tests

The transaction logic also has Rust unit tests (cross-service composition,
read-then-write lock upgrade, nested do, and no partial writes on failure
*within a single transaction* -- note that an `@test` block is not one, so a
`do` that already committed survives a later failure in the block):

    cargo test --lib

Read-your-own-writes inside a transaction (a def read after a write it depends
on sees the new value, `var`s stay non-reactive, a failed recompute aborts the
transaction) is specified by its own suite. The `var` test runs normally; the
others are ignored until compute-on-read lands (tasks/12-compute-on-read.md),
and `--ignored` runs them and shows the current gaps:

    cargo test --test txn_reactivity_test -- --ignored

Commit ordering for a node in the middle of a chain -- it must commit the nodes
it composed onto before recomputing its own derived members from them -- is
covered by a test that stands in for the node below with a bare network peer,
so the ordering is observable rather than timing-dependent:

    cargo test --test participant_commit_order_test

What happens when that commit does not get through -- a participant refusing it
or never answering -- uses the same stand-in peer, so the outcome is chosen by
the test. The originator must report the failure rather than let the CLI print
`@test(...) passed` for a transaction only part of which committed:

    cargo test --test commit_failure_test

Wait-die's "wait" half at the node that started the transaction: a participant
parks a contended request in the server's wait queue, but an originator has no
queue to park in, so it waits by running the transaction again:

    cargo test --test originator_wait_test
