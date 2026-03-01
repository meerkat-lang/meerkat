use crate::types::MeerkatMessage;
use futures::{AsyncReadExt, AsyncWriteExt};
use libp2p::StreamProtocol;

/// Protocol identifier for Meerkat messages
pub const MEERKAT_PROTOCOL: StreamProtocol = StreamProtocol::new("/meerkat/1.0.0");

/// Serialize and send a message over a stream
pub async fn send_message(
    stream: &mut (impl AsyncWriteExt + Unpin),
    msg: &MeerkatMessage,
) -> anyhow::Result<()> {
    let data = serde_json::to_vec(msg)?;
    let len = (data.len() as u32).to_be_bytes();
    stream.write_all(&len).await?;
    stream.write_all(&data).await?;
    stream.flush().await?;
    Ok(())
}

/// Receive and deserialize a message from a stream
pub async fn recv_message(
    stream: &mut (impl AsyncReadExt + Unpin),
) -> anyhow::Result<MeerkatMessage> {
    let mut len_bytes = [0u8; 4];
    stream.read_exact(&mut len_bytes).await?;
    let len = u32::from_be_bytes(len_bytes) as usize;
    let mut data = vec![0u8; len];
    stream.read_exact(&mut data).await?;
    let msg = serde_json::from_slice(&data)?;
    Ok(msg)
}
