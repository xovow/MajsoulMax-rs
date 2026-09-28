use anyhow::Result;
use bytes::Bytes;
use http_body_util::BodyExt;
use hudsucker::{
    Body, HttpContext, HttpHandler, RequestOrResponse, WebSocketContext, WebSocketHandler,
    futures::{Sink, SinkExt, Stream, StreamExt},
    hyper::{Method, Request, Response, StatusCode, Uri, header},
    hyper_util::client::legacy::Error as ClientError,
    tokio_tungstenite::tungstenite::{self, Message},
};
use std::{future::pending, sync::Arc, time::Instant};
use tokio::sync::mpsc;

use crate::{
    connections::{ConnectionKey, ConnectionState, Connections},
    modder::Modder,
    parser::{MessageKind, ParsedMessage},
    proto::base::ResponseError,
};

const RESOURCE_TARGET: &str = "majsoul_max_rs::resource";
const HTTP_TARGET: &str = "majsoul_max_rs::http";
const WEBSOCKET_TARGET: &str = "majsoul_max_rs::websocket";
const YIMAN_JS: &str = include_str!("yiman.js");

/// 调试日志逐条记录的协议方法：Mod 改写的角色、装扮、表情相关消息，
/// 以及对局中的表情广播和动作（和牌、鸣牌、立直特效依赖玩家的装扮）。
const RESOURCE_METHODS: &[&str] = &[
    ".lq.Lobby.login",
    ".lq.Lobby.oauth2Login",
    ".lq.Lobby.fetchAccountInfo",
    ".lq.Lobby.fetchInfo",
    ".lq.Lobby.fetchCharacterInfo",
    ".lq.Lobby.fetchBagInfo",
    ".lq.Lobby.fetchAllCommonViews",
    ".lq.Lobby.fetchCommonViews",
    ".lq.Lobby.saveCommonViews",
    ".lq.Lobby.useCommonView",
    ".lq.Lobby.changeMainCharacter",
    ".lq.Lobby.changeCharacterSkin",
    ".lq.Lobby.updateCharacterSort",
    ".lq.Lobby.useTitle",
    ".lq.Lobby.setLoadingImage",
    ".lq.Lobby.fetchTitleList",
    ".lq.Lobby.receiveCharacterRewards",
    ".lq.Lobby.addFinishedEnding",
    ".lq.Lobby.setRandomCharacter",
    ".lq.Lobby.fetchRandomCharacter",
    ".lq.Lobby.setHiddenCharacter",
    ".lq.Lobby.createRoom",
    ".lq.Lobby.fetchRoom",
    ".lq.FastTest.authGame",
    ".lq.FastTest.enterGame",
    ".lq.FastTest.syncGame",
    ".lq.FastTest.broadcastInGame",
    ".lq.NotifyAccountUpdate",
    ".lq.NotifyRoomPlayerUpdate",
    ".lq.NotifyGameBroadcast",
];

/// 这些响应的 1 号字段不是 lq.Error，不能用 [`ResponseError`] 探测。
const RESPONSES_WITHOUT_ERROR: &[&str] = &[
    ".lq.Lobby.fetchAllCommonViews",
    ".lq.Lobby.fetchCommonViews",
];

#[derive(Clone)]
pub struct Handler {
    modder: Option<Arc<Modder>>,
    connections: Arc<Connections>,
    /// hudsucker 用同一个实例处理一对 HTTP 请求与响应，这里暂存请求信息供日志使用。
    http_request: Option<HttpRequestInfo>,
}

#[derive(Clone)]
struct HttpRequestInfo {
    method: Method,
    uri: Uri,
    started: Instant,
    inject_yiman: bool,
}

