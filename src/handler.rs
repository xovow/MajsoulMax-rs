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
use tracing::Instrument;

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

// 由协议描述生成，覆盖所有具有标准错误字段的 RPC 响应。
include!(concat!(env!("OUT_DIR"), "/error_response_methods.rs"));

#[derive(Clone)]
pub struct Handler {
    modder: Option<Arc<Modder>>,
    connections: Arc<Connections>,
    /// hudsucker 用同一个实例处理一对 HTTP 请求与响应，这里暂存请求信息供日志使用。
    http_request: Option<HttpRequestInfo>,
}

#[derive(Clone)]
struct HttpRequestInfo {
    diagnostic: Option<HttpDiagnostic>,
    inject_yiman: bool,
}

#[derive(Clone)]
struct HttpDiagnostic {
    method: Method,
    uri: Uri,
    started: Instant,
}

impl HttpRequestInfo {
    /// 只记录失败的响应；成功的静态资源请求数量大且无排查价值。
    fn log_response(&self, status: StatusCode) {
        if let Some(diagnostic) = &self.diagnostic {
            log_http_response(
                &diagnostic.method,
                &diagnostic.uri,
                diagnostic.started,
                status,
            );
        }
    }
}

fn log_http_response(method: &Method, uri: &Uri, started: Instant, status: StatusCode) {
    if status.is_client_error() || status.is_server_error() {
        tracing::error!(
            target: HTTP_TARGET,
            status = status.as_u16(),
            %method,
            %uri,
            elapsed_ms = started.elapsed().as_millis(),
            "HTTP 请求返回错误状态"
        );
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
                            if !send_message(&mut sink, Message::Binary(injected), from_client, connection.as_deref()).await {
                                break;
                            }
                            continue;
                        }
                        message = stream.next() => {
                            match message {
                                Some(Ok(message)) => message,
                                Some(Err(error)) => {
                                    log_websocket_error("读取", &error, from_client, connection.as_deref());
                                    send_message(&mut sink, Message::Close(None), from_client, connection.as_deref()).await;
                                    break;
                                }
                                None => break,
                            }
                        }
                    };
                    let closing = matches!(message, Message::Close(_));
                    if let Message::Close(Some(frame)) = &message
                        && !matches!(
                            frame.code,
                            tungstenite::protocol::frame::coding::CloseCode::Normal
                                | tungstenite::protocol::frame::coding::CloseCode::Away
                        )
                    {
                        tracing::error!(
                            target: WEBSOCKET_TARGET,
                            direction = direction(from_client),
                            connection = ?connection.as_deref().map(|state| tracing::field::display(state.key())),
                            code = %frame.code,
                            reason = %frame.reason,
                            "WebSocket 异常关闭"
                        );
                    }
                    if let Some(message) = self
                        .modify_message(from_client, message, connection.as_deref())
                        .await
                        && !send_message(&mut sink, message, from_client, connection.as_deref())
                            .await
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
                tracing::error!(
                    target: RESOURCE_TARGET,
                    connection = %connection.key(),
                    "{dir} 无法解析的帧（{len} 字节），原样转发：{error:#}"
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
                    tracing::error!(
                        target: RESOURCE_TARGET,
                        connection = %connection.key(),
                        "{dir} {kind} 找不到对应的请求，未改写直接转发"
                    );
                    return Some(Message::Binary(buf));
                }
            },
            MessageKind::Notify => parsed.envelope.method_name.clone(),
        };
        if matches!(kind, MessageKind::Response(_)) {
            log_response_error(&method_name, kind, &parsed.envelope.data, Some(connection));
        }
        let res = modder
            .modify_parsed(buf.clone(), &method_name, parsed)
            // 每条消息按当前开关创建短生命周期 span，切换后不会保留旧连接的日志状态。
            .instrument(tracing::error_span!(
                target: WEBSOCKET_TARGET,
                "websocket",
                connection = %connection.key(),
                direction = direction(from_client)
            ))
            .await;
        if res.msg.is_none()
            && let MessageKind::Request(id) = kind
        {
            let _ = connection.take_request(id);
        }
        if let Some(injected) = res.inject_msg
            && connection.injection_tx.send(injected).await.is_err()
        {
            tracing::error!(
                target: RESOURCE_TARGET,
                connection = %connection.key(),
                "{dir} {kind} {method_name} 的注入通知发送失败：连接已关闭"
            );
        }
        res.msg.map(Message::Binary)
    }
}

