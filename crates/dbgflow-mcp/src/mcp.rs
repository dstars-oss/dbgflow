use crate::logging::FileLogSink;
use crate::tools::{ToolCallError, ToolService};
use dbgflow_common::logging::{noop_logger, LogEvent, LogLevel, LogSink};
use dbgflow_common::process::{ProcessLaunchConfig, ToolCallContext};
use dbgflow_common::proxy::ProxyEnvironment;
use dbgflow_debug::session::worker::{ProcessWorkerLauncher, SessionWorkerLauncher};
use dbgflow_debug::session::SessionId;
use dbgflow_reverse::ida::{
    IdaRuntimeConfig, IdaSessionManager, ProcessReverseWorkerLauncher, ReverseWorkerLauncher,
};
use dbgflow_trace::profile::ProfileManager;
use dbgflow_trace::ttd::{TtdRecorderRuntime, TtdRecordingManager};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Instant;

const JSONRPC_VERSION: &str = "2.0";
const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2024-11-05", "2025-03-26", DEFAULT_PROTOCOL_VERSION];

#[derive(Clone)]
pub struct McpServer {
    service: ToolService,
    logger: Arc<dyn LogSink>,
}

#[derive(Clone)]
pub struct McpServerConfig {
    pub data_dir: PathBuf,
    pub proxy: ProxyEnvironment,
    pub process_launch: ProcessLaunchConfig,
    pub ttd_dir: Option<PathBuf>,
    pub ida_install_dir: Option<PathBuf>,
    pub symbol_path: Option<String>,
    pub logger: Arc<dyn LogSink>,
}

impl McpServerConfig {
    pub fn new(data_dir: impl Into<PathBuf>, logger: Arc<dyn LogSink>) -> Self {
        Self {
            data_dir: data_dir.into(),
            proxy: ProxyEnvironment::none(),
            process_launch: ProcessLaunchConfig::default(),
            ttd_dir: None,
            ida_install_dir: None,
            symbol_path: None,
            logger,
        }
    }

    pub fn with_proxy(mut self, proxy: ProxyEnvironment) -> Self {
        self.proxy = proxy;
        self
    }

    pub fn with_ttd_dir(mut self, ttd_dir: Option<PathBuf>) -> Self {
        self.ttd_dir = ttd_dir;
        self
    }

    pub fn with_process_launch(mut self, process_launch: ProcessLaunchConfig) -> Self {
        self.process_launch = process_launch;
        self
    }

    pub fn with_ida_install_dir(mut self, ida_install_dir: Option<PathBuf>) -> Self {
        self.ida_install_dir = ida_install_dir;
        self
    }

    pub fn with_symbol_path(mut self, symbol_path: Option<String>) -> Self {
        self.symbol_path = symbol_path;
        self
    }
}

impl McpServer {
    pub fn new(service: ToolService) -> Self {
        Self::new_with_logger(service, noop_logger())
    }

    pub fn new_with_logger(service: ToolService, logger: Arc<dyn LogSink>) -> Self {
        Self { service, logger }
    }

    pub fn handle_message(&self, message: Value) -> Option<Value> {
        self.handle_message_with_context(message, None, ToolCallContext::default())
    }

    pub(crate) fn handle_message_with_http_request_id(
        &self,
        message: Value,
        http_request_id: u64,
    ) -> Option<Value> {
        self.handle_message_with_context(message, Some(http_request_id), ToolCallContext::default())
    }

    pub(crate) fn handle_message_with_http_context(
        &self,
        message: Value,
        http_request_id: u64,
        tool_context: ToolCallContext,
    ) -> Option<Value> {
        self.handle_message_with_context(message, Some(http_request_id), tool_context)
    }