impl HttpRequestInfo {
    /// 只记录失败的响应；成功的静态资源请求数量大且无排查价值。
    fn log_response(&self, status: StatusCode) {
        if !(status.is_client_error() || status.is_server_error()) {
            return;
        }
        let Self {
            method,
            uri,
            started,
            ..
        } = self;
        let elapsed = started.elapsed().as_millis();
        tracing::warn!(target: HTTP_TARGET, "{status} {method} {uri} ({elapsed} ms)");
    }
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
            http_request: None,
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
                                Some(Err(error)) => {
                                    let dir = direction(from_client);
                                    tracing::warn!(
                                        target: WEBSOCKET_TARGET,
                                        "{dir} 读取失败，断开连接：{error}"
                                    );
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
        let dir = direction(from_client);
        let parsed = match ParsedMessage::decode(&buf, from_client) {
            Ok(parsed) => parsed,
            Err(error) => {
                let len = buf.len();
                tracing::debug!(
                    target: RESOURCE_TARGET,
                    "{dir} 无法解析的帧（{len} 字节），原样转发：{error}"
                );
                return Some(Message::Binary(buf));
            }
        };
        let kind = parsed.kind;
        let method_name = match kind {
            MessageKind::Request(id) => {
                // Keep the original method even when the modder substitutes loginBeat.
                connection.track_request(id, parsed.envelope.method_name.clone());
                parsed.envelope.method_name.clone()
            }
            MessageKind::Response(id) => match connection.take_request(id) {
                Some(method) => method,
                None => {
                    tracing::warn!(
                        target: RESOURCE_TARGET,
                        "{dir} {kind} 找不到对应的请求，未改写直接转发"
                    );
                    return Some(Message::Binary(buf));
                }
            },
            MessageKind::Notify => parsed.envelope.method_name.clone(),
        };
        let logged = is_resource_method(&method_name);
        if logged && matches!(kind, MessageKind::Response(_)) {
            log_response_error(&method_name, kind, &parsed.envelope.data);
        }
        let res = modder
            .modify_parsed(buf.clone(), &method_name, parsed)
            .await;
        // 原样转发的消息不记录，只留下 Mod 实际改写或拦截的。
        let outcome = match &res.msg {
            None => Some("已拦截，不转发"),
            Some(out) if *out == buf => None,
            Some(_) => Some("已改写后转发"),
        };
        if logged && let Some(outcome) = outcome {
            tracing::debug!(target: RESOURCE_TARGET, "{dir} {kind} {method_name}：{outcome}");
        }
        if res.msg.is_none()
            && let MessageKind::Request(id) = kind
        {
            let _ = connection.take_request(id);
        }
        if let Some(injected) = res.inject_msg
            && connection.injection_tx.send(injected).await.is_err()
        {
            tracing::warn!(target: RESOURCE_TARGET, "{method_name} 的注入通知发送失败：连接已关闭");
        }
        res.msg.map(Message::Binary)
    }
}

fn is_resource_method(method_name: &str) -> bool {
    RESOURCE_METHODS.contains(&method_name)
}

/// 记录服务器对资源相关请求返回的错误码，例如使用未拥有的角色表情被拒绝。
fn log_response_error(method_name: &str, kind: MessageKind, data: &Bytes) {
    if RESPONSES_WITHOUT_ERROR.contains(&method_name) {
        return;
    }
    let error = <ResponseError as prost::Message>::decode(data.clone())
        .ok()
        .and_then(|response| response.error)
        .filter(|error| error.code != 0);
    if let Some(error) = error {
        let code = error.code;
        let message = &error.message;
        tracing::warn!(
            target: RESOURCE_TARGET,
            "{kind} {method_name} 服务器返回错误 code={code} {message}"
        );
    }
}

fn direction(from_client: bool) -> &'static str {
    if from_client {
        "[客户端→服务器]"
    } else {
        "[服务器→客户端]"
    }
}

fn error_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str("：");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

impl HttpHandler for Handler {
    fn handle_request(
        &mut self,
        _ctx: &HttpContext,
        mut req: Request<Body>,
    ) -> impl Future<Output = RequestOrResponse> + Send {
        async move {
            // CONNECT 只建立隧道，里面的每个请求会再单独经过这里。
            if req.method() != Method::CONNECT {
                let inject_yiman = if is_yiman_page(req.method(), req.uri()) {
                    match self.modder.as_ref() {
                        Some(modder) => modder.yiman_effect_on().await,
                        None => false,
                    }
                } else {
                    false
                };
                if inject_yiman {
                    prepare_yiman_request(&mut req);
                }
                self.http_request = Some(HttpRequestInfo {
                    method: req.method().clone(),
                    uri: req.uri().clone(),
                    started: Instant::now(),
                    inject_yiman,
                });
            }
            req.into()
        }
    }

