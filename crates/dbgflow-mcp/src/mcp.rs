use crate::logging::FileLogSink;
use crate::tools::{ToolCallError, ToolService};
use dbgflow_core::logging::LogSink;
use dbgflow_core::proxy::ProxyEnvironment;
use dbgflow_core::session::worker::{ProcessWorkerLauncher, SessionWorkerLauncher};
use dbgflow_core::session::SessionId;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;

const JSONRPC_VERSION: &str = "2.0";
const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2024-11-05", "2025-03-26", DEFAULT_PROTOCOL_VERSION];

#[derive(Clone)]
pub struct McpServer {
    service: ToolService,
}

impl McpServer {
    pub fn new(service: ToolService) -> Self {
        Self { service }
    }

    pub fn handle_message(&self, message: Value) -> Option<Value> {
        let Some(object) = message.as_object() else {
            return Some(error_response(Value::Null, -32600, "invalid request"));
        };

        let id = object.get("id").cloned();
        let response_id = id.clone().unwrap_or(Value::Null);
        if object.get("jsonrpc").and_then(Value::as_str) != Some(JSONRPC_VERSION) {
            return Some(error_response(
                response_id,
                -32600,
                "invalid JSON-RPC version",
            ));
        }

        if id.as_ref().is_some_and(|id| !is_valid_request_id(id)) {
            return Some(error_response(
                Value::Null,
                -32600,
                "invalid JSON-RPC request id",
            ));
        }

        let method = match message.get("method").and_then(Value::as_str) {
            Some(method) => method,
            None => return Some(error_response(response_id, -32600, "invalid request")),
        };
        let is_notification = id.is_none();

        let result = match method {
            "initialize" => self.initialize(message.get("params").cloned().unwrap_or_default()),
            "notifications/initialized" => return None,
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": self.service.tool_descriptors() })),
            "tools/call" => self.call_tool(message.get("params").cloned().unwrap_or_default()),
            "resources/list" => self.resources_list(),
            "resources/read" => {
                self.resources_read(message.get("params").cloned().unwrap_or_default())
            }
            _ => Err(ServerError::new(
                -32601,
                format!("method not found: {method}"),
            )),
        };

        if is_notification {
            return Some(error_response(
                Value::Null,
                -32600,
                "request method requires id",
            ));
        }

        let id = id.expect("request id checked above");
        Some(match result {
            Ok(result) => success_response(id, result),
            Err(error) => error_response(id, error.code, &error.message),
        })
    }

    fn initialize(&self, params: Value) -> std::result::Result<Value, ServerError> {
        let requested_protocol_version = params
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_PROTOCOL_VERSION);
        let protocol_version = if SUPPORTED_PROTOCOL_VERSIONS.contains(&requested_protocol_version)
        {
            requested_protocol_version
        } else {
            DEFAULT_PROTOCOL_VERSION
        };