    fn handle_message_with_context(
        &self,
        message: Value,
        http_request_id: Option<u64>,
        tool_context: ToolCallContext,
    ) -> Option<Value> {
        let Some(object) = message.as_object() else {
            self.log_with_context(
                LogEvent::new(LogLevel::Error, "mcp", "mcp_request_rejected")
                    .error("invalid request"),
                http_request_id,
            );
            return Some(error_response(Value::Null, -32600, "invalid request"));
        };

        let id = object.get("id").cloned();
        let response_id = id.clone().unwrap_or(Value::Null);
        if object.get("jsonrpc").and_then(Value::as_str) != Some(JSONRPC_VERSION) {
            self.log_with_context(
                LogEvent::new(LogLevel::Error, "mcp", "mcp_request_rejected")
                    .field("jsonrpc_id", jsonrpc_id_label(id.as_ref()))
                    .error("invalid JSON-RPC version"),
                http_request_id,
            );
            return Some(error_response(
                response_id,
                -32600,
                "invalid JSON-RPC version",
            ));
        }

        if id.as_ref().is_some_and(|id| !is_valid_request_id(id)) {
            self.log_with_context(
                LogEvent::new(LogLevel::Error, "mcp", "mcp_request_rejected")
                    .field("jsonrpc_id", jsonrpc_id_label(id.as_ref()))
                    .error("invalid JSON-RPC request id"),
                http_request_id,
            );
            return Some(error_response(
                Value::Null,
                -32600,
                "invalid JSON-RPC request id",
            ));
        }

        let method = match message.get("method").and_then(Value::as_str) {
            Some(method) => method,
            None => {
                self.log_with_context(
                    LogEvent::new(LogLevel::Error, "mcp", "mcp_request_rejected")
                        .field("jsonrpc_id", jsonrpc_id_label(id.as_ref()))
                        .error("invalid request"),
                    http_request_id,
                );
                return Some(error_response(response_id, -32600, "invalid request"));
            }
        };
        let is_notification = id.is_none();
        let started = Instant::now();
        self.log_with_context(
            LogEvent::new(LogLevel::Info, "mcp", "mcp_request_started")
                .field("method", method)
                .field("jsonrpc_id", jsonrpc_id_label(id.as_ref()))
                .field("is_notification", is_notification),
            http_request_id,
        );

        let result = match method {
            "initialize" => self.initialize(message.get("params").cloned().unwrap_or_default()),
            "notifications/initialized" => {
                self.log_with_context(
                    LogEvent::new(LogLevel::Info, "mcp", "mcp_request_finished")
                        .duration_ms(started.elapsed().as_millis())
                        .field("method", method)
                        .field("jsonrpc_id", jsonrpc_id_label(id.as_ref()))
                        .field("is_notification", true)
                        .field("response", "accepted"),
                    http_request_id,
                );
                return None;
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": self.service.tool_descriptors() })),
            "tools/call" => self.call_tool(
                message.get("params").cloned().unwrap_or_default(),
                http_request_id,
                id.as_ref(),
                tool_context,
            ),
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
            self.log_with_context(
                LogEvent::new(LogLevel::Error, "mcp", "mcp_request_finished")
                    .duration_ms(started.elapsed().as_millis())
                    .field("method", method)
                    .field("jsonrpc_id", jsonrpc_id_label(id.as_ref()))
                    .field("is_notification", true)
                    .field("error_code", -32600)
                    .error("request method requires id"),
                http_request_id,
            );
            return Some(error_response(
                Value::Null,
                -32600,
                "request method requires id",
            ));
        }

        let id = id.expect("request id checked above");
        Some(match result {
            Ok(result) => {
                self.log_with_context(
                    LogEvent::new(LogLevel::Info, "mcp", "mcp_request_finished")
                        .duration_ms(started.elapsed().as_millis())
                        .field("method", method)
                        .field("jsonrpc_id", jsonrpc_id_label(Some(&id)))
                        .field("is_notification", false),
                    http_request_id,
                );
                success_response(id, result)
            }
            Err(error) => {
                self.log_with_context(
                    LogEvent::new(LogLevel::Error, "mcp", "mcp_request_finished")
                        .duration_ms(started.elapsed().as_millis())
                        .field("method", method)
                        .field("jsonrpc_id", jsonrpc_id_label(Some(&id)))
                        .field("is_notification", false)
                        .field("error_code", error.code)
                        .error(error.message.clone()),
                    http_request_id,
                );
                error_response(id, error.code, &error.message)
            }
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

    fn call_tool(
        &self,
        params: Value,
        http_request_id: Option<u64>,
        jsonrpc_id: Option<&Value>,
        tool_context: ToolCallContext,
    ) -> std::result::Result<Value, ServerError> {
        let params: CallToolParams = serde_json::from_value(params).map_err(|error| {
            ServerError::new(-32602, format!("invalid tools/call params: {error}"))
        })?;

        let arguments = params
            .arguments
            .unwrap_or(Value::Object(Default::default()));
        let jsonrpc_id = jsonrpc_id_label(jsonrpc_id);
        let started = Instant::now();
        self.log_with_context(
            LogEvent::new(LogLevel::Info, "mcp", "mcp_tool_call_started")
                .field("tool_name", params.name.clone())
                .field("jsonrpc_id", jsonrpc_id.clone()),
            http_request_id,
        );
        let result =
            match self
                .service
                .call_tool_with_context(&params.name, arguments, tool_context)
            {
                Ok(value) => tool_success(value),
                Err(ToolCallError::InvalidRequest(message)) => {
                    self.log_with_context(
                        LogEvent::new(LogLevel::Error, "mcp", "mcp_tool_call_finished")
                            .duration_ms(started.elapsed().as_millis())
                            .field("tool_name", params.name.clone())
                            .field("jsonrpc_id", jsonrpc_id.clone())
                            .field("error_kind", "invalid_request")
                            .error(message.clone()),
                        http_request_id,
                    );
                    return Err(ServerError::new(-32602, message));
                }
                Err(ToolCallError::Execution(message)) => {
                    self.log_with_context(
                        LogEvent::new(LogLevel::Warn, "mcp", "mcp_tool_call_finished")
                            .duration_ms(started.elapsed().as_millis())
                            .field("tool_name", params.name.clone())
                            .field("jsonrpc_id", jsonrpc_id.clone())
                            .field("is_tool_error", true)
                            .error(message.clone()),
                        http_request_id,
                    );
                    return Ok(tool_error(message));
                }
            };

        self.log_with_context(
            LogEvent::new(LogLevel::Info, "mcp", "mcp_tool_call_finished")
                .duration_ms(started.elapsed().as_millis())
                .field("tool_name", params.name)
                .field("jsonrpc_id", jsonrpc_id)
                .field("is_tool_error", false),
            http_request_id,
        );
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

    pub(crate) fn log(&self, event: LogEvent) {
        self.logger.log(event);
    }

    fn log_with_context(&self, event: LogEvent, http_request_id: Option<u64>) {
        self.log(with_http_request_id(event, http_request_id));
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
    server_with_data_dir_proxy_ttd_ida_and_symbol_path(data_dir, proxy, None, None, None)
}

pub fn server_with_data_dir_proxy_ttd_and_symbol_path(
    data_dir: impl Into<PathBuf>,
    proxy: ProxyEnvironment,
    ttd_dir: Option<PathBuf>,
    symbol_path: Option<String>,
) -> std::result::Result<McpServer, String> {
    server_with_data_dir_proxy_ttd_ida_and_symbol_path(data_dir, proxy, ttd_dir, None, symbol_path)
}

pub fn server_with_data_dir_proxy_ttd_ida_and_symbol_path(
    data_dir: impl Into<PathBuf>,
    proxy: ProxyEnvironment,
    ttd_dir: Option<PathBuf>,
    ida_install_dir: Option<PathBuf>,
    symbol_path: Option<String>,
) -> std::result::Result<McpServer, String> {
    server_with_data_dir_proxy_ttd_ida_symbol_path_and_process(
        data_dir,
        proxy,
        ttd_dir,
        ida_install_dir,
        symbol_path,
        ProcessLaunchConfig::default(),
    )
}

pub fn server_with_data_dir_proxy_ttd_ida_symbol_path_and_process(
    data_dir: impl Into<PathBuf>,
    proxy: ProxyEnvironment,
    ttd_dir: Option<PathBuf>,
    ida_install_dir: Option<PathBuf>,
    symbol_path: Option<String>,
    process_launch: ProcessLaunchConfig,
) -> std::result::Result<McpServer, String> {
    let data_dir = data_dir.into();
    let logger = Arc::new(
        FileLogSink::new(data_dir.join("logs"), 7)
            .map_err(|error| format!("initialize log directory: {error}"))?,
    );
    Ok(server_from_config(
        McpServerConfig::new(data_dir, logger)
            .with_proxy(proxy)
            .with_ttd_dir(ttd_dir)
            .with_ida_install_dir(ida_install_dir)
            .with_symbol_path(symbol_path)
            .with_process_launch(process_launch),
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
    server_with_data_dir_proxy_symbol_path_and_logger(data_dir, proxy, None, logger)
}

pub fn server_with_data_dir_proxy_symbol_path_and_logger(
    data_dir: impl Into<PathBuf>,
    proxy: ProxyEnvironment,
    symbol_path: Option<String>,
    logger: Arc<dyn LogSink>,
) -> McpServer {
    server_with_data_dir_proxy_ttd_symbol_path_and_logger(
        data_dir,
        proxy,
        None,
        symbol_path,
        logger,
    )
}

pub fn server_with_data_dir_proxy_ttd_symbol_path_and_logger(
    data_dir: impl Into<PathBuf>,
    proxy: ProxyEnvironment,
    ttd_dir: Option<PathBuf>,
    symbol_path: Option<String>,
    logger: Arc<dyn LogSink>,
) -> McpServer {
    server_with_data_dir_proxy_ttd_ida_symbol_path_and_logger(
        data_dir,
        proxy,
        ttd_dir,
        None,
        symbol_path,
        logger,
    )
}

pub fn server_with_data_dir_proxy_ttd_ida_symbol_path_and_logger(
    data_dir: impl Into<PathBuf>,
    proxy: ProxyEnvironment,
    ttd_dir: Option<PathBuf>,
    ida_install_dir: Option<PathBuf>,
    symbol_path: Option<String>,
    logger: Arc<dyn LogSink>,
) -> McpServer {
    server_with_data_dir_proxy_ttd_ida_symbol_path_process_and_logger(
        data_dir,
        proxy,
        ttd_dir,
        ida_install_dir,
        symbol_path,
        ProcessLaunchConfig::default(),
        logger,
    )
}

pub fn server_with_data_dir_proxy_ttd_ida_symbol_path_process_and_logger(
    data_dir: impl Into<PathBuf>,
    proxy: ProxyEnvironment,
    ttd_dir: Option<PathBuf>,
    ida_install_dir: Option<PathBuf>,
    symbol_path: Option<String>,
    process_launch: ProcessLaunchConfig,
    logger: Arc<dyn LogSink>,
) -> McpServer {
    server_from_config(
        McpServerConfig::new(data_dir, logger)
            .with_proxy(proxy)
            .with_ttd_dir(ttd_dir)
            .with_ida_install_dir(ida_install_dir)
            .with_symbol_path(symbol_path)
            .with_process_launch(process_launch),
    )
}

pub fn server_from_config(config: McpServerConfig) -> McpServer {
    let artifact_root = config.data_dir.join("artifacts");
    let sessions =
        dbgflow_debug::session::SessionManager::with_worker_launcher_proxy_symbol_path_process_and_logger(
            default_process_worker_launcher(),
            &artifact_root,
            config.proxy,
            config.symbol_path,
            config.process_launch.clone(),
            config.logger.clone(),
        );
    let profiles = ProfileManager::with_logger_and_process_launch(
        &artifact_root,
        config.logger.clone(),
        config.process_launch.clone(),
    );
    let ttd_recordings = TtdRecordingManager::with_runtime_logger_and_process_launch(
        &artifact_root,
        config
            .ttd_dir
            .map(TtdRecorderRuntime::with_ttd_dir)
            .unwrap_or_else(TtdRecorderRuntime::unavailable),
        config.logger.clone(),
        config.process_launch.clone(),
    );
    let ida_sessions = IdaSessionManager::with_worker_launcher_runtime_process_and_logger(
        default_process_reverse_worker_launcher(),
        &artifact_root,
        IdaRuntimeConfig {
            install_dir: config.ida_install_dir,
        },
        config.process_launch,
        config.logger.clone(),
    );
    McpServer::new_with_logger(
        ToolService::with_profiles_ttd_and_reverse(
            sessions,
            profiles,
            ttd_recordings,
            ida_sessions,
        ),
        config.logger,
    )
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

fn default_process_reverse_worker_launcher() -> Arc<dyn ReverseWorkerLauncher> {
    let executable = std::env::current_exe()
        .or_else(|_| {
            std::env::args_os()
                .next()
                .map(PathBuf::from)
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "current exe"))
        })
        .unwrap_or_else(|_| PathBuf::from("dbgflow-mcp"));
    Arc::new(ProcessReverseWorkerLauncher::with_executable(executable))
}

fn is_valid_request_id(id: &Value) -> bool {
    id.is_string() || id.is_number() || id.is_null()
}

fn jsonrpc_id_label(id: Option<&Value>) -> String {
    match id {
        Some(Value::String(value)) => value.clone(),
        Some(value) => value.to_string(),
        None => "<notification>".to_string(),
    }
}

fn with_http_request_id(event: LogEvent, http_request_id: Option<u64>) -> LogEvent {
    match http_request_id {
        Some(http_request_id) => event.field("http_request_id", http_request_id),
        None => event,
    }
}

#[cfg(test)]
mod tests {
    use super::McpServer;
    use crate::tools::ToolService;
    use dbgflow_common::logging::noop_logger;
    use dbgflow_common::process::{ProcessLaunchConfig, ProcessLaunchContext, ToolCallContext};
    use dbgflow_common::proxy::ProxyEnvironment;
    use dbgflow_common::Result;
    use dbgflow_debug::backend::{CreateBackendSession, ExecuteBackendResult};
    use dbgflow_debug::session::worker::{SessionWorker, SessionWorkerLauncher, WorkerSession};
    use dbgflow_debug::session::{SessionId, SessionManager};
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
            tool["name"] == "dbg.create_session" && tool["inputSchema"]["type"] == "object"
        }));
        let create_session = tools
            .iter()
            .find(|tool| tool["name"] == "dbg.create_session")
            .expect("dbg.create_session tool");
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
            tool["name"] == "dbg.eval" && tool["inputSchema"]["required"][0] == "session_id"
        }));
        let eval = tools
            .iter()
            .find(|tool| tool["name"] == "dbg.eval")
            .expect("dbg.eval tool");
        assert!(eval["inputSchema"]["properties"]
            .get("timeout_ms")
            .is_none());
        assert!(tools.iter().any(|tool| tool["name"] == "dbg.get_session"));
        assert!(tools.iter().any(|tool| tool["name"] == "dbg.add_symbols"));
        assert!(tools
            .iter()
            .any(|tool| tool["name"] == "trace.record_profile"));
        assert!(tools.iter().any(|tool| tool["name"] == "trace.record_ttd"));
        let ida_create = tools
            .iter()
            .find(|tool| tool["name"] == "ida.create_session")
            .expect("ida.create_session tool");
        let ida_target_schema = &ida_create["inputSchema"]["properties"]["target"]["oneOf"];
        assert!(ida_target_schema
            .as_array()
            .expect("ida target variants")
            .iter()
            .any(|target| target["properties"]["kind"]["const"] == "binary"));
        assert!(ida_target_schema
            .as_array()
            .expect("ida target variants")
            .iter()
            .any(|target| target["properties"]["kind"]["const"] == "database"));
        assert!(tools.iter().any(|tool| {
            tool["name"] == "ida.list_segments"
                && tool["inputSchema"]["required"][0] == "session_id"
        }));
        assert!(tools
            .iter()
            .any(|tool| tool["name"] == "ida.list_functions"));
        assert!(tools.iter().any(|tool| tool["name"] == "ida.decompile"));
        assert!(tools.iter().any(|tool| tool["name"] == "ida.rename"));
        let ida_close = tools
            .iter()
            .find(|tool| tool["name"] == "ida.close_session")
            .expect("ida.close_session tool");
        assert!(ida_close["inputSchema"]["properties"].get("save").is_some());
        assert!(!tools.iter().any(|tool| tool["name"] == "set_symbols"));
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
                    "name": "dbg.create_session",
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
                    "name": "dbg.eval",
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
    fn tools_call_create_session_passes_tool_context_to_worker_launcher() {
        let launcher = Arc::new(TestWorkerLauncher::default());
        let process_launch = ProcessLaunchConfig::installed_service_default();
        let server = McpServer::new(ToolService::new(
            SessionManager::with_worker_launcher_proxy_symbol_path_process_and_logger(
                launcher.clone(),
                test_artifact_root("mcp-context-propagation"),
                ProxyEnvironment::none(),
                None,
                process_launch.clone(),
                noop_logger(),
            ),
        ));
        let dump_path = test_dump_path("mcp-context-dump");
        let tool_context = ToolCallContext {
            peer_pid: Some(1234),
            peer_session_id: Some(56),
        };

        let response = server
            .handle_message_with_context(
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/call",
                    "params": {
                        "name": "dbg.create_session",
                        "arguments": {
                            "target": { "kind": "dump", "path": dump_path }
                        }
                    }
                }),
                Some(42),
                tool_context,
            )
            .expect("create response");