    fn handle_response(
        &mut self,
        _ctx: &HttpContext,
        res: Response<Body>,
    ) -> impl Future<Output = Response<Body>> + Send {
        let inject_yiman = if let Some(request) = self.http_request.take() {
            request.log_response(res.status());
            request.inject_yiman
        } else {
            false
        };
        async move {
            if inject_yiman {
                inject_yiman_response(res).await
            } else {
                res
            }
        }
    }

    /// Keep hudsucker's default 502 response, but record why forwarding failed.
    fn handle_error(
        &mut self,
        _ctx: &HttpContext,
        err: ClientError,
    ) -> impl Future<Output = Response<Body>> + Send {
        let reason = error_chain(&err);
        match self.http_request.take() {
            Some(HttpRequestInfo { method, uri, .. }) => {
                tracing::error!(target: HTTP_TARGET, "请求转发失败 {method} {uri}：{reason}");
            }
            None => tracing::error!(target: HTTP_TARGET, "请求转发失败：{reason}"),
        }
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::BAD_GATEWAY;
        async move { response }
    }

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
        if let WebSocketContext::ClientToServer { dst, .. } = &ctx {
            tracing::debug!(target: WEBSOCKET_TARGET, "已连接 {dst}");
        }
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

fn is_yiman_page(method: &Method, uri: &Uri) -> bool {
    method == Method::GET
        && uri.host() == Some("game.maj-soul.com")
        && matches!(uri.path(), "/1/" | "/1/index.html")
}

fn prepare_yiman_request(req: &mut Request<Body>) {
    let headers = req.headers_mut();
    headers.insert(
        header::ACCEPT_ENCODING,
        header::HeaderValue::from_static("identity"),
    );
    for name in [
        header::IF_NONE_MATCH,
        header::IF_MODIFIED_SINCE,
        header::IF_RANGE,
        header::RANGE,
    ] {
        headers.remove(name);
    }
}

fn inject_yiman_html(html: &str) -> Option<String> {
    if html.contains("data-majsoulmax-yiman") || !html.contains("createUnityInstance") {
        return None;
    }
    let at = html.to_ascii_lowercase().find("</head>")?;
    Some(format!(
        "{}<script data-majsoulmax-yiman=\"1\">\n{YIMAN_JS}\n</script>\n{}",
        &html[..at],
        &html[at..],
    ))
}

async fn inject_yiman_response(res: Response<Body>) -> Response<Body> {
    let value = |name| res.headers().get(name).and_then(|v| v.to_str().ok());
    let html = value(header::CONTENT_TYPE).is_some_and(|v| {
        v.split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .eq_ignore_ascii_case("text/html")
    });
    if res.status() != StatusCode::OK
        || !html
        || !value(header::CONTENT_ENCODING)
            .unwrap_or("identity")
            .eq_ignore_ascii_case("identity")
    {
        return res;
    }
    let (mut parts, body) = res.into_parts();
    let bytes = match body.collect().await {
        Ok(body) => body.to_bytes(),
        Err(error) => {
            tracing::warn!(target: HTTP_TARGET, "读取游戏页面失败：{error}");
            let mut res = Response::new(Body::empty());
            *res.status_mut() = StatusCode::BAD_GATEWAY;
            return res;
        }
    };
    let Some(html) = std::str::from_utf8(&bytes).ok().and_then(inject_yiman_html) else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    // The modified document must not reuse the upstream entity's cache metadata.
    for name in ["transfer-encoding", "etag", "last-modified", "content-md5"] {
        parts.headers.remove(name);
    }
    parts.headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    parts.headers.insert(
        header::CONTENT_LENGTH,
        header::HeaderValue::from_str(&html.len().to_string()).expect("decimal content length"),
    );
    tracing::debug!(target: HTTP_TARGET, "已注入役满动画脚本");
    Response::from_parts(parts, Body::from(html))
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

    #[test]
    fn only_verified_entry_points_are_selected() {
        for url in [
            "https://game.maj-soul.com/1/",
            "https://game.maj-soul.com/1/index.html?paipu=test",
        ] {
            assert!(is_yiman_page(&Method::GET, &url.parse().unwrap()));
        }
        for url in [
            "https://game.maj-soul.com.example/1/",
            "https://other.example/1/",
            "https://game.maj-soul.com/assetbundles/DXT/test.majset",
            "https://game.maj-soul.com/1/version.json",
        ] {
            assert!(!is_yiman_page(&Method::GET, &url.parse().unwrap()));
        }
        assert!(!is_yiman_page(
            &Method::POST,
            &"https://game.maj-soul.com/1/".parse().unwrap(),
        ));
    }

    #[test]
    fn injection_requests_bypass_conditional_and_partial_responses() {
        let mut req = Request::builder()
            .header(header::ACCEPT_ENCODING, "gzip, br")
            .header(header::IF_NONE_MATCH, "old")
            .header(header::IF_MODIFIED_SINCE, "old")
            .header(header::RANGE, "bytes=0-100")
            .header(header::IF_RANGE, "old")
            .body(Body::empty())
            .unwrap();
        prepare_yiman_request(&mut req);
        assert_eq!(req.headers()[header::ACCEPT_ENCODING], "identity");
        for name in [
            header::IF_NONE_MATCH,
            header::IF_MODIFIED_SINCE,
            header::IF_RANGE,
            header::RANGE,
        ] {
            assert!(!req.headers().contains_key(name));
        }
    }

    #[tokio::test]
    async fn rewritten_html_has_fresh_headers_and_only_one_injection() {
        let html = "<html><head></head><body><script>createUnityInstance()</script></body></html>";
        let res = Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .header(header::CONTENT_LENGTH, html.len().to_string())
            .header(header::ETAG, "original")
            .header(header::CACHE_CONTROL, "max-age=3600")
            .body(Body::from(html))
            .unwrap();
        let res = inject_yiman_response(res).await;
        assert_eq!(res.headers()[header::CACHE_CONTROL], "no-store");
        assert!(!res.headers().contains_key(header::ETAG));
        let length = res.headers()[header::CONTENT_LENGTH]
            .to_str()
            .unwrap()
            .parse::<usize>()
            .unwrap();
        let body = res.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body.len(), length);
        let html = std::str::from_utf8(&body).unwrap();
        assert!(html.contains("LoadMgr"));
        assert_eq!(html.matches("data-majsoulmax-yiman").count(), 1);
        assert!(inject_yiman_html(html).is_none());
        assert!(inject_yiman_html("<html><head></head><body>Laya</body></html>").is_none());
    }

