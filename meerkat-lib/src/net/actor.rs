use super::{messages::*, protocol::*, types::*};
use futures::AsyncWriteExt;
use futures::StreamExt;
use kameo::Actor;
use libp2p::core::multiaddr::Protocol;
use libp2p::core::transport::ListenerId;
use libp2p::Stream;
use libp2p::{Multiaddr, PeerId};
use libp2p_stream as stream;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc;
use tokio::sync::oneshot::Sender;

/// The connection timeout for idle swarm connections
#[cfg(not(target_arch = "wasm32"))]
const IDLE_CONNECTION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The maximum duration to wait for a relay reservation response
const RELAY_RESERVATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(libp2p::swarm::NetworkBehaviour)]
struct MeerkatBehaviour {
    stream: stream::Behaviour,
    relay: libp2p::relay::Behaviour,
    relay_client: libp2p::relay::client::Behaviour,
    identify: libp2p::identify::Behaviour,
}

/// State of a pending relay reservation attempt
enum PendingRelayState {
    /// Actively dialing the relay server before calling `swarm.listen_on`
    Dialing(Address, Sender<Result<Address, String>>),
    /// Dial completed and currently listening on the circuit relay address
    Listening(Address, Sender<Result<Address, String>>, ListenerId),
}

// Command payloads intentionally carry a full MeerkatMessage; boxing buys
// nothing for these short-lived channel messages.
#[allow(clippy::large_enum_variant)]
enum SwarmCommand {
    Send {
        id: MessageId,
        addr: Address,
        msg: MeerkatMessage,
    },
    Listen {
        addr: Address,
        reply_tx: Sender<Result<Address, String>>,
    },
    ListenViaRelay {
        relay_addr: Address,
        reply_tx: Sender<Result<Address, String>>,
    },
}

#[derive(Actor)]
pub struct NetworkActor {
    next_message_id: AtomicU64,
    local_peer_id: PeerId,
    local_addrs: Vec<Address>,
    node_type: NodeType,
    command_tx: mpsc::UnboundedSender<SwarmCommand>,
    pub event_rx: mpsc::UnboundedReceiver<NetworkEvent>,
}

#[cfg(not(target_arch = "wasm32"))]
async fn build_swarm(
    identity: Option<libp2p::identity::Keypair>,
) -> anyhow::Result<(libp2p::Swarm<MeerkatBehaviour>, PeerId)> {
    use libp2p::identify;

    // #151: use a caller-provided keypair when present (stable Peer ID across
    // restarts); otherwise fall back to a fresh random identity.
    let builder = match identity {
        Some(keypair) => libp2p::SwarmBuilder::with_existing_identity(keypair),
        None => libp2p::SwarmBuilder::with_new_identity(),
    };
    let swarm = builder
        .with_tokio()
        .with_tcp(
            libp2p::tcp::Config::default(),
            libp2p::noise::Config::new,
            libp2p::yamux::Config::default,
        )?
        .with_websocket(libp2p::noise::Config::new, libp2p::yamux::Config::default)
        .await?
        .with_relay_client(libp2p::noise::Config::new, libp2p::yamux::Config::default)?
        .with_behaviour(|keypair, relay_client| {
            let relay_config = libp2p::relay::Config {
                max_reservations: 1000,
                max_circuits: 1000,
                max_circuits_per_peer: 100,
                ..Default::default()
            };

            Ok(MeerkatBehaviour {
                stream: stream::Behaviour::new(),
                relay: libp2p::relay::Behaviour::new(keypair.public().to_peer_id(), relay_config),
                relay_client,
                identify: identify::Behaviour::new(identify::Config::new(
                    "/meerkat/1.0.0".to_string(),
                    keypair.public(),
                )),
            })
        })?
        .with_swarm_config(|c| c.with_idle_connection_timeout(IDLE_CONNECTION_TIMEOUT))
        .build();

    let peer_id = *swarm.local_peer_id();
    Ok((swarm, peer_id))
}

