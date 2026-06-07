use crate::mcp::{session_resource_uri, McpServer};
use dbgflow_core::logging::{LogEvent, LogLevel};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::thread;
use std::time::{Duration, Instant};

const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;
const CONNECTION_READ_TIMEOUT: Duration = Duration::from_secs(10);
static NEXT_HTTP_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpConfig {
    pub bind: SocketAddr,
}

pub fn run_http(server: McpServer, config: HttpConfig, shutdown: Receiver<()>) -> io::Result<()> {
    let listener = TcpListener::bind(config.bind)?;
    listener.set_nonblocking(true)?;
    server.log(
        LogEvent::new(LogLevel::Info, "http", "http_server_started")
            .field("bind", config.bind.to_string()),
    );

    loop {
        if shutdown.try_recv().is_ok() {
            server.log(
                LogEvent::new(LogLevel::Info, "http", "http_server_stopped")
                    .field("bind", config.bind.to_string())
                    .field("reason", "shutdown_requested"),
            );
            return Ok(());
        }

        match listener.accept() {
            Ok((stream, _peer)) => {
                let server = server.clone();
                thread::spawn(move || {
                    let _ = handle_connection(server, stream);
                });
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => {
                server.log(
                    LogEvent::new(LogLevel::Error, "http", "http_accept_failed")
                        .field("bind", config.bind.to_string())
                        .error(error.to_string()),
                );
                return Err(error);
            }
        }
    }
}

fn handle_connection(server: McpServer, mut stream: TcpStream) -> io::Result<()> {
    let request_id = NEXT_HTTP_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    let peer = stream
        .peer_addr()
        .map(|peer| peer.to_string())
        .unwrap_or_else(|_| "<unknown>".to_string());
    stream.set_read_timeout(Some(CONNECTION_READ_TIMEOUT))?;
    let request = match read_request(&mut stream) {
        Ok(request) => request,
        Err(error) => {
            server.log(
                LogEvent::new(LogLevel::Error, "http", "http_request_rejected")
                    .field("http_request_id", request_id)
                    .field("peer", peer)
                    .field("status", 400)
                    .error(error.to_string()),
            );
            write_response(
                &mut stream,
                HttpResponse::json(400, json!({ "error": error.to_string() })),
            )?;
            return Ok(());
        }
    };
    let method = request.method.clone();
    let path = request.path.clone();
    let body_bytes = request.body.len();
    let started = Instant::now();
    server.log(
        LogEvent::new(LogLevel::Info, "http", "http_request_started")
            .field("http_request_id", request_id)
            .field("peer", peer.clone())
            .field("method", method.clone())
            .field("path", path.clone())
            .field("body_bytes", body_bytes),
    );

    if is_mcp_sse_request(&request) {
        return handle_mcp_sse(server, request_id, request, stream, started);
    }

    let response = route_request(server.clone(), request, request_id);
    let status = response.status;
    let response_bytes = response.body.len();
    server.log(
        LogEvent::new(LogLevel::Info, "http", "http_request_finished")
            .duration_ms(started.elapsed().as_millis())
            .field("http_request_id", request_id)
            .field("peer", peer)
            .field("method", method)
            .field("path", path)
            .field("status", status)
            .field("response_bytes", response_bytes),
    );
    write_response(&mut stream, response)
}

fn route_request(server: McpServer, request: HttpRequest, request_id: u64) -> HttpResponse {
    if !origin_is_allowed(request.headers.get("origin")) {
        server.log(
            LogEvent::new(LogLevel::Warn, "http", "http_origin_rejected")
                .field("http_request_id", request_id)
                .field("origin", request.headers.get("origin").cloned()),
        );
        return HttpResponse::json(403, json!({ "error": "forbidden origin" }));
    }

    let path = request
        .path
        .split_once('?')
        .map(|(path, _query)| path)
        .unwrap_or(&request.path);

    match (request.method.as_str(), path) {
        ("GET", "/healthz") => HttpResponse::json(200, json!({ "status": "ok" })),
        ("GET", "/mcp") => HttpResponse::empty(405),
        ("POST", "/mcp") => handle_mcp_post(server, request, request_id),
        _ => HttpResponse::json(404, json!({ "error": "not found" })),
    }
}

fn is_mcp_sse_request(request: &HttpRequest) -> bool {
    let path = request
        .path
        .split_once('?')
        .map(|(path, _query)| path)
        .unwrap_or(&request.path);
    request.method == "GET" && path == "/mcp"
}

fn handle_mcp_sse(
    server: McpServer,
    request_id: u64,
    request: HttpRequest,
    mut stream: TcpStream,
    started: Instant,
) -> io::Result<()> {
    if !origin_is_allowed(request.headers.get("origin")) {
        server.log(
            LogEvent::new(LogLevel::Warn, "http", "http_origin_rejected")
                .field("http_request_id", request_id)
                .field("origin", request.headers.get("origin").cloned()),
        );
        write_response(
            &mut stream,
            HttpResponse::json(403, json!({ "error": "forbidden origin" })),
        )?;
        return Ok(());
    }

    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n"
    )?;
    stream.flush()?;
    server.log(
        LogEvent::new(LogLevel::Info, "http", "http_sse_opened")
            .field("http_request_id", request_id)
            .field("path", request.path.clone()),
    );

    let updates = server.session_update_receiver();
    loop {
        match updates.recv_timeout(Duration::from_secs(30)) {
            Ok(session_id) => {
                let notification = json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/resources/updated",
                    "params": {
                        "uri": session_resource_uri(session_id)
                    }
                });
                if let Err(error) = write_sse_json(&mut stream, &notification) {
                    server.log(
                        LogEvent::new(LogLevel::Warn, "http", "http_sse_closed")
                            .duration_ms(started.elapsed().as_millis())
                            .field("http_request_id", request_id)
                            .error(error.to_string()),
                    );
                    return Err(error);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if let Err(error) = stream.write_all(b": keepalive\n\n") {
                    server.log(
                        LogEvent::new(LogLevel::Warn, "http", "http_sse_closed")
                            .duration_ms(started.elapsed().as_millis())
                            .field("http_request_id", request_id)
                            .error(error.to_string()),
                    );
                    return Err(error);
                }
                if let Err(error) = stream.flush() {
                    server.log(
                        LogEvent::new(LogLevel::Warn, "http", "http_sse_closed")
                            .duration_ms(started.elapsed().as_millis())
                            .field("http_request_id", request_id)
                            .error(error.to_string()),
                    );
                    return Err(error);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                server.log(
                    LogEvent::new(LogLevel::Info, "http", "http_sse_closed")
                        .duration_ms(started.elapsed().as_millis())
                        .field("http_request_id", request_id)
                        .field("reason", "updates_disconnected"),
                );
                return Ok(());
            }
        }
    }
}

