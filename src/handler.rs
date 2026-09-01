use anyhow::Result;
use hudsucker::{
    Body, HttpContext, RequestOrResponse,
    futures::{Sink, SinkExt, Stream, StreamExt},
    hyper::{Request, Response, StatusCode},
    tokio_tungstenite::tungstenite::{self, Message},
    *,
};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::*;

use crate::{modder::Modder, parser::Parser};

#[derive(Clone)]
pub struct Handler {
    modder: Option<Arc<Modder>>,
    inject_msg: Option<Message>,
    parser: Arc<RwLock<Parser>>,
}

impl Handler {
    pub fn new(modder: Option<Arc<Modder>>) -> Self {
        Self {
            modder,
            inject_msg: None,
            parser: Arc::new(RwLock::new(Parser::new())),
        }
    }
}

impl HttpHandler for Handler {
    async fn handle_request(
        &mut self,
        _ctx: &HttpContext,
        req: Request<Body>,
    ) -> RequestOrResponse {
        if req.uri().path() == "/ping" {
            Response::builder()
                .status(StatusCode::OK)
                .body(Body::from("pong"))
                .expect("Failed to build ping response")
                .into()
        } else {
            req.into()
        }
    }
}

impl WebSocketHandler for Handler {
    async fn handle_websocket(
        mut self,
        ctx: WebSocketContext,
        mut stream: impl Stream<Item = Result<Message, tungstenite::Error>> + Unpin + Send + 'static,
        mut sink: impl Sink<Message, Error = tungstenite::Error> + Unpin + Send + 'static,
    ) {
        if let WebSocketContext::ServerToClient { .. } = ctx
            && let Some(msg) = self.inject_msg.take()
            && let Err(e) = sink.send(msg).await
        {
            error!("Failed to send injected message: {e}");
        }
        while let Some(message) = stream.next().await {
            match message {
                Ok(message) => {
                    let Some(message) = self.handle_message(&ctx, message).await else {
                        continue;
                    };

                    match sink.send(message).await {
                        Err(tungstenite::Error::ConnectionClosed) => (),
                        Err(e) => error!("WebSocket send error: {e}"),
                        _ => (),
                    }
                }
                Err(e) => {
                    error!("WebSocket message error: {e}");

                    match sink.send(Message::Close(None)).await {
                        Err(tungstenite::Error::ConnectionClosed) => (),
                        Err(e) => error!("WebSocket close error: {e}"),
                        _ => (),
                    };

                    break;
                }
            }
        }
    }

    async fn handle_message(&mut self, _ctx: &WebSocketContext, msg: Message) -> Option<Message> {
        let (direction_char, uri) = match _ctx {
            WebSocketContext::ServerToClient { src, .. } => ('\u{2193}', src),
            WebSocketContext::ClientToServer { dst, .. } => ('\u{2191}', dst),
        };

        if uri.path() == "/ob" {
            // ignore ob messages
            return Some(msg);
        }

        debug!("{direction_char} {uri}");

        let Message::Binary(buf) = msg else {
            return Some(msg);
        };

        let Some(ref modder) = self.modder else {
            return Some(Message::Binary(buf));
        };

        let mut parser = self.parser.write().await;
        let Ok(method_name) = parser.parse(&buf) else {
            error!("Failed to parse message");
            return Some(Message::Binary(buf));
        };
        drop(parser);

        let res = modder
            .modify(buf, direction_char == '\u{2191}', method_name)
            .await;
        if let Some(inj) = res.inject_msg {
            self.inject_msg = Some(Message::Binary(inj));
        }
        res.msg.map(Message::Binary)
    }
}