#[cfg(target_arch = "wasm32")]
async fn build_swarm(
    identity_keypair: Option<libp2p::identity::Keypair>,
) -> anyhow::Result<(libp2p::Swarm<MeerkatBehaviour>, PeerId)> {
    use libp2p::{core::upgrade, identity, Transport};

    // #151: browser clients are ephemeral; a persistent identity is a server
    // concern. If one is provided here it is a likely misuse (e.g. the server
    // path being wired into a wasm build), so warn to the JS console but keep
    // going with a fresh ephemeral identity rather than failing.
    if identity_keypair.is_some() {
        web_sys::console::warn_1(&wasm_bindgen::JsValue::from_str(
            "meerkat: a persistent identity was provided to a browser (wasm) \
             client, which always uses an ephemeral identity; the keypair is \
             ignored.",
        ));
    }

    let id_keys = identity::Keypair::generate_ed25519();
    let local_peer_id = PeerId::from(id_keys.public());

    let (relay_transport, relay_client) = libp2p::relay::client::new(local_peer_id);

    let transport = libp2p::websocket_websys::Transport::default()
        .or_transport(relay_transport)
        .upgrade(upgrade::Version::V1)
        .authenticate(libp2p::noise::Config::new(&id_keys)?)
        .multiplex(libp2p::yamux::Config::default())
        .boxed();

    let relay_config = libp2p::relay::Config {
        max_reservations: 1000,
        max_circuits: 1000,
        max_circuits_per_peer: 100,
        ..Default::default()
    };

    let behaviour = MeerkatBehaviour {
        stream: stream::Behaviour::new(),
        relay: libp2p::relay::Behaviour::new(local_peer_id, relay_config),
        relay_client,
        identify: libp2p::identify::Behaviour::new(libp2p::identify::Config::new(
            "/meerkat/1.0.0".to_string(),
            id_keys.public(),
        )),
    };

    let swarm = libp2p::Swarm::new(
        transport,
        behaviour,
        local_peer_id,
        libp2p::swarm::Config::with_wasm_executor(),
    );

    Ok((swarm, local_peer_id))
}

#[cfg(not(target_arch = "wasm32"))]
fn spawn_event_loop(fut: impl std::future::Future<Output = ()> + Send + 'static) {
    tokio::spawn(fut);
}

#[cfg(target_arch = "wasm32")]
fn spawn_event_loop(fut: impl std::future::Future<Output = ()> + 'static) {
    wasm_bindgen_futures::spawn_local(fut);
}

impl NetworkActor {
    pub async fn new(node_type: NodeType) -> anyhow::Result<Self> {
        Self::new_with_identity(node_type, None).await
    }

    // #151: like `new`, but uses a caller-provided keypair when present so the
    // node keeps a stable Peer ID across restarts. A `None` identity yields a
    // fresh random one, matching `new`.
    pub async fn new_with_identity(
        node_type: NodeType,
        identity: Option<libp2p::identity::Keypair>,
    ) -> anyhow::Result<Self> {
        let (swarm, local_peer_id) = build_swarm(identity).await?;

        let (command_tx, command_rx) = mpsc::unbounded_channel::<SwarmCommand>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<NetworkEvent>();

        spawn_event_loop(Self::event_loop(swarm, command_rx, event_tx));

        Ok(Self {
            next_message_id: AtomicU64::new(1),
            local_peer_id,
            local_addrs: Vec::new(),
            node_type,
            command_tx,
            event_rx,
        })
    }

    pub fn local_peer_id(&self) -> String {
        self.local_peer_id.to_string()
    }

