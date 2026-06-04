use crate::tools::{ToolCallError, ToolService};
use dbgflow_core::session::SessionId;
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

const JSONRPC_VERSION: &str = "2.0";
const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2024-11-05", "2025-03-26", DEFAULT_PROTOCOL_VERSION];
const ARTIFACT_ROOT_ENV: &str = "DBGFLOW_ARTIFACT_ROOT";

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
            "instructions": "Use dbgflow tools to create debug sessions and run denylist-protected debugger commands."
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

pub fn run_stdio<R, W>(server: McpServer, input: R, mut output: W) -> std::io::Result<()>
where
    R: BufRead,
    W: Write,
{
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<Value>(&line) {
            Ok(message) => server.handle_message(message),
            Err(error) => Some(error_response(
                Value::Null,
                -32700,
                &format!("parse error: {error}"),
            )),
        };

        if let Some(response) = response {
            serde_json::to_writer(&mut output, &response)?;
            writeln!(output)?;
            output.flush()?;
        }
    }

    Ok(())
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

pub fn default_server() -> McpServer {
    server_with_artifact_root(default_artifact_root())
}

pub fn server_with_artifact_root(artifact_root: impl Into<PathBuf>) -> McpServer {
    McpServer::new(ToolService::new(
        dbgflow_core::session::SessionManager::with_default_backends_at(artifact_root),
    ))
}

pub fn default_artifact_root() -> PathBuf {
    if let Some(path) = std::env::var_os(ARTIFACT_ROOT_ENV) {
        return make_absolute(PathBuf::from(path));
    }

    workspace_root().join("artifacts")
}

fn workspace_root() -> PathBuf {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| manifest_dir.to_path_buf())
}

fn make_absolute(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        workspace_root().join(path)
    }
}

fn is_valid_request_id(id: &Value) -> bool {
    id.is_string() || id.is_number() || id.is_null()
}

#[cfg(test)]
mod tests {
    use super::{default_artifact_root, run_stdio, McpServer};
    use crate::tools::ToolService;
    use dbgflow_core::backend::mock::MockBackend;
    use dbgflow_core::session::SessionManager;
    use serde_json::{json, Value};
    use std::io::Cursor;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

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
        let target_schema = &create_session["inputSchema"]["properties"]["target"]["oneOf"];
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
            tool["name"] == "execute" && tool["inputSchema"]["required"][0] == "session_id"
        }));
        assert!(tools.iter().any(|tool| tool["name"] == "get_session"));
        assert!(tools.iter().any(|tool| tool["name"] == "set_symbols"));
    }

    #[test]
    fn tools_call_can_create_and_execute_mock_session() {
        let server = test_server();
        let create_response = server
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
            .expect("create response");

        let session = tool_text_json(&create_response);
        let session_id = session["id"].as_str().expect("session id");
        wait_for_ready(&server, session_id);

        let execute_response = server
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "execute",
                    "arguments": {
                        "session_id": session_id,
                        "command": "k"
                    }
                }
            }))
            .expect("execute response");

        let execute = tool_text_json(&execute_response);
        assert!(execute["output"]
            .as_str()
            .expect("output")
            .contains("mock executed: k"));
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
                    "name": "execute",
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
    fn tools_call_returns_tool_error_for_execution_failure() {
        let server = test_server();
        let create_response = server
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "create_session",
                    "arguments": { "target": { "kind": "mock" } }
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
                    "name": "execute",
                    "arguments": {
                        "session_id": session_id,
                        "command": ".shell dir"
                    }
                }
            }))
            .expect("response");

        assert_eq!(response["result"]["isError"], true);
        assert!(response["result"]["content"][0]["text"]
            .as_str()
            .expect("tool error text")
            .contains("command denied"));
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

    #[test]
    fn default_artifact_root_is_workspace_scoped() {
        let root = default_artifact_root();
        let expected = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("workspace root")
            .join("artifacts");

        assert!(root.is_absolute());
        assert_eq!(root, expected);
    }

    #[test]
    fn stdio_runner_writes_line_delimited_responses() {
        let server = test_server();
        let input = Cursor::new(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}
"#,
        );
        let mut output = Vec::new();

        run_stdio(server, input, &mut output).expect("run stdio");

        let line = String::from_utf8(output).expect("utf8 output");
        let response: Value = serde_json::from_str(line.trim()).expect("json response");
        assert_eq!(response["id"], 1);
        assert_eq!(response["result"]["serverInfo"]["name"], "dbgflow");
    }

    fn test_server() -> McpServer {
        let artifact_root = std::env::temp_dir().join(format!(
            "dbgflow-mcp-test-{}-{}",
            std::process::id(),
            NEXT_TEST_ARTIFACT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));

        McpServer::new(ToolService::new(SessionManager::with_artifact_root(
            vec![Arc::new(MockBackend::new())],
            artifact_root,
        )))
    }

    fn tool_text_json(response: &Value) -> Value {
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .expect("tool text");
        serde_json::from_str(text).expect("tool json text")
    }

    fn wait_for_ready(server: &McpServer, session_id: &str) {
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
            if session["state"] == "Ready" {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "session did not become ready: {session}"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}
