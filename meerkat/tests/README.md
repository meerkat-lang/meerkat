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

## Local (single process)

test_cross_service_txn.mkt runs two services in one process where an action in
s1 composes s2's action:

    cargo run -- -f meerkat/tests/test_cross_service_txn.mkt

Expected: @test(s1) passed.

## Cross-node (two nodes)

Each server prints a line "Service URL: <addr>/<service>" on startup. Copy the
URL for the service you need and pass it to the client with -i.

1. Start the s2 server (owns w and the bump action):

    cargo run -- -f meerkat/tests/dist_s2_server.mkt -s -p 9100

2. In another terminal, run the client. s1's action does x = x + 1; do s2.bump;
   under one transaction:

    cargo run -- -f meerkat/tests/dist_s1_client.mkt -i <s2 Service URL>

   Expected: @test(s1) passed (local write committed).

3. Confirm the remote write committed by reading it back in a fresh transaction:

    cargo run -- -f meerkat/tests/dist_check.mkt -i <s2 Service URL>

   Expected: @test(chk) passed (reads s2.w_val == 15).

dist_abort.mkt is the same composition but with a failing assertion; running it
against the s2 server shows the transaction aborting and releasing the remote
lock (a subsequent dist_s1_client.mkt run still succeeds, i.e. no leaked lock).

## Transitive (three nodes)

Demonstrates s1 -> s2 -> s3, where s2's composed action itself composes s3.

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

## Read-your-own-writes across nodes

A transaction must see its own writes reflected in the `def`s derived from
them, including when the write happened on another node. These two run
standalone as a single process (imports resolve from disk) and distributed
against real servers; the assertions are the same either way.

Two remote services, so that refreshing a def after an action on one does not
discard an earlier action on the other:

    cargo run -- -f meerkat/tests/ryow_a.mkt -s -p 9200 --local
    cargo run -- -f meerkat/tests/ryow_b.mkt -s -p 9300 --local
    cargo run -- -f meerkat/tests/dist_ryow_two_remotes.mkt -i <ryow_a URL> -i <ryow_b URL> --local

Expected: @test(two_rem) passed.

A local write after a remote action, so that recomputing the def a second time
does not fall back to the remote service's committed value:

    cargo run -- -f meerkat/tests/ryow_a.mkt -s -p 9200 --local
    cargo run -- -f meerkat/tests/dist_ryow_local_write.mkt -i <ryow_a URL> --local

Expected: @test(loc_write) passed.

The single-process forms need no server:

    cargo run -- -f meerkat/tests/dist_ryow_two_remotes.mkt
    cargo run -- -f meerkat/tests/dist_ryow_local_write.mkt

## Unit tests

The transaction logic also has Rust unit tests (cross-service composition,
read-then-write lock upgrade, nested do, no partial writes on failure):

    cargo test --lib

The transaction-local reactivity rules (derived values refreshed within a
transaction, `var`s staying non-reactive, a failed recompute aborting) have
their own suite:

    cargo test --test txn_reactivity_test