    pub async fn handle_command(&mut self, cmd: NetworkCommand) -> NetworkReply {
        match cmd {
            NetworkCommand::SendMessage { addr, msg } => {
                let msg_id = MessageId(self.next_message_id.fetch_add(1, Ordering::SeqCst));
                let local_addr = match self.translate_address(&addr) {
                    Ok(a) => a,
                    Err(e) => return NetworkReply::Failure(e.to_string()),
                };
                let _ = self.command_tx.send(SwarmCommand::Send {
                    id: msg_id,
                    addr: local_addr,
                    msg,
                });
                NetworkReply::MessageSent { msg_id }
            }
            NetworkCommand::Listen { addr } => {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                let _ = self
                    .command_tx
                    .send(SwarmCommand::Listen { addr, reply_tx });
                match reply_rx.await {
                    Ok(Ok(actual_addr)) => {
                        self.local_addrs.push(actual_addr.clone());
                        NetworkReply::ListenSuccess { addr: actual_addr }
                    }
                    Ok(Err(e)) => NetworkReply::Failure(e),
                    Err(_) => NetworkReply::Failure("Event loop dropped".to_string()),
                }
            }
            NetworkCommand::GetLocalAddresses => NetworkReply::LocalAddresses {
                addrs: self.local_addrs.clone(),
            },
            NetworkCommand::ListenViaRelay { relay_addr } => {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                let _ = self.command_tx.send(SwarmCommand::ListenViaRelay {
                    relay_addr,
                    reply_tx,
                });
                match tokio::time::timeout(RELAY_RESERVATION_TIMEOUT, reply_rx).await {
                    Ok(Ok(Ok(addr))) => {
                        self.local_addrs.push(addr.clone());
                        NetworkReply::ListenSuccess { addr }
                    }
                    Ok(Ok(Err(e))) => NetworkReply::Failure(e),
                    Ok(Err(_)) => NetworkReply::Failure("Event loop dropped".to_string()),
                    Err(_) => {
                        NetworkReply::Failure("Timeout waiting for relay reservation".to_string())
                    }
                }
            }
        }
    }

    fn translate_address(&self, canonical: &Address) -> anyhow::Result<Address> {
        match &self.node_type {
            NodeType::Server => Ok(canonical.clone()),
            NodeType::BrowserClient { relay_server } => {
                // Check if address already goes through OUR relay
                if canonical.0.starts_with(&relay_server.0) && canonical.0.contains("/p2p-circuit")
                {
                    // Already using our relay, no translation needed
                    Ok(canonical.clone())
                } else if canonical.0.starts_with("/ip4/") || canonical.0.starts_with("/ip6/") {
                    // Regular IP address or circuit through different relay, add our relay hop
                    Ok(Address::new(format!(
                        "{}/p2p-circuit/{}",
                        relay_server.0, canonical.0
                    )))
                } else {
                    Ok(canonical.clone())
                }
            }
        }
    }

    pub fn translate_address_pub(&self, canonical: &Address) -> Address {
        self.translate_address(canonical).unwrap()
    }
}

