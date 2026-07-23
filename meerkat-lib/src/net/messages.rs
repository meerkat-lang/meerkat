use super::types::*;
use kameo::message::Message;
use kameo::Reply;

/// Messages TO Network Actor (from Manager)
#[derive(Debug)]
// Command payloads intentionally carry a full MeerkatMessage; boxing buys
// nothing for these short-lived channel messages.
#[allow(clippy::large_enum_variant)]
pub enum NetworkCommand {
    SendMessage { addr: Address, msg: MeerkatMessage },
    Listen { addr: Address },
    GetLocalAddresses,
    ListenViaRelay { relay_addr: Address },
}

/// Reply from Network Actor - single unified enum
#[derive(Debug, Reply)]
#[must_use = "network replies contain execution/delivery status and must be handled"]
pub enum NetworkReply {
    MessageSent { msg_id: MessageId },
    ListenSuccess { addr: Address },
    LocalAddresses { addrs: Vec<Address> },
    Failure(String), // renamed from Error to avoid conflict with Reply trait
}

impl Message<NetworkCommand> for super::NetworkActor {
    type Reply = NetworkReply;

    async fn handle(
        &mut self,
        msg: NetworkCommand,
        _ctx: &mut kameo::message::Context<Self, Self::Reply>,
    ) -> Self::Reply {
        self.handle_command(msg).await
    }
}

/// Messages FROM Network Actor TO Manager Actor
#[derive(Debug, Clone)]
pub enum NetworkEvent {
    MessageReceived { peer: String, msg: MeerkatMessage },
    SendFailed { msg_id: MessageId, error: SendError },
    PeerConnected { peer: String },
    PeerDisconnected { peer: String },
}
