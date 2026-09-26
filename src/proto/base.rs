// 雀魂 WebSocket 的外层信封类型。
//
// 原先由 prost-build 从 proto/basic.proto 生成，该文件在适配 Unity 新版数据时
// 一并移除了（liqi.desc 里不含此包）。这个结构极小且稳定，故改为手工维护。
#[derive(Clone, PartialEq, Eq, Hash, ::prost::Message)]
pub struct BaseMessage {
    #[prost(string, tag = "1")]
    pub method_name: ::prost::alloc::string::String,
    #[prost(bytes = "bytes", tag = "2")]
    pub data: ::bytes::Bytes,
}

// 绝大多数响应的 1 号字段都是 lq.Error。调试日志只解码这一个字段，
// 以便统一记录服务器拒绝的请求；其余字段按未知字段跳过。
// 调用方需确认目标响应类型的 1 号字段确实是 lq.Error。
#[derive(Clone, PartialEq, ::prost::Message)]
pub struct ResponseError {
    #[prost(message, optional, tag = "1")]
    pub error: ::core::option::Option<super::lq::Error>,
}