impl NetworkActor {
    async fn event_loop(
        mut swarm: libp2p::Swarm<MeerkatBehaviour>,
        mut command_rx: mpsc::UnboundedReceiver<SwarmCommand>,
        event_tx: mpsc::UnboundedSender<NetworkEvent>,
    ) {
        let mut control = swarm.behaviour().stream.new_control();
        let mut incoming = control.accept(MEERKAT_PROTOCOL).unwrap();
        let mut pending_sends: HashMap<PeerId, Vec<(MessageId, MeerkatMessage)>> = HashMap::new();
        let mut pending_listen: Option<(ListenerId, Sender<Result<Address, String>>)> = None;
        let mut pending_relay: Option<PendingRelayState> = None;

        loop {
            tokio::select! {
                Some(cmd) = command_rx.recv() => {
                    match cmd {
                        SwarmCommand::Send { id, addr, msg } => {
                            Self::do_send(
                                &mut swarm,
                                &mut control,
                                &mut pending_sends,
                                &event_tx,
                                id,
                                addr,
                                msg,
                            ).await;
                        }
                        SwarmCommand::Listen { addr, reply_tx } => {
                            match addr.0.parse::<Multiaddr>() {
                                Ok(multiaddr) => {
                                    match swarm.listen_on(multiaddr) {
                                        Ok(listener_id) => {
                                            pending_listen = Some((listener_id, reply_tx));
                                        }
                                        Err(e) => {
                                            let _ = reply_tx.send(Err(format!("{:?}", e)));
                                        }
                                    }
                                }
                                Err(e) => {
                                    let _ = reply_tx.send(Err(format!("Invalid address: {}", e)));
                                }
                            }
                        }
                        SwarmCommand::ListenViaRelay { relay_addr, reply_tx } => {
                            //println!("ListenViaRelay command received for relay: {}", relay_addr.0);
                            match relay_addr.0.parse::<Multiaddr>() {
                                Ok(relay_multiaddr) => {
                                    if let Some(relay_peer) = Self::extract_peer_id(&relay_multiaddr) {
                                        if swarm.is_connected(&relay_peer) {
                                            // Already connected listen via relay immediately
                                            let circuit_listen_addr = relay_multiaddr.with(Protocol::P2pCircuit);
                                            match swarm.listen_on(circuit_listen_addr) {
                                                Ok(listener_id) => {
                                                    pending_relay = Some(PendingRelayState::Listening(
                                                        relay_addr,
                                                        reply_tx,
                                                        listener_id,
                                                    ));
                                                }
                                                Err(e) => {
                                                    let _ = reply_tx.send(Err(format!("Failed to listen on relay: {:?}", e)));
                                                }
                                            }
                                        } else {
                                            // Not connected dial the relay
                                            if let Err(e) = swarm.dial(relay_multiaddr.clone()) {
                                                let _ = reply_tx.send(Err(format!("Failed to dial relay: {:?}", e)));
                                            } else {
                                                pending_relay = Some(PendingRelayState::Dialing(
                                                    relay_addr,
                                                    reply_tx,
                                                ));
                                            }
                                        }
                                    } else {
                                        let _ = reply_tx.send(Err("No peer ID in relay address".to_string()));
                                    }
                                }
                                Err(e) => {
                                    let _ = reply_tx.send(Err(format!("Invalid relay address: {}", e)));
                                }
                            }
                        }
                    }
                }

                Some((peer, mut stream)) = incoming.next() => {
                    let event_tx = event_tx.clone();
                    // #39: spawn via the platform-split helper. tokio::spawn
                    // panics ("no reactor running") on wasm, which has no tokio
                    // runtime; spawn_event_loop uses spawn_local there.
                    spawn_event_loop(async move {
                        Self::handle_incoming(peer, &mut stream, event_tx).await;
                    });
                }

                event = swarm.next() => {
                    if let Some(event) = event {
                        Self::handle_swarm_event(
                            event,
                            &mut swarm,
                            &mut control,
                            &mut pending_sends,
                            &event_tx,
                            &mut pending_listen,
                            &mut pending_relay,
                        ).await;
                    }
                }
            }
        }
    }

    async fn do_send(
        swarm: &mut libp2p::Swarm<MeerkatBehaviour>,
        control: &mut stream::Control,
        pending_sends: &mut HashMap<PeerId, Vec<(MessageId, MeerkatMessage)>>,
        event_tx: &mpsc::UnboundedSender<NetworkEvent>,
        msg_id: MessageId,
        addr: Address,
        msg: MeerkatMessage,
    ) {
        let multiaddr = match addr.0.parse::<Multiaddr>() {
            Ok(m) => m,
            Err(_) => {
                let mut parsed = None;
                if let Some(pos) = addr.0.rfind("/p2p/") {
                    let after_p2p = &addr.0[pos + 5..];
                    if let Some(slash_idx) = after_p2p.find('/') {
                        let trimmed = &addr.0[..pos + 5 + slash_idx];
                        if let Ok(m) = trimmed.parse::<Multiaddr>() {
                            parsed = Some(m);
                        }
                    }
                }
                match parsed {
                    Some(m) => m,
                    None => {
                        let _ = event_tx.send(NetworkEvent::SendFailed {
                            msg_id,
                            error: SendError::UnreachableAddress(addr),
                        });
                        return;
                    }
                }
            }
        };

        let peer_id = match Self::extract_peer_id(&multiaddr) {
            Some(id) => id,
            None => {
                let _ = event_tx.send(NetworkEvent::SendFailed {
                    msg_id,
                    error: SendError::ProtocolError("No peer ID in address".to_string()),
                });
                return;
            }
        };

        if swarm.is_connected(&peer_id) {
            Self::send_to_peer(control, peer_id, msg_id, msg, event_tx).await;
        } else {
            // Track if we are already dialing this `peer_id` inside `pending_sends` to prevent duplicate dials
            let is_dialing = pending_sends.contains_key(&peer_id);
            pending_sends
                .entry(peer_id)
                .or_default()
                .push((msg_id, msg));

            if !is_dialing {
                // Call `swarm.dial` for the target peer since no active dial request exists
                if let Err(e) = swarm.dial(multiaddr) {
                    let _ = event_tx.send(NetworkEvent::SendFailed {
                        msg_id,
                        error: SendError::ProtocolError(format!("Dial failed: {:?}", e)),
                    });
                    pending_sends.remove(&peer_id);
                }
            }
        }
    }

