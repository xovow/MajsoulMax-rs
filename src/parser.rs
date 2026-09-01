use anyhow::{Result, bail, ensure};
use bytes::Bytes;
use prost::Message;

use crate::proto::base::BaseMessage;

/// Request / response frames: a type byte followed by a little-endian u16 ID.
pub(crate) const HEADER_LEN: usize = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MessageKind {
    Notify,
    Request(u16),
    Response(u16),
}

#[derive(Debug)]
pub(crate) struct ParsedMessage {
    pub kind: MessageKind,
    pub envelope: BaseMessage,
}

impl ParsedMessage {
    /// Decode once, retaining a shared slice of the original frame for the payload.
    pub fn decode(buf: &Bytes, from_client: bool) -> Result<Self> {
        let (kind, header_len) = match buf.first() {
            Some(1) => (MessageKind::Notify, 1),
            Some(2) => {
                ensure!(from_client, "Request message came from the server");
                (MessageKind::Request(message_id(buf)?), HEADER_LEN)
            }
            Some(3) => {
                ensure!(!from_client, "Respond message came from the client");
                (MessageKind::Response(message_id(buf)?), HEADER_LEN)
            }
            Some(msg_type) => bail!("Invalid message type: {msg_type}"),
            None => bail!("Empty websocket payload"),
        };
        let envelope = BaseMessage::decode(buf.slice(header_len..))?;
        if matches!(kind, MessageKind::Response(_)) {
            ensure!(
                envelope.method_name.is_empty(),
                "Non-empty respond method name"
            );
        }
        Ok(Self { kind, envelope })
    }
}

fn message_id(buf: &[u8]) -> Result<u16> {
    ensure!(buf.len() >= HEADER_LEN, "Truncated message header");
    Ok(u16::from_le_bytes([buf[1], buf[2]]))
}
