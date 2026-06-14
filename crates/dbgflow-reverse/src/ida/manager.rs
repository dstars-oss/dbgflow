use super::install::IdaRuntimeConfig;
use super::model::{
    CloseDatabaseResult, IdaOpenMode, IdaSessionHealth, IdaToolCallResult, ReverseSession,
    ReverseSessionState, SaveStatus, UpstreamIdaToolRequest, UpstreamToolDescriptor,
};
use super::target::{validate_ida_target, IdaTarget};
use super::worker::{
    download_ida_mcp_output, fallback_tool_descriptors, is_allowed_upstream_tool, IdaSupervisor,
    OpenIdaDatabase, ProcessIdaSupervisor, SupervisorToolCallResult, UPSTREAM_IDB_SAVE,
};
use dbgflow_common::artifacts::{ArtifactManager, ArtifactRef, ReverseSessionArtifactEvent};
use dbgflow_common::logging::{noop_logger, LogEvent, LogLevel, LogSink};
use dbgflow_common::process::{ProcessLaunchConfig, ProcessLaunchContext, ToolCallContext};
use dbgflow_common::time::now_unix_ms;
use dbgflow_common::{DbgFlowError, Result, SessionId};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const BACKEND_NAME: &str = "ida-pro-mcp";
const DEFAULT_STARTUP_TIMEOUT_MS: u64 = 1_800_000;
const DEFAULT_IDLE_TTL_SEC: u64 = 3_600;
static OUTPUT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateIdaSession {
    pub target: IdaTarget,
    #[serde(default = "default_run_auto_analysis")]
    pub run_auto_analysis: bool,
    #[serde(default = "default_build_caches")]
    pub build_caches: bool,
    #[serde(default = "default_init_hexrays")]
    pub init_hexrays: bool,
    #[serde(default)]
    pub mode: IdaOpenMode,
    #[serde(default = "default_idle_ttl_sec")]
    pub idle_ttl_sec: u64,
    pub startup_timeout_ms: Option<u64>,
}

#[derive(Clone)]
pub struct IdaSessionManager {
    supervisor: Arc<dyn IdaSupervisor>,
    sessions: Arc<Mutex<HashMap<SessionId, ReverseSession>>>,
    database_ids: Arc<Mutex<HashMap<SessionId, String>>>,
    operation_locks: Arc<Mutex<HashMap<SessionId, Arc<Mutex<()>>>>>,
    create_lock: Arc<Mutex<()>>,
    artifacts: ArtifactManager,
    logger: Arc<dyn LogSink>,
}

impl IdaSessionManager {
    pub fn new(artifact_root: impl Into<PathBuf>, runtime: IdaRuntimeConfig) -> Self {
        Self::with_runtime_process_and_logger(
            artifact_root,
            runtime,
            ProcessLaunchConfig::default(),
            noop_logger(),
        )
    }

    pub fn with_logger(
        artifact_root: impl Into<PathBuf>,
        runtime: IdaRuntimeConfig,
        logger: Arc<dyn LogSink>,
    ) -> Self {
        Self::with_runtime_process_and_logger(
            artifact_root,
            runtime,
            ProcessLaunchConfig::default(),
            logger,
        )
    }

    pub fn with_runtime_process_and_logger(
        artifact_root: impl Into<PathBuf>,
        runtime: IdaRuntimeConfig,
        process_launch: ProcessLaunchConfig,
        logger: Arc<dyn LogSink>,
    ) -> Self {
        let artifact_root = artifact_root.into();
        let supervisor = Arc::new(ProcessIdaSupervisor::new(
            &artifact_root,
            runtime,
            ProcessLaunchContext::new(process_launch, ToolCallContext::default()),
            logger.clone(),
        ));
        Self::with_supervisor_and_logger(supervisor, artifact_root, logger)
    }

    pub fn with_supervisor_and_logger(
        supervisor: Arc<dyn IdaSupervisor>,
        artifact_root: impl Into<PathBuf>,
        logger: Arc<dyn LogSink>,
    ) -> Self {
        Self {
            supervisor,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            database_ids: Arc::new(Mutex::new(HashMap::new())),
            operation_locks: Arc::new(Mutex::new(HashMap::new())),
            create_lock: Arc::new(Mutex::new(())),
            artifacts: ArtifactManager::new(artifact_root),
            logger,
        }
    }

    pub fn create_session(&self, request: CreateIdaSession) -> Result<ReverseSession> {
        self.create_session_with_context(request, ToolCallContext::default())
    }