    async fn send_to_peer(
        control: &mut stream::Control,
        peer: PeerId,
        msg_id: MessageId,
        msg: MeerkatMessage,
        event_tx: &mpsc::UnboundedSender<NetworkEvent>,
    ) {
        match control.open_stream(peer, MEERKAT_PROTOCOL).await {
            Ok(mut stream) => {
                if let Err(e) = send_message(&mut stream, &msg).await {
                    let _ = event_tx.send(NetworkEvent::SendFailed {
                        msg_id,
                        error: SendError::ProtocolError(format!("Send failed: {}", e)),
                    });
                }
                let _ = stream.close().await;
            }
            Err(e) => {
                let _ = event_tx.send(NetworkEvent::SendFailed {
                    msg_id,
                    error: SendError::ProtocolError(format!("Stream open: {:?}", e)),
                });
            }
        }
    }

    async fn handle_incoming(
        peer: PeerId,
        stream: &mut Stream,
        event_tx: mpsc::UnboundedSender<NetworkEvent>,
    ) {
        match recv_message(stream).await {
            Ok(msg) => {
                let _ = event_tx.send(NetworkEvent::MessageReceived {
                    peer: peer.to_string(),
                    msg,
                });
            }
            Err(e) => {
                eprintln!("Failed to receive message from {}: {}", peer, e);
            }
        }
        let _ = stream.close().await;
    }

