use crate::messages::*;

/// The interface the Manager uses to talk to the network layer.
/// Both NetworkActor (real libp2p) and MockNetwork (in-memory) implement this.
#[allow(async_fn_in_trait)]
pub trait NetworkLayer {
    async fn handle_command(&mut self, cmd: NetworkCommand) -> NetworkReply;
    fn local_peer_id(&self) -> String;
    fn try_recv_event(&mut self) -> Option<NetworkEvent>;
}