    pub fn create_session_with_context(
        &self,
        mut request: CreateIdaSession,
        _tool_context: ToolCallContext,
    ) -> Result<ReverseSession> {
        let requested_target = request.target.clone();
        request.target = match validate_ida_target(request.target) {
            Ok(target) => target,
            Err(error) => {
                self.log(
                    LogEvent::new(LogLevel::Error, "reverse_ida", "create_session_rejected")
                        .operation("ida.create_session")
                        .field("target", requested_target)
                        .error(error.to_string()),
                );
                return Err(error);
            }
        };

        let _create_guard = self
            .create_lock
            .lock()
            .map_err(|_| DbgFlowError::Backend("IDA create session lock poisoned".to_string()))?;

        if let Some(existing) = self.reusable_session_for_target(&request.target)? {
            self.log(
                LogEvent::new(LogLevel::Info, "reverse_ida", "create_session_reused")
                    .session_id(existing.id)
                    .operation("ida.create_session")
                    .field("target", &existing.target),
            );
            return Ok(existing);
        }

        let now = now_unix_ms();
        let mut session = ReverseSession {
            id: SessionId::new(),
            backend: BACKEND_NAME.to_string(),
            target: request.target.clone(),
            state: ReverseSessionState::Starting,
            database_id: None,
            ida_backend: None,
            adopted: None,
            owned: None,
            pid: None,
            worker_pid: None,
            is_active: None,
            last_health: None,
            created_at_unix_ms: now,
            updated_at_unix_ms: now,
            warnings: Vec::new(),
            artifacts: Vec::new(),
            error: None,
        };
        self.initialize_session_audit(&mut session, &request)?;
        self.insert_session(session.clone())?;

        let operation_lock = self.operation_lock(session.id)?;
        let _operation_guard = operation_lock.lock().map_err(|_| {
            DbgFlowError::Backend("IDA session operation lock poisoned".to_string())
        })?;

        let startup_timeout_ms = request
            .startup_timeout_ms
            .unwrap_or(DEFAULT_STARTUP_TIMEOUT_MS)
            .max(1);
        let started = Instant::now();
        let open_result = self.supervisor.open_session(OpenIdaDatabase {
            input_path: request.target.path().to_path_buf(),
            mode: request.mode.clone(),
            run_auto_analysis: request.run_auto_analysis,
            build_caches: request.build_caches,
            init_hexrays: request.init_hexrays,
            idle_ttl_sec: request.idle_ttl_sec,
            preferred_session_id: format!("dbgflow-{}", session.id),
            startup_timeout_ms,
        });

        match open_result {
            Ok(opened) => {
                self.database_ids
                    .lock()
                    .map_err(|_| {
                        DbgFlowError::Backend("IDA database map lock poisoned".to_string())
                    })?
                    .insert(session.id, opened.session.database_id.clone());
                let mut session = self.update_session(session.id, |session| {
                    session.state = ReverseSessionState::Ready;
                    session.database_id = Some(opened.session.database_id.clone());
                    session.ida_backend = opened.session.backend.clone();
                    session.adopted = opened.session.adopted;
                    session.owned = opened.session.owned;
                    session.pid = opened.session.pid;
                    session.worker_pid = opened.session.worker_pid;
                    session.is_active = opened.session.is_active;
                    session.last_health = Some(IdaSessionHealth {
                        reachable: true,
                        detail: Some(json!({
                            "session": opened.session,
                            "warmup": opened.warmup,
                            "message": opened.message,
                        })),
                        error: None,
                    });
                    session.error = None;
                })?;
                self.log(
                    LogEvent::new(LogLevel::Info, "reverse_ida", "create_session_finished")
                        .session_id(session.id)
                        .operation("ida.create_session")
                        .duration_ms(started.elapsed().as_millis())
                        .field("database_id", session.database_id.clone()),
                );
                self.record_event(
                    session.id,
                    "create_session_finished",
                    Some(ReverseSessionState::Starting),
                    Some(ReverseSessionState::Ready),
                    Some("ida.create_session"),
                    None,
                    None,
                    fields([("database_id", json!(session.database_id))]),
                );
                session = self.get_session(session.id)?;
                Ok(session)
            }
            Err(error) => {
                let error_text = error.to_string();
                self.update_session(session.id, |session| {
                    session.state = ReverseSessionState::Error;
                    session.error = Some(error_text.clone());
                    session.last_health = Some(IdaSessionHealth {
                        reachable: false,
                        detail: None,
                        error: Some(error_text.clone()),
                    });
                })?;
                self.log(
                    LogEvent::new(LogLevel::Error, "reverse_ida", "create_session_failed")
                        .session_id(session.id)
                        .operation("ida.create_session")
                        .duration_ms(started.elapsed().as_millis())
                        .error(error_text.clone()),
                );
                Err(error)
            }
        }
    }

