use meerkat_net::*;
use tokio::time::{sleep, Duration};

#[tokio::test(flavor = "multi_thread")]
async fn test_send_and_receive() {
    let mut server = NetworkActor::new(NodeType::Server).await.unwrap();

    let reply = server.handle_command(NetworkCommand::Listen {
        addr: Address::new("/ip4/127.0.0.1/tcp/0"),
    }).await;

    let server_addr = match reply {
        NetworkReply::ListenSuccess { addr } => addr,
        other => panic!("Expected ListenSuccess, got {:?}", other),
    };

    let server_peer_id = server.local_peer_id();
    let full_addr = Address::new(format!("{}/p2p/{}", server_addr.0, server_peer_id));
    println!("Server full address: {}", full_addr.0);

    let mut client = NetworkActor::new(NodeType::Server).await.unwrap();

    let send_reply = client.handle_command(NetworkCommand::SendMessage {
        addr: full_addr,
        msg: MeerkatMessage::Ping {
            content: "hello from client".to_string(),
        },
    }).await;

    println!("Send reply: {:?}", send_reply);

    let mut received = false;
    for _ in 0..50 {
        sleep(Duration::from_millis(100)).await;
        if let Ok(event) = server.event_rx.try_recv() {
            println!("Server got event: {:?}", event);
            if let NetworkEvent::MessageReceived { msg, .. } = event {
                if let MeerkatMessage::Ping { content } = msg {
                    assert_eq!(content, "hello from client");
                    received = true;
                    break;
                }
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
    let canonical = Address::new("/ip4/203.0.113.10/tcp/9000/p2p/12D3KooWXXX/p2p-circuit/p2p/12D3KooWYYY");
    let translated = server.translate_address_pub(&canonical);
    assert_eq!(translated.0, canonical.0);
    println!("✓ Server address translation test passed!");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_translate_address_browser_client() {
    let relay = Address::new("/ip4/server1-ip/tcp/9001/ws/p2p/12D3KooWSERVER1");
    let client = NetworkActor::new(NodeType::BrowserClient {
        relay_server: relay.clone(),
    }).await.unwrap();

    let canonical = Address::new("/ip4/203.0.113.10/tcp/9000/p2p/12D3KooWSERVER2/p2p-circuit/p2p/12D3KooWCLIENT2");
    let translated = client.translate_address_pub(&canonical);

    let expected = format!("{}/p2p-circuit/{}", relay.0, canonical.0);
    assert_eq!(translated.0, expected);
    println!("✓ Browser client address translation test passed!");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_multiple_messages() {
    let mut server = NetworkActor::new(NodeType::Server).await.unwrap();

    let reply = server.handle_command(NetworkCommand::Listen {
        addr: Address::new("/ip4/127.0.0.1/tcp/0"),
    }).await;

    let server_addr = match reply {
        NetworkReply::ListenSuccess { addr } => addr,
        other => panic!("Expected ListenSuccess, got {:?}", other),
    };

    let server_peer_id = server.local_peer_id();
    let full_addr = Address::new(format!("{}/p2p/{}", server_addr.0, server_peer_id));

    let mut client = NetworkActor::new(NodeType::Server).await.unwrap();

    for i in 0..5 {
        client.handle_command(NetworkCommand::SendMessage {
            addr: full_addr.clone(),
            msg: MeerkatMessage::Ping {
                content: format!("Message {}", i),
            },
        }).await;
    }

    let mut received = 0;
    for _ in 0..100 {
        sleep(Duration::from_millis(100)).await;
        while let Ok(event) = server.event_rx.try_recv() {
            if let NetworkEvent::MessageReceived { .. } = event {
                received += 1;
            }
        }
        if received >= 5 { break; }
    }

    assert_eq!(received, 5, "Server should have received all 5 messages, got {}", received);
    println!("✓ Multiple messages test passed!");
}

// ── Mock network tests ────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn test_mock_send_and_receive() {
    let registry = MockNetwork::new_registry();

    let mut server = MockNetwork::new_with_registry(registry.clone());
    let mut client = MockNetwork::new_with_registry(registry.clone());

    // Listen to get a routable address
    let reply = server.handle_command(NetworkCommand::Listen {
        addr: Address::new("/ip4/127.0.0.1/tcp/9000"),
    }).await;

    let server_addr = match reply {
        NetworkReply::ListenSuccess { addr } => addr,
        other => panic!("Expected ListenSuccess, got {:?}", other),
    };

    println!("Mock server address: {}", server_addr.0);

    // Send from client to server
    client.handle_command(NetworkCommand::SendMessage {
        addr: server_addr,
        msg: MeerkatMessage::Ping {
            content: "hello from mock client".to_string(),
        },
    }).await;

    // Message should be delivered instantly — no sleep needed
    let event = server.event_rx.try_recv().expect("Server should have received a message");

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

    let reply = server.handle_command(NetworkCommand::Listen {
        addr: Address::new("/ip4/127.0.0.1/tcp/9000"),
    }).await;

    let server_addr = match reply {
        NetworkReply::ListenSuccess { addr } => addr,
        other => panic!("Expected ListenSuccess, got {:?}", other),
    };

    for i in 0..5 {
        client.handle_command(NetworkCommand::SendMessage {
            addr: server_addr.clone(),
            msg: MeerkatMessage::Ping {
                content: format!("Message {}", i),
            },
        }).await;
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

    client.handle_command(NetworkCommand::SendMessage {
        addr: Address::new("/ip4/127.0.0.1/tcp/9000/p2p/nonexistent-peer"),
        msg: MeerkatMessage::Ping {
            content: "this should fail".to_string(),
        },
    }).await;

    let event = client.event_rx.try_recv().expect("Should have received SendFailed");
    assert!(
        matches!(event, NetworkEvent::SendFailed { .. }),
        "Expected SendFailed, got {:?}", event
    );
    println!("✓ Mock unreachable address test passed!");
}

// ── NetworkLayer trait tests ──────────────────────────────────────────────────

async fn send_ping_via_trait<N: meerkat_net::NetworkLayer>(
    sender: &mut N,
    addr: Address,
) {
    sender.handle_command(NetworkCommand::SendMessage {
        addr,
        msg: MeerkatMessage::Ping {
            content: "via trait".to_string(),
        },
    }).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_trait_with_mock() {
    let registry = MockNetwork::new_registry();
    let mut server = MockNetwork::new_with_registry(registry.clone());
    let mut client = MockNetwork::new_with_registry(registry.clone());

    let reply = server.handle_command(NetworkCommand::Listen {
        addr: Address::new("/ip4/127.0.0.1/tcp/9000"),
    }).await;

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

    let reply = server.handle_command(NetworkCommand::Listen {
        addr: Address::new("/ip4/127.0.0.1/tcp/0"),
    }).await;

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
        if let Some(event) = server.try_recv_event() {
            if let NetworkEvent::MessageReceived { .. } = event {
                received = true;
                break;
            }
        }
    }

    assert!(received, "Server never received the ping via trait");
    println!("✓ Trait with real network test passed!");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_circuit_relay() {
    use std::time::Duration;
    use tokio::time::sleep;

    // ── Step 1: Start relay server ────────────────────────────────────────────
    let mut relay_server = NetworkActor::new(NodeType::Server).await.unwrap();

    let relay_listen_reply = relay_server.handle_command(NetworkCommand::Listen {
        addr: Address::new("/ip4/127.0.0.1/tcp/0"),
    }).await;

    let relay_addr = match relay_listen_reply {
        NetworkReply::ListenSuccess { addr } => addr,
        other => panic!("Relay listen failed: {:?}", other),
    };

    let relay_full_addr = Address::new(format!("{}/p2p/{}", relay_addr.0, relay_server.local_peer_id()));
    println!("Relay server address: {}", relay_full_addr.0);

    // ── Step 2: Start client2, get circuit relay address ─────────────────────
    let mut client2 = NetworkActor::new(NodeType::BrowserClient {
        relay_server: relay_full_addr.clone(),
    }).await.unwrap();

    let _client2_peer_id = client2.local_peer_id();

    let circuit_reply = client2.handle_command(NetworkCommand::ListenViaRelay {
        relay_addr: relay_full_addr.clone(),
    }).await;

    let client2_circuit_addr = match circuit_reply {
        NetworkReply::ListenSuccess { addr } => addr,
        other => panic!("Circuit relay listen failed: {:?}", other),
    };

    println!("client2 circuit relay address: {}", client2_circuit_addr.0);
    assert!(
        client2_circuit_addr.0.contains("p2p-circuit"),
        "Expected circuit relay address, got: {}", client2_circuit_addr.0
    );

    // Wait for relay server to confirm reservation before client1 dials through it
    sleep(Duration::from_secs(2)).await;
    while let Ok(e) = relay_server.event_rx.try_recv() {
        println!("relay server event (pre-send): {:?}", e);
    }
    println!("Starting client1 send...");

    // ── Step 3: Start client1, also a relay client (needs relay transport to dial circuit addrs)
    let mut client1 = NetworkActor::new(NodeType::BrowserClient {
        relay_server: relay_full_addr.clone(),
    }).await.unwrap();

    // Wait for client1 to finish identify with relay before sending via circuit
    sleep(Duration::from_secs(3)).await;

    client1.handle_command(NetworkCommand::SendMessage {
        addr: client2_circuit_addr.clone(),
        msg: MeerkatMessage::Ping { content: "hello via relay".to_string() },
    }).await;

    // ── Step 4: Poll until client2 receives the message ──────────────────────
    let mut received = false;
    for _ in 0..300 {
        sleep(Duration::from_millis(200)).await;

        while let Ok(e) = client2.event_rx.try_recv() {
            println!("client2 got: {:?}", e);
            if let NetworkEvent::MessageReceived { msg: MeerkatMessage::Ping { content }, .. } = e {
                if content == "hello via relay" {
                    received = true;
                }
            }
        }

        while let Ok(e) = relay_server.event_rx.try_recv() {
            println!("relay server event: {:?}", e);
        }

        while let Ok(e) = client1.event_rx.try_recv() {
            println!("client1 event: {:?}", e);
        }

        if received { break; }
    }

    assert!(received, "client2 never received the message via circuit relay");
    println!("✓ Circuit relay test passed!");
}