        Ok(json!({
            "protocolVersion": protocol_version,
            "capabilities": {
                "tools": {
                    "listChanged": false
                },
                "resources": {
                    "subscribe": false,
                    "listChanged": false
                }
            },
            "serverInfo": {
                "name": "dbgflow",
                "version": env!("CARGO_PKG_VERSION")
            },
            "instructions": "Use dbgflow tools to create debug sessions and run audited native debugger commands in trusted local environments."
        }))
    }

    fn call_tool(&self, params: Value) -> std::result::Result<Value, ServerError> {
        let params: CallToolParams = serde_json::from_value(params).map_err(|error| {
            ServerError::new(-32602, format!("invalid tools/call params: {error}"))
        })?;

        let arguments = params
            .arguments
            .unwrap_or(Value::Object(Default::default()));
        let result = match self.service.call_tool(&params.name, arguments) {
            Ok(value) => tool_success(value),
            Err(ToolCallError::InvalidRequest(message)) => {
                return Err(ServerError::new(-32602, message));
            }
            Err(ToolCallError::Execution(message)) => tool_error(message),
        };

        Ok(result)
    }

    fn resources_list(&self) -> std::result::Result<Value, ServerError> {
        let sessions = self
            .service
            .list_sessions()
            .map_err(|error| ServerError::new(-32000, error.to_string()))?;
        let resources = sessions
            .into_iter()
            .map(|session| {
                json!({
                    "uri": session_resource_uri(session.id),
                    "name": format!("dbgflow session {}", session.id),
                    "description": format!("dbgflow session state: {:?}", session.state),
                    "mimeType": "application/json"
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({ "resources": resources }))
    }

    fn resources_read(&self, params: Value) -> std::result::Result<Value, ServerError> {
        let params: ResourceReadParams = serde_json::from_value(params).map_err(|error| {
            ServerError::new(-32602, format!("invalid resources/read params: {error}"))
        })?;
        let session_id = parse_session_resource_uri(&params.uri)?;
        let session = self
            .service
            .query_session(session_id)
            .map_err(|error| ServerError::new(-32000, error.to_string()))?;
        let text = serde_json::to_string_pretty(&session)
            .map_err(|error| ServerError::new(-32000, format!("serialize session: {error}")))?;
        Ok(json!({
            "contents": [
                {
                    "uri": params.uri,
                    "mimeType": "application/json",
                    "text": text
                }
            ]
        }))
    }

    pub fn session_update_receiver(&self) -> mpsc::Receiver<SessionId> {
        self.service.subscribe_session_updates()
    }
}

#[derive(Debug, Deserialize)]
struct CallToolParams {
    name: String,
    arguments: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct ResourceReadParams {
    uri: String,
}

#[derive(Debug)]
struct ServerError {
    code: i64,
    message: String,
}

impl ServerError {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

fn success_response(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "result": result
    })
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "error": {
            "code": code,
            "message": message
        }
    })
}

fn tool_success(value: Value) -> Value {
    json!({
        "content": [
            {
                "type": "text",
                "text": serde_json::to_string_pretty(&value)
                    .unwrap_or_else(|_| value.to_string())
            }
        ],
        "isError": false
    })
}

fn tool_error(message: String) -> Value {
    json!({
        "content": [
            {
                "type": "text",
                "text": message
            }
        ],
        "isError": true
    })
}

pub fn session_resource_uri(session_id: SessionId) -> String {
    format!("dbgflow://sessions/{session_id}")
}

fn parse_session_resource_uri(uri: &str) -> std::result::Result<SessionId, ServerError> {
    let id = uri
        .strip_prefix("dbgflow://sessions/")
        .ok_or_else(|| ServerError::new(-32602, format!("invalid session resource uri: {uri}")))?;
    serde_json::from_value(Value::String(id.to_string()))
        .map_err(|error| ServerError::new(-32602, format!("invalid session id in uri: {error}")))
}

pub fn server_with_data_dir(
    data_dir: impl Into<PathBuf>,
) -> std::result::Result<McpServer, String> {
    server_with_data_dir_and_proxy(data_dir, ProxyEnvironment::none())
}

pub fn server_with_data_dir_and_proxy(
    data_dir: impl Into<PathBuf>,
    proxy: ProxyEnvironment,
) -> std::result::Result<McpServer, String> {
    let data_dir = data_dir.into();
    let logger = Arc::new(
        FileLogSink::new(data_dir.join("logs"), 7)
            .map_err(|error| format!("initialize log directory: {error}"))?,
    );
    Ok(server_with_data_dir_proxy_and_logger(
        data_dir, proxy, logger,
    ))
}

pub fn server_with_data_dir_and_logger(
    data_dir: impl Into<PathBuf>,
    logger: Arc<dyn LogSink>,
) -> McpServer {
    server_with_data_dir_proxy_and_logger(data_dir, ProxyEnvironment::none(), logger)
}

pub fn server_with_data_dir_proxy_and_logger(
    data_dir: impl Into<PathBuf>,
    proxy: ProxyEnvironment,
    logger: Arc<dyn LogSink>,
) -> McpServer {
    let data_dir = data_dir.into();
    McpServer::new(ToolService::new(
        dbgflow_core::session::SessionManager::with_worker_launcher_proxy_and_logger(
            default_process_worker_launcher(),
            data_dir.join("artifacts"),
            proxy,
            logger,
        ),
    ))
}