    pub fn get_session(&self, session_id: SessionId) -> Result<ReverseSession> {
        let _ = self.refresh_session_health(session_id);
        self.sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("IDA session manager lock poisoned".to_string()))?
            .get(&session_id)
            .cloned()
            .ok_or(DbgFlowError::SessionNotFound(session_id))
    }

    pub fn list_sessions(&self) -> Result<Vec<ReverseSession>> {
        let ids = self
            .sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("IDA session manager lock poisoned".to_string()))?
            .keys()
            .copied()
            .collect::<Vec<_>>();
        for id in ids {
            let _ = self.refresh_session_health(id);
        }
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("IDA session manager lock poisoned".to_string()))?
            .values()
            .cloned()
            .collect::<Vec<_>>();
        sessions.sort_by_key(|session| session.created_at_unix_ms);
        Ok(sessions)
    }

    pub fn close_session(&self, session_id: SessionId) -> Result<ReverseSession> {
        self.close_session_with_save(session_id, true)
    }

    pub fn close_session_with_save(
        &self,
        session_id: SessionId,
        save: bool,
    ) -> Result<ReverseSession> {
        let operation_lock = self.operation_lock(session_id)?;
        let _operation_guard = operation_lock.lock().map_err(|_| {
            DbgFlowError::Backend("IDA session operation lock poisoned".to_string())
        })?;
        let previous_state = self.get_session(session_id)?.state;
        if previous_state == ReverseSessionState::Closed {
            return Err(DbgFlowError::SessionClosed(session_id));
        }
        let database_id = self.database_id(session_id)?;
        self.update_session_state(
            session_id,
            ReverseSessionState::Closing,
            "ida.close_session",
            None,
        )?;
        let started = Instant::now();
        let close_result = if save {
            let save_result =
                self.supervisor
                    .call_tool(&database_id, UPSTREAM_IDB_SAVE, Map::new());
            match save_result {
                Ok(result) => {
                    let mut warnings = Vec::new();
                    let artifact = self.write_tool_output(
                        session_id,
                        "idb_save",
                        &Value::Object(Map::new()),
                        &result,
                        &mut warnings,
                    )?;
                    self.append_artifact_to_session(session_id, artifact)?;
                    CloseDatabaseResult {
                        save_requested: true,
                        save_status: if result.is_error
                            || result.structured_content.get("ok").and_then(Value::as_bool)
                                == Some(false)
                        {
                            SaveStatus::Failed
                        } else {
                            SaveStatus::Saved
                        },
                        warning: None,
                        error: result.error_message.clone().or_else(|| {
                            result
                                .structured_content
                                .get("error")
                                .and_then(Value::as_str)
                                .map(ToString::to_string)
                        }),
                    }
                }
                Err(error) => CloseDatabaseResult {
                    save_requested: true,
                    save_status: SaveStatus::Failed,
                    warning: None,
                    error: Some(error.to_string()),
                },
            }
        } else {
            CloseDatabaseResult {
                save_requested: false,
                save_status: SaveStatus::NotRequested,
                warning: None,
                error: None,
            }
        };

        if close_result.save_status == SaveStatus::Failed || close_result.error.is_some() {
            let error = close_result
                .error
                .clone()
                .unwrap_or_else(|| "ida-pro-mcp idb_save failed".to_string());
            self.append_session_warning(session_id, &error)?;
            self.update_session(session_id, |session| {
                session.state = previous_state.clone();
                session.last_health = Some(IdaSessionHealth {
                    reachable: true,
                    detail: Some(json!({
                        "save": close_result,
                        "detached": false,
                    })),
                    error: Some(error.clone()),
                });
                session.error = Some(error.clone());
            })?;
            self.log(
                LogEvent::new(LogLevel::Error, "reverse_ida", "close_session_failed")
                    .session_id(session_id)
                    .operation("ida.close_session")
                    .field("save", save)
                    .field("save_status", json!(close_result.save_status))
                    .duration_ms(started.elapsed().as_millis())
                    .error(error.clone()),
            );
            self.record_event(
                session_id,
                "close_session_failed",
                Some(ReverseSessionState::Closing),
                Some(previous_state),
                Some("ida.close_session"),
                None,
                Some(error.clone()),
                close_fields(save, &close_result),
            );
            return Err(DbgFlowError::Backend(error));
        }

        self.database_ids
            .lock()
            .map_err(|_| DbgFlowError::Backend("IDA database map lock poisoned".to_string()))?
            .remove(&session_id);
        if let Some(warning) = close_result.warning.as_deref() {
            self.append_session_warning(session_id, warning)?;
        }
        if let Some(error) = close_result.error.as_deref() {
            self.append_session_warning(session_id, error)?;
        }
        self.update_session(session_id, |session| {
            session.state = ReverseSessionState::Closed;
            session.database_id = None;
            session.is_active = Some(false);
            session.last_health = Some(IdaSessionHealth {
                reachable: false,
                detail: Some(json!({
                    "save": close_result,
                    "detached": true,
                    "worker_lifecycle": "upstream idle TTL"
                })),
                error: None,
            });
            session.error = None;
        })?;
        self.log(
            LogEvent::new(LogLevel::Info, "reverse_ida", "close_session_finished")
                .session_id(session_id)
                .operation("ida.close_session")
                .field("save", save)
                .field("save_status", json!(close_result.save_status))
                .duration_ms(started.elapsed().as_millis()),
        );
        self.record_event(
            session_id,
            "close_session_finished",
            Some(previous_state),
            Some(ReverseSessionState::Closed),
            Some("ida.close_session"),
            None,
            None,
            close_fields(save, &close_result),
        );
        self.get_session(session_id)
    }

    pub fn upstream_tool_descriptors(&self) -> Vec<UpstreamToolDescriptor> {
        self.supervisor
            .tool_descriptors()
            .unwrap_or_else(|_| fallback_tool_descriptors())
    }

    pub fn is_allowed_upstream_tool(&self, tool_name: &str) -> bool {
        is_allowed_upstream_tool(tool_name)
    }

    pub fn call_upstream_tool(
        &self,
        tool_name: &str,
        request: UpstreamIdaToolRequest,
    ) -> Result<IdaToolCallResult> {
        if !is_allowed_upstream_tool(tool_name) {
            return Err(DbgFlowError::Backend(format!(
                "IDA upstream tool is not exposed by dbgflow: {tool_name}"
            )));
        }
        if request.arguments.contains_key("database") {
            return Err(DbgFlowError::Backend(
                "IDA tools accept dbgflow session_id, not upstream database".to_string(),
            ));
        }

        let operation_lock = self.operation_lock(request.session_id)?;
        let _operation_guard = operation_lock.lock().map_err(|_| {
            DbgFlowError::Backend("IDA session operation lock poisoned".to_string())
        })?;
        self.ensure_ready(request.session_id, tool_name)?;
        let database_id = self.database_id(request.session_id)?;
        let request_value = Value::Object(request.arguments.clone());
        let mut warnings = Vec::new();
        let result = match self
            .supervisor
            .call_tool(&database_id, tool_name, request.arguments)
        {
            Ok(result) => result,
            Err(error) => {
                let artifact = self.write_tool_error_output(
                    request.session_id,
                    tool_name,
                    &request_value,
                    &error.to_string(),
                )?;
                self.append_artifact_to_session(request.session_id, artifact.clone())?;
                self.record_event(
                    request.session_id,
                    "upstream_tool_failed",
                    None,
                    None,
                    Some(&format!("ida.{tool_name}")),
                    Some(artifact.path.clone()),
                    Some(error.to_string()),
                    fields([("tool", json!(tool_name))]),
                );
                return Err(error);
            }
        };
        let artifact = self.write_tool_output(
            request.session_id,
            tool_name,
            &request_value,
            &result,
            &mut warnings,
        )?;
        self.append_artifact_to_session(request.session_id, artifact.clone())?;
        if result.is_error {
            let error = result
                .error_message
                .clone()
                .unwrap_or_else(|| "ida-pro-mcp upstream tool failed".to_string());
            self.record_event(
                request.session_id,
                "upstream_tool_failed",
                None,
                None,
                Some(&format!("ida.{tool_name}")),
                Some(artifact.path.clone()),
                Some(error.clone()),
                fields([
                    ("tool", json!(tool_name)),
                    ("warnings", json!(warnings.clone())),
                ]),
            );
            return Err(DbgFlowError::Backend(error));
        }
        self.record_event(
            request.session_id,
            "upstream_tool_finished",
            None,
            None,
            Some(&format!("ida.{tool_name}")),
            Some(artifact.path.clone()),
            None,
            fields([
                ("tool", json!(tool_name)),
                ("warnings", json!(warnings.clone())),
            ]),
        );
        Ok(IdaToolCallResult {
            session_id: request.session_id,
            tool: format!("ida.{tool_name}"),
            result: result.structured_content,
            artifact,
            warnings,
        })
    }

    fn write_tool_output(
        &self,
        session_id: SessionId,
        tool_name: &str,
        request: &Value,
        response: &SupervisorToolCallResult,
        warnings: &mut Vec<String>,
    ) -> Result<ArtifactRef> {
        let mut output = json!({
            "tool": format!("ida.{tool_name}"),
            "request": request,
            "structured_content": response.structured_content,
            "mcp_result": response.mcp_result,
        });
        if let Some(download_url) = ida_mcp_download_url(&response.mcp_result) {
            match download_ida_mcp_output(download_url) {
                Ok(full) => {
                    output["downloaded_full_output"] = full;
                }
                Err(error) => {
                    let warning = format!("download full ida-pro-mcp output failed: {error}");
                    warnings.push(warning.clone());
                    output["download_warning"] = json!(warning);
                }
            }
        }
        let artifact_name = unique_output_name(&format!("{tool_name}.json"));
        self.artifacts
            .write_reverse_session_output(session_id, &artifact_name, &output)
    }

    fn write_tool_error_output(
        &self,
        session_id: SessionId,
        tool_name: &str,
        request: &Value,
        error: &str,
    ) -> Result<ArtifactRef> {
        let output = json!({
            "tool": format!("ida.{tool_name}"),
            "request": request,
            "error": error,
        });
        let artifact_name = unique_output_name(&format!("{tool_name}.error.json"));
        self.artifacts
            .write_reverse_session_output(session_id, &artifact_name, &output)
    }

    fn refresh_session_health(&self, session_id: SessionId) -> Result<()> {
        let session = self
            .sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("IDA session manager lock poisoned".to_string()))?
            .get(&session_id)
            .cloned()
            .ok_or(DbgFlowError::SessionNotFound(session_id))?;
        if session.state.is_terminal() {
            return Ok(());
        }
        let Some(database_id) = session.database_id.clone() else {
            return Ok(());
        };
        match self.supervisor.list_sessions() {
            Ok(upstream) => {
                let found = upstream
                    .into_iter()
                    .find(|entry| entry.database_id == database_id);
                match found {
                    Some(entry) => {
                        self.update_session(session_id, |session| {
                            session.ida_backend = entry.backend.clone();
                            session.adopted = entry.adopted;
                            session.owned = entry.owned;
                            session.pid = entry.pid;
                            session.worker_pid = entry.worker_pid;
                            session.is_active = entry.is_active;
                            session.last_health = Some(IdaSessionHealth {
                                reachable: entry.is_active.unwrap_or(true),
                                detail: Some(json!(entry)),
                                error: None,
                            });
                        })?;
                    }
                    None => {
                        let warning = "upstream IDA database session is no longer listed";
                        self.append_session_warning(session_id, warning)?;
                        self.update_session(session_id, |session| {
                            session.state = ReverseSessionState::Error;
                            session.error = Some(warning.to_string());
                            session.is_active = Some(false);
                            session.last_health = Some(IdaSessionHealth {
                                reachable: false,
                                detail: None,
                                error: Some(warning.to_string()),
                            });
                        })?;
                    }
                }
            }
            Err(error) => {
                let error_text = error.to_string();
                self.update_session(session_id, |session| {
                    session.last_health = Some(IdaSessionHealth {
                        reachable: false,
                        detail: None,
                        error: Some(error_text.clone()),
                    });
                })?;
            }
        }
        Ok(())
    }

    fn reusable_session_for_target(&self, target: &IdaTarget) -> Result<Option<ReverseSession>> {
        let sessions = self
            .sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("IDA session manager lock poisoned".to_string()))?;
        Ok(sessions
            .values()
            .find(|session| session.target == *target && session.state.is_reusable())
            .cloned())
    }

    fn ensure_ready(&self, session_id: SessionId, operation: &str) -> Result<()> {
        let session = self.get_session(session_id)?;
        match session.state {
            ReverseSessionState::Ready => Ok(()),
            ReverseSessionState::Closed => Err(DbgFlowError::SessionClosed(session_id)),
            state => Err(DbgFlowError::Backend(format!(
                "{operation} requires Ready IDA session; current state is {state:?}"
            ))),
        }
    }

    fn database_id(&self, session_id: SessionId) -> Result<String> {
        self.database_ids
            .lock()
            .map_err(|_| DbgFlowError::Backend("IDA database map lock poisoned".to_string()))?
            .get(&session_id)
            .cloned()
            .ok_or_else(|| {
                DbgFlowError::Backend(format!(
                    "IDA session {session_id} is not bound to an upstream database"
                ))
            })
    }

    fn insert_session(&self, session: ReverseSession) -> Result<()> {
        self.sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("IDA session manager lock poisoned".to_string()))?
            .insert(session.id, session);
        Ok(())
    }

    fn update_session(
        &self,
        session_id: SessionId,
        update: impl FnOnce(&mut ReverseSession),
    ) -> Result<ReverseSession> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("IDA session manager lock poisoned".to_string()))?;
        let session = sessions
            .get_mut(&session_id)
            .ok_or(DbgFlowError::SessionNotFound(session_id))?;
        update(session);
        session.updated_at_unix_ms = now_unix_ms();
        let metadata = serde_json::to_value(&*session)
            .map_err(|error| DbgFlowError::Backend(error.to_string()))?;
        let _ = self
            .artifacts
            .write_reverse_session_metadata(session_id, &metadata);
        Ok(session.clone())
    }

    fn update_session_state(
        &self,
        session_id: SessionId,
        state: ReverseSessionState,
        operation: &str,
        error: Option<String>,
    ) -> Result<ReverseSessionState> {
        let previous = self.get_session(session_id)?.state;
        self.update_session(session_id, |session| {
            session.state = state.clone();
            session.error = error.clone();
        })?;
        self.record_event(
            session_id,
            "state_changed",
            Some(previous.clone()),
            Some(state),
            Some(operation),
            None,
            error,
            Map::new(),
        );
        Ok(previous)
    }

    fn operation_lock(&self, session_id: SessionId) -> Result<Arc<Mutex<()>>> {
        let mut locks = self
            .operation_locks
            .lock()
            .map_err(|_| DbgFlowError::Backend("IDA operation locks map poisoned".to_string()))?;
        Ok(locks
            .entry(session_id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone())
    }

    fn initialize_session_audit(
        &self,
        session: &mut ReverseSession,
        request: &CreateIdaSession,
    ) -> Result<()> {
        let request_artifact = self.artifacts.write_reverse_session_request(
            session.id,
            &json!({
                "target": request.target,
                "run_auto_analysis": request.run_auto_analysis,
                "build_caches": request.build_caches,
                "init_hexrays": request.init_hexrays,
                "mode": request.mode,
                "idle_ttl_sec": request.idle_ttl_sec,
                "startup_timeout_ms": request.startup_timeout_ms,
            }),
        )?;
        let metadata_artifact = self.artifacts.write_reverse_session_metadata(
            session.id,
            &serde_json::to_value(&*session).unwrap(),
        )?;
        session.artifacts.push(request_artifact.clone());
        session.artifacts.push(metadata_artifact.clone());
        self.record_event(
            session.id,
            "create_session_started",
            None,
            Some(ReverseSessionState::Starting),
            Some("ida.create_session"),
            Some(request_artifact.path),
            None,
            fields([
                ("backend", json!(BACKEND_NAME)),
                ("target", json!(session.target)),
            ]),
        );
        Ok(())
    }

    fn append_artifact_to_session(
        &self,
        session_id: SessionId,
        artifact: ArtifactRef,
    ) -> Result<()> {
        self.update_session(session_id, |session| {
            session.artifacts.push(artifact);
        })?;
        Ok(())
    }

    fn append_session_warning(&self, session_id: SessionId, warning: &str) -> Result<()> {
        self.update_session(session_id, |session| {
            session.warnings.push(warning.to_string());
        })?;
        Ok(())
    }

    fn record_event(
        &self,
        session_id: SessionId,
        event: &'static str,
        previous_state: Option<ReverseSessionState>,
        new_state: Option<ReverseSessionState>,
        operation: Option<&str>,
        artifact: Option<PathBuf>,
        error: Option<String>,
        fields: Map<String, Value>,
    ) {
        let artifact_event = ReverseSessionArtifactEvent {
            timestamp_unix_ms: now_unix_ms(),
            event: event.to_string(),
            session_id: session_id.to_string(),
            previous_state: previous_state.map(|state| format!("{state:?}")),
            new_state: new_state.map(|state| format!("{state:?}")),
            operation: operation.map(ToString::to_string),
            artifact_path: artifact,
            error,
            fields,
        };
        let _ = self
            .artifacts
            .append_reverse_session_event(session_id, &artifact_event);
    }

    fn log(&self, event: LogEvent) {
        self.logger.log(event);
    }
}