fn write_sse_json(stream: &mut TcpStream, value: &Value) -> io::Result<()> {
    let text = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    write!(stream, "data: {text}\n\n")?;
    stream.flush()
}

fn handle_mcp_post(server: McpServer, request: HttpRequest, request_id: u64) -> HttpResponse {
    if !content_type_is_json(request.headers.get("content-type")) {
        server.log(
            LogEvent::new(LogLevel::Warn, "http", "http_request_invalid_content_type")
                .field("http_request_id", request_id)
                .field("content_type", request.headers.get("content-type").cloned()),
        );
        return HttpResponse::json(
            415,
            json!({ "error": "content type must be application/json" }),
        );
    }

    let message = match serde_json::from_slice::<Value>(&request.body) {
        Ok(message) => message,
        Err(error) => {
            server.log(
                LogEvent::new(LogLevel::Error, "http", "http_json_parse_failed")
                    .field("http_request_id", request_id)
                    .field("body_bytes", request.body.len())
                    .error(error.to_string()),
            );
            return HttpResponse::json(400, json!({ "error": format!("parse error: {error}") }));
        }
    };

    if is_jsonrpc_response(&message) {
        server.log(
            LogEvent::new(LogLevel::Info, "http", "http_mcp_response_accepted")
                .field("http_request_id", request_id),
        );
        return HttpResponse::empty(202);
    }

    if is_jsonrpc_notification(&message) {
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("<invalid>")
            .to_string();
        if method == "notifications/initialized" {
            let _ = server.handle_message_with_http_request_id(message, request_id);
        }
        server.log(
            LogEvent::new(LogLevel::Info, "http", "http_mcp_notification_accepted")
                .field("http_request_id", request_id)
                .field("method", method),
        );
        return HttpResponse::empty(202);
    }

    match server.handle_message_with_http_request_id(message, request_id) {
        Some(response) => HttpResponse::json(200, response),
        None => HttpResponse::empty(202),
    }
}

fn is_jsonrpc_notification(message: &Value) -> bool {
    message.get("method").is_some() && message.get("id").is_none()
}

fn is_jsonrpc_response(message: &Value) -> bool {
    message.get("method").is_none()
        && message.get("id").is_some()
        && (message.get("result").is_some() || message.get("error").is_some())
}

fn content_type_is_json(value: Option<&String>) -> bool {
    value
        .map(|value| {
            value
                .split(';')
                .next()
                .map(str::trim)
                .is_some_and(|content_type| content_type.eq_ignore_ascii_case("application/json"))
        })
        .unwrap_or(false)
}