/// 不记录成功响应；仅对协议明确声明的错误字段进行解码。
fn log_response_error(
    method_name: &str,
    kind: MessageKind,
    data: &Bytes,
    connection: Option<&ConnectionState>,
) {
    if !tracing::enabled!(target: RESOURCE_TARGET, tracing::Level::ERROR)
        || ERROR_RESPONSE_METHODS.binary_search(&method_name).is_err()
    {
        return;
    }
    let response = match <ResponseError as prost::Message>::decode(data.clone()) {
        Ok(response) => response,
        Err(error) => {
            tracing::error!(
                target: RESOURCE_TARGET,
                connection = ?connection.map(|state| tracing::field::display(state.key())),
                payload_bytes = data.len(),
                "{kind} {method_name} 响应错误字段解码失败：{error}"
            );
            return;
        }
    };
    let error = response.error.filter(|error| error.code != 0);
    if let Some(error) = error {
        let code = error.code;
        let message = &error.message;
        tracing::error!(
            target: RESOURCE_TARGET,
            connection = ?connection.map(|state| tracing::field::display(state.key())),
            payload_bytes = data.len(),
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
        text.push('：');
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

impl HttpHandler for Handler {
    async fn handle_request(
        &mut self,
        _ctx: &HttpContext,
        mut req: Request<Body>,
    ) -> RequestOrResponse {
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
                diagnostic: tracing::enabled!(target: HTTP_TARGET, tracing::Level::ERROR).then(
                    || HttpDiagnostic {
                        method: req.method().clone(),
                        uri: req.uri().clone(),
                        started: Instant::now(),
                    },
                ),
                inject_yiman,
            });
        }
        req.into()
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
        let diagnostic = self
            .http_request
            .take()
            .and_then(|request| request.diagnostic);
        if tracing::enabled!(target: HTTP_TARGET, tracing::Level::ERROR) {
            let reason = error_chain(&err);
            match diagnostic {
                Some(HttpDiagnostic {
                    method,
                    uri,
                    started,
                }) => {
                    tracing::error!(
                        target: HTTP_TARGET,
                        elapsed_ms = started.elapsed().as_millis(),
                        "请求转发失败 {method} {uri}：{reason}"
                    );
                }
                None => tracing::error!(target: HTTP_TARGET, "请求转发失败：{reason}"),
            }
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
    from_client: bool,
    connection: Option<&ConnectionState>,
) -> bool {
    let bytes = message.len();
    match sink.send(message).await {
        Ok(()) => true,
        Err(error) => {
            if !matches!(error, tungstenite::Error::ConnectionClosed) {
                tracing::error!(
                    target: WEBSOCKET_TARGET,
                    direction = direction(from_client),
                    connection = ?connection.map(|state| tracing::field::display(state.key())),
                    frame_bytes = bytes,
                    error = %error_chain(&error),
                    "WebSocket 发送失败，断开连接"
                );
            }
            false
        }
    }
}

fn log_websocket_error(
    operation: &str,
    error: &tungstenite::Error,
    from_client: bool,
    connection: Option<&ConnectionState>,
) {
    // 已完成关闭握手是正常终止，不把它当作故障。
    if !matches!(error, tungstenite::Error::ConnectionClosed) {
        tracing::error!(
            target: WEBSOCKET_TARGET,
            direction = direction(from_client),
            connection = ?connection.map(|state| tracing::field::display(state.key())),
            operation,
            error = %error_chain(error),
            "WebSocket 失败，断开连接"
        );
    }
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