fn ida_mcp_download_url(result: &Value) -> Option<&str> {
    result
        .get("_meta")?
        .get("ida_mcp")?
        .get("download_url")?
        .as_str()
}

fn unique_output_name(base: &str) -> String {
    let sequence = OUTPUT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let sanitized = base
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    format!("{sequence:016x}-{sanitized}")
}

fn close_fields(save: bool, close_result: &CloseDatabaseResult) -> Map<String, Value> {
    fields([
        ("save_requested", json!(save)),
        ("save_status", json!(close_result.save_status)),
        ("warning", json!(close_result.warning)),
        ("error", json!(close_result.error)),
        ("detached", json!(true)),
    ])
}

fn fields(items: impl IntoIterator<Item = (&'static str, Value)>) -> Map<String, Value> {
    items
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
}

fn default_run_auto_analysis() -> bool {
    true
}

fn default_build_caches() -> bool {
    true
}

fn default_init_hexrays() -> bool {
    true
}

fn default_idle_ttl_sec() -> u64 {
    DEFAULT_IDLE_TTL_SEC
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbgflow_common::logging::noop_logger;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::thread;
    use std::time::Duration;

    #[test]
    fn create_session_maps_dbgflow_id_to_upstream_database() {
        let root = test_root("create");
        let supervisor = Arc::new(MockSupervisor::default());
        let manager = manager_with_mock(&root, supervisor.clone());
        let target = fake_binary(&root);

        let session = manager
            .create_session(CreateIdaSession {
                target: IdaTarget::Binary { path: target },
                run_auto_analysis: true,
                build_caches: true,
                init_hexrays: true,
                mode: IdaOpenMode::PreferHeadless,
                idle_ttl_sec: 3600,
                startup_timeout_ms: Some(10_000),
            })
            .expect("create");

        assert_eq!(session.state, ReverseSessionState::Ready);
        assert_eq!(session.database_id.as_deref(), Some("upstream-db"));
        assert_eq!(
            manager.database_id(session.id).expect("database id"),
            "upstream-db"
        );
        let opened = supervisor.opened.lock().expect("opened");
        assert_eq!(opened.len(), 1);
        assert!(opened[0].preferred_session_id.starts_with("dbgflow-"));
    }

    #[test]
    fn create_session_reuses_same_target() {
        let root = test_root("reuse");
        let supervisor = Arc::new(MockSupervisor::default());
        let manager = manager_with_mock(&root, supervisor.clone());
        let target = fake_binary(&root);

        let first = manager
            .create_session(CreateIdaSession {
                target: IdaTarget::Binary {
                    path: target.clone(),
                },
                run_auto_analysis: true,
                build_caches: true,
                init_hexrays: true,
                mode: IdaOpenMode::PreferHeadless,
                idle_ttl_sec: 3600,
                startup_timeout_ms: None,
            })
            .expect("first");
        let second = manager
            .create_session(CreateIdaSession {
                target: IdaTarget::Binary { path: target },
                run_auto_analysis: true,
                build_caches: true,
                init_hexrays: true,
                mode: IdaOpenMode::PreferHeadless,
                idle_ttl_sec: 3600,
                startup_timeout_ms: None,
            })
            .expect("second");

        assert_eq!(first.id, second.id);
        assert_eq!(supervisor.opened.lock().expect("opened").len(), 1);
    }

    #[test]
    fn concurrent_create_session_reuses_same_target() {
        let root = test_root("reuse-concurrent");
        let supervisor = Arc::new(MockSupervisor::default());
        supervisor.sleep_ms.store(50, Ordering::Relaxed);
        let manager = manager_with_mock(&root, supervisor.clone());
        let target = fake_binary(&root);
        let barrier = Arc::new(std::sync::Barrier::new(2));

        let first_manager = manager.clone();
        let first_target = target.clone();
        let first_barrier = barrier.clone();
        let t1 = thread::spawn(move || {
            first_barrier.wait();
            first_manager
                .create_session(CreateIdaSession {
                    target: IdaTarget::Binary { path: first_target },
                    run_auto_analysis: true,
                    build_caches: true,
                    init_hexrays: true,
                    mode: IdaOpenMode::PreferHeadless,
                    idle_ttl_sec: 3600,
                    startup_timeout_ms: None,
                })
                .expect("first")
        });
        let second_manager = manager.clone();
        let second_barrier = barrier.clone();
        let t2 = thread::spawn(move || {
            second_barrier.wait();
            second_manager
                .create_session(CreateIdaSession {
                    target: IdaTarget::Binary { path: target },
                    run_auto_analysis: true,
                    build_caches: true,
                    init_hexrays: true,
                    mode: IdaOpenMode::PreferHeadless,
                    idle_ttl_sec: 3600,
                    startup_timeout_ms: None,
                })
                .expect("second")
        });

        let first = t1.join().expect("t1");
        let second = t2.join().expect("t2");

        assert_eq!(first.id, second.id);
        assert_eq!(supervisor.opened.lock().expect("opened").len(), 1);
    }

    #[test]
    fn upstream_tool_injects_database_and_writes_artifact() {
        let root = test_root("tool");
        let supervisor = Arc::new(MockSupervisor::default());
        let manager = manager_with_mock(&root, supervisor.clone());
        let session = create_ready_session(&manager, &root);

        let result = manager
            .call_upstream_tool(
                "decompile",
                UpstreamIdaToolRequest {
                    session_id: session.id,
                    arguments: fields([("addr", json!("main"))]),
                },
            )
            .expect("tool");

        assert_eq!(result.tool, "ida.decompile");
        assert_eq!(result.result["ok"], true);
        let calls = supervisor.calls.lock().expect("calls");
        assert_eq!(calls[0].0, "upstream-db");
        assert_eq!(calls[0].1, "decompile");
        assert_eq!(calls[0].2["addr"], "main");
        assert!(result.artifact.path.is_file());
    }

    #[test]
    fn upstream_tool_error_writes_artifact_before_returning_error() {
        let root = test_root("tool-error");
        let supervisor = Arc::new(MockSupervisor::default());
        supervisor.tool_error.store(true, Ordering::Relaxed);
        let manager = manager_with_mock(&root, supervisor.clone());
        let session = create_ready_session(&manager, &root);

        let error = manager
            .call_upstream_tool(
                "decompile",
                UpstreamIdaToolRequest {
                    session_id: session.id,
                    arguments: fields([("addr", json!("main"))]),
                },
            )
            .expect_err("tool error");

        assert!(error.to_string().contains("fake upstream tool failed"));
        let refreshed = manager.get_session(session.id).expect("get");
        assert!(refreshed
            .artifacts
            .iter()
            .any(|artifact| artifact.path.to_string_lossy().contains("decompile")));
    }

    #[test]
    fn upstream_tool_rejects_database_argument() {
        let root = test_root("reject-database");
        let manager = manager_with_mock(&root, Arc::new(MockSupervisor::default()));
        let session = create_ready_session(&manager, &root);
        let error = manager
            .call_upstream_tool(
                "decompile",
                UpstreamIdaToolRequest {
                    session_id: session.id,
                    arguments: fields([("database", json!("raw"))]),
                },
            )
            .expect_err("reject");

        assert!(error.to_string().contains("session_id"));
    }

    #[test]
    fn close_session_saves_then_detaches_without_killing_supervisor() {
        let root = test_root("close");
        let supervisor = Arc::new(MockSupervisor::default());
        let manager = manager_with_mock(&root, supervisor.clone());
        let session = create_ready_session(&manager, &root);

        let closed = manager
            .close_session_with_save(session.id, true)
            .expect("close");

        assert_eq!(closed.state, ReverseSessionState::Closed);
        assert!(closed.database_id.is_none());
        assert!(supervisor
            .calls
            .lock()
            .expect("calls")
            .iter()
            .any(|(_, tool, _)| tool == "idb_save"));
        assert!(!supervisor.killed.load(Ordering::Relaxed));
    }

    #[test]
    fn close_session_save_failure_keeps_session_open() {
        let root = test_root("close-save-failure");
        let supervisor = Arc::new(MockSupervisor::default());
        supervisor.save_fails.store(true, Ordering::Relaxed);
        let manager = manager_with_mock(&root, supervisor.clone());
        let session = create_ready_session(&manager, &root);

        let error = manager
            .close_session_with_save(session.id, true)
            .expect_err("save failure");
        let refreshed = manager.get_session(session.id).expect("get");

        assert!(error.to_string().contains("fake save failed"));
        assert_eq!(refreshed.state, ReverseSessionState::Ready);
        assert_eq!(refreshed.database_id.as_deref(), Some("upstream-db"));
        assert_eq!(
            manager.database_id(session.id).expect("database id"),
            "upstream-db"
        );
    }

    #[test]
    fn same_session_operations_are_serialized() {
        let root = test_root("serialization");
        let supervisor = Arc::new(MockSupervisor::default());
        supervisor.sleep_ms.store(50, Ordering::Relaxed);
        let manager = manager_with_mock(&root, supervisor.clone());
        let session = create_ready_session(&manager, &root);

        let first = manager.clone();
        let second = manager.clone();
        let id = session.id;
        let t1 = thread::spawn(move || {
            first
                .call_upstream_tool(
                    "decompile",
                    UpstreamIdaToolRequest {
                        session_id: id,
                        arguments: Map::new(),
                    },
                )
                .expect("first");
        });
        let t2 = thread::spawn(move || {
            second
                .call_upstream_tool(
                    "disasm",
                    UpstreamIdaToolRequest {
                        session_id: id,
                        arguments: Map::new(),
                    },
                )
                .expect("second");
        });
        t1.join().expect("t1");
        t2.join().expect("t2");

        assert!(!supervisor.concurrent.load(Ordering::Relaxed));
    }

    #[test]
    fn different_session_operations_can_overlap() {
        let root = test_root("parallel-sessions");
        let supervisor = Arc::new(MockSupervisor::default());
        supervisor.sleep_ms.store(50, Ordering::Relaxed);
        let manager = manager_with_mock(&root, supervisor.clone());
        let first_session = create_ready_session(&manager, &root);
        let second_session = manager
            .create_session(CreateIdaSession {
                target: IdaTarget::Binary {
                    path: fake_binary_named(&root, "other.exe"),
                },
                run_auto_analysis: true,
                build_caches: true,
                init_hexrays: true,
                mode: IdaOpenMode::PreferHeadless,
                idle_ttl_sec: 3600,
                startup_timeout_ms: None,
            })
            .expect("second");
        supervisor.concurrent.store(false, Ordering::SeqCst);

        let first = manager.clone();
        let second = manager.clone();
        let first_id = first_session.id;
        let second_id = second_session.id;
        let t1 = thread::spawn(move || {
            first
                .call_upstream_tool(
                    "decompile",
                    UpstreamIdaToolRequest {
                        session_id: first_id,
                        arguments: Map::new(),
                    },
                )
                .expect("first");
        });
        let t2 = thread::spawn(move || {
            second
                .call_upstream_tool(
                    "disasm",
                    UpstreamIdaToolRequest {
                        session_id: second_id,
                        arguments: Map::new(),
                    },
                )
                .expect("second");
        });
        t1.join().expect("t1");
        t2.join().expect("t2");

        assert!(supervisor.concurrent.load(Ordering::Relaxed));
    }

    #[test]
    fn missing_upstream_session_marks_error_on_refresh() {
        let root = test_root("stale");
        let supervisor = Arc::new(MockSupervisor::default());
        let manager = manager_with_mock(&root, supervisor.clone());
        let session = create_ready_session(&manager, &root);
        supervisor.list_missing.store(true, Ordering::Relaxed);

        let refreshed = manager.get_session(session.id).expect("get");

        assert_eq!(refreshed.state, ReverseSessionState::Error);
        assert!(refreshed.error.unwrap().contains("no longer listed"));
    }

    fn create_ready_session(manager: &IdaSessionManager, root: &std::path::Path) -> ReverseSession {
        manager
            .create_session(CreateIdaSession {
                target: IdaTarget::Binary {
                    path: fake_binary(root),
                },
                run_auto_analysis: true,
                build_caches: true,
                init_hexrays: true,
                mode: IdaOpenMode::PreferHeadless,
                idle_ttl_sec: 3600,
                startup_timeout_ms: None,
            })
            .expect("create")
    }

    fn manager_with_mock(
        root: &std::path::Path,
        supervisor: Arc<MockSupervisor>,
    ) -> IdaSessionManager {
        IdaSessionManager::with_supervisor_and_logger(
            supervisor,
            root.join("artifacts"),
            noop_logger(),
        )
    }

    #[derive(Default)]
    struct MockSupervisor {
        opened: Mutex<Vec<OpenIdaDatabase>>,
        calls: Mutex<Vec<(String, String, Map<String, Value>)>>,
        list_missing: AtomicBool,
        killed: AtomicBool,
        save_fails: AtomicBool,
        tool_error: AtomicBool,
        in_flight: AtomicUsize,
        concurrent: AtomicBool,
        sleep_ms: AtomicU64,
    }

    impl MockSupervisor {
        fn enter(&self) -> MockOperation<'_> {
            if self.in_flight.fetch_add(1, Ordering::SeqCst) != 0 {
                self.concurrent.store(true, Ordering::SeqCst);
            }
            let sleep_ms = self.sleep_ms.load(Ordering::Relaxed);
            if sleep_ms > 0 {
                thread::sleep(Duration::from_millis(sleep_ms));
            }
            MockOperation { supervisor: self }
        }
    }

    struct MockOperation<'a> {
        supervisor: &'a MockSupervisor,
    }

    impl Drop for MockOperation<'_> {
        fn drop(&mut self) {
            self.supervisor.in_flight.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl IdaSupervisor for MockSupervisor {
        fn tool_descriptors(&self) -> Result<Vec<UpstreamToolDescriptor>> {
            Ok(vec![UpstreamToolDescriptor {
                name: "decompile".to_string(),
                description: "Decompile".to_string(),
                input_schema: json!({"type": "object"}),
                output_schema: None,
            }])
        }

        fn open_session(
            &self,
            request: OpenIdaDatabase,
        ) -> Result<super::super::worker::OpenIdaDatabaseResult> {
            let _guard = self.enter();
            self.opened.lock().expect("opened").push(request);
            Ok(super::super::worker::OpenIdaDatabaseResult {
                session: upstream_session(),
                warmup: Some(json!({"ok": true})),
                message: Some("opened".to_string()),
            })
        }

        fn list_sessions(&self) -> Result<Vec<super::super::model::IdaUpstreamSession>> {
            if self.list_missing.load(Ordering::Relaxed) {
                Ok(Vec::new())
            } else {
                Ok(vec![upstream_session()])
            }
        }

        fn call_tool(
            &self,
            database_id: &str,
            tool_name: &str,
            arguments: Map<String, Value>,
        ) -> Result<SupervisorToolCallResult> {
            let _guard = self.enter();
            self.calls.lock().expect("calls").push((
                database_id.to_string(),
                tool_name.to_string(),
                arguments,
            ));
            if tool_name == "idb_save" && self.save_fails.load(Ordering::Relaxed) {
                return Ok(SupervisorToolCallResult {
                    structured_content: json!({"ok": false, "error": "fake save failed"}),
                    mcp_result: json!({
                        "structuredContent": {"ok": false, "error": "fake save failed"},
                        "isError": false
                    }),
                    is_error: false,
                    error_message: None,
                });
            }
            if self.tool_error.load(Ordering::Relaxed) {
                return Ok(SupervisorToolCallResult {
                    structured_content: json!({"error": "fake upstream tool failed"}),
                    mcp_result: json!({
                        "content": [{"type": "text", "text": "{\"error\":\"fake upstream tool failed\"}"}],
                        "isError": true
                    }),
                    is_error: true,
                    error_message: Some("fake upstream tool failed".to_string()),
                });
            }
            Ok(SupervisorToolCallResult {
                structured_content: json!({"ok": true, "tool": tool_name}),
                mcp_result: json!({"structuredContent": {"ok": true}, "isError": false}),
                is_error: false,
                error_message: None,
            })
        }

        fn has_exited(&self) -> Result<bool> {
            Ok(false)
        }

        fn kill(&self, _reason: &str) -> Result<()> {
            self.killed.store(true, Ordering::Relaxed);
            Ok(())
        }
    }

    fn upstream_session() -> super::super::model::IdaUpstreamSession {
        super::super::model::IdaUpstreamSession {
            database_id: "upstream-db".to_string(),
            input_path: PathBuf::from(r"C:\sample.exe"),
            filename: "sample.exe".to_string(),
            backend: Some("worker".to_string()),
            adopted: Some(true),
            owned: Some(true),
            pid: Some(1234),
            worker_pid: Some(1234),
            is_active: Some(true),
            is_analyzing: Some(false),
            metadata: json!({}),
        }
    }

    fn fake_binary(root: &std::path::Path) -> PathBuf {
        fake_binary_named(root, "sample.exe")
    }

    fn fake_binary_named(root: &std::path::Path, name: &str) -> PathBuf {
        let path = root.join(name);
        if !path.exists() {
            std::fs::write(&path, b"MZ").expect("write binary");
        }
        path
    }

    fn test_root(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("dbgflow-ida-manager-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create root");
        root
    }
}