        assert_eq!(response["result"]["isError"], false);
        let recorded = wait_for_launch_context(&launcher);
        assert_eq!(recorded.config, process_launch);
        assert_eq!(recorded.tool_call, tool_context);
    }

    #[test]
    fn tools_call_add_symbols_accepts_raw_symbol_path() {
        let server = test_server();
        let dump_path = test_dump_path("mcp-add-symbols-raw");
        let create_response = server
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "dbg.create_session",
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
                    "name": "dbg.add_symbols",
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
            .contains(&format!(".sympath+ {symbol_path}")));
    }

    #[test]
    fn tools_call_add_symbols_rejects_line_separators() {
        let server = test_server();
        let dump_path = test_dump_path("mcp-add-symbols-line-separator");
        let create_response = server
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "dbg.create_session",
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
                    "name": "dbg.add_symbols",
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
    fn tools_call_add_symbols_rejects_append_field() {
        let server = test_server();
        let response = server
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "dbg.add_symbols",
                    "arguments": {
                        "session_id": "00000000-0000-0000-0000-000000000000",
                        "paths": ["srv*C:\\symbols"],
                        "append": true
                    }
                }
            }))
            .expect("response");

        assert_eq!(response["error"]["code"], -32602);
        assert!(response["error"]["message"]
            .as_str()
            .expect("error message")
            .contains("unknown field"));
    }

    #[test]
    fn tools_call_add_symbols_rejects_timeout_field() {
        let server = test_server();
        let response = server
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "dbg.add_symbols",
                    "arguments": {
                        "session_id": "00000000-0000-0000-0000-000000000000",
                        "paths": ["srv*C:\\symbols"],
                        "timeout_ms": 1000
                    }
                }
            }))
            .expect("response");

        assert_eq!(response["error"]["code"], -32602);
        assert!(response["error"]["message"]
            .as_str()
            .expect("error message")
            .contains("unknown field"));
    }

    #[test]
    fn tools_call_lists_ida_sessions() {
        let server = test_server();
        let response = server
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "ida.list_sessions",
                    "arguments": {}
                }
            }))
            .expect("response");

        let sessions = tool_text_json(&response);
        assert_eq!(sessions.as_array().expect("sessions array").len(), 0);
    }

    #[test]
    fn tools_call_rejects_legacy_flat_names() {
        let server = test_server();
        let response = server
            .handle_message(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "set_symbols",
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
                    "name": "dbg.eval",
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
                    "name": "dbg.create_session",
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
                    "name": "dbg.create_session",
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
                    "name": "dbg.create_session",
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
                    "name": "dbg.eval",
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
                        "name": "dbg.get_session",
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
        contexts: Mutex<Vec<ProcessLaunchContext>>,
    }

    impl SessionWorkerLauncher for TestWorkerLauncher {
        fn spawn(
            &self,
            _session_id: SessionId,
            _logger: Arc<dyn dbgflow_common::logging::LogSink>,
            _proxy: ProxyEnvironment,
            launch_context: ProcessLaunchContext,
        ) -> Result<Arc<dyn SessionWorker>> {
            self.contexts
                .lock()
                .expect("contexts lock")
                .push(launch_context);
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            Ok(Arc::new(TestWorker {
                backend_session_id: format!("fake-{id}"),
                closed: Mutex::new(false),
            }))
        }
    }

    fn wait_for_launch_context(launcher: &TestWorkerLauncher) -> ProcessLaunchContext {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(context) = launcher
                .contexts
                .lock()
                .expect("contexts lock")
                .last()
                .cloned()
            {
                return context;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "worker launcher did not receive process launch context"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
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
            _event_sink: std::sync::Arc<dyn dbgflow_debug::backend::BackendEventSink>,
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
                return Err(dbgflow_common::DbgFlowError::Backend(
                    "fake kill failed".to_string(),
                ));
            }
            Ok(())
        }
    }
}
