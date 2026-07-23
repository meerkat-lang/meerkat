use meerkat_lib::net::*;
use tokio::time::{sleep, Duration};

/// #39: unique temp directory per test invocation, so file-serving tests do
/// not share filesystem state.
fn unique_test_dir(label: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "meerkat_it_{}_{}_{}_{}",
        label,
        std::process::id(),
        nanos,
        n
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test(flavor = "multi_thread")]
async fn test_send_and_receive() {
    let mut server = NetworkActor::new(NodeType::Server).await.unwrap();

    let reply = server
        .handle_command(NetworkCommand::Listen {
            addr: Address::new("/ip4/127.0.0.1/tcp/0"),
        })
        .await;

    let server_addr = match reply {
        NetworkReply::ListenSuccess { addr } => addr,
        other => panic!("Expected ListenSuccess, got {:?}", other),
    };

    let server_peer_id = server.local_peer_id();
    let full_addr = Address::new(format!("{}/p2p/{}", server_addr.0, server_peer_id));
    println!("Server full address: {}", full_addr.0);

    let mut client = NetworkActor::new(NodeType::Server).await.unwrap();

    let send_reply = client
        .handle_command(NetworkCommand::SendMessage {
            addr: full_addr,
            msg: MeerkatMessage::Ping {
                content: "hello from client".to_string(),
            },
        })
        .await;

    println!("Send reply: {:?}", send_reply);

    let mut received = false;
    for _ in 0..50 {
        sleep(Duration::from_millis(100)).await;
        if let Ok(event) = server.event_rx.try_recv() {
            println!("Server got event: {:?}", event);
            if let NetworkEvent::MessageReceived {
                msg: MeerkatMessage::Ping { content },
                ..
            } = event
            {
                assert_eq!(content, "hello from client");
                received = true;
                break;
            }
        }
    }

    assert!(received, "Server never received the ping");
    println!("✓ Server-to-server test passed!");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_translate_address_server() {
    let server = NetworkActor::new(NodeType::Server).await.unwrap();
    // Server should use canonical address directly - no translation
    let canonical =
        Address::new("/ip4/203.0.113.10/tcp/9000/p2p/12D3KooWXXX/p2p-circuit/p2p/12D3KooWYYY");
    let translated = server.translate_address_pub(&canonical);
    assert_eq!(translated.0, canonical.0);
    println!("✓ Server address translation test passed!");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_translate_address_browser_client() {
    let relay = Address::new("/ip4/server1-ip/tcp/9001/ws/p2p/12D3KooWSERVER1");
    let client = NetworkActor::new(NodeType::BrowserClient {
        relay_server: relay.clone(),
    })
    .await
    .unwrap();

    let canonical = Address::new(
        "/ip4/203.0.113.10/tcp/9000/p2p/12D3KooWSERVER2/p2p-circuit/p2p/12D3KooWCLIENT2",
    );
    let translated = client.translate_address_pub(&canonical);

    let expected = format!("{}/p2p-circuit/{}", relay.0, canonical.0);
    assert_eq!(translated.0, expected);
    println!("✓ Browser client address translation test passed!");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_multiple_messages() {
    let mut server = NetworkActor::new(NodeType::Server).await.unwrap();

    let reply = server
        .handle_command(NetworkCommand::Listen {
            addr: Address::new("/ip4/127.0.0.1/tcp/0"),
        })
        .await;

    let server_addr = match reply {
        NetworkReply::ListenSuccess { addr } => addr,
        other => panic!("Expected ListenSuccess, got {:?}", other),
    };

    let server_peer_id = server.local_peer_id();
    let full_addr = Address::new(format!("{}/p2p/{}", server_addr.0, server_peer_id));

    let mut client = NetworkActor::new(NodeType::Server).await.unwrap();

    for i in 0..5 {
        client
            .handle_command(NetworkCommand::SendMessage {
                addr: full_addr.clone(),
                msg: MeerkatMessage::Ping {
                    content: format!("Message {}", i),
                },
            })
            .await;
    }

    let mut received = 0;
    let _ = tokio::time::timeout(tokio::time::Duration::from_secs(10), async {
        while let Some(event) = server.event_rx.recv().await {
            if let NetworkEvent::MessageReceived { .. } = event {
                received += 1;
                if received >= 5 {
                    break;
                }
            }
        }
    })
    .await;

    assert_eq!(
        received, 5,
        "Server should have received all 5 messages, got {}",
        received
    );
    println!("✓ Multiple messages test passed!");
}

// ── Mock network tests ────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_mock_send_and_receive() {
    let registry = MockNetwork::new_registry();

    let mut server = MockNetwork::new_with_registry(registry.clone());
    let mut client = MockNetwork::new_with_registry(registry.clone());

    // Listen to get a routable address
    let reply = server
        .handle_command(NetworkCommand::Listen {
            addr: Address::new("/ip4/127.0.0.1/tcp/9000"),
        })
        .await;

    let server_addr = match reply {
        NetworkReply::ListenSuccess { addr } => addr,
        other => panic!("Expected ListenSuccess, got {:?}", other),
    };

    println!("Mock server address: {}", server_addr.0);

    // Send from client to server
    client
        .handle_command(NetworkCommand::SendMessage {
            addr: server_addr,
            msg: MeerkatMessage::Ping {
                content: "hello from mock client".to_string(),
            },
        })
        .await;

    // Message should be delivered instantly — no sleep needed
    let event = server
        .event_rx
        .try_recv()
        .expect("Server should have received a message");

    if let NetworkEvent::MessageReceived { msg, .. } = event {
        if let MeerkatMessage::Ping { content } = msg {
            assert_eq!(content, "hello from mock client");
            println!("✓ Mock send and receive test passed!");
        }
    } else {
        panic!("Expected MessageReceived event");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_mock_multiple_messages() {
    let registry = MockNetwork::new_registry();
    let mut server = MockNetwork::new_with_registry(registry.clone());
    let mut client = MockNetwork::new_with_registry(registry.clone());

    let reply = server
        .handle_command(NetworkCommand::Listen {
            addr: Address::new("/ip4/127.0.0.1/tcp/9000"),
        })
        .await;

    let server_addr = match reply {
        NetworkReply::ListenSuccess { addr } => addr,
        other => panic!("Expected ListenSuccess, got {:?}", other),
    };

    for i in 0..5 {
        client
            .handle_command(NetworkCommand::SendMessage {
                addr: server_addr.clone(),
                msg: MeerkatMessage::Ping {
                    content: format!("Message {}", i),
                },
            })
            .await;
    }

    let mut received = 0;
    while let Ok(event) = server.event_rx.try_recv() {
        if let NetworkEvent::MessageReceived { .. } = event {
            received += 1;
        }
    }

    assert_eq!(received, 5, "Expected 5 messages, got {}", received);
    println!("✓ Mock multiple messages test passed!");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_mock_unreachable_address() {
    let mut client = MockNetwork::new();

    client
        .handle_command(NetworkCommand::SendMessage {
            addr: Address::new("/ip4/127.0.0.1/tcp/9000/p2p/nonexistent-peer"),
            msg: MeerkatMessage::Ping {
                content: "this should fail".to_string(),
            },
        })
        .await;

    let event = client
        .event_rx
        .try_recv()
        .expect("Should have received SendFailed");
    assert!(
        matches!(event, NetworkEvent::SendFailed { .. }),
        "Expected SendFailed, got {:?}",
        event
    );
    println!("✓ Mock unreachable address test passed!");
}

// ── NetworkLayer trait tests ──────────────────────────────────────────────────

async fn send_ping_via_trait<N: meerkat_lib::net::NetworkLayer>(sender: &mut N, addr: Address) {
    sender
        .handle_command(NetworkCommand::SendMessage {
            addr,
            msg: MeerkatMessage::Ping {
                content: "via trait".to_string(),
            },
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_trait_with_mock() {
    let registry = MockNetwork::new_registry();
    let mut server = MockNetwork::new_with_registry(registry.clone());
    let mut client = MockNetwork::new_with_registry(registry.clone());

    let reply = server
        .handle_command(NetworkCommand::Listen {
            addr: Address::new("/ip4/127.0.0.1/tcp/9000"),
        })
        .await;

    let server_addr = match reply {
        NetworkReply::ListenSuccess { addr } => addr,
        other => panic!("Expected ListenSuccess, got {:?}", other),
    };

    send_ping_via_trait(&mut client, server_addr).await;

    let event = server.try_recv_event().expect("Should have received event");
    assert!(matches!(event, NetworkEvent::MessageReceived { .. }));
    println!("✓ Trait with mock test passed!");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_trait_with_real_network() {
    let mut server = NetworkActor::new(NodeType::Server).await.unwrap();

    let reply = server
        .handle_command(NetworkCommand::Listen {
            addr: Address::new("/ip4/127.0.0.1/tcp/0"),
        })
        .await;

    let server_addr = match reply {
        NetworkReply::ListenSuccess { addr } => addr,
        other => panic!("Expected ListenSuccess, got {:?}", other),
    };

    let full_addr = Address::new(format!("{}/p2p/{}", server_addr.0, server.local_peer_id()));

    let mut client = NetworkActor::new(NodeType::Server).await.unwrap();
    send_ping_via_trait(&mut client, full_addr).await;

    let mut received = false;
    for _ in 0..50 {
        sleep(Duration::from_millis(100)).await;
        if let Some(NetworkEvent::MessageReceived { .. }) = server.try_recv_event() {
            received = true;
            break;
        }
    }

    assert!(received, "Server never received the ping via trait");
    println!("✓ Trait with real network test passed!");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_circuit_relay() {
    use std::time::Duration;
    use tokio::time::sleep;

    // Start relay server
    let mut relay_server = NetworkActor::new(NodeType::Server).await.unwrap();

    let relay_listen_reply = relay_server
        .handle_command(NetworkCommand::Listen {
            addr: Address::new("/ip4/127.0.0.1/tcp/0"),
        })
        .await;

    let relay_addr = match relay_listen_reply {
        NetworkReply::ListenSuccess { addr } => addr,
        other => panic!("relay listen failed: {:?}", other),
    };

    let relay_full_addr = Address::new(format!(
        "{}/p2p/{}",
        relay_addr.0,
        relay_server.local_peer_id()
    ));
    println!("relay server: {}", relay_full_addr.0);

    // Start `client2` and register circuit relay reservation
    let mut client2 = NetworkActor::new(NodeType::BrowserClient {
        relay_server: relay_full_addr.clone(),
    })
    .await
    .unwrap();

    let circuit_reply = client2
        .handle_command(NetworkCommand::ListenViaRelay {
            relay_addr: relay_full_addr.clone(),
        })
        .await;

    let client2_circuit_addr = match circuit_reply {
        NetworkReply::ListenSuccess { addr } => addr,
        other => panic!("`ListenViaRelay` failed: {:?}", other),
    };

    println!("client2 circuit addr: {}", client2_circuit_addr.0);
    assert!(
        client2_circuit_addr.0.contains("p2p-circuit"),
        "expected circuit relay address, got: {}",
        client2_circuit_addr.0
    );

    // Wait for relay server to confirm `client2` connection before `client1` dials
    let mut client2_connected = false;
    for _ in 0..50 {
        while let Ok(e) = relay_server.event_rx.try_recv() {
            println!("relay server: {:?}", e);
            if let NetworkEvent::PeerConnected { peer } = &e {
                if *peer == client2.local_peer_id() {
                    client2_connected = true;
                }
            }
        }
        if client2_connected {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    assert!(
        client2_connected,
        "`client2` never connected to the relay server"
    );

    // Start `client1` and send to `client2` through the relay.
    // `client1` does not auto-dial the relay on creation; the relay transport
    // connects as part of dialling the circuit address in `SendMessage`
    let mut client1 = NetworkActor::new(NodeType::BrowserClient {
        relay_server: relay_full_addr.clone(),
    })
    .await
    .unwrap();

    // Retry send with a hard 10-second timeout (5 attempts x 10 polls x 100ms)
    let received = tokio::time::timeout(Duration::from_secs(10), async {
        for attempt in 0..5usize {
            client1
                .handle_command(NetworkCommand::SendMessage {
                    addr: client2_circuit_addr.clone(),
                    msg: MeerkatMessage::Ping {
                        content: "hello via relay".to_string(),
                    },
                })
                .await;

            for _ in 0..10usize {
                sleep(Duration::from_millis(100)).await;

                while let Ok(e) = client2.event_rx.try_recv() {
                    println!("client2: {:?}", e);
                    if let NetworkEvent::MessageReceived {
                        msg: MeerkatMessage::Ping { content },
                        ..
                    } = e
                    {
                        if content == "hello via relay" {
                            return true;
                        }
                    }
                }

                // Drain other queues to keep event loops responsive
                while let Ok(e) = relay_server.event_rx.try_recv() {
                    println!("relay server: {:?}", e);
                }
                while let Ok(e) = client1.event_rx.try_recv() {
                    println!("client1: {:?}", e);
                }
            }
            println!("attempt {} failed, retrying", attempt + 1);
        }
        false
    })
    .await
    .unwrap_or(false);

    assert!(
        received,
        "client2 never received the message via circuit relay"
    );

    // Send to a peer not registered with the relay; the relay rejects the circuit,
    // producing `OutgoingConnectionError`. Verify `SendFailed` is emitted
    let fake_peer = Address::new(format!(
        "{}/p2p-circuit/p2p/12D3KooW8Zr3nQ7mL4xK9vJ2pY6sF1gT5hR",
        relay_full_addr.0,
    ));
    client1
        .handle_command(NetworkCommand::SendMessage {
            addr: fake_peer,
            msg: MeerkatMessage::Ping {
                content: "should fail".to_string(),
            },
        })
        .await;

    let send_failed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            while let Ok(e) = client1.event_rx.try_recv() {
                if matches!(e, NetworkEvent::SendFailed { .. }) {
                    return true;
                }
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or(false);
    assert!(
        send_failed,
        "`SendFailed` not received after `OutgoingConnectionError`"
    );

    // Verify `ListenViaRelay` on an unreachable relay returns `Failure` promptly
    let dead_relay = Address::new("/ip4/127.0.0.1/tcp/19998/p2p/12D3KooW4nR7qL2mK8vJ3pY9sF6gT1hX");
    let mut orphan = NetworkActor::new(NodeType::BrowserClient {
        relay_server: dead_relay.clone(),
    })
    .await
    .unwrap();

    let relay_reply = tokio::time::timeout(
        Duration::from_secs(5),
        orphan.handle_command(NetworkCommand::ListenViaRelay {
            relay_addr: dead_relay,
        }),
    )
    .await
    .expect("`ListenViaRelay` on unreachable relay must not hang");

    assert!(
        matches!(relay_reply, NetworkReply::Failure(_)),
        "`ListenViaRelay` on unreachable relay returned {:?}, expected `Failure`",
        relay_reply
    );
    println!("circuit relay test passed");
}

/// #39: whole-file round-trip of the service-code protocol over the mock
/// network. A client requests a file by path; the server side validates the
/// request and replies with the whole file source (not a single sliced
/// service), which the client receives. The client then processes that source
/// through the normal program-loading path (exercised elsewhere); here we
/// assert the transport and whole-file return.
#[tokio::test(flavor = "multi_thread")]
async fn test_service_code_request_roundtrip() {
    use meerkat_lib::net::codec::serve_service_code;

    let registry = MockNetwork::new_registry();
    let mut server = MockNetwork::new_with_registry(registry.clone());
    let mut client = MockNetwork::new_with_registry(registry.clone());

    let server_reply = server
        .handle_command(NetworkCommand::Listen {
            addr: Address::new("/ip4/127.0.0.1/tcp/9000"),
        })
        .await;
    let server_addr = match server_reply {
        NetworkReply::ListenSuccess { addr } => addr,
        other => panic!("Expected ListenSuccess, got {:?}", other),
    };
    let client_reply = client
        .handle_command(NetworkCommand::Listen {
            addr: Address::new("/ip4/127.0.0.1/tcp/9001"),
        })
        .await;
    let client_addr = match client_reply {
        NetworkReply::ListenSuccess { addr } => addr,
        other => panic!("Expected ListenSuccess, got {:?}", other),
    };

    // The server serves files from a base directory; write the requested
    // .mkt file there so the server can resolve and read it.
    let served_dir = unique_test_dir("roundtrip");
    let served_source = "service counter { pub var count = 0; }\nservice other { var z = 1; }";
    std::fs::write(served_dir.join("counter.mkt"), served_source).unwrap();

    client
        .handle_command(NetworkCommand::SendMessage {
            addr: server_addr,
            msg: MeerkatMessage::ServiceCodeRequest {
                request_id: 1,
                path: "counter.mkt".to_string(),
                reply_to: client_addr.0.clone(),
            },
        })
        .await;

    let event = server
        .event_rx
        .try_recv()
        .expect("Server should have received the code request");
    let (request_id, path, reply_to) = match event {
        NetworkEvent::MessageReceived {
            msg:
                MeerkatMessage::ServiceCodeRequest {
                    request_id,
                    path,
                    reply_to,
                },
            ..
        } => (request_id, path, reply_to),
        other => panic!("Expected ServiceCodeRequest, got {:?}", other),
    };

    // Server serves the file via the same helper run_server uses (validate,
    // resolve the path against the base dir, read the file).
    let response = serve_service_code(request_id, path, &reply_to, &served_dir);

    server
        .handle_command(NetworkCommand::SendMessage {
            addr: Address::new(&reply_to),
            msg: response,
        })
        .await;

    let event = client
        .event_rx
        .try_recv()
        .expect("Client should have received the code response");
    match event {
        NetworkEvent::MessageReceived {
            msg: MeerkatMessage::ServiceCodeResponse { source, .. },
            ..
        } => {
            // The whole file is returned, both services included.
            assert_eq!(source, served_source);
        }
        other => panic!("Expected ServiceCodeResponse, got {:?}", other),
    }

    let _ = std::fs::remove_dir_all(&served_dir);
}

/// #39: validation error path. A client sends a request whose path exceeds the
/// length limit; the server side rejects it via validate_service_code_request
/// and replies with a ServiceCodeError, which the client receives.
#[tokio::test(flavor = "multi_thread")]
async fn test_service_code_request_rejects_oversized_path() {
    use meerkat_lib::net::codec::serve_service_code;
    use meerkat_lib::runtime::limits::MAX_NET_REQUEST_STRING_LENGTH;

    let registry = MockNetwork::new_registry();
    let mut server = MockNetwork::new_with_registry(registry.clone());
    let mut client = MockNetwork::new_with_registry(registry.clone());

    let server_reply = server
        .handle_command(NetworkCommand::Listen {
            addr: Address::new("/ip4/127.0.0.1/tcp/9002"),
        })
        .await;
    let server_addr = match server_reply {
        NetworkReply::ListenSuccess { addr } => addr,
        other => panic!("Expected ListenSuccess, got {:?}", other),
    };
    let client_reply = client
        .handle_command(NetworkCommand::Listen {
            addr: Address::new("/ip4/127.0.0.1/tcp/9003"),
        })
        .await;
    let client_addr = match client_reply {
        NetworkReply::ListenSuccess { addr } => addr,
        other => panic!("Expected ListenSuccess, got {:?}", other),
    };

    let oversized_path = "a".repeat(MAX_NET_REQUEST_STRING_LENGTH + 1);

    client
        .handle_command(NetworkCommand::SendMessage {
            addr: server_addr,
            msg: MeerkatMessage::ServiceCodeRequest {
                request_id: 2,
                path: oversized_path,
                reply_to: client_addr.0.clone(),
            },
        })
        .await;

    let event = server
        .event_rx
        .try_recv()
        .expect("Server should have received the code request");
    let (request_id, path, reply_to) = match event {
        NetworkEvent::MessageReceived {
            msg:
                MeerkatMessage::ServiceCodeRequest {
                    request_id,
                    path,
                    reply_to,
                },
            ..
        } => (request_id, path, reply_to),
        other => panic!("Expected ServiceCodeRequest, got {:?}", other),
    };

    // Validation rejects the oversized path before any file I/O, so the base
    // dir does not need to contain anything.
    let served_dir = unique_test_dir("oversized");
    let response = serve_service_code(request_id, path, &reply_to, &served_dir);
    let _ = std::fs::remove_dir_all(&served_dir);

    server
        .handle_command(NetworkCommand::SendMessage {
            addr: Address::new(&reply_to),
            msg: response,
        })
        .await;

    let event = client
        .event_rx
        .try_recv()
        .expect("Client should have received the code error");
    match event {
        NetworkEvent::MessageReceived {
            msg: MeerkatMessage::ServiceCodeError { error, .. },
            ..
        } => {
            assert!(
                error.contains("path"),
                "error should mention path: {}",
                error
            );
        }
        other => panic!("Expected ServiceCodeError, got {:?}", other),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_lock_request_roundtrip() {
    use meerkat_lib::net::{Address, LockGroup, MeerkatMessage};
    use meerkat_lib::runtime::txn::TxnId;
    use std::collections::{HashMap, HashSet};

    let registry = MockNetwork::new_registry();
    let mut server = MockNetwork::new_with_registry(registry.clone());
    let mut client = MockNetwork::new_with_registry(registry.clone());

    let server_reply = server
        .handle_command(NetworkCommand::Listen {
            addr: Address::new("/ip4/127.0.0.1/tcp/9000"),
        })
        .await;
    let server_addr = match server_reply {
        NetworkReply::ListenSuccess { addr } => addr,
        other => panic!("Expected ListenSuccess, got {:?}", other),
    };
    let client_reply = client
        .handle_command(NetworkCommand::Listen {
            addr: Address::new("/ip4/127.0.0.1/tcp/9001"),
        })
        .await;
    let client_addr = match client_reply {
        NetworkReply::ListenSuccess { addr } => addr,
        other => panic!("Expected ListenSuccess, got {:?}", other),
    };

    let txn_id = TxnId {
        timestamp: 100,
        node_id: 1,
        iteration: 0,
    };

    let mut services = HashMap::new();
    services.insert(
        "SvcA".to_string(),
        LockGroup {
            service_level_lock: true,
            reads: HashSet::from(["x".to_string()]),
            writes: HashSet::from(["y".to_string()]),
        },
    );

    // Send LockRequest from client to server
    client
        .handle_command(NetworkCommand::SendMessage {
            addr: server_addr.clone(),
            msg: MeerkatMessage::LockRequest {
                request_id: 42,
                txn_id: txn_id.clone(),
                services: services.clone(),
                reply_to: client_addr.0.clone(),
            },
        })
        .await;

    // Server receives LockRequest
    let event = server
        .event_rx
        .try_recv()
        .expect("Server should have received LockRequest");

    let (req_id, recv_txn_id, recv_services, reply_to) = match event {
        NetworkEvent::MessageReceived {
            msg:
                MeerkatMessage::LockRequest {
                    request_id,
                    txn_id,
                    services,
                    reply_to,
                },
            ..
        } => (request_id, txn_id, services, reply_to),
        other => panic!("Expected LockRequest, got {:?}", other),
    };

    assert_eq!(req_id, 42);
    assert_eq!(recv_txn_id, txn_id);
    assert_eq!(recv_services, services);
    assert_eq!(reply_to, client_addr.0);

    // Server sends LockResponse back to client
    server
        .handle_command(NetworkCommand::SendMessage {
            addr: Address::new(&reply_to),
            msg: MeerkatMessage::LockResponse {
                request_id: req_id,
                txn_id: recv_txn_id,
                success: true,
                error: None,
            },
        })
        .await;

    // Client receives LockResponse
    let event = client
        .event_rx
        .try_recv()
        .expect("Client should have received LockResponse");

    match event {
        NetworkEvent::MessageReceived {
            msg:
                MeerkatMessage::LockResponse {
                    request_id,
                    txn_id: resp_txn_id,
                    success,
                    error,
                },
            ..
        } => {
            assert_eq!(request_id, 42);
            assert_eq!(resp_txn_id, txn_id);
            assert!(success);
            assert!(error.is_none());
        }
        other => panic!("Expected LockResponse, got {:?}", other),
    }
}

/// Verify that a LockRequest with invalid service or member identifiers is rejected over network
#[tokio::test(flavor = "multi_thread")]
async fn test_lock_request_invalid_identifier_rejected_over_network() {
    use meerkat_lib::net::{Address, LockGroup, MeerkatMessage};
    use meerkat_lib::runtime::txn::TxnId;
    use std::collections::{HashMap, HashSet};

    let registry = MockNetwork::new_registry();
    let mut server = MockNetwork::new_with_registry(registry.clone());
    let mut client = MockNetwork::new_with_registry(registry.clone());

    let server_reply = server
        .handle_command(NetworkCommand::Listen {
            addr: Address::new("/ip4/127.0.0.1/tcp/9100"),
        })
        .await;
    let server_addr = match server_reply {
        NetworkReply::ListenSuccess { addr } => addr,
        other => panic!("Expected ListenSuccess, got {:?}", other),
    };
    let client_reply = client
        .handle_command(NetworkCommand::Listen {
            addr: Address::new("/ip4/127.0.0.1/tcp/9101"),
        })
        .await;
    let client_addr = match client_reply {
        NetworkReply::ListenSuccess { addr } => addr,
        other => panic!("Expected ListenSuccess, got {:?}", other),
    };

    let txn_id = TxnId::new(1);
    let mut services = HashMap::new();
    services.insert(
        "invalid-service-name!".to_string(),
        LockGroup {
            service_level_lock: false,
            reads: HashSet::new(),
            writes: HashSet::new(),
        },
    );

    // Send LockRequest from client to server
    client
        .handle_command(NetworkCommand::SendMessage {
            addr: server_addr.clone(),
            msg: MeerkatMessage::LockRequest {
                request_id: 100,
                txn_id: txn_id.clone(),
                services: services.clone(),
                reply_to: client_addr.0.clone(),
            },
        })
        .await;

    // Server receives LockRequest
    let event = server
        .event_rx
        .try_recv()
        .expect("Server should have received LockRequest");

    let (req_id, recv_txn_id, recv_services, reply_to) = match event {
        NetworkEvent::MessageReceived {
            msg:
                MeerkatMessage::LockRequest {
                    request_id,
                    txn_id,
                    services,
                    reply_to,
                },
            ..
        } => (request_id, txn_id, services, reply_to),
        other => panic!("Expected LockRequest, got {:?}", other),
    };

    // Route through server dispatch logic (validating & replying)
    let response = MeerkatMessage::LockResponse {
        request_id: req_id,
        txn_id: recv_txn_id,
        success: false,
        error: codec::validate_lock_request(&recv_services)
            .err()
            .map(|e| e.to_string()),
    };

    server
        .handle_command(NetworkCommand::SendMessage {
            addr: Address::new(&reply_to),
            msg: response,
        })
        .await;

    // Client receives LockResponse over network
    let event = client
        .event_rx
        .try_recv()
        .expect("Client should have received LockResponse");

    match event {
        NetworkEvent::MessageReceived {
            msg:
                MeerkatMessage::LockResponse {
                    request_id,
                    txn_id: resp_txn_id,
                    success: false,
                    error: Some(err),
                },
            ..
        } => {
            assert_eq!(request_id, 100);
            assert_eq!(resp_txn_id, txn_id);
            assert!(err.contains("service"));
        }
        other => panic!("Expected failure LockResponse, got {:?}", other),
    }
}

/// Verify that codec request validation helper functions reject invalid network inputs
#[test]
fn test_codec_request_validation() {
    use meerkat_lib::net::LockGroup;
    use std::collections::{HashMap, HashSet};

    let mut invalid_svc_map = HashMap::new();
    invalid_svc_map.insert(
        "invalid-service!".to_string(),
        LockGroup {
            service_level_lock: false,
            reads: HashSet::new(),
            writes: HashSet::new(),
        },
    );

    assert!(codec::validate_lock_request(&invalid_svc_map).is_err());
    assert!(codec::validate_lookup_request("invalid-service!", "member").is_err());
    assert!(codec::validate_action_request("invalid-service!").is_err());
}

/// Verify that on_node_startup preserves the remote peer ID string on buffered MessageReceived events
#[tokio::test(flavor = "multi_thread")]
async fn test_on_node_startup_preserves_buffered_peer_id() {
    use meerkat_lib::runtime::node::Node;
    use std::collections::HashMap;

    let mut node = Node::new();

    let dir = unique_test_dir("node_startup_peer_id");
    let file_path = dir.join("test.mkt");
    std::fs::write(&file_path, "service LocalSvc { pub def val = 1; }").unwrap();

    let remote_map = HashMap::new();
    let file_str = file_path.to_str().unwrap();

    let identity = libp2p::identity::Keypair::generate_ed25519();
    let (opt_net, _buffered_events, _) = node
        .on_node_startup(file_str, remote_map, Some(identity))
        .await
        .expect("on_node_startup succeeds");

    assert!(opt_net.is_some());
    let mut server = opt_net.unwrap();

    let server_peer_id = server.local_peer_id();
    let addrs_reply = server
        .handle_command(NetworkCommand::GetLocalAddresses)
        .await;
    let server_addr = match addrs_reply {
        NetworkReply::LocalAddresses { addrs } => addrs[0].clone(),
        other => panic!("Expected LocalAddresses, got {:?}", other),
    };

    let full_addr = Address::new(format!("{}/p2p/{}", server_addr.0, server_peer_id));

    let mut client = NetworkActor::new(NodeType::Server).await.unwrap();
    let client_peer_id = client.local_peer_id().to_string();

    let _ = client
        .handle_command(NetworkCommand::SendMessage {
            addr: full_addr,
            msg: MeerkatMessage::Ping {
                content: "ping_test".to_string(),
            },
        })
        .await;

    let mut recvd_peer = String::new();
    for _ in 0..50 {
        sleep(Duration::from_millis(100)).await;
        if let Ok(NetworkEvent::MessageReceived { peer, .. }) = server.event_rx.try_recv() {
            recvd_peer = peer;
            break;
        }
    }

    assert!(!recvd_peer.is_empty(), "Peer ID should not be empty");
    assert_eq!(recvd_peer, client_peer_id);
}

/// Verify that run_static_checks_with_imports fetches remote service
/// dependencies over libp2p network and executes static checks
#[tokio::test(flavor = "multi_thread")]
async fn test_static_checks_remote_imports() {
    use meerkat_lib::runtime::node::Node;
    use std::collections::HashMap;

    let dir_remote = unique_test_dir("remote_svc_dir");
    let remote_file = dir_remote.join("RemoteSvc.mkt");
    std::fs::write(
        &remote_file,
        "service RemoteSvc {\n    pub def val = 42;\n}",
    )
    .unwrap();

    let mut server = NetworkActor::new(NodeType::Server).await.unwrap();
    let reply = server
        .handle_command(NetworkCommand::Listen {
            addr: Address::new("/ip4/127.0.0.1/tcp/0"),
        })
        .await;

    let server_addr = match reply {
        NetworkReply::ListenSuccess { addr } => addr,
        other => panic!("Expected ListenSuccess, got {:?}", other),
    };

    let server_peer_id = server.local_peer_id();
    let full_addr = format!("{}/p2p/{}", server_addr.0, server_peer_id);

    let served_dir = dir_remote.clone();
    tokio::spawn(async move {
        loop {
            if let Ok(NetworkEvent::MessageReceived {
                msg:
                    MeerkatMessage::ServiceCodeRequest {
                        request_id,
                        path,
                        reply_to,
                    },
                ..
            }) = server.event_rx.try_recv()
            {
                let response = codec::serve_service_code(request_id, path, &reply_to, &served_dir);
                server
                    .handle_command(NetworkCommand::SendMessage {
                        addr: Address::new(&reply_to),
                        msg: response,
                    })
                    .await;
            }
            sleep(Duration::from_millis(10)).await;
        }
    });

    let dir_local = unique_test_dir("local_svc_dir");
    let main_file = dir_local.join("main.mkt");
    std::fs::write(
        &main_file,
        "import RemoteSvc\nservice MainSvc {\n    pub def res = RemoteSvc.val;\n}",
    )
    .unwrap();

    let mut remote_url_map = HashMap::new();
    remote_url_map.insert("RemoteSvc".to_string(), full_addr);

    let mut node = Node::new();
    let file_str = main_file.to_str().unwrap();

    let res = node
        .resolve_imports(file_str, remote_url_map)
        .await
        .expect("resolve_imports failed")
        .static_checks();
    assert!(res.is_ok(), "static_checks failed: {:?}", res);
}
