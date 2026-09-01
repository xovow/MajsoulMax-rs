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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_payload_shares_the_original_frame() {
        let payload = Bytes::from(vec![0x5a; 4096]);
        let envelope = BaseMessage {
            method_name: ".lq.Lobby.login".into(),
            data: payload.clone(),
        };
        let mut wire = vec![2, 7, 0];
        envelope.encode(&mut wire).unwrap();
        let wire = Bytes::from(wire);

        let parsed = ParsedMessage::decode(&wire, true).unwrap();
        assert_eq!(parsed.kind, MessageKind::Request(7));
        assert_eq!(parsed.envelope.method_name, ".lq.Lobby.login");
        assert_eq!(parsed.envelope.data, payload);
        assert_eq!(
            parsed.envelope.data.as_ptr(),
            wire[wire.len() - payload.len()..].as_ptr()
        );
    }

    #[test]
    fn parses_response_and_notification_headers() {
        let response = ParsedMessage::decode(&Bytes::from_static(&[3, 0xff, 0xff]), false).unwrap();
        assert_eq!(response.kind, MessageKind::Response(u16::MAX));
        let notification = ParsedMessage::decode(&Bytes::from_static(&[1]), false).unwrap();
        assert_eq!(notification.kind, MessageKind::Notify);
    }

    #[test]
    fn rejects_truncated_malformed_and_wrong_direction_frames() {
        for wire in [vec![], vec![2], vec![2, 0], vec![3], vec![3, 0], vec![4]] {
            for from_client in [false, true] {
                assert!(ParsedMessage::decode(&Bytes::from(wire.clone()), from_client).is_err());
            }
        }
        assert!(ParsedMessage::decode(&Bytes::from_static(&[2, 0, 0]), false).is_err());
        assert!(ParsedMessage::decode(&Bytes::from_static(&[3, 0, 0]), true).is_err());
        assert!(ParsedMessage::decode(&Bytes::from_static(&[2, 0, 0, 0x0a, 0xff]), true).is_err());
        let response_with_name = Bytes::from_static(&[3, 0, 0, 0x0a, 1, b'x']);
        assert!(ParsedMessage::decode(&response_with_name, false).is_err());
    }
}
