// 原始 HTTP 跟踪客户端：当设置了 AGENT_TRACE_HTTP 环境变量时，
// 把发往模型的完整请求体与返回的原始 SSE 响应字节追加写入 logs/http-trace.log，
// 用于排查「模型返回内容与预期不符」类问题（例如某供应商标记了 4366 token
// 但最终 content 字段只剩几个字符）。
//
// Raw HTTP tracing client: when the AGENT_TRACE_HTTP env var is set, the full
// request body sent to the model and the raw SSE response bytes are appended to
// logs/http-trace.log, to diagnose "model returns malformed content" issues
// (e.g. a provider reporting 4366 tokens but only a fragment in `content`).

use std::future::Future;
use std::io::Write;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures::StreamExt;
use rig_core::http_client::sse::BoxedStream;
use rig_core::http_client::{
    Error, HttpClientExt, LazyBody, MultipartForm, Request, Response, Result as HttpResult,
    StreamingResponse,
};
use rig_core::wasm_compat::WasmCompatSend;

/// 是否启用原始 HTTP 跟踪。
/// Whether raw HTTP tracing is enabled.
fn trace_enabled() -> bool {
    std::env::var("AGENT_TRACE_HTTP")
        .map(|v| matches!(v.as_str(), "1" | "true" | "on" | "yes"))
        .unwrap_or(false)
}

/// 跟踪文件路径。
/// The trace file path.
fn trace_path() -> std::path::PathBuf {
    std::path::PathBuf::from("logs").join("http-trace.log")
}

/// 把原始响应字节追加到跟踪文件（忽略写入错误，仅调试用途）。
/// Appends raw response bytes to the trace file (write errors ignored; debug only).
fn append_trace(file: &std::fs::File, data: &[u8]) {
    let mut f = file;
    let _ = f.write_all(data);
    let _ = f.flush();
}

/// 实现 rig `HttpClientExt` 的包装客户端：委托给 reqwest，传输期间做原始字节跟踪。
/// An `HttpClientExt` wrapper that delegates to reqwest, tee-ing raw bytes while tracing.
#[derive(Debug, Clone)]
pub struct TracingHttpClient {
    inner: reqwest::Client,
    trace: Option<Arc<Mutex<std::fs::File>>>,
}

impl Default for TracingHttpClient {
    fn default() -> Self {
        Self {
            inner: reqwest::Client::new(),
            trace: None,
        }
    }
}

impl TracingHttpClient {
    /// 以 reqwest 客户端为基底构造。跟踪关闭时内部 trace 句柄为 None。
    /// Builds from a reqwest client. `trace` is None when tracing is disabled.
    pub fn new(inner: reqwest::Client) -> anyhow::Result<Self> {
        let trace = if trace_enabled() {
            if let Some(parent) = trace_path().parent() {
                std::fs::create_dir_all(parent)?;
            }
            let f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(trace_path())?;
            Some(Arc::new(Mutex::new(f)))
        } else {
            None
        };
        Ok(Self { inner, trace })
    }
}

impl HttpClientExt for TracingHttpClient {
    fn send<T, U>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = HttpResult<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        T: Into<Bytes>,
        T: WasmCompatSend,
        U: From<Bytes>,
        U: WasmCompatSend + 'static,
    {
        self.inner.send(req)
    }

    fn send_multipart<U>(
        &self,
        req: Request<MultipartForm>,
    ) -> impl Future<Output = HttpResult<Response<LazyBody<U>>>> + WasmCompatSend + 'static
    where
        U: From<Bytes>,
        U: WasmCompatSend + 'static,
    {
        self.inner.send_multipart(req)
    }

    fn send_streaming<T>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = HttpResult<StreamingResponse>> + WasmCompatSend
    where
        T: Into<Bytes> + WasmCompatSend,
    {
        let inner = self.inner.clone();
        let trace = self.trace.clone();

        let (parts, body) = req.into_parts();
        let body_bytes: Bytes = body.into();
        let method = parts.method;
        let uri = parts.uri.to_string();
        let headers = parts.headers;

        // 写请求行 + 完整请求体（这是发往模型的 JSON，含有 messages/tools 等）。
        // Log the request line + full request body (the JSON sent to the model).
        if let Some(tf) = &trace {
            let mut f = tf.lock().unwrap();
            let _ = writeln!(f, "\n===== REQUEST {} {} =====", method, uri);
            let _ = f.write_all(&body_bytes);
            let _ = writeln!(f, "\n===== RESPONSE STREAM =====");
        }

        async move {
            let req_builder = inner
                .request(method, uri)
                .headers(headers)
                .body(body_bytes)
                .build()
                .map_err(|e| Error::Instance(Box::new(e)))?;

            let response = inner
                .execute(req_builder)
                .await
                .map_err(|e| Error::Instance(Box::new(e)))?;

            if !response.status().is_success() {
                let status = response.status();
                let msg = response.text().await.unwrap_or_default();
                if let Some(tf) = &trace {
                    let mut f = tf.lock().unwrap();
                    let _ = writeln!(f, "\n===== HTTP ERROR {status} =====\n{msg}\n");
                }
                return Err(Error::InvalidStatusCodeWithMessage(status, msg));
            }

            let mut res = Response::builder()
                .status(response.status())
                .version(response.version());
            if let Some(hs) = res.headers_mut() {
                *hs = response.headers().clone();
            }

            // tee：每个原始字节块同时写进跟踪文件，再交给 rig 的 SSE 解析。
            // tee: write each raw byte chunk into the trace file, then forward to rig's SSE parser.
            let tee = trace.clone();
            let mapped = response
                .bytes_stream()
                .inspect(move |chunk| {
                    if let (Some(tf), Ok(bytes)) = (&tee, chunk) {
                        let f = tf.lock().unwrap();
                        append_trace(&f, bytes);
                    }
                })
                .map(|chunk| chunk.map_err(|e| Error::Instance(Box::new(e))));

            let mapped_stream: BoxedStream = Box::pin(mapped);
            res.body(mapped_stream).map_err(Error::Protocol)
        }
    }
}