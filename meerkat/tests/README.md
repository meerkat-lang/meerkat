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

## Unit tests

The transaction logic also has Rust unit tests (cross-service composition,
read-then-write lock upgrade, nested do, and no partial writes on failure
*within a single transaction* -- note that an `@test` block is not one, so a
`do` that already committed survives a later failure in the block):

    cargo test --lib