fn origin_is_allowed(value: Option<&String>) -> bool {
    let Some(origin) = value
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    else {
        return true;
    };

    let origin = origin.to_ascii_lowercase();
    origin == "http://localhost"
        || origin == "https://localhost"
        || origin.starts_with("http://localhost:")
        || origin.starts_with("https://localhost:")
        || origin == "http://127.0.0.1"
        || origin == "https://127.0.0.1"
        || origin.starts_with("http://127.0.0.1:")
        || origin.starts_with("https://127.0.0.1:")
        || origin == "http://[::1]"
        || origin == "https://[::1]"
        || origin.starts_with("http://[::1]:")
        || origin.starts_with("https://[::1]:")
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn read_request(stream: &mut TcpStream) -> io::Result<HttpRequest> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    if request_line.trim().is_empty() {
        return Err(invalid_input("empty request"));
    }

    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| invalid_input("missing HTTP method"))?
        .to_string();
    let path = parts
        .next()
        .ok_or_else(|| invalid_input("missing HTTP path"))?
        .to_string();
    let version = parts
        .next()
        .ok_or_else(|| invalid_input("missing HTTP version"))?;
    if !version.starts_with("HTTP/1.") {
        return Err(invalid_input("unsupported HTTP version"));
    }

    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(invalid_input("invalid HTTP header"));
        };
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
    }

    let content_length = headers
        .get("content-length")
        .map(|value| value.parse::<usize>())
        .transpose()
        .map_err(|_| invalid_input("invalid content length"))?
        .unwrap_or(0);
    if content_length > MAX_REQUEST_BYTES {
        return Err(invalid_input("request body too large"));
    }

    let mut body = vec![0; content_length];
    reader.read_exact(&mut body)?;

    Ok(HttpRequest {
        method,
        path,
        headers,
        body,
    })
}

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    content_type: Option<&'static str>,
    headers: Vec<(&'static str, &'static str)>,
    body: Vec<u8>,
}

impl HttpResponse {
    fn json(status: u16, value: Value) -> Self {
        Self {
            status,
            content_type: Some("application/json"),
            headers: Vec::new(),
            body: serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec()),
        }
    }

    fn empty(status: u16) -> Self {
        Self {
            status,
            content_type: None,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }
}

fn write_response(stream: &mut TcpStream, response: HttpResponse) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        reason_phrase(response.status),
        response.body.len()
    )?;
    if let Some(content_type) = response.content_type {
        write!(stream, "Content-Type: {content_type}; charset=utf-8\r\n")?;
    }
    for (name, value) in response.headers {
        write!(stream, "{name}: {value}\r\n")?;
    }
    write!(stream, "\r\n")?;
    stream.write_all(&response.body)?;
    stream.flush()
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        _ => "Internal Server Error",
    }
}

