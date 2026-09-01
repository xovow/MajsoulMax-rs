use anyhow::{Context, Result, ensure};
use prost::Message;
use std::{collections::HashMap, sync::Arc};

use crate::proto::base::BaseMessage;

const HEADER_LEN: usize = 3;

#[derive(Debug, Default)]
pub struct Parser {
    respond_type: HashMap<usize, Arc<str>>,
}

impl Parser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Extracts the RPC method name so the modder can match responses to requests.
    pub fn parse(&mut self, buf: &[u8]) -> Result<Arc<str>> {
        match buf.first() {
            Some(1) => Ok(Arc::from("")),
            Some(2) => self.parse_request(buf),
            Some(3) => self.parse_response(buf),
            Some(msg_type) => anyhow::bail!("Invalid message type: {msg_type}"),
            None => anyhow::bail!("Empty message"),
        }
    }

    fn parse_request(&mut self, buf: &[u8]) -> Result<Arc<str>> {
        ensure!(buf.len() >= HEADER_LEN, "Truncated request message");
        let msg_id = u16::from_le_bytes([buf[1], buf[2]]) as usize;
        let msg_block = BaseMessage::decode(&buf[HEADER_LEN..])?;
        let method_name: Arc<str> = Arc::from(msg_block.method_name);
        self.respond_type.insert(msg_id, method_name.clone());
        Ok(method_name)
    }

    fn parse_response(&mut self, buf: &[u8]) -> Result<Arc<str>> {
        ensure!(buf.len() >= HEADER_LEN, "Truncated response message");
        let msg_id = u16::from_le_bytes([buf[1], buf[2]]) as usize;
        self.respond_type
            .remove(&msg_id)
            .context("No corresponding request")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_bytes(msg_id: u16, method_name: &str) -> Vec<u8> {
        let payload = BaseMessage {
            method_name: method_name.to_string(),
            data: Vec::new(),
        }
        .encode_to_vec();
        let mut buf = Vec::with_capacity(HEADER_LEN + payload.len());
        buf.push(2);
        buf.extend_from_slice(&msg_id.to_le_bytes());
        buf.extend_from_slice(&payload);
        buf
    }

    #[test]
    fn pairs_response_with_request_method() {
        let mut parser = Parser::new();
        let method = parser.parse(&request_bytes(7, ".lq.Lobby.login")).unwrap();
        assert_eq!(&*method, ".lq.Lobby.login");

        let response = [3, 7, 0];
        assert_eq!(&*parser.parse(&response).unwrap(), ".lq.Lobby.login");
    }
}