    async fn handle_swarm_event(
        event: libp2p::swarm::SwarmEvent<MeerkatBehaviourEvent>,
        swarm: &mut libp2p::Swarm<MeerkatBehaviour>,
        control: &mut stream::Control,
        pending_sends: &mut HashMap<PeerId, Vec<(MessageId, MeerkatMessage)>>,
        event_tx: &mpsc::UnboundedSender<NetworkEvent>,
        pending_listen: &mut Option<(ListenerId, Sender<Result<Address, String>>)>,
        pending_relay: &mut Option<PendingRelayState>,
    ) {
        match event {
            libp2p::swarm::SwarmEvent::NewListenAddr {
                listener_id,
                address,
            } => {
                swarm.add_external_address(address.clone());
                let addr = Address(address.to_string());

                // Check if this `NewListenAddr` belongs to our `pending_relay` reservation
                let mut matched_relay = false;
                if let Some(PendingRelayState::Listening(_, _, expected_id)) = pending_relay {
                    if *expected_id == listener_id {
                        matched_relay = true;
                    }
                }

                // If matched resolve the `pending_relay` oneshot channel and clear the state
                if matched_relay {
                    if let Some(PendingRelayState::Listening(_, reply_tx, _)) = pending_relay.take()
                    {
                        let _ = reply_tx.send(Ok(addr));
                        return;
                    }
                }

                // Check if this address belongs to a `pending_listen` attempt
                let mut matched_listen = false;
                if let Some((expected_id, _)) = pending_listen {
                    if *expected_id == listener_id {
                        matched_listen = true;
                    }
                }

                // If matched resolve the `pending_listen` oneshot channel
                if matched_listen {
                    if let Some((_, tx)) = pending_listen.take() {
                        let _ = tx.send(Ok(addr));
                    }
                }
            }
            libp2p::swarm::SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                let _ = event_tx.send(NetworkEvent::PeerConnected {
                    peer: peer_id.to_string(),
                });

                // Check if we were actively dialing the relay server and the dial completed
                let mut to_listen = None;
                if let Some(PendingRelayState::Dialing(relay_addr, _)) = pending_relay {
                    if let Ok(relay_multiaddr) = relay_addr.0.parse::<Multiaddr>() {
                        if let Some(relay_peer) = Self::extract_peer_id(&relay_multiaddr) {
                            if relay_peer == peer_id {
                                let circuit_listen_addr =
                                    relay_multiaddr.with(Protocol::P2pCircuit);
                                to_listen = Some(circuit_listen_addr);
                            }
                        }
                    }
                }

                // If dialing completed start listening on the relay circuit immediately
                if let Some(circuit_listen_addr) = to_listen {
                    match swarm.listen_on(circuit_listen_addr) {
                        Ok(listener_id) => {
                            // Transition `pending_relay` from `Dialing` to `Listening` with the `ListenerId`
                            if let Some(PendingRelayState::Dialing(relay_addr, reply_tx)) =
                                pending_relay.take()
                            {
                                *pending_relay = Some(PendingRelayState::Listening(
                                    relay_addr,
                                    reply_tx,
                                    listener_id,
                                ));
                            }
                        }
                        Err(e) => {
                            // If `swarm.listen_on` failed synchronously resolve the channel with an error
                            if let Some(PendingRelayState::Dialing(_, reply_tx)) =
                                pending_relay.take()
                            {
                                let _ = reply_tx
                                    .send(Err(format!("Failed to listen on relay: {:?}", e)));
                            }
                        }
                    }
                }

                // Send any queued messages in `pending_sends` waiting for connection establishment
                if let Some(messages) = pending_sends.remove(&peer_id) {
                    for (msg_id, msg) in messages {
                        Self::send_to_peer(control, peer_id, msg_id, msg, event_tx).await;
                    }
                }
            }
            libp2p::swarm::SwarmEvent::ConnectionClosed { peer_id, .. } => {
                let _ = event_tx.send(NetworkEvent::PeerDisconnected {
                    peer: peer_id.to_string(),
                });

                // If connection to the expected relay closed fail the `pending_relay` attempt
                if Self::get_pending_relay_peer(pending_relay) == Some(peer_id) {
                    if let Some(state) = pending_relay.take() {
                        let tx = match state {
                            PendingRelayState::Dialing(_, tx) => tx,
                            PendingRelayState::Listening(_, tx, _) => tx,
                        };
                        let _ = tx.send(Err("Relay connection closed".to_string()));
                    }
                }
            }
            libp2p::swarm::SwarmEvent::Behaviour(MeerkatBehaviourEvent::RelayClient(event)) => {
                log::debug!("Relay client event: {:?}", event);
            }
            libp2p::swarm::SwarmEvent::Behaviour(MeerkatBehaviourEvent::Relay(_event)) => {
                //println!("Relay server event: {:?}", event);
            }
            libp2p::swarm::SwarmEvent::Behaviour(MeerkatBehaviourEvent::Identify(
                libp2p::identify::Event::Received { info, .. },
            )) => {
                swarm.add_external_address(info.observed_addr.clone());
            }
            libp2p::swarm::SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                // Check if this `OutgoingConnectionError` matches our `pending_relay` peer
                let matches_relay = Self::get_pending_relay_peer(pending_relay)
                    .is_some_and(|relay_peer| peer_id == Some(relay_peer));

                // Fail the `pending_relay` reservation if it matches the dial error
                if matches_relay {
                    if let Some(state) = pending_relay.take() {
                        let tx = match state {
                            PendingRelayState::Dialing(_, tx) => tx,
                            PendingRelayState::Listening(_, tx, _) => tx,
                        };
                        let _ = tx.send(Err(format!("Relay dial failed: {:?}", error)));
                    }
                }

                // Fail any message sends in `pending_sends` that were queued up waiting for this connection
                if let Some(peer_id) = peer_id {
                    if let Some(messages) = pending_sends.remove(&peer_id) {
                        for (msg_id, _) in messages {
                            let _ = event_tx.send(NetworkEvent::SendFailed {
                                msg_id,
                                error: SendError::ProtocolError(format!(
                                    "Outgoing connection failed: {:?}",
                                    error
                                )),
                            });
                        }
                    }
                }
            }
            libp2p::swarm::SwarmEvent::ListenerError {
                listener_id, error, ..
            } => {
                // Check if this error belongs to our `pending_listen` attempt
                let mut matched_listen = false;
                if let Some((expected_id, _)) = pending_listen {
                    if *expected_id == listener_id {
                        matched_listen = true;
                    }
                }
                if matched_listen {
                    if let Some((_, tx)) = pending_listen.take() {
                        let _ = tx.send(Err(format!("Listener error: {:?}", error)));
                    }
                }

                // Check if this error belongs to our `pending_relay` listener
                let mut matched_relay = false;
                if let Some(PendingRelayState::Listening(_, _, expected_id)) = pending_relay {
                    if *expected_id == listener_id {
                        matched_relay = true;
                    }
                }
                if matched_relay {
                    if let Some(PendingRelayState::Listening(_, tx, _)) = pending_relay.take() {
                        let _ = tx.send(Err(format!("Relay listener error: {:?}", error)));
                    }
                }
            }
            libp2p::swarm::SwarmEvent::ListenerClosed {
                listener_id,
                reason,
                ..
            } => {
                // Check if this closed listener belongs to our `pending_relay` listener
                let mut matched_relay = false;
                if let Some(PendingRelayState::Listening(_, _, expected_id)) = pending_relay {
                    if *expected_id == listener_id {
                        matched_relay = true;
                    }
                }
                if matched_relay {
                    if let Some(PendingRelayState::Listening(_, tx, _)) = pending_relay.take() {
                        let _ = tx.send(Err(format!("Relay listener closed: {:?}", reason)));
                    }
                }
            }
            _ => {}
        }
    }

    fn extract_peer_id(addr: &Multiaddr) -> Option<PeerId> {
        // For circuit relay addresses we need the last `PeerId` which is the destination
        // Format: /ip4/.../p2p/RELAY/p2p-circuit/p2p/DEST
        let mut peer_ids: Vec<PeerId> = addr
            .iter()
            .filter_map(|proto| {
                if let Protocol::P2p(peer_id) = proto {
                    Some(peer_id)
                } else {
                    None
                }
            })
            .collect();

        // Return the last `PeerId` found for circuits as the destination
        peer_ids.pop()
    }

    /// Extracts the `PeerId` of the expected relay server from a `PendingRelayState`
    fn get_pending_relay_peer(pending_relay: &Option<PendingRelayState>) -> Option<PeerId> {
        let relay_addr = match pending_relay.as_ref()? {
            PendingRelayState::Dialing(addr, _) => addr,
            PendingRelayState::Listening(addr, _, _) => addr,
        };
        let multiaddr = relay_addr.0.parse::<Multiaddr>().ok()?;
        Self::extract_peer_id(&multiaddr)
    }
}

impl super::network_layer::NetworkLayer for NetworkActor {
    async fn handle_command(&mut self, cmd: NetworkCommand) -> NetworkReply {
        self.handle_command(cmd).await
    }

    fn local_peer_id(&self) -> String {
        self.local_peer_id()
    }

    fn try_recv_event(&mut self) -> Option<NetworkEvent> {
        self.event_rx.try_recv().ok()
    }
}
