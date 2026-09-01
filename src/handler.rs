use anyhow::Result;
use bytes::Bytes;
use hudsucker::{
    Body, HttpContext, HttpHandler, WebSocketContext, WebSocketHandler,
    futures::{Sink, SinkExt, Stream, StreamExt},
    hyper::Request,
    tokio_tungstenite::tungstenite::{self, Message},
};
use std::{future::pending, sync::Arc};
use tokio::sync::mpsc;

use crate::{
    connections::{ConnectionKey, ConnectionState, Connections},
    modder::Modder,
    parser::{MessageKind, ParsedMessage},
};

#[derive(Clone)]
pub struct Handler {
    modder: Option<Arc<Modder>>,
    connections: Arc<Connections>,
}

struct ForwarderGuard(Arc<ConnectionState>);

impl Drop for ForwarderGuard {
    fn drop(&mut self) {
        self.0.close();
    }
}

impl Handler {
    pub fn new(modder: Option<Arc<Modder>>) -> Self {
        Self {
            modder,
            connections: Arc::new(Connections::default()),
        }
    }

    fn forward_messages(
        self,
        from_client: bool,
        stream: impl Stream<Item = Result<Message, tungstenite::Error>> + Unpin + Send + 'static,
        mut sink: impl Sink<Message, Error = tungstenite::Error> + Unpin + Send + 'static,
        connection: Option<Arc<ConnectionState>>,
        mut injections: Option<mpsc::Receiver<Bytes>>,
    ) -> impl Future<Output = ()> + Send {
        let guard = connection
            .as_ref()
            .map(|state| ForwarderGuard(Arc::clone(state)));
        let mut closed = connection.as_ref().map(|state| state.subscribe_close());
        async move {
            let _guard = guard;
            let mut stream = stream;
            let forwarding = async {
                loop {
                    let message = tokio::select! {
                        Some(injected) = async {
                            match injections.as_mut() {
                                Some(receiver) => receiver.recv().await,
                                None => pending().await,
                            }
                        } => {
                            if !send_message(&mut sink, Message::Binary(injected)).await {
                                break;
                            }
                            continue;
                        }
                        message = stream.next() => {
                            match message {
                                Some(Ok(message)) => message,
                                Some(Err(_)) => {
                                    send_message(&mut sink, Message::Close(None)).await;
                                    break;
                                }
                                None => break,
                            }
                        }
                    };
                    let closing = matches!(message, Message::Close(_));
                    if let Some(message) = self
                        .modify_message(from_client, message, connection.as_deref())
                        .await
                        && !send_message(&mut sink, message).await
                    {
                        break;
                    }
                    if closing {
                        break;
                    }
                }
            };
            tokio::select! {
                biased;
                _ = async {
                    match closed.as_mut() {
                        Some(receiver) => {
                            let already_closed = *receiver.borrow_and_update();
                            if !already_closed {
                                let _ = receiver.changed().await;
                            }
                        }
                        None => pending().await,
                    }
                } => {}
                _ = forwarding => {}
            }
        }
    }

    async fn modify_message(
        &self,
        from_client: bool,
        msg: Message,
        connection: Option<&ConnectionState>,
    ) -> Option<Message> {
        let (Some(modder), Some(connection)) = (&self.modder, connection) else {
            return Some(msg);
        };
        let Message::Binary(buf) = msg else {
            return Some(msg);
        };
        let Ok(parsed) = ParsedMessage::decode(&buf, from_client) else {
            return Some(Message::Binary(buf));
        };
        let kind = parsed.kind;
        let response_method = match kind {
            MessageKind::Request(id) => {
                // Keep the original method even when the modder substitutes loginBeat.
                connection.track_request(id, parsed.envelope.method_name.clone());
                None
            }
            MessageKind::Response(id) => match connection.take_request(id) {
                Some(method) => Some(method),
                None => return Some(Message::Binary(buf)),
            },
            MessageKind::Notify => None,
        };
        let res = modder
            .modify_parsed(buf, response_method.as_deref().unwrap_or_default(), parsed)
            .await;
        if res.msg.is_none()
            && let MessageKind::Request(id) = kind
        {
            let _ = connection.take_request(id);
        }
        if let Some(injected) = res.inject_msg {
            let _ = connection.injection_tx.send(injected).await;
        }
        res.msg.map(Message::Binary)
    }
}

