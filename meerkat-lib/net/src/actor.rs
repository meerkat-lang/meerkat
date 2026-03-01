use crate::{messages::*, types::*, protocol::*};
use futures::AsyncWriteExt;
use kameo::Actor;
use libp2p::{Multiaddr, PeerId};
use libp2p::core::multiaddr::Protocol;
use libp2p_stream as stream;
use futures::StreamExt;
use libp2p::Stream;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc;

#[derive(libp2p::swarm::NetworkBehaviour)]
struct MeerkatBehaviour {
    stream: stream::Behaviour,
    relay: libp2p::relay::Behaviour,
    relay_client: libp2p::relay::client::Behaviour,
    identify: libp2p::identify::Behaviour,
}

enum SwarmCommand {
    Send {
        id: MessageId,
        addr: Address,
        msg: MeerkatMessage,
    },
    Listen {
        addr: Address,
        reply_tx: tokio::sync::oneshot::Sender<Result<Address, String>>,
    },
    ListenViaRelay {
        relay_addr: Address,
        reply_tx: tokio::sync::oneshot::Sender<Result<Address, String>>,
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
async fn build_swarm() -> anyhow::Result<(libp2p::Swarm<MeerkatBehaviour>, PeerId)> {
    use libp2p::identify;
    
    let swarm = libp2p::SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_tcp(
            libp2p::tcp::Config::default(),
            libp2p::noise::Config::new,
            libp2p::yamux::Config::default,
        )?
        .with_websocket(
            libp2p::noise::Config::new,
            libp2p::yamux::Config::default,
        )
        .await?
        .with_relay_client(
            libp2p::noise::Config::new,
            libp2p::yamux::Config::default,
        )?
        .with_behaviour(|keypair, relay_client| {
            let relay_config = libp2p::relay::Config {
                max_reservations: 1000,
                max_circuits: 1000,
                max_circuits_per_peer: 100,
                ..Default::default()
            };
            
            Ok(MeerkatBehaviour {
                stream: stream::Behaviour::new(),
                relay: libp2p::relay::Behaviour::new(
                    keypair.public().to_peer_id(),
                    relay_config,
                ),
                relay_client,
                identify: identify::Behaviour::new(identify::Config::new(
                    "/meerkat/1.0.0".to_string(),
                    keypair.public(),
                )),
            })
        })?
        .with_swarm_config(|c| {
            c.with_idle_connection_timeout(std::time::Duration::from_secs(60))
        })
        .build();
    
    let peer_id = *swarm.local_peer_id();
    Ok((swarm, peer_id))
}

#[cfg(target_arch = "wasm32")]
async fn build_swarm() -> anyhow::Result<(libp2p::Swarm<MeerkatBehaviour>, PeerId)> {
    use libp2p::{core::upgrade, identity, Transport};

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
        relay: libp2p::relay::Behaviour::new(
            local_peer_id,
            relay_config,
        ),
        relay_client,
        identify: libp2p::identify::Behaviour::new(
            libp2p::identify::Config::new(
                "/meerkat/1.0.0".to_string(),
                id_keys.public(),
            )
        ),
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
        let (swarm, local_peer_id) = build_swarm().await?;

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
                let msg_id = MessageId(
                    self.next_message_id.fetch_add(1, Ordering::SeqCst)
                );
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
                let _ = self.command_tx.send(SwarmCommand::Listen {
                    addr,
                    reply_tx,
                });
                match reply_rx.await {
                    Ok(Ok(actual_addr)) => {
                        self.local_addrs.push(actual_addr.clone());
                        NetworkReply::ListenSuccess { addr: actual_addr }
                    }
                    Ok(Err(e)) => NetworkReply::Failure(e),
                    Err(_) => NetworkReply::Failure("Event loop dropped".to_string()),
                }
            }
            NetworkCommand::GetLocalAddresses => {
                NetworkReply::LocalAddresses {
                    addrs: self.local_addrs.clone(),
                }
            }
            NetworkCommand::ListenViaRelay { relay_addr } => {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                let _ = self.command_tx.send(SwarmCommand::ListenViaRelay { 
                    relay_addr, 
                    reply_tx 
                });
                match reply_rx.await {
                    Ok(Ok(circuit_addr)) => {
                        self.local_addrs.push(circuit_addr.clone());
                        NetworkReply::ListenSuccess { addr: circuit_addr }
                    }
                    Ok(Err(e)) => NetworkReply::Failure(e),
                    Err(_) => NetworkReply::Failure("Event loop dropped".to_string()),
                }
            }
        }
    }

    fn translate_address(&self, canonical: &Address) -> anyhow::Result<Address> {
        match &self.node_type {
            NodeType::Server => Ok(canonical.clone()),
            NodeType::BrowserClient { relay_server } => {
                // Check if address already goes through OUR relay
                if canonical.0.starts_with(&relay_server.0) && canonical.0.contains("/p2p-circuit") {
                    // Already using our relay, no translation needed
                    Ok(canonical.clone())
                } else if canonical.0.starts_with("/ip4/") || canonical.0.starts_with("/ip6/") {
                    // Regular IP address or circuit through different relay, add our relay hop
                    Ok(Address::new(format!(
                        "{}/p2p-circuit/{}",
                        relay_server.0,
                        canonical.0
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
        let mut pending_listen: Option<tokio::sync::oneshot::Sender<Result<Address, String>>> = None;
        let mut pending_relay: Option<(Address, tokio::sync::oneshot::Sender<Result<Address, String>>)> = None;

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
                                    if let Err(e) = swarm.listen_on(multiaddr) {
                                        let _ = reply_tx.send(Err(format!("{:?}", e)));
                                    } else {
                                        pending_listen = Some(reply_tx);
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
                                    //println!("Dialing relay at: {}", relay_multiaddr);
                                    if let Err(e) = swarm.dial(relay_multiaddr.clone()) {
                                        let _ = reply_tx.send(Err(format!("Failed to dial relay: {:?}", e)));
                                    } else {
                                        pending_relay = Some((relay_addr, reply_tx));
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
                    tokio::spawn(async move {
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
                let _ = event_tx.send(NetworkEvent::SendFailed {
                    msg_id,
                    error: SendError::UnreachableAddress(addr),
                });
                return;
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
            pending_sends.entry(peer_id).or_default().push((msg_id, msg));
            if let Err(e) = swarm.dial(multiaddr) {
                let _ = event_tx.send(NetworkEvent::SendFailed {
                    msg_id,
                    error: SendError::ProtocolError(format!("Dial failed: {:?}", e)),
                });
                pending_sends.remove(&peer_id);
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
        pending_listen: &mut Option<tokio::sync::oneshot::Sender<Result<Address, String>>>,
        pending_relay: &mut Option<(Address, tokio::sync::oneshot::Sender<Result<Address, String>>)>,
    ) {
        match event {
            libp2p::swarm::SwarmEvent::NewListenAddr { address, .. } => {
                let addr = Address(address.to_string());
                //println!("New listen addr: {}", addr.0);
                
                if addr.0.contains("/p2p-circuit") {
                    //println!("Circuit relay address detected!");
                    if let Some((_, reply_tx)) = pending_relay.take() {
                        //println!("✓ Sending circuit address to pending_relay: {}", addr.0);
                        let _ = reply_tx.send(Ok(addr));
                        return;
                    }
                }
                
                if let Some(tx) = pending_listen.take() {
                    let _ = tx.send(Ok(addr));
                }
            }
            libp2p::swarm::SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                //println!("Connection established with {}", peer_id);
                let _ = event_tx.send(NetworkEvent::PeerConnected {
                    peer: peer_id.to_string(),
                });
                
                if let Some((relay_addr, _)) = pending_relay.as_ref() {
                    if let Ok(relay_multiaddr) = relay_addr.0.parse::<Multiaddr>() {
                        if let Some(relay_peer) = Self::extract_peer_id(&relay_multiaddr) {
                            if relay_peer == peer_id {
                                //println!("Connected to relay {}, now listening via circuit", peer_id);
                                let circuit_listen_addr = relay_multiaddr.with(Protocol::P2pCircuit);
                                //println!("Calling listen_on with: {}", circuit_listen_addr);
                                swarm.listen_on(circuit_listen_addr).ok();
                            }
                        }
                    }
                }
                
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
            }
            libp2p::swarm::SwarmEvent::Behaviour(MeerkatBehaviourEvent::RelayClient(_event)) => {
                //println!("Relay client event: {:?}", event);
            }
            libp2p::swarm::SwarmEvent::Behaviour(MeerkatBehaviourEvent::Relay(_event)) => {
                //println!("Relay server event: {:?}", event);
            }
            libp2p::swarm::SwarmEvent::Behaviour(MeerkatBehaviourEvent::Identify(event)) => {
                if let libp2p::identify::Event::Received { info, .. } = &event {
                    //println!("Adding external address from identify: {}", info.observed_addr);
                    swarm.add_external_address(info.observed_addr.clone());
                }
                //println!("Identify event: {:?}", event);
            }
            _ => {}
        }
    }

    fn extract_peer_id(addr: &Multiaddr) -> Option<PeerId> {
        // For circuit relay addresses, we need the LAST peer ID (the destination)
        // Format: /ip4/.../p2p/RELAY/p2p-circuit/p2p/DEST
        let mut peer_ids: Vec<PeerId> = addr.iter()
            .filter_map(|proto| {
                if let Protocol::P2p(peer_id) = proto {
                    Some(peer_id)
                } else {
                    None
                }
            })
            .collect();
        
        // Return the last peer ID found (for circuits, this is the destination)
        peer_ids.pop()
    }
}

impl crate::network_layer::NetworkLayer for NetworkActor {
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