fn default_process_worker_launcher() -> Arc<dyn SessionWorkerLauncher> {
    let executable = std::env::current_exe()
        .or_else(|_| {
            std::env::args_os()
                .next()
                .map(PathBuf::from)
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "current exe"))
        })
        .unwrap_or_else(|_| PathBuf::from("dbgflow-mcp"));
    Arc::new(ProcessWorkerLauncher::with_executable(executable))
}

fn is_valid_request_id(id: &Value) -> bool {
    id.is_string() || id.is_number() || id.is_null()
}

#[cfg(test)]
mod tests {
    use super::McpServer;
    use crate::tools::ToolService;
    use dbgflow_core::backend::{CreateBackendSession, ExecuteBackendResult};
    use dbgflow_core::proxy::ProxyEnvironment;
    use dbgflow_core::session::worker::{SessionWorker, SessionWorkerLauncher, WorkerSession};
    use dbgflow_core::session::{SessionId, SessionManager};
    use dbgflow_core::Result;
    use serde_json::{json, Value};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    static NEXT_TEST_ARTIFACT_ROOT: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn initialize_returns_tool_capability() {
        let server = test_server();
        let response = server
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": { "protocolVersion": "2025-06-18" }
            }))
            .expect("response");

        assert_eq!(response["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(
            response["result"]["capabilities"]["tools"]["listChanged"],
            false
        );
    }

    #[test]
    fn initialize_keeps_legacy_supported_protocol_version() {
        let server = test_server();
        let response = server
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": { "protocolVersion": "2024-11-05" }
            }))
            .expect("response");

        assert_eq!(response["result"]["protocolVersion"], "2024-11-05");
    }

    #[test]
    fn initialize_falls_back_for_unsupported_protocol_version() {
        let server = test_server();
        let response = server
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": { "protocolVersion": "unsupported-version" }
            }))
            .expect("response");

        assert_eq!(response["result"]["protocolVersion"], "2025-06-18");
    }

    #[test]
    fn tools_list_includes_input_schemas() {
        let server = test_server();
        let response = server
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": "tools",
                "method": "tools/list"
            }))
            .expect("response");

        let tools = response["result"]["tools"].as_array().expect("tools array");
        assert!(tools.iter().any(|tool| {
            tool["name"] == "create_session" && tool["inputSchema"]["type"] == "object"
        }));
        let create_session = tools
            .iter()
            .find(|tool| tool["name"] == "create_session")
            .expect("create_session tool");
        assert!(create_session["inputSchema"]["properties"]
            .get("startup_timeout_ms")
            .is_none());
        let target_schema = &create_session["inputSchema"]["properties"]["target"]["oneOf"];
        assert_eq!(create_session["inputSchema"]["required"][0], "target");
        assert!(!target_schema
            .as_array()
            .expect("target variants")
            .iter()
            .any(|target| target["properties"]["kind"]["const"] == "mock"));
        assert!(target_schema
            .as_array()
            .expect("target variants")
            .iter()
            .any(|target| target["properties"]["kind"]["const"] == "attach"));
        assert!(target_schema
            .as_array()
            .expect("target variants")
            .iter()
            .any(|target| target["properties"]["kind"]["const"] == "launch"));
        assert!(tools.iter().any(|tool| {
            tool["name"] == "eval" && tool["inputSchema"]["required"][0] == "session_id"
        }));
        let eval = tools
            .iter()
            .find(|tool| tool["name"] == "eval")
            .expect("eval tool");
        assert!(eval["inputSchema"]["properties"]
            .get("timeout_ms")
            .is_none());
        assert!(tools.iter().any(|tool| tool["name"] == "get_session"));
        assert!(tools.iter().any(|tool| tool["name"] == "set_symbols"));
    }

    #[test]
    fn tools_call_can_create_and_eval_dump_session() {
        let server = test_server();
        let dump_path = test_dump_path("mcp-create-eval");
        let create_response = server
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "create_session",
                    "arguments": {
                        "target": { "kind": "dump", "path": dump_path }
                    }
                }
            }))
            .expect("create response");

        let session = tool_text_json(&create_response);
        let session_id = session["id"].as_str().expect("session id");
        wait_for_break(&server, session_id);

        let eval_response = server
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "eval",
                    "arguments": {
                        "session_id": session_id,
                        "command": "k"
                    }
                }
            }))
            .expect("eval response");

        let eval = tool_text_json(&eval_response);
        assert!(eval["output"]
            .as_str()
            .expect("output")
            .contains("fake worker executed: k"));
    }

    #[test]
    fn tools_call_set_symbols_accepts_raw_symbol_path() {
        let server = test_server();
        let dump_path = test_dump_path("mcp-set-symbols-raw");
        let create_response = server
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "create_session",
                    "arguments": {
                        "target": { "kind": "dump", "path": dump_path }
                    }
                }
            }))
            .expect("create response");

        let session = tool_text_json(&create_response);
        let session_id = session["id"].as_str().expect("session id");
        wait_for_break(&server, session_id);

        let symbol_path = "srv*C:\\symbols*https://msdl.microsoft.com/download/symbols";
        let response = server
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "set_symbols",
                    "arguments": {
                        "session_id": session_id,
                        "paths": [symbol_path]
                    }
                }
            }))
            .expect("set symbols response");

        let result = tool_text_json(&response);
        assert!(result["output"]
            .as_str()
            .expect("output")
            .contains(&format!(".sympath {symbol_path}")));
    }

    #[test]
    fn tools_call_set_symbols_rejects_line_separators() {
        let server = test_server();
        let dump_path = test_dump_path("mcp-set-symbols-line-separator");
        let create_response = server
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "create_session",
                    "arguments": {
                        "target": { "kind": "dump", "path": dump_path }
                    }
                }
            }))
            .expect("create response");

        let session = tool_text_json(&create_response);
        let session_id = session["id"].as_str().expect("session id");
        wait_for_break(&server, session_id);

        let response = server
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "set_symbols",
                    "arguments": {
                        "session_id": session_id,
                        "paths": ["srv*C:\\symbols\r\n.shell dir"]
                    }
                }
            }))
            .expect("set symbols response");

        assert_eq!(response["result"]["isError"], true);
        assert!(response["result"]["content"][0]["text"]
            .as_str()
            .expect("tool error text")
            .contains("symbol path contains unsupported control characters"));
    }

    #[test]
    fn tools_call_returns_protocol_error_for_unknown_tool() {
        let server = test_server();
        let response = server
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "missing_tool",
                    "arguments": {}
                }
            }))
            .expect("response");

        assert_eq!(response["error"]["code"], -32602);
        assert!(response["error"]["message"]
            .as_str()
            .expect("error message")
            .contains("unknown tool"));
    }

    #[test]
    fn tools_call_returns_protocol_error_for_invalid_arguments() {
        let server = test_server();
        let response = server
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "eval",
                    "arguments": {
                        "session_id": 123,
                        "command": "k"
                    }
                }
            }))
            .expect("response");

        assert_eq!(response["error"]["code"], -32602);
        assert!(response["error"]["message"]
            .as_str()
            .expect("error message")
            .contains("invalid tool arguments"));
    }

    #[test]
    fn tools_call_rejects_create_session_without_target() {
        let server = test_server();
        let response = server
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "create_session",
                    "arguments": {}
                }
            }))
            .expect("response");

        assert_eq!(response["error"]["code"], -32602);
        assert!(response["error"]["message"]
            .as_str()
            .expect("error message")
            .contains("invalid tool arguments"));
    }

    #[test]
    fn tools_call_rejects_mock_target_at_runtime() {
        let server = test_server();
        let response = server
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "create_session",
                    "arguments": {
                        "target": { "kind": "mock" }
                    }
                }
            }))
            .expect("response");

        assert_eq!(response["error"]["code"], -32602);
        assert!(response["error"]["message"]
            .as_str()
            .expect("error message")
            .contains("invalid tool arguments"));
    }

    #[test]
    fn tools_call_returns_tool_error_for_empty_eval_command() {
        let server = test_server();
        let dump_path = test_dump_path("mcp-empty-command");
        let create_response = server
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "create_session",
                    "arguments": { "target": { "kind": "dump", "path": dump_path } }
                }
            }))
            .expect("create response");
        let session = tool_text_json(&create_response);
        let session_id = session["id"].as_str().expect("session id");

        let response = server
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "eval",
                    "arguments": {
                        "session_id": session_id,
                        "command": " "
                    }
                }
            }))
            .expect("response");

        assert_eq!(response["result"]["isError"], true);
        assert!(response["result"]["content"][0]["text"]
            .as_str()
            .expect("tool error text")
            .contains("empty command"));
    }

    #[test]
    fn rejects_invalid_jsonrpc_envelope() {
        let server = test_server();
        let response = server
            .handle_message(json!({
                "jsonrpc": "1.0",
                "id": 1,
                "method": "tools/list"
            }))
            .expect("response");

        assert_eq!(response["error"]["code"], -32600);
        assert!(response["error"]["message"]
            .as_str()
            .expect("error message")
            .contains("JSON-RPC"));
    }

    #[test]
    fn rejects_request_methods_without_id() {
        let server = test_server();
        let response = server
            .handle_message(json!({
                "jsonrpc": "2.0",
                "method": "tools/list"
            }))
            .expect("response");

        assert_eq!(response["id"], Value::Null);
        assert_eq!(response["error"]["code"], -32600);
    }

    fn test_server() -> McpServer {
        McpServer::new(ToolService::new(SessionManager::with_worker_launcher(
            Arc::new(TestWorkerLauncher::default()),
            test_artifact_root("dbgflow-mcp-test"),
        )))
    }

    fn test_artifact_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "{name}-{}-{}",
            std::process::id(),
            NEXT_TEST_ARTIFACT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create test artifact root");
        root
    }

    fn test_dump_path(name: &str) -> PathBuf {
        let root = test_artifact_root(name);
        let path = root.join("test.dmp");
        fs::write(&path, b"not a real dump").expect("write fake dump");
        path
    }

    fn tool_text_json(response: &Value) -> Value {
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .expect("tool text");
        serde_json::from_str(text).expect("tool json text")
    }

    fn wait_for_break(server: &McpServer, session_id: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let response = server
                .handle_message(json!({
                    "jsonrpc": "2.0",
                    "id": "get-session",
                    "method": "tools/call",
                    "params": {
                        "name": "get_session",
                        "arguments": {
                            "session_id": session_id
                        }
                    }
                }))
                .expect("get session response");
            let session = tool_text_json(&response);
            if session["state"] == "Break" {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "session did not break: {session}"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[derive(Default)]
    struct TestWorkerLauncher {
        next_id: AtomicU64,
    }

    impl SessionWorkerLauncher for TestWorkerLauncher {
        fn spawn(
            &self,
            _session_id: SessionId,
            _logger: Arc<dyn dbgflow_core::logging::LogSink>,
            _proxy: ProxyEnvironment,
        ) -> Result<Arc<dyn SessionWorker>> {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            Ok(Arc::new(TestWorker {
                backend_session_id: format!("fake-{id}"),
                closed: Mutex::new(false),
            }))
        }
    }

    struct TestWorker {
        backend_session_id: String,
        closed: Mutex<bool>,
    }

    impl SessionWorker for TestWorker {
        fn create_session(&self, _request: CreateBackendSession) -> Result<WorkerSession> {
            Ok(WorkerSession {
                backend: "fake".to_string(),
                backend_session_id: self.backend_session_id.clone(),
                warnings: Vec::new(),
            })
        }

        fn execute(
            &self,
            command: String,
            _event_sink: std::sync::Arc<dyn dbgflow_core::backend::BackendEventSink>,
        ) -> Result<ExecuteBackendResult> {
            Ok(ExecuteBackendResult {
                output: format!("fake worker executed: {command}"),
                warnings: Vec::new(),
                final_state: None,
            })
        }

        fn has_exited(&self) -> Result<bool> {
            Ok(*self.closed.lock().expect("closed lock"))
        }

        fn close(&self) -> Result<()> {
            *self.closed.lock().expect("closed lock") = true;
            Ok(())
        }

        fn kill(&self, reason: &str) -> Result<()> {
            *self.closed.lock().expect("closed lock") = true;
            if reason == "fail-kill" {
                return Err(dbgflow_core::DbgFlowError::Backend(
                    "fake kill failed".to_string(),
                ));
            }
            Ok(())
        }
    }
}