impl HttpHandler for Handler {
    /// With Mod off nothing is rewritten, so tunnel CONNECT requests unchanged
    /// instead of paying for TLS interception on every page asset.
    fn should_intercept_connect(
        &mut self,
        _ctx: &HttpContext,
        _req: &Request<Body>,
    ) -> impl Future<Output = bool> + Send {
        let intercept = self.modder.is_some();
        async move { intercept }
    }
}

impl WebSocketHandler for Handler {
    fn handle_websocket(
        self,
        ctx: WebSocketContext,
        stream: impl Stream<Item = Result<Message, tungstenite::Error>> + Unpin + Send + 'static,
        sink: impl Sink<Message, Error = tungstenite::Error> + Unpin + Send + 'static,
    ) -> impl Future<Output = ()> + Send {
        let from_client = matches!(ctx, WebSocketContext::ClientToServer { .. });
        let key = ConnectionKey::from_context(&ctx);
        let connection = if self.modder.is_some() && !key.is_observer() {
            Some(self.connections.get(key, from_client))
        } else {
            None
        };
        let injections = if from_client {
            None
        } else {
            connection
                .as_ref()
                .and_then(|state| state.take_injections())
        };
        // Register before the future is polled so both forwarders share the same state.
        self.forward_messages(from_client, stream, sink, connection, injections)
    }
}