    #[tokio::test]
    async fn unexpected_encoding_or_content_is_forwarded_unchanged() {
        for (status, content_type, encoding) in [
            (StatusCode::OK, "text/html", "gzip"),
            (StatusCode::OK, "application/octet-stream", "identity"),
            (StatusCode::NOT_FOUND, "text/html", "identity"),
        ] {
            let res = Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, content_type)
                .header(header::CONTENT_ENCODING, encoding)
                .header(header::ETAG, "original")
                .body(Body::from("original body"))
                .unwrap();
            let res = inject_yiman_response(res).await;
            assert_eq!(res.status(), status);
            assert_eq!(res.headers()[header::ETAG], "original");
            let bytes = res.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(bytes.as_ref(), b"original body");
        }
    }

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

    /// 调试日志应记录资源相关消息的改写结果，以及服务器返回的错误码。
    #[tokio::test]
    async fn debug_log_records_resource_messages_and_server_errors() {
        let dir = std::env::temp_dir().join(format!(
            "majsoul-max-log-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let guard = crate::debug_log::start(&dir).unwrap();
        let handler = handler();
        let connection = connection(&handler, 10_003);

        handler
            .modify_message(
                true,
                frame(2, 1, ".lq.Lobby.changeMainCharacter", vec![]),
                Some(&connection),
            )
            .await;

        let rejected = lq::ResCommon {
            error: Some(lq::Error {
                code: 1203,
                message: "未拥有该表情".into(),
                ..Default::default()
            }),
        };
        handler
            .modify_message(
                false,
                frame(3, 1, "", rejected.encode_to_vec()),
                Some(&connection),
            )
            .await;

        drop(guard);
        let log = std::fs::read_dir(&dir)
            .unwrap()
            .find_map(|entry| {
                let path = entry.ok()?.path();
                path.extension()?.eq("log").then_some(path)
            })
            .expect("应生成日志文件");
        let content = std::fs::read_to_string(&log).unwrap();
        assert!(content.contains("changeMainCharacter"), "{content}");
        assert!(content.contains("code=1203"), "{content}");
        assert!(content.contains("未拥有该表情"), "{content}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