fn invalid_input(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::{
        content_type_is_json, origin_is_allowed, route_request, HttpRequest, HttpResponse,
    };
    use crate::mcp::McpServer;
    use crate::tools::ToolService;
    use dbgflow_core::logging::{LogEvent, LogSink};
    use dbgflow_core::session::SessionManager;
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[test]
    fn validates_json_content_type() {
        assert!(content_type_is_json(Some(&"application/json".to_string())));
        assert!(content_type_is_json(Some(
            &"application/json; charset=utf-8".to_string()
        )));
        assert!(!content_type_is_json(Some(&"text/plain".to_string())));
        assert!(!content_type_is_json(None));
    }

    #[test]
    fn request_body_limit_is_sixteen_mib() {
        assert_eq!(super::MAX_REQUEST_BYTES, 16 * 1024 * 1024);
    }

    #[test]
    fn allows_only_empty_or_localhost_origin() {
        assert!(origin_is_allowed(None));
        assert!(origin_is_allowed(Some(
            &"http://localhost:7331".to_string()
        )));
        assert!(origin_is_allowed(Some(&"http://127.0.0.1".to_string())));
        assert!(origin_is_allowed(Some(&"http://[::1]:7331".to_string())));
        assert!(!origin_is_allowed(Some(&"http://example.com".to_string())));
    }

    #[test]
    fn healthz_returns_ok() {
        let response = route_request(
            test_server(),
            request("GET", "/healthz", HashMap::new(), Vec::new()),
            1,
        );

        assert_eq!(response.status, 200);
        assert_eq!(json_body(response)["status"], "ok");
    }

    #[test]
    fn mcp_get_route_is_method_not_allowed_for_plain_request() {
        let response = route_request(
            test_server(),
            request("GET", "/mcp", HashMap::new(), Vec::new()),
            1,
        );

        assert_eq!(response.status, 405);
    }

    #[test]
    fn mcp_post_initialize_returns_json_response() {
        let mut headers = HashMap::new();
        headers.insert("content-type".to_string(), "application/json".to_string());
        let body = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {}
        }))
        .expect("serialize body");

        let response = route_request(test_server(), request("POST", "/mcp", headers, body), 1);

        assert_eq!(response.status, 200);
        assert_eq!(json_body(response)["id"], 1);
    }

    #[test]
    fn mcp_post_notification_returns_accepted() {
        let mut headers = HashMap::new();
        headers.insert("content-type".to_string(), "application/json".to_string());
        let body = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))
        .expect("serialize body");

        let response = route_request(test_server(), request("POST", "/mcp", headers, body), 1);

        assert_eq!(response.status, 202);
        assert!(response.body.is_empty());
    }

    #[test]
    fn mcp_post_accepts_plain_request() {
        let mut headers = HashMap::new();
        headers.insert("content-type".to_string(), "application/json".to_string());
        let body = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {}
        }))
        .expect("serialize body");

        let response = route_request(test_server(), request("POST", "/mcp", headers, body), 1);

        assert_eq!(response.status, 200);
    }

    #[test]
    fn mcp_post_rejects_non_localhost_origin() {
        let mut headers = HashMap::new();
        headers.insert("content-type".to_string(), "application/json".to_string());
        headers.insert("origin".to_string(), "http://example.com".to_string());

        let response = route_request(
            test_server(),
            request("POST", "/mcp", headers, b"{}".to_vec()),
            1,
        );

        assert_eq!(response.status, 403);
    }

    #[test]
    fn mcp_post_logs_http_and_mcp_events() {
        let logger = Arc::new(RecordingLogSink::default());
        let server = test_server_with_logger(logger.clone());
        let mut headers = HashMap::new();
        headers.insert("content-type".to_string(), "application/json".to_string());
        let body = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": "tools",
            "method": "tools/call",
            "params": {
                "name": "missing_tool",
                "arguments": {}
            }
        }))
        .expect("serialize body");

        let response = route_request(server, request("POST", "/mcp", headers, body), 42);

        assert_eq!(response.status, 200);
        let events = logger.events();
        assert!(events.iter().any(|event| {
            event.component == "mcp"
                && event.event == "mcp_request_started"
                && event.fields["method"] == "tools/call"
                && event.fields["jsonrpc_id"] == "tools"
                && event.fields["http_request_id"] == json!(42)
        }));
        assert!(events.iter().any(|event| {
            event.component == "mcp"
                && event.event == "mcp_request_finished"
                && event.fields["method"] == "tools/call"
                && event.fields["jsonrpc_id"] == "tools"
                && event.fields["http_request_id"] == json!(42)
        }));
        assert!(events.iter().any(|event| {
            event.component == "mcp"
                && event.event == "mcp_tool_call_started"
                && event.fields["tool_name"] == "missing_tool"
                && event.fields["jsonrpc_id"] == "tools"
                && event.fields["http_request_id"] == json!(42)
        }));
        assert!(events.iter().any(|event| {
            event.component == "mcp"
                && event.event == "mcp_tool_call_finished"
                && event.fields["tool_name"] == "missing_tool"
                && event.fields["jsonrpc_id"] == "tools"
                && event.fields["http_request_id"] == json!(42)
        }));
    }

    fn test_server() -> McpServer {
        McpServer::new(ToolService::new(SessionManager::with_artifact_root(
            std::env::temp_dir().join(format!("dbgflow-http-test-{}", std::process::id())),
        )))
    }

    fn test_server_with_logger(logger: Arc<dyn LogSink>) -> McpServer {
        McpServer::new_with_logger(
            ToolService::new(SessionManager::with_artifact_root(
                std::env::temp_dir().join(format!("dbgflow-http-test-{}", std::process::id())),
            )),
            logger,
        )
    }

    fn request(
        method: &str,
        path: &str,
        headers: HashMap<String, String>,
        body: Vec<u8>,
    ) -> HttpRequest {
        HttpRequest {
            method: method.to_string(),
            path: path.to_string(),
            headers,
            body,
        }
    }

    fn json_body(response: HttpResponse) -> Value {
        serde_json::from_slice(&response.body).expect("json response")
    }

    #[derive(Default)]
    struct RecordingLogSink {
        events: Mutex<Vec<LogEvent>>,
    }

    impl RecordingLogSink {
        fn events(&self) -> Vec<LogEvent> {
            self.events.lock().expect("events lock").clone()
        }
    }

    impl LogSink for RecordingLogSink {
        fn log(&self, event: LogEvent) {
            self.events.lock().expect("events lock").push(event);
        }
    }
}