async fn send_message(
    sink: &mut (impl Sink<Message, Error = tungstenite::Error> + Unpin + Send),
    message: Message,
) -> bool {
    sink.send(message).await.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        proto::{base::BaseMessage, lq},
        settings::{MaxData, ModSettings},
    };
    use hudsucker::futures::{channel::mpsc as futures_mpsc, sink, stream};
    use prost::Message as _;
    use std::{convert::Infallible, net::SocketAddr, time::Duration};
    use tokio::sync::RwLock;

    fn handler() -> Handler {
        Handler::new(Some(Arc::new(Modder::new(
            RwLock::new(ModSettings::default()),
            MaxData::default(),
        ))))
    }

    fn connection(handler: &Handler, port: u16) -> Arc<ConnectionState> {
        let key = ConnectionKey::new(
            SocketAddr::from(([127, 0, 0, 1], port)),
            "wss://game.example.test/gateway".parse().unwrap(),
        );
        let connection = handler.connections.get(key.clone(), true);
        let _peer = handler.connections.get(key, false);
        connection
    }

    fn frame(kind: u8, id: u16, method: &str, data: Vec<u8>) -> Message {
        let envelope = BaseMessage {
            method_name: method.into(),
            data: data.into(),
        };
        let mut wire = vec![kind];
        if kind != 1 {
            wire.extend_from_slice(&id.to_le_bytes());
        }
        envelope.encode(&mut wire).unwrap();
        Message::Binary(wire.into())
    }

    fn discard_sink() -> impl Sink<Message, Error = tungstenite::Error> + Unpin + Send {
        sink::drain().sink_map_err(|error: Infallible| match error {})
    }

    #[tokio::test]
    async fn same_request_id_on_other_connection_does_not_change_response_routing() {
        let handler = handler();
        let first = connection(&handler, 10_001);
        let second = connection(&handler, 10_002);
        handler
            .modify_message(
                true,
                frame(2, 7, ".lq.Lobby.fetchAccountInfo", vec![]),
                Some(&first),
            )
            .await;
        handler
            .modify_message(
                true,
                frame(2, 7, ".lq.Route.heartbeat", vec![]),
                Some(&second),
            )
            .await;

        let account = lq::ResAccountInfo {
            account: Some(lq::Account::default()),
            ..Default::default()
        };
        let result = handler
            .modify_message(true, Message::Ping(Bytes::new()), Some(&first))
            .await;
        assert!(matches!(result, Some(Message::Ping(_))));
        let result = handler
            .modify_message(
                false,
                frame(3, 7, "", account.encode_to_vec()),
                Some(&first),
            )
            .await;
        let Some(Message::Binary(wire)) = result else {
            panic!("expected an account response");
        };
        let envelope = BaseMessage::decode(wire.slice(3..)).unwrap();
        let response = lq::ResAccountInfo::decode(envelope.data).unwrap();
        assert_eq!(response.account.unwrap().avatar_id, 400101);
        assert!(first.take_request(7).is_none());
        assert_eq!(
            second.take_request(7).as_deref(),
            Some(".lq.Route.heartbeat")
        );
    }

    #[tokio::test]
    async fn dropped_requests_leave_no_pending_entry() {
        let handler = handler();
        let connection = connection(&handler, 10_001);
        let result = handler
            .modify_message(
                true,
                frame(2, 42, ".lq.Lobby.addFinishedEnding", vec![]),
                Some(&connection),
            )
            .await;
        assert!(result.is_none());
        assert!(connection.take_request(42).is_none());
    }

    #[tokio::test]
    async fn login_beat_substitution_keeps_contract_and_original_request_method() {
        let handler = handler();
        let connection = connection(&handler, 10_001);
        let beat = lq::ReqLoginBeat {
            contract: "test-contract".into(),
        };
        let heartbeat = frame(2, 1, ".lq.Lobby.loginBeat", beat.encode_to_vec());
        assert_eq!(
            handler
                .modify_message(true, heartbeat.clone(), Some(&connection))
                .await,
            Some(heartbeat)
        );
        let result = handler
            .modify_message(
                true,
                frame(2, 2, ".lq.Lobby.receiveCharacterRewards", vec![]),
                Some(&connection),
            )
            .await;
        let Some(Message::Binary(wire)) = result else {
            panic!("expected a substitute heartbeat");
        };
        let envelope = BaseMessage::decode(wire.slice(3..)).unwrap();
        assert_eq!(envelope.method_name, ".lq.Lobby.loginBeat");
        assert_eq!(
            lq::ReqLoginBeat::decode(envelope.data).unwrap().contract,
            "test-contract"
        );
        assert_eq!(
            connection.take_request(2).as_deref(),
            Some(".lq.Lobby.receiveCharacterRewards")
        );
    }

    #[tokio::test]
    async fn close_frame_releases_state_without_waiting_for_another_frame() {
        let handler = handler();
        let connection = connection(&handler, 10_001);
        connection.track_request(42, ".lq.Lobby.login".into());
        let weak = Arc::downgrade(&connection);
        let stream = stream::iter([Ok(Message::Close(None))]).chain(stream::pending());

        tokio::time::timeout(
            Duration::from_secs(1),
            handler.forward_messages(true, stream, discard_sink(), Some(connection), None),
        )
        .await
        .expect("a close frame must finish the forwarder");
        assert!(weak.upgrade().is_none());
    }

    #[tokio::test]
    async fn eof_stops_the_idle_peer_and_releases_both_directions() {
        let handler = handler();
        let connection = connection(&handler, 10_001);
        let weak = Arc::downgrade(&connection);
        let injections = connection.take_injections();
        let outgoing = handler.clone().forward_messages(
            true,
            stream::empty(),
            discard_sink(),
            Some(Arc::clone(&connection)),
            None,
        );
        let incoming = handler.forward_messages(
            false,
            stream::pending(),
            discard_sink(),
            Some(connection),
            injections,
        );

        tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(outgoing, incoming);
        })
        .await
        .expect("an idle peer must stop when the other direction ends");
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn dropping_an_unpolled_forwarder_notifies_the_peer() {
        let handler = handler();
        let connection = connection(&handler, 10_001);
        let closed = connection.subscribe_close();
        let weak = Arc::downgrade(&connection);
        let forwarder = handler.forward_messages(
            true,
            stream::pending(),
            discard_sink(),
            Some(connection),
            None,
        );

        drop(forwarder);
        assert!(*closed.borrow());
        assert!(weak.upgrade().is_none());
    }

    #[tokio::test]
    async fn injection_is_forwarded_while_upstream_is_idle_and_abort_releases_state() {
        let handler = handler();
        let connection = connection(&handler, 10_001);
        let weak = Arc::downgrade(&connection);
        let injections = connection.take_injections();
        let sender = connection.injection_tx.clone();
        let (sink, mut received) = futures_mpsc::unbounded::<Message>();
        let sink = sink.sink_map_err(|_| tungstenite::Error::ConnectionClosed);
        let task = tokio::spawn(handler.forward_messages(
            false,
            stream::pending(),
            sink,
            Some(connection),
            injections,
        ));
        let payload = Bytes::from_static(b"injected notification");
        sender.send(payload.clone()).await.unwrap();
        let result = tokio::time::timeout(Duration::from_secs(1), received.next())
            .await
            .expect("injection must not wait for an upstream message");
        assert_eq!(result, Some(Message::Binary(payload)));

        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(weak.upgrade().is_none());
        assert!(sender.is_closed());
    }
}
