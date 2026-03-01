use crate::{messages::*, types::*};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// Shared registry mapping peer_id -> event sender
/// Allows mock peers to deliver messages directly to each other
type Registry = Arc<Mutex<HashMap<String, mpsc::UnboundedSender<NetworkEvent>>>>;

pub struct MockNetwork {
    peer_id: String,
    local_addrs: Vec<Address>,
    registry: Registry,
    next_message_id: u64,
    pub event_rx: mpsc::UnboundedReceiver<NetworkEvent>,
}

impl MockNetwork {
    /// Create a standalone mock node with its own registry
    pub fn new() -> Self {
        let registry = Arc::new(Mutex::new(HashMap::new()));
        Self::new_with_registry(registry)
    }

    /// Create a mock node sharing a registry with other nodes
    /// — nodes on the same registry can exchange messages
    pub fn new_with_registry(registry: Registry) -> Self {
        let peer_id = format!("mock-peer-{}", uuid_simple());
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        registry.lock().unwrap().insert(peer_id.clone(), event_tx);
        Self {
            peer_id,
            local_addrs: Vec::new(),
            registry,
            next_message_id: 1,
            event_rx,
        }
    }

    /// Create a shared registry — pass to multiple MockNetwork::new_with_registry
    pub fn new_registry() -> Registry {
        Arc::new(Mutex::new(HashMap::new()))
    }

    pub fn local_peer_id(&self) -> String {
        self.peer_id.clone()
    }

    pub async fn handle_command(&mut self, cmd: NetworkCommand) -> NetworkReply {
        match cmd {
            NetworkCommand::Listen { addr } => {
                // Mock listen — just record the address, no real binding
                let bound = Address::new(format!("{}/p2p/{}", addr.0, self.peer_id));
                self.local_addrs.push(bound.clone());
                NetworkReply::ListenSuccess { addr: bound }
            }

            NetworkCommand::SendMessage { addr, msg } => {
                let msg_id = MessageId(self.next_message_id);
                self.next_message_id += 1;

                // Extract peer id from address (last /p2p/<id> segment)
                let target_peer = extract_peer_id_from_addr(&addr.0);

                match target_peer {
                    Some(peer) => {
                        let registry = self.registry.lock().unwrap();
                        match registry.get(&peer) {
                            Some(tx) => {
                                let _ = tx.send(NetworkEvent::MessageReceived {
                                    peer: self.peer_id.clone(),
                                    msg,
                                });
                            }
                            None => {
                                // Peer not found — fire SendFailed
                                drop(registry);
                                let event_tx = {
                                    let reg = self.registry.lock().unwrap();
                                    reg.get(&self.peer_id).cloned()
                                };
                                if let Some(tx) = event_tx {
                                    let _ = tx.send(NetworkEvent::SendFailed {
                                        msg_id,
                                        error: SendError::UnreachableAddress(addr),
                                    });
                                }
                            }
                        }
                    }
                    None => {
                        let event_tx = {
                            let reg = self.registry.lock().unwrap();
                            reg.get(&self.peer_id).cloned()
                        };
                        if let Some(tx) = event_tx {
                            let _ = tx.send(NetworkEvent::SendFailed {
                                msg_id,
                                error: SendError::ProtocolError(
                                    "No peer ID in address".to_string()
                                ),
                            });
                        }
                    }
                }

                NetworkReply::MessageSent { msg_id }
            }

            NetworkCommand::ListenViaRelay { .. } => {
                NetworkReply::Failure("Mock does not support relay".to_string())
            }
            NetworkCommand::GetLocalAddresses => {
                NetworkReply::LocalAddresses {
                    addrs: self.local_addrs.clone(),
                }
            }
        }
    }
}

fn extract_peer_id_from_addr(addr: &str) -> Option<String> {
    // Find last /p2p/<id> segment
    let parts: Vec<&str> = addr.split('/').collect();
    for i in 0..parts.len() {
        if parts[i] == "p2p" && i + 1 < parts.len() {
            return Some(parts[i + 1].to_string());
        }
    }
    None
}

fn uuid_simple() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    format!("{:08x}", t)
}

impl crate::network_layer::NetworkLayer for MockNetwork {
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
