use bytes::{Buf, BytesMut};
use tokio_util::codec::{Decoder, Encoder};
use anyhow::{Result, bail};
use super::messages::{ServerMessage, PeerMessage, MessageDecoder};

/// Codec for server messages
pub struct ServerCodec;

impl Decoder for ServerCodec {
    type Item = ServerMessage;
    type Error = anyhow::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>> {
        ServerMessage::decode(src)
    }
}

impl Encoder<ServerMessage> for ServerCodec {
    type Error = anyhow::Error;

    fn encode(&mut self, item: ServerMessage, dst: &mut BytesMut) -> Result<()> {
        use super::messages::MessageEncoder;
        let bytes = item.encode()?;
        dst.extend_from_slice(&bytes);
        Ok(())
    }
}

/// Codec for peer messages
pub struct PeerCodec;

impl Decoder for PeerCodec {
    type Item = PeerMessage;
    type Error = anyhow::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>> {
        PeerMessage::decode(src)
    }
}

impl Encoder<PeerMessage> for PeerCodec {
    type Error = anyhow::Error;

    fn encode(&mut self, item: PeerMessage, dst: &mut BytesMut) -> Result<()> {
        use super::messages::MessageEncoder;
        let bytes = item.encode()?;
        dst.extend_from_slice(&bytes);
        Ok(())
    }
}
