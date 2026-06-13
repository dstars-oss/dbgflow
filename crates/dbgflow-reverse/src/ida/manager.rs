use super::install::{resolve_ida_install, IdaRuntimeConfig};
use super::model::{
    BasicBlocksRequest, BasicBlocksResult, CloseDatabaseResult, DecompileRequest, DecompileResult,
    DisassembleRequest, Disassembly, ExportInfo, FunctionInfo, FunctionLookup, IdaMetadata,
    ImportInfo, ListXrefsRequest, LookupFunctionsRequest, MutationItemResult, PageInfo,
    PageRequest, RenameRequest, ReverseSession, ReverseSessionState, SegmentInfo,
    SetCommentRequest, SetTypeRequest, StringInfo, XrefsResult,
};
use super::target::{validate_ida_target, IdaTarget};
use super::worker::{
    OpenIdaDatabase, ProcessReverseWorkerLauncher, ReverseWorker, ReverseWorkerLauncher,
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
use std::sync::mpsc;
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant};

const BACKEND_NAME: &str = "ida-dynamic";
const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
const DEFAULT_PAGE_LIMIT: usize = 100;
const MAX_PAGE_LIMIT: usize = 10_000;
static OUTPUT_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateIdaSession {
    pub target: IdaTarget,
    #[serde(default = "default_run_auto_analysis")]
    pub run_auto_analysis: bool,
    pub startup_timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListSegmentsResult {
    pub session_id: SessionId,
    pub segments: Vec<SegmentInfo>,
    pub page: PageInfo,
    pub artifact: ArtifactRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListFunctionsResult {
    pub session_id: SessionId,
    pub functions: Vec<FunctionInfo>,
    pub page: PageInfo,
    pub artifact: ArtifactRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetadataResult {
    pub session_id: SessionId,
    pub metadata: IdaMetadata,
    pub artifact: ArtifactRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListStringsResult {
    pub session_id: SessionId,
    pub strings: Vec<StringInfo>,
    pub artifact: ArtifactRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListImportsResult {
    pub session_id: SessionId,
    pub imports: Vec<ImportInfo>,
    pub artifact: ArtifactRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListExportsResult {
    pub session_id: SessionId,
    pub exports: Vec<ExportInfo>,
    pub artifact: ArtifactRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LookupFunctionsResult {
    pub session_id: SessionId,
    pub functions: Vec<FunctionLookup>,
    pub artifact: ArtifactRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisassembleResult {
    pub session_id: SessionId,
    pub disassembly: Disassembly,
    pub artifact: ArtifactRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecompileSessionResult {
    pub session_id: SessionId,
    pub decompile: DecompileResult,
    pub artifact: ArtifactRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListXrefsResult {
    pub session_id: SessionId,
    pub xrefs: XrefsResult,
    pub artifact: ArtifactRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListBasicBlocksResult {
    pub session_id: SessionId,
    pub basic_blocks: BasicBlocksResult,
    pub artifact: ArtifactRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MutationResult {
    pub session_id: SessionId,
    pub results: Vec<MutationItemResult>,
    pub artifact: ArtifactRef,
}

#[derive(Clone)]
pub struct IdaSessionManager {
    worker_launcher: Arc<dyn ReverseWorkerLauncher>,
    workers: Arc<Mutex<HashMap<SessionId, Arc<dyn ReverseWorker>>>>,
    sessions: Arc<Mutex<HashMap<SessionId, ReverseSession>>>,
    operation_locks: Arc<Mutex<HashMap<SessionId, Arc<Mutex<()>>>>>,
    artifacts: ArtifactManager,
    runtime: IdaRuntimeConfig,
    logger: Arc<dyn LogSink>,
    process_launch: ProcessLaunchConfig,
}

impl IdaSessionManager {
    pub fn new(artifact_root: impl Into<PathBuf>, runtime: IdaRuntimeConfig) -> Self {
        Self::with_worker_launcher_runtime_process_and_logger(
            Arc::new(ProcessReverseWorkerLauncher::new()),
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
        Self::with_worker_launcher_runtime_process_and_logger(
            Arc::new(ProcessReverseWorkerLauncher::new()),
            artifact_root,
            runtime,
            ProcessLaunchConfig::default(),
            logger,
        )
    }

    pub fn with_worker_launcher_runtime_and_logger(
        worker_launcher: Arc<dyn ReverseWorkerLauncher>,
        artifact_root: impl Into<PathBuf>,
        runtime: IdaRuntimeConfig,
        logger: Arc<dyn LogSink>,
    ) -> Self {
        Self::with_worker_launcher_runtime_process_and_logger(
            worker_launcher,
            artifact_root,
            runtime,
            ProcessLaunchConfig::default(),
            logger,
        )
    }

    pub fn with_worker_launcher_runtime_process_and_logger(
        worker_launcher: Arc<dyn ReverseWorkerLauncher>,
        artifact_root: impl Into<PathBuf>,
        runtime: IdaRuntimeConfig,
        process_launch: ProcessLaunchConfig,
        logger: Arc<dyn LogSink>,
    ) -> Self {
        Self {
            worker_launcher,
            workers: Arc::new(Mutex::new(HashMap::new())),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            operation_locks: Arc::new(Mutex::new(HashMap::new())),
            artifacts: ArtifactManager::new(artifact_root),
            runtime,
            logger,
            process_launch,
        }
    }

    pub fn create_session(&self, request: CreateIdaSession) -> Result<ReverseSession> {
        self.create_session_with_context(request, ToolCallContext::default())
    }

    pub fn create_session_with_context(
        &self,
        mut request: CreateIdaSession,
        tool_context: ToolCallContext,
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

        if let Some(existing) = self.reusable_session_for_target(&request.target)? {
            self.log(
                LogEvent::new(LogLevel::Info, "reverse_ida", "create_session_reused")
                    .session_id(existing.id)
                    .operation("ida.create_session")
                    .field("state", format!("{:?}", existing.state))
                    .field("target", &existing.target),
            );
            return Ok(existing);
        }

        let install = resolve_ida_install(&self.runtime)?;
        let now = now_unix_ms();
        let mut session = ReverseSession {
            id: SessionId::new(),
            backend: BACKEND_NAME.to_string(),
            target: request.target.clone(),
            state: ReverseSessionState::Starting,
            ida: None,
            created_at_unix_ms: now,
            updated_at_unix_ms: now,
            warnings: Vec::new(),
            artifacts: Vec::new(),
            error: None,
        };
        let startup_timeout = request
            .startup_timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_STARTUP_TIMEOUT);

        self.initialize_session_audit(&mut session, &request, &install.install_dir)?;
        self.insert_session(session.clone())?;

        let operation_lock = self.operation_lock(session.id)?;
        let _operation_guard = operation_lock.lock().map_err(|_| {
            DbgFlowError::Backend("IDA session operation lock poisoned".to_string())
        })?;

        self.log(
            LogEvent::new(LogLevel::Info, "reverse_ida", "create_session_started")
                .session_id(session.id)
                .operation("ida.create_session")
                .field("target", &session.target)
                .field("install_dir", install.install_dir.display().to_string())
                .field("run_auto_analysis", request.run_auto_analysis)
                .field("startup_timeout_ms", startup_timeout.as_millis() as u64),
        );

        let started = Instant::now();
        let launch_context = ProcessLaunchContext::new(self.process_launch.clone(), tool_context);
        let worker = match self.worker_launcher.spawn(
            session.id,
            install.clone(),
            self.artifacts.reverse_session_worker_log_path(session.id),
            launch_context,
            self.logger.clone(),
        ) {
            Ok(worker) => worker,
            Err(error) => {
                return self.finish_create_failed(session.id, started, error);
            }
        };
        if let Err(error) = self.insert_worker(session.id, worker.clone()) {
            let _ = worker.kill("insert_worker_failed");
            return self.finish_create_failed(session.id, started, error);
        }

        let open_request = OpenIdaDatabase {
            install_dir: install.install_dir,
            target: request.target,
            run_auto_analysis: request.run_auto_analysis,
        };
        let open_result =
            self.open_database_with_timeout(worker.clone(), open_request, startup_timeout);
        match open_result {
            Ok(result) => {
                let session = self.finish_create_ready(session.id, started, result)?;
                self.spawn_worker_monitor(session.id, worker);
                Ok(session)
            }
            Err(error) => {
                let _ = self.remove_worker(session.id);
                let _ = worker.kill("create_session_failed");
                self.finish_create_failed(session.id, started, error)
            }
        }
    }

    pub fn get_session(&self, session_id: SessionId) -> Result<ReverseSession> {
        self.sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("IDA session manager lock poisoned".to_string()))?
            .get(&session_id)
            .cloned()
            .ok_or(DbgFlowError::SessionNotFound(session_id))
    }

    pub fn list_sessions(&self) -> Result<Vec<ReverseSession>> {
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

        let previous_state = self.update_session_state(
            session_id,
            ReverseSessionState::Closing,
            "ida.close_session",
            None,
        )?;
        if previous_state == ReverseSessionState::Closed {
            return Err(DbgFlowError::SessionClosed(session_id));
        }

        self.log(
            LogEvent::new(LogLevel::Info, "reverse_ida", "close_session_started")
                .session_id(session_id)
                .operation("ida.close_session")
                .field("previous_state", format!("{:?}", previous_state))
                .field("save", save),
        );

        let started = Instant::now();
        let close_result = self
            .remove_worker(session_id)
            .map(|worker| worker.close(save))
            .unwrap_or_else(|| Ok(CloseDatabaseResult::no_worker(save)));
        match close_result {
            Ok(close_result) => {
                self.update_session_state(
                    session_id,
                    ReverseSessionState::Closed,
                    "ida.close_session",
                    None,
                )?;
                if let Some(warning) = close_result.warning.as_deref() {
                    self.append_session_warning(session_id, warning)?;
                }
                self.log(
                    LogEvent::new(LogLevel::Info, "reverse_ida", "close_session_finished")
                        .session_id(session_id)
                        .operation("ida.close_session")
                        .field("save", save)
                        .field("save_status", json!(&close_result.save_status))
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
                let session = self.get_session(session_id)?;
                Ok(session)
            }
            Err(error) => {
                let error_text = error.to_string();
                self.update_session_state(
                    session_id,
                    ReverseSessionState::Error,
                    "ida.close_session",
                    Some(error_text.clone()),
                )?;
                self.log(
                    LogEvent::new(LogLevel::Error, "reverse_ida", "close_session_failed")
                        .session_id(session_id)
                        .operation("ida.close_session")
                        .field("save", save)
                        .duration_ms(started.elapsed().as_millis())
                        .error(error_text),
                );
                let session = self.get_session(session_id)?;
                Ok(session)
            }
        }
    }

    pub fn list_segments(&self, session_id: SessionId) -> Result<ListSegmentsResult> {
        self.list_segments_page(session_id, PageRequest::default())
    }

    pub fn list_segments_page(
        &self,
        session_id: SessionId,
        page_request: PageRequest,
    ) -> Result<ListSegmentsResult> {
        let operation_lock = self.operation_lock(session_id)?;
        let _operation_guard = operation_lock.lock().map_err(|_| {
            DbgFlowError::Backend("IDA session operation lock poisoned".to_string())
        })?;
        self.ensure_ready(session_id, "ida.list_segments")?;
        let worker = self.worker(session_id)?;
        let all_segments = match worker.list_segments() {
            Ok(segments) => segments,
            Err(error) => {
                self.record_worker_error(session_id, &worker, "ida.list_segments", &error);
                return Err(error);
            }
        };
        let filtered_segments = filter_items(
            all_segments,
            page_request.filter.as_deref(),
            segment_matches,
        );
        let (segments, page) = page_vec(
            filtered_segments.clone(),
            page_request.offset,
            page_request.limit,
        );
        let artifact_name = unique_output_name("segments.json");
        let artifact = self.artifacts.write_reverse_session_output(
            session_id,
            &artifact_name,
            &json!({ "segments": filtered_segments, "page": page }),
        )?;
        self.append_artifact_to_session(session_id, artifact.clone())?;
        self.record_event(
            session_id,
            "list_segments_finished",
            None,
            None,
            Some("ida.list_segments"),
            Some(artifact.path.clone()),
            None,
            fields([
                ("count", json!(segments.len())),
                ("total", json!(page.total)),
            ]),
        );
        Ok(ListSegmentsResult {
            session_id,
            segments,
            page,
            artifact,
        })
    }

    pub fn list_functions(&self, session_id: SessionId) -> Result<ListFunctionsResult> {
        self.list_functions_page(session_id, PageRequest::default())
    }

    pub fn list_functions_page(
        &self,
        session_id: SessionId,
        page_request: PageRequest,
    ) -> Result<ListFunctionsResult> {
        let operation_lock = self.operation_lock(session_id)?;
        let _operation_guard = operation_lock.lock().map_err(|_| {
            DbgFlowError::Backend("IDA session operation lock poisoned".to_string())
        })?;
        self.ensure_ready(session_id, "ida.list_functions")?;
        let worker = self.worker(session_id)?;
        let all_functions = match worker.list_functions() {
            Ok(functions) => functions,
            Err(error) => {
                self.record_worker_error(session_id, &worker, "ida.list_functions", &error);
                return Err(error);
            }
        };
        let filtered_functions = filter_items(
            all_functions,
            page_request.filter.as_deref(),
            function_matches,
        );
        let (functions, page) = page_vec(
            filtered_functions.clone(),
            page_request.offset,
            page_request.limit,
        );
        let artifact_name = unique_output_name("functions.json");
        let artifact = self.artifacts.write_reverse_session_output(
            session_id,
            &artifact_name,
            &json!({ "functions": filtered_functions, "page": page }),
        )?;
        self.append_artifact_to_session(session_id, artifact.clone())?;
        self.record_event(
            session_id,
            "list_functions_finished",
            None,
            None,
            Some("ida.list_functions"),
            Some(artifact.path.clone()),
            None,
            fields([
                ("count", json!(functions.len())),
                ("total", json!(page.total)),
            ]),
        );
        Ok(ListFunctionsResult {
            session_id,
            functions,
            page,
            artifact,
        })
    }

    pub fn get_metadata(&self, session_id: SessionId) -> Result<MetadataResult> {
        let metadata = self.worker_query(session_id, "ida.get_metadata", |worker| {
            worker.get_metadata()
        })?;
        let artifact =
            self.write_output(session_id, "metadata.json", "ida.get_metadata", &metadata)?;
        Ok(MetadataResult {
            session_id,
            metadata,
            artifact,
        })
    }

    pub fn list_strings(
        &self,
        session_id: SessionId,
        request: PageRequest,
    ) -> Result<ListStringsResult> {
        let request = normalize_page_request(request);
        let strings = self.worker_query(session_id, "ida.list_strings", |worker| {
            worker.list_strings(request)
        })?;
        let artifact =
            self.write_output(session_id, "strings.json", "ida.list_strings", &strings)?;
        Ok(ListStringsResult {
            session_id,
            strings,
            artifact,
        })
    }

    pub fn list_imports(
        &self,
        session_id: SessionId,
        request: PageRequest,
    ) -> Result<ListImportsResult> {
        let request = normalize_page_request(request);
        let imports = self.worker_query(session_id, "ida.list_imports", |worker| {
            worker.list_imports(request)
        })?;
        let artifact =
            self.write_output(session_id, "imports.json", "ida.list_imports", &imports)?;
        Ok(ListImportsResult {
            session_id,
            imports,
            artifact,
        })
    }

    pub fn list_exports(
        &self,
        session_id: SessionId,
        request: PageRequest,
    ) -> Result<ListExportsResult> {
        let request = normalize_page_request(request);
        let exports = self.worker_query(session_id, "ida.list_exports", |worker| {
            worker.list_exports(request)
        })?;
        let artifact =
            self.write_output(session_id, "exports.json", "ida.list_exports", &exports)?;
        Ok(ListExportsResult {
            session_id,
            exports,
            artifact,
        })
    }

    pub fn lookup_functions(
        &self,
        session_id: SessionId,
        request: LookupFunctionsRequest,
    ) -> Result<LookupFunctionsResult> {
        let functions = self.worker_query(session_id, "ida.lookup_functions", |worker| {
            worker.lookup_functions(request)
        })?;
        let artifact = self.write_output(
            session_id,
            "function_lookup.json",
            "ida.lookup_functions",
            &functions,
        )?;
        Ok(LookupFunctionsResult {
            session_id,
            functions,
            artifact,
        })
    }

    pub fn disassemble(
        &self,
        session_id: SessionId,
        request: DisassembleRequest,
    ) -> Result<DisassembleResult> {
        let request = normalize_disassemble_request(request);
        let disassembly = self.worker_query(session_id, "ida.disassemble", |worker| {
            worker.disassemble(request)
        })?;
        let artifact = self.write_output(
            session_id,
            "disassembly.json",
            "ida.disassemble",
            &disassembly,
        )?;
        Ok(DisassembleResult {
            session_id,
            disassembly,
            artifact,
        })
    }

    pub fn decompile(
        &self,
        session_id: SessionId,
        request: DecompileRequest,
    ) -> Result<DecompileSessionResult> {
        let decompile = self.worker_query(session_id, "ida.decompile", |worker| {
            worker.decompile(request)
        })?;
        let artifact =
            self.write_output(session_id, "decompile.json", "ida.decompile", &decompile)?;
        Ok(DecompileSessionResult {
            session_id,
            decompile,
            artifact,
        })
    }

    pub fn list_xrefs(
        &self,
        session_id: SessionId,
        request: ListXrefsRequest,
    ) -> Result<ListXrefsResult> {
        let request = normalize_xrefs_request(request);
        let xrefs = self.worker_query(session_id, "ida.list_xrefs", |worker| {
            worker.list_xrefs(request)
        })?;
        let artifact = self.write_output(session_id, "xrefs.json", "ida.list_xrefs", &xrefs)?;
        Ok(ListXrefsResult {
            session_id,
            xrefs,
            artifact,
        })
    }

    pub fn list_basic_blocks(
        &self,
        session_id: SessionId,
        request: BasicBlocksRequest,
    ) -> Result<ListBasicBlocksResult> {
        let basic_blocks = self.worker_query(session_id, "ida.list_basic_blocks", |worker| {
            worker.list_basic_blocks(request)
        })?;
        let artifact = self.write_output(
            session_id,
            "basic_blocks.json",
            "ida.list_basic_blocks",
            &basic_blocks,
        )?;
        Ok(ListBasicBlocksResult {
            session_id,
            basic_blocks,
            artifact,
        })
    }

    pub fn rename(&self, session_id: SessionId, request: RenameRequest) -> Result<MutationResult> {
        let dry_run = request.dry_run;
        let results =
            self.worker_query(session_id, "ida.rename", |worker| worker.rename(request))?;
        let artifact = self.write_output(session_id, "rename.json", "ida.rename", &results)?;
        self.record_event(
            session_id,
            "mutation_finished",
            None,
            None,
            Some("ida.rename"),
            Some(artifact.path.clone()),
            None,
            mutation_fields(&results, dry_run),
        );
        Ok(MutationResult {
            session_id,
            results,
            artifact,
        })
    }

    pub fn set_comment(
        &self,
        session_id: SessionId,
        request: SetCommentRequest,
    ) -> Result<MutationResult> {
        let results = self.worker_query(session_id, "ida.set_comment", |worker| {
            worker.set_comment(request)
        })?;
        let artifact =
            self.write_output(session_id, "comments.json", "ida.set_comment", &results)?;
        self.record_event(
            session_id,
            "mutation_finished",
            None,
            None,
            Some("ida.set_comment"),
            Some(artifact.path.clone()),
            None,
            mutation_fields(&results, false),
        );
        Ok(MutationResult {
            session_id,
            results,
            artifact,
        })
    }

    pub fn set_type(
        &self,
        session_id: SessionId,
        request: SetTypeRequest,
    ) -> Result<MutationResult> {
        let dry_run = request.dry_run;
        let results = self.worker_query(session_id, "ida.set_type", |worker| {
            worker.set_type(request)
        })?;
        let artifact = self.write_output(session_id, "types.json", "ida.set_type", &results)?;
        self.record_event(
            session_id,
            "mutation_finished",
            None,
            None,
            Some("ida.set_type"),
            Some(artifact.path.clone()),
            None,
            mutation_fields(&results, dry_run),
        );
        Ok(MutationResult {
            session_id,
            results,
            artifact,
        })
    }

    fn worker_query<T>(
        &self,
        session_id: SessionId,
        operation: &str,
        call: impl FnOnce(&Arc<dyn ReverseWorker>) -> Result<T>,
    ) -> Result<T> {
        let operation_lock = self.operation_lock(session_id)?;
        let _operation_guard = operation_lock.lock().map_err(|_| {
            DbgFlowError::Backend("IDA session operation lock poisoned".to_string())
        })?;
        self.ensure_ready(session_id, operation)?;
        let worker = self.worker(session_id)?;
        match call(&worker) {
            Ok(result) => Ok(result),
            Err(error) => {
                self.record_worker_error(session_id, &worker, operation, &error);
                Err(error)
            }
        }
    }

    fn write_output(
        &self,
        session_id: SessionId,
        file_name: &str,
        operation: &str,
        output: &impl Serialize,
    ) -> Result<ArtifactRef> {
        let value = serde_json::to_value(output)
            .map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
        let file_name = unique_output_name(file_name);
        let artifact = self
            .artifacts
            .write_reverse_session_output(session_id, &file_name, &value)?;
        self.append_artifact_to_session(session_id, artifact.clone())?;
        let count = value_count(&value);
        self.record_event(
            session_id,
            "query_finished",
            None,
            None,
            Some(operation),
            Some(artifact.path.clone()),
            None,
            fields([("count", json!(count))]),
        );
        Ok(artifact)
    }

    fn record_worker_error(
        &self,
        session_id: SessionId,
        worker: &Arc<dyn ReverseWorker>,
        operation: &str,
        error: &DbgFlowError,
    ) {
        if worker.has_exited().unwrap_or(false) {
            self.mark_worker_unavailable(session_id, error.to_string());
        }
        self.record_event(
            session_id,
            "operation_failed",
            None,
            None,
            Some(operation),
            None,
            Some(error.to_string()),
            Map::new(),
        );
    }

    fn reusable_session_for_target(&self, target: &IdaTarget) -> Result<Option<ReverseSession>> {
        Ok(self
            .sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("IDA session manager lock poisoned".to_string()))?
            .values()
            .find(|session| session.target == *target && session.state.is_reusable())
            .cloned())
    }

    fn initialize_session_audit(
        &self,
        session: &mut ReverseSession,
        request: &CreateIdaSession,
        install_dir: &std::path::Path,
    ) -> Result<()> {
        self.artifacts
            .initialize_reverse_session_artifacts(session.id)?;
        let request_artifact = self.artifacts.write_reverse_session_request(
            session.id,
            &json!({
                "target": request.target,
                "run_auto_analysis": request.run_auto_analysis,
                "startup_timeout_ms": request.startup_timeout_ms,
                "ida": {
                    "install_dir": install_dir
                }
            }),
        )?;
        session.artifacts.push(request_artifact);
        let metadata_artifact = self
            .artifacts
            .write_reverse_session_metadata(session.id, &json!(session))?;
        session.artifacts.push(metadata_artifact);
        self.record_event(
            session.id,
            "create_session_started",
            None,
            Some(ReverseSessionState::Starting),
            Some("ida.create_session"),
            None,
            None,
            fields([
                ("target", json!(session.target)),
                ("run_auto_analysis", json!(request.run_auto_analysis)),
            ]),
        );
        Ok(())
    }

    fn insert_session(&self, session: ReverseSession) -> Result<()> {
        self.operation_locks
            .lock()
            .map_err(|_| DbgFlowError::Backend("IDA session lock map poisoned".to_string()))?
            .insert(session.id, Arc::new(Mutex::new(())));
        self.sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("IDA session manager lock poisoned".to_string()))?
            .insert(session.id, session);
        Ok(())
    }

    fn insert_worker(&self, session_id: SessionId, worker: Arc<dyn ReverseWorker>) -> Result<()> {
        self.workers
            .lock()
            .map_err(|_| DbgFlowError::Backend("IDA worker registry lock poisoned".to_string()))?
            .insert(session_id, worker);
        Ok(())
    }

    fn remove_worker(&self, session_id: SessionId) -> Option<Arc<dyn ReverseWorker>> {
        self.workers
            .lock()
            .ok()
            .and_then(|mut workers| workers.remove(&session_id))
    }

    fn worker(&self, session_id: SessionId) -> Result<Arc<dyn ReverseWorker>> {
        self.workers
            .lock()
            .map_err(|_| DbgFlowError::Backend("IDA worker registry lock poisoned".to_string()))?
            .get(&session_id)
            .cloned()
            .ok_or_else(|| {
                DbgFlowError::Backend("IDA session worker is not initialized".to_string())
            })
    }

    fn operation_lock(&self, session_id: SessionId) -> Result<Arc<Mutex<()>>> {
        self.operation_locks
            .lock()
            .map_err(|_| DbgFlowError::Backend("IDA session lock map poisoned".to_string()))?
            .get(&session_id)
            .cloned()
            .ok_or(DbgFlowError::SessionNotFound(session_id))
    }

    fn open_database_with_timeout(
        &self,
        worker: Arc<dyn ReverseWorker>,
        request: OpenIdaDatabase,
        timeout: Duration,
    ) -> Result<super::worker::OpenIdaDatabaseResult> {
        let (tx, rx) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let _ = tx.send(worker.open_database(request));
        });
        match rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(DbgFlowError::Backend(format!(
                "IDA open_database timed out after {} ms",
                timeout.as_millis()
            ))),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(DbgFlowError::Backend(
                "IDA open_database thread exited without a response".to_string(),
            )),
        }
    }

    fn finish_create_ready(
        &self,
        session_id: SessionId,
        started: Instant,
        result: super::worker::OpenIdaDatabaseResult,
    ) -> Result<ReverseSession> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("IDA session manager lock poisoned".to_string()))?;
        let session = sessions
            .get_mut(&session_id)
            .ok_or(DbgFlowError::SessionNotFound(session_id))?;
        let previous_state = session.state.clone();
        session.state = ReverseSessionState::Ready;
        session.ida = Some(result.ida);
        session.warnings = result.warnings;
        session.updated_at_unix_ms = now_unix_ms();
        session.error = None;
        let updated = session.clone();
        drop(sessions);

        self.write_session_metadata(&updated)?;
        self.record_event(
            session_id,
            "create_session_finished",
            Some(previous_state),
            Some(ReverseSessionState::Ready),
            Some("ida.create_session"),
            None,
            None,
            fields([("duration_ms", json!(started.elapsed().as_millis() as u64))]),
        );
        self.log(
            LogEvent::new(LogLevel::Info, "reverse_ida", "create_session_finished")
                .session_id(session_id)
                .operation("ida.create_session")
                .duration_ms(started.elapsed().as_millis())
                .field("state", "Ready"),
        );
        Ok(updated)
    }

    fn finish_create_failed(
        &self,
        session_id: SessionId,
        started: Instant,
        error: DbgFlowError,
    ) -> Result<ReverseSession> {
        let error_text = error.to_string();
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("IDA session manager lock poisoned".to_string()))?;
        let session = sessions
            .get_mut(&session_id)
            .ok_or(DbgFlowError::SessionNotFound(session_id))?;
        let previous_state = session.state.clone();
        session.state = ReverseSessionState::Error;
        session.updated_at_unix_ms = now_unix_ms();
        session.error = Some(error_text.clone());
        let updated = session.clone();
        drop(sessions);

        let _ = self.write_session_metadata(&updated);
        self.record_event(
            session_id,
            "create_session_failed",
            Some(previous_state),
            Some(ReverseSessionState::Error),
            Some("ida.create_session"),
            None,
            Some(error_text.clone()),
            fields([("duration_ms", json!(started.elapsed().as_millis() as u64))]),
        );
        self.log(
            LogEvent::new(LogLevel::Error, "reverse_ida", "create_session_failed")
                .session_id(session_id)
                .operation("ida.create_session")
                .duration_ms(started.elapsed().as_millis())
                .error(error_text),
        );
        Err(error)
    }

    fn ensure_ready(&self, session_id: SessionId, operation: &str) -> Result<ReverseSession> {
        let session = self.get_session(session_id)?;
        match session.state {
            ReverseSessionState::Ready => Ok(session),
            ReverseSessionState::Closed => Err(DbgFlowError::SessionClosed(session_id)),
            state => {
                let error = DbgFlowError::Backend(format!("IDA session is not ready: {state:?}"));
                self.log(
                    LogEvent::new(LogLevel::Warn, "reverse_ida", "session_not_ready")
                        .session_id(session_id)
                        .operation(operation)
                        .field("state", format!("{state:?}"))
                        .error(error.to_string()),
                );
                Err(error)
            }
        }
    }

    fn update_session_state(
        &self,
        session_id: SessionId,
        new_state: ReverseSessionState,
        operation: &str,
        error: Option<String>,
    ) -> Result<ReverseSessionState> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("IDA session manager lock poisoned".to_string()))?;
        let session = sessions
            .get_mut(&session_id)
            .ok_or(DbgFlowError::SessionNotFound(session_id))?;
        let previous_state = session.state.clone();
        if previous_state == ReverseSessionState::Closed
            && new_state == ReverseSessionState::Closing
        {
            return Ok(previous_state);
        }
        session.state = new_state.clone();
        session.updated_at_unix_ms = now_unix_ms();
        session.error = error.clone();
        let updated = session.clone();
        drop(sessions);

        self.write_session_metadata(&updated)?;
        self.record_event(
            session_id,
            "session_state_changed",
            Some(previous_state.clone()),
            Some(new_state),
            Some(operation),
            None,
            error,
            Map::new(),
        );
        Ok(previous_state)
    }

    fn append_artifact_to_session(
        &self,
        session_id: SessionId,
        artifact: ArtifactRef,
    ) -> Result<()> {
        let updated = {
            let mut sessions = self.sessions.lock().map_err(|_| {
                DbgFlowError::Backend("IDA session manager lock poisoned".to_string())
            })?;
            let session = sessions
                .get_mut(&session_id)
                .ok_or(DbgFlowError::SessionNotFound(session_id))?;
            session.artifacts.push(artifact);
            session.updated_at_unix_ms = now_unix_ms();
            session.clone()
        };
        self.write_session_metadata(&updated)
    }

    fn append_session_warning(&self, session_id: SessionId, warning: &str) -> Result<()> {
        let updated = {
            let mut sessions = self.sessions.lock().map_err(|_| {
                DbgFlowError::Backend("IDA session manager lock poisoned".to_string())
            })?;
            let session = sessions
                .get_mut(&session_id)
                .ok_or(DbgFlowError::SessionNotFound(session_id))?;
            if !session.warnings.iter().any(|existing| existing == warning) {
                session.warnings.push(warning.to_string());
                session.updated_at_unix_ms = now_unix_ms();
            }
            session.clone()
        };
        self.write_session_metadata(&updated)
    }

    fn write_session_metadata(&self, session: &ReverseSession) -> Result<()> {
        let artifact = self
            .artifacts
            .write_reverse_session_metadata(session.id, &json!(session))?;
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| DbgFlowError::Backend("IDA session manager lock poisoned".to_string()))?;
        if let Some(session) = sessions.get_mut(&session.id) {
            if !session
                .artifacts
                .iter()
                .any(|existing| existing.path == artifact.path)
            {
                session.artifacts.push(artifact);
            }
        }
        Ok(())
    }

    fn spawn_worker_monitor(&self, session_id: SessionId, worker: Arc<dyn ReverseWorker>) {
        let sessions = Arc::downgrade(&self.sessions);
        let workers = Arc::downgrade(&self.workers);
        let artifacts = self.artifacts.clone();
        let logger = self.logger.clone();
        thread::spawn(move || loop {
            thread::sleep(Duration::from_millis(250));
            let exited = match worker.has_exited() {
                Ok(exited) => exited,
                Err(error) => {
                    mark_worker_unavailable(
                        session_id,
                        error.to_string(),
                        &sessions,
                        &workers,
                        &artifacts,
                        &logger,
                    );
                    return;
                }
            };
            if exited {
                mark_worker_unavailable(
                    session_id,
                    "IDA reverse worker exited unexpectedly".to_string(),
                    &sessions,
                    &workers,
                    &artifacts,
                    &logger,
                );
                return;
            }
        });
    }

    fn mark_worker_unavailable(&self, session_id: SessionId, error: String) {
        mark_worker_unavailable(
            session_id,
            error,
            &Arc::downgrade(&self.sessions),
            &Arc::downgrade(&self.workers),
            &self.artifacts,
            &self.logger,
        );
    }

    fn record_event(
        &self,
        session_id: SessionId,
        event: &str,
        previous_state: Option<ReverseSessionState>,
        new_state: Option<ReverseSessionState>,
        operation: Option<&str>,
        artifact_path: Option<PathBuf>,
        error: Option<String>,
        fields: Map<String, Value>,
    ) {
        let event = ReverseSessionArtifactEvent {
            timestamp_unix_ms: now_unix_ms(),
            event: event.to_string(),
            session_id: session_id.to_string(),
            previous_state: previous_state.map(|state| format!("{state:?}")),
            new_state: new_state.map(|state| format!("{state:?}")),
            operation: operation.map(ToString::to_string),
            artifact_path,
            error,
            fields,
        };
        if let Err(error) = self
            .artifacts
            .append_reverse_session_event(session_id, &event)
        {
            self.log(
                LogEvent::new(LogLevel::Warn, "reverse_ida", "artifact_event_failed")
                    .session_id(session_id)
                    .field("event", event.event)
                    .error(error.to_string()),
            );
        }
    }

    fn log(&self, event: LogEvent) {
        self.logger.log(event);
    }
}

impl Default for IdaSessionManager {
    fn default() -> Self {
        Self::new("artifacts", IdaRuntimeConfig::default())
    }
}

fn mark_worker_unavailable(
    session_id: SessionId,
    error: String,
    sessions: &Weak<Mutex<HashMap<SessionId, ReverseSession>>>,
    workers: &Weak<Mutex<HashMap<SessionId, Arc<dyn ReverseWorker>>>>,
    artifacts: &ArtifactManager,
    logger: &Arc<dyn LogSink>,
) {
    let Some(sessions) = sessions.upgrade() else {
        return;
    };
    let mut updated = None;
    if let Ok(mut sessions) = sessions.lock() {
        if let Some(session) = sessions.get_mut(&session_id) {
            if session.state.is_terminal() || session.state == ReverseSessionState::Closing {
                return;
            }
            let previous_state = session.state.clone();
            session.state = ReverseSessionState::Error;
            session.updated_at_unix_ms = now_unix_ms();
            session.error = Some(error.clone());
            updated = Some((previous_state, session.clone()));
        }
    }
    let Some((previous_state, session)) = updated else {
        return;
    };
    if let Some(workers) = workers.upgrade() {
        if let Ok(mut workers) = workers.lock() {
            workers.remove(&session_id);
        }
    }
    let _ = artifacts.write_reverse_session_metadata(session_id, &json!(session));
    let event = ReverseSessionArtifactEvent {
        timestamp_unix_ms: now_unix_ms(),
        event: "worker_exited_unexpectedly".to_string(),
        session_id: session_id.to_string(),
        previous_state: Some(format!("{previous_state:?}")),
        new_state: Some(format!("{:?}", ReverseSessionState::Error)),
        operation: None,
        artifact_path: None,
        error: Some(error.clone()),
        fields: Map::new(),
    };
    let _ = artifacts.append_reverse_session_event(session_id, &event);
    logger.log(
        LogEvent::new(LogLevel::Error, "reverse_ida", "worker_exited_unexpectedly")
            .session_id(session_id)
            .error(error),
    );
}

fn default_run_auto_analysis() -> bool {
    true
}

fn fields<const N: usize>(entries: [(&str, Value); N]) -> Map<String, Value> {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
}

fn close_fields(save: bool, result: &CloseDatabaseResult) -> Map<String, Value> {
    let mut fields = fields([
        ("save", json!(save)),
        ("save_requested", json!(result.save_requested)),
        ("save_status", json!(&result.save_status)),
    ]);
    if let Some(warning) = &result.warning {
        fields.insert("warning".to_string(), json!(warning));
    }
    if let Some(error) = &result.error {
        fields.insert("save_error".to_string(), json!(error));
    }
    fields
}

fn mutation_fields(results: &[MutationItemResult], dry_run: bool) -> Map<String, Value> {
    let items = results
        .iter()
        .map(|result| {
            json!({
                "target": &result.target,
                "old": &result.old,
                "new": &result.new,
                "success": result.success,
                "dry_run": result.dry_run,
                "error": &result.error,
            })
        })
        .collect::<Vec<_>>();
    fields([
        ("count", json!(results.len())),
        (
            "success_count",
            json!(results.iter().filter(|result| result.success).count()),
        ),
        (
            "error_count",
            json!(results
                .iter()
                .filter(|result| result.error.is_some())
                .count()),
        ),
        ("dry_run", json!(dry_run)),
        ("items", json!(items)),
    ])
}

fn unique_output_name(file_name: &str) -> String {
    let timestamp = now_unix_ms();
    let sequence = OUTPUT_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    match file_name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() && !extension.is_empty() => {
            format!("{stem}-{timestamp}-{sequence}.{extension}")
        }
        _ => format!("{file_name}-{timestamp}-{sequence}"),
    }
}

fn normalize_page_request(mut request: PageRequest) -> PageRequest {
    request.limit = Some(normalize_limit(request.limit));
    request
}

fn normalize_disassemble_request(mut request: DisassembleRequest) -> DisassembleRequest {
    request.limit = Some(normalize_limit(request.limit));
    request
}

fn normalize_xrefs_request(mut request: ListXrefsRequest) -> ListXrefsRequest {
    request.limit = Some(normalize_limit(request.limit));
    request
}

fn normalize_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_PAGE_LIMIT).clamp(1, MAX_PAGE_LIMIT)
}

fn value_count(value: &Value) -> usize {
    match value {
        Value::Array(items) => items.len(),
        Value::Object(object) => object
            .values()
            .find_map(|value| value.as_array().map(Vec::len))
            .unwrap_or(object.len()),
        _ => 1,
    }
}

fn filter_items<T>(items: Vec<T>, filter: Option<&str>, matches: fn(&T, &str) -> bool) -> Vec<T> {
    let Some(filter) = filter
        .map(str::trim)
        .filter(|filter| !filter.is_empty() && *filter != "*")
    else {
        return items;
    };
    let needle = filter.to_ascii_lowercase();
    items
        .into_iter()
        .filter(|item| matches(item, &needle))
        .collect()
}

fn page_vec<T>(items: Vec<T>, offset: usize, limit: Option<usize>) -> (Vec<T>, PageInfo) {
    let limit = normalize_limit(limit);
    let total = items.len();
    let returned = items
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>();
    let next = offset.saturating_add(returned.len());
    let next_offset = (next < total).then_some(next);
    (
        returned,
        PageInfo {
            offset,
            limit,
            total,
            returned: next.saturating_sub(offset),
            next_offset,
        },
    )
}

fn segment_matches(segment: &SegmentInfo, needle: &str) -> bool {
    contains_ci(&segment.start_ea, needle)
        || contains_ci(&segment.end_ea, needle)
        || segment
            .name
            .as_deref()
            .is_some_and(|name| contains_ci(name, needle))
        || segment
            .class
            .as_deref()
            .is_some_and(|class| contains_ci(class, needle))
        || contains_ci(&segment.perm, needle)
}

fn function_matches(function: &FunctionInfo, needle: &str) -> bool {
    contains_ci(&function.start_ea, needle)
        || contains_ci(&function.end_ea, needle)
        || contains_ci(&function.flags, needle)
        || function
            .name
            .as_deref()
            .is_some_and(|name| contains_ci(name, needle))
        || function
            .segment
            .as_deref()
            .is_some_and(|segment| contains_ci(segment, needle))
        || function
            .prototype
            .as_deref()
            .is_some_and(|prototype| contains_ci(prototype, needle))
}

fn contains_ci(value: &str, needle: &str) -> bool {
    value.to_ascii_lowercase().contains(needle)
}

#[cfg(test)]
mod tests {
    use super::super::install::IdaInstall;
    use super::super::model::{IdaInfo, IdaVersion};
    use super::super::worker::OpenIdaDatabaseResult;
    use super::*;
    use crate::ida::{BasicBlockInfo, DisassemblyLine, RenameItem};
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

    #[test]
    fn create_session_opens_database_and_reuses_same_target() {
        let root = test_root("reuse");
        let install = fake_ida_install(&root);
        let target = fake_binary(&root);
        let launcher = Arc::new(MockReverseWorkerLauncher::default());
        let manager = IdaSessionManager::with_worker_launcher_runtime_and_logger(
            launcher.clone(),
            root.join("artifacts"),
            IdaRuntimeConfig {
                install_dir: Some(install.install_dir.clone()),
            },
            noop_logger(),
        );

        let first = manager
            .create_session(CreateIdaSession {
                target: IdaTarget::Binary {
                    path: target.clone(),
                },
                run_auto_analysis: true,
                startup_timeout_ms: Some(1000),
            })
            .expect("create first");
        let second = manager
            .create_session(CreateIdaSession {
                target: IdaTarget::Binary { path: target },
                run_auto_analysis: true,
                startup_timeout_ms: Some(1000),
            })
            .expect("reuse session");

        assert_eq!(first.id, second.id);
        assert_eq!(first.state, ReverseSessionState::Ready);
        assert_eq!(launcher.spawn_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn create_session_with_context_passes_process_launch_context_to_worker_launcher() {
        let root = test_root("process-context");
        let install = fake_ida_install(&root);
        let target = fake_binary(&root);
        let launcher = Arc::new(MockReverseWorkerLauncher::default());
        let process_launch = ProcessLaunchConfig::installed_service_default();
        let tool_context = ToolCallContext {
            peer_pid: Some(4321),
            peer_session_id: Some(87),
        };
        let manager = IdaSessionManager::with_worker_launcher_runtime_process_and_logger(
            launcher.clone(),
            root.join("artifacts"),
            IdaRuntimeConfig {
                install_dir: Some(install.install_dir.clone()),
            },
            process_launch.clone(),
            noop_logger(),
        );

        manager
            .create_session_with_context(
                CreateIdaSession {
                    target: IdaTarget::Binary { path: target },
                    run_auto_analysis: true,
                    startup_timeout_ms: Some(1000),
                },
                tool_context,
            )
            .expect("create");

        let recorded = launcher.last_context();
        assert_eq!(recorded.config, process_launch);
        assert_eq!(recorded.tool_call, tool_context);
    }

    #[test]
    fn list_segments_and_functions_write_outputs() {
        let root = test_root("list");
        let install = fake_ida_install(&root);
        let target = fake_binary(&root);
        let manager = manager_with_mock(&root, install);
        let session = manager
            .create_session(CreateIdaSession {
                target: IdaTarget::Binary { path: target },
                run_auto_analysis: true,
                startup_timeout_ms: Some(1000),
            })
            .expect("create");

        let segments = manager
            .list_segments_page(
                session.id,
                PageRequest {
                    offset: 0,
                    limit: Some(1),
                    filter: None,
                },
            )
            .expect("segments");
        let functions = manager
            .list_functions_page(
                session.id,
                PageRequest {
                    offset: 0,
                    limit: Some(1),
                    filter: None,
                },
            )
            .expect("functions");

        assert_eq!(segments.segments[0].start_ea, "0x1000");
        assert_eq!(segments.segments.len(), 1);
        assert_eq!(segments.page.total, 2);
        assert!(segments
            .artifact
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("segments-") && name.ends_with(".json")));
        let segment_artifact: Value = serde_json::from_str(
            &std::fs::read_to_string(&segments.artifact.path).expect("segments artifact"),
        )
        .expect("segments json");
        assert_eq!(
            segment_artifact["segments"]
                .as_array()
                .expect("segments array")
                .len(),
            2
        );
        assert_eq!(functions.functions[0].flags, "0x1");
        assert_eq!(functions.functions.len(), 1);
        assert_eq!(functions.page.total, 2);
        assert!(functions
            .artifact
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("functions-") && name.ends_with(".json")));
        let function_artifact: Value = serde_json::from_str(
            &std::fs::read_to_string(&functions.artifact.path).expect("functions artifact"),
        )
        .expect("functions json");
        assert_eq!(
            function_artifact["functions"]
                .as_array()
                .expect("functions array")
                .len(),
            2
        );
    }

    #[test]
    fn close_session_closes_worker_and_state() {
        let root = test_root("close");
        let install = fake_ida_install(&root);
        let target = fake_binary(&root);
        let launcher = Arc::new(MockReverseWorkerLauncher::default());
        let manager = IdaSessionManager::with_worker_launcher_runtime_and_logger(
            launcher.clone(),
            root.join("artifacts"),
            IdaRuntimeConfig {
                install_dir: Some(install.install_dir.clone()),
            },
            noop_logger(),
        );
        let session = manager
            .create_session(CreateIdaSession {
                target: IdaTarget::Binary { path: target },
                run_auto_analysis: true,
                startup_timeout_ms: Some(1000),
            })
            .expect("create");

        let closed = manager.close_session(session.id).expect("close");

        assert_eq!(closed.state, ReverseSessionState::Closed);
        assert!(launcher.last_worker().closed.load(Ordering::Relaxed));
        assert!(launcher.last_worker().saved.load(Ordering::Relaxed));
        assert!(closed
            .warnings
            .iter()
            .any(|warning| warning.contains("does not report whether saving succeeded")));
        let events = std::fs::read_to_string(
            root.join("artifacts")
                .join("reverse_sessions")
                .join(session.id.to_string())
                .join("events.jsonl"),
        )
        .expect("events");
        assert!(events.contains("\"save_status\":\"unknown\""));
    }

    #[test]
    fn close_session_can_skip_save() {
        let root = test_root("close-no-save");
        let install = fake_ida_install(&root);
        let target = fake_binary(&root);
        let launcher = Arc::new(MockReverseWorkerLauncher::default());
        let manager = IdaSessionManager::with_worker_launcher_runtime_and_logger(
            launcher.clone(),
            root.join("artifacts"),
            IdaRuntimeConfig {
                install_dir: Some(install.install_dir.clone()),
            },
            noop_logger(),
        );
        let session = manager
            .create_session(CreateIdaSession {
                target: IdaTarget::Binary { path: target },
                run_auto_analysis: true,
                startup_timeout_ms: Some(1000),
            })
            .expect("create");

        manager
            .close_session_with_save(session.id, false)
            .expect("close");

        assert!(launcher.last_worker().closed.load(Ordering::Relaxed));
        assert!(!launcher.last_worker().saved.load(Ordering::Relaxed));
    }

    #[test]
    fn same_session_operations_are_serialized() {
        let root = test_root("serialized");
        let install = fake_ida_install(&root);
        let target = fake_binary(&root);
        let launcher = Arc::new(MockReverseWorkerLauncher::default());
        let manager = IdaSessionManager::with_worker_launcher_runtime_and_logger(
            launcher.clone(),
            root.join("artifacts"),
            IdaRuntimeConfig {
                install_dir: Some(install.install_dir.clone()),
            },
            noop_logger(),
        );
        let session = manager
            .create_session(CreateIdaSession {
                target: IdaTarget::Binary { path: target },
                run_auto_analysis: true,
                startup_timeout_ms: Some(1000),
            })
            .expect("create");
        let worker = launcher.last_worker();
        let left = manager.clone();
        let right = manager.clone();

        let a = thread::spawn(move || left.list_segments(session.id));
        let b = thread::spawn(move || right.list_segments(session.id));

        a.join().expect("join a").expect("list a");
        b.join().expect("join b").expect("list b");
        assert!(!worker.concurrent_operation.load(Ordering::Relaxed));
    }

    #[test]
    fn startup_failure_marks_session_error() {
        let root = test_root("startup-failure");
        let install = fake_ida_install(&root);
        let target = fake_binary(&root);
        let launcher = Arc::new(MockReverseWorkerLauncher::with_open_error("open failed"));
        let manager = IdaSessionManager::with_worker_launcher_runtime_and_logger(
            launcher,
            root.join("artifacts"),
            IdaRuntimeConfig {
                install_dir: Some(install.install_dir),
            },
            noop_logger(),
        );

        let error = manager
            .create_session(CreateIdaSession {
                target: IdaTarget::Binary { path: target },
                run_auto_analysis: true,
                startup_timeout_ms: Some(1000),
            })
            .expect_err("create fails");

        assert!(error.to_string().contains("open failed"));
        let sessions = manager.list_sessions().expect("list");
        assert_eq!(sessions[0].state, ReverseSessionState::Error);
    }

    #[test]
    fn rich_queries_write_outputs_and_mutations_are_audited() {
        let root = test_root("rich");
        let install = fake_ida_install(&root);
        let target = fake_binary(&root);
        let manager = manager_with_mock(&root, install);
        let session = manager
            .create_session(CreateIdaSession {
                target: IdaTarget::Binary { path: target },
                run_auto_analysis: true,
                startup_timeout_ms: Some(1000),
            })
            .expect("create");

        let metadata = manager.get_metadata(session.id).expect("metadata");
        let strings = manager
            .list_strings(session.id, PageRequest::default())
            .expect("strings");
        let disassembly = manager
            .disassemble(
                session.id,
                DisassembleRequest {
                    target: "0x1100".to_string(),
                    offset: 0,
                    limit: Some(usize::MAX),
                },
            )
            .expect("disassemble");
        let xrefs = manager
            .list_xrefs(
                session.id,
                ListXrefsRequest {
                    target: "0x1100".to_string(),
                    direction: super::super::model::XrefDirection::Both,
                    kind: super::super::model::XrefKind::Any,
                    offset: 0,
                    limit: None,
                },
            )
            .expect("xrefs");
        let rename = manager
            .rename(
                session.id,
                RenameRequest {
                    items: vec![RenameItem {
                        target: "0x1100".to_string(),
                        name: "renamed_main".to_string(),
                        kind: Some("function".to_string()),
                    }],
                    dry_run: true,
                    allow_overwrite: false,
                },
            )
            .expect("rename");

        assert!(metadata
            .artifact
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("metadata-") && name.ends_with(".json")));
        assert_eq!(strings.strings[0].value, "hello");
        assert_eq!(disassembly.disassembly.page.limit, MAX_PAGE_LIMIT);
        assert_eq!(xrefs.xrefs.page.limit, DEFAULT_PAGE_LIMIT);
        assert!(rename
            .artifact
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("rename-") && name.ends_with(".json")));
        assert!(rename.results[0].dry_run);
        let second_rename = manager
            .rename(
                session.id,
                RenameRequest {
                    items: vec![RenameItem {
                        target: "0x1200".to_string(),
                        name: "renamed_helper".to_string(),
                        kind: Some("function".to_string()),
                    }],
                    dry_run: true,
                    allow_overwrite: false,
                },
            )
            .expect("second rename");
        assert_ne!(rename.artifact.path, second_rename.artifact.path);
        let first_artifact = std::fs::read_to_string(&rename.artifact.path).expect("first rename");
        let second_artifact =
            std::fs::read_to_string(&second_rename.artifact.path).expect("second rename");
        assert!(first_artifact.contains("renamed_main"));
        assert!(second_artifact.contains("renamed_helper"));
        let events = std::fs::read_to_string(
            root.join("artifacts")
                .join("reverse_sessions")
                .join(session.id.to_string())
                .join("events.jsonl"),
        )
        .expect("events");
        assert!(events.contains("ida.rename"));
        assert!(events.contains("mutation_finished"));
        assert!(events.contains("old_name"));
        assert!(events.contains("renamed_main"));
    }

    #[test]
    fn rich_query_failure_keeps_session_ready_when_worker_is_alive() {
        let root = test_root("rich-error");
        let install = fake_ida_install(&root);
        let target = fake_binary(&root);
        let launcher = Arc::new(MockReverseWorkerLauncher::with_rich_error(
            "IDA direct rich API is unavailable",
        ));
        let manager = IdaSessionManager::with_worker_launcher_runtime_and_logger(
            launcher,
            root.join("artifacts"),
            IdaRuntimeConfig {
                install_dir: Some(install.install_dir.clone()),
            },
            noop_logger(),
        );
        let session = manager
            .create_session(CreateIdaSession {
                target: IdaTarget::Binary { path: target },
                run_auto_analysis: true,
                startup_timeout_ms: Some(1000),
            })
            .expect("create");

        let error = manager
            .list_strings(session.id, PageRequest::default())
            .expect_err("rich unsupported");
        let session_after = manager.get_session(session.id).expect("session");

        assert!(error.to_string().contains("direct rich API"));
        assert_eq!(session_after.state, ReverseSessionState::Ready);
    }

    #[test]
    #[ignore = "requires a licensed local IDA Professional runtime"]
    fn real_ida_create_binary_session() {
        if std::env::var("DBGFLOW_REAL_IDA_TEST").as_deref() != Ok("1") {
            return;
        }
        let ida_dir = std::env::var_os("DBGFLOW_IDA_DIR")
            .map(PathBuf::from)
            .expect("DBGFLOW_IDA_DIR must be set");
        let root = test_root("real-create");
        let manager = IdaSessionManager::new(
            root.join("artifacts"),
            IdaRuntimeConfig {
                install_dir: Some(ida_dir),
            },
        );
        let target = std::env::current_exe().expect("current test exe");

        let session = manager
            .create_session(CreateIdaSession {
                target: IdaTarget::Binary { path: target },
                run_auto_analysis: true,
                startup_timeout_ms: Some(60_000),
            })
            .expect("create real IDA session");

        assert_eq!(session.state, ReverseSessionState::Ready);
        let _ = manager.close_session(session.id);
    }

    #[test]
    #[ignore = "requires a licensed local IDA Professional runtime"]
    fn real_ida_list_segments_functions() {
        if std::env::var("DBGFLOW_REAL_IDA_TEST").as_deref() != Ok("1") {
            return;
        }
        let ida_dir = std::env::var_os("DBGFLOW_IDA_DIR")
            .map(PathBuf::from)
            .expect("DBGFLOW_IDA_DIR must be set");
        let root = test_root("real-list");
        let manager = IdaSessionManager::new(
            root.join("artifacts"),
            IdaRuntimeConfig {
                install_dir: Some(ida_dir),
            },
        );
        let target = std::env::current_exe().expect("current test exe");
        let session = manager
            .create_session(CreateIdaSession {
                target: IdaTarget::Binary { path: target },
                run_auto_analysis: true,
                startup_timeout_ms: Some(60_000),
            })
            .expect("create real IDA session");

        let segments = manager.list_segments(session.id).expect("segments");
        let functions = manager.list_functions(session.id).expect("functions");

        assert!(!segments.segments.is_empty());
        assert!(!functions.functions.is_empty());
        let _ = manager.close_session(session.id);
    }

    #[test]
    #[ignore = "requires a licensed local IDA Professional runtime with direct rich bindings"]
    fn real_ida_rich_tools_direct_bindings() {
        if std::env::var("DBGFLOW_REAL_IDA_DIRECT_TEST").as_deref() != Ok("1") {
            return;
        }
        let ida_dir = std::env::var_os("DBGFLOW_IDA_DIR")
            .map(PathBuf::from)
            .expect("DBGFLOW_IDA_DIR must be set");
        let root = test_root("real-rich");
        let manager = IdaSessionManager::new(
            root.join("artifacts"),
            IdaRuntimeConfig {
                install_dir: Some(ida_dir),
            },
        );
        let target = std::env::current_exe().expect("current test exe");
        let session = manager
            .create_session(CreateIdaSession {
                target: IdaTarget::Binary { path: target },
                run_auto_analysis: true,
                startup_timeout_ms: Some(60_000),
            })
            .expect("create real IDA session");

        let metadata = manager.get_metadata(session.id).expect("metadata");
        assert!(metadata.metadata.rich_api.available);
        let functions = manager.list_functions(session.id).expect("functions");
        let first = functions
            .functions
            .first()
            .expect("at least one function")
            .start_ea
            .clone();
        let disassembly = manager
            .disassemble(
                session.id,
                DisassembleRequest {
                    target: first.clone(),
                    offset: 0,
                    limit: Some(10),
                },
            )
            .expect("disassemble");
        assert!(disassembly.disassembly.error.is_none());
        let _ = manager.decompile(
            session.id,
            DecompileRequest {
                target: first,
                include_addresses: true,
            },
        );
        let _ = manager.close_session_with_save(session.id, false);
    }

    fn manager_with_mock(root: &std::path::Path, install: IdaInstall) -> IdaSessionManager {
        IdaSessionManager::with_worker_launcher_runtime_and_logger(
            Arc::new(MockReverseWorkerLauncher::default()),
            root.join("artifacts"),
            IdaRuntimeConfig {
                install_dir: Some(install.install_dir),
            },
            noop_logger(),
        )
    }

    #[derive(Default)]
    struct MockReverseWorkerLauncher {
        spawn_count: AtomicU64,
        workers: Mutex<Vec<Arc<MockReverseWorker>>>,
        contexts: Mutex<Vec<ProcessLaunchContext>>,
        open_error: Mutex<Option<String>>,
        rich_error: Mutex<Option<String>>,
    }

    impl MockReverseWorkerLauncher {
        fn with_open_error(message: &str) -> Self {
            Self {
                open_error: Mutex::new(Some(message.to_string())),
                ..Default::default()
            }
        }

        fn with_rich_error(message: &str) -> Self {
            Self {
                rich_error: Mutex::new(Some(message.to_string())),
                ..Default::default()
            }
        }

        fn last_worker(&self) -> Arc<MockReverseWorker> {
            self.workers
                .lock()
                .expect("workers")
                .last()
                .expect("worker")
                .clone()
        }

        fn last_context(&self) -> ProcessLaunchContext {
            self.contexts
                .lock()
                .expect("contexts")
                .last()
                .expect("context")
                .clone()
        }
    }

    impl ReverseWorkerLauncher for MockReverseWorkerLauncher {
        fn spawn(
            &self,
            _session_id: SessionId,
            _install: IdaInstall,
            _worker_log_path: PathBuf,
            launch_context: ProcessLaunchContext,
            _logger: Arc<dyn LogSink>,
        ) -> Result<Arc<dyn ReverseWorker>> {
            self.spawn_count.fetch_add(1, Ordering::Relaxed);
            self.contexts.lock().expect("contexts").push(launch_context);
            let worker = Arc::new(MockReverseWorker {
                open_error: self.open_error.lock().expect("open error").clone(),
                rich_error: self.rich_error.lock().expect("rich error").clone(),
                ..Default::default()
            });
            self.workers.lock().expect("workers").push(worker.clone());
            Ok(worker)
        }
    }

    #[derive(Default)]
    struct MockReverseWorker {
        closed: AtomicBool,
        saved: AtomicBool,
        killed: AtomicBool,
        in_flight: AtomicUsize,
        concurrent_operation: AtomicBool,
        open_error: Option<String>,
        rich_error: Option<String>,
    }

    impl MockReverseWorker {
        fn enter(&self) -> OperationGuard<'_> {
            if self.in_flight.fetch_add(1, Ordering::SeqCst) != 0 {
                self.concurrent_operation.store(true, Ordering::SeqCst);
            }
            OperationGuard { worker: self }
        }

        fn check_rich(&self) -> Result<()> {
            if let Some(error) = &self.rich_error {
                return Err(DbgFlowError::Backend(error.clone()));
            }
            Ok(())
        }
    }

    struct OperationGuard<'a> {
        worker: &'a MockReverseWorker,
    }

    impl Drop for OperationGuard<'_> {
        fn drop(&mut self) {
            self.worker.in_flight.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl ReverseWorker for MockReverseWorker {
        fn open_database(&self, _request: OpenIdaDatabase) -> Result<OpenIdaDatabaseResult> {
            if let Some(error) = &self.open_error {
                return Err(DbgFlowError::Backend(error.clone()));
            }
            Ok(OpenIdaDatabaseResult {
                ida: IdaInfo {
                    install_dir: PathBuf::from(r"C:\FakeIDA"),
                    version: IdaVersion {
                        major: 9,
                        minor: 3,
                        build: 260327,
                    },
                },
                warnings: Vec::new(),
            })
        }

        fn list_segments(&self) -> Result<Vec<SegmentInfo>> {
            let _guard = self.enter();
            thread::sleep(Duration::from_millis(25));
            Ok(vec![
                SegmentInfo {
                    index: 0,
                    start_ea: "0x1000".to_string(),
                    end_ea: "0x2000".to_string(),
                    size: "0x1000".to_string(),
                    name: Some(".text".to_string()),
                    class: Some("CODE".to_string()),
                    perm: "r-x".to_string(),
                    bitness: 64,
                },
                SegmentInfo {
                    index: 1,
                    start_ea: "0x2000".to_string(),
                    end_ea: "0x3000".to_string(),
                    size: "0x1000".to_string(),
                    name: Some(".rdata".to_string()),
                    class: Some("DATA".to_string()),
                    perm: "r--".to_string(),
                    bitness: 64,
                },
            ])
        }

        fn list_functions(&self) -> Result<Vec<FunctionInfo>> {
            Ok(vec![
                FunctionInfo {
                    index: 0,
                    start_ea: "0x1100".to_string(),
                    end_ea: "0x1200".to_string(),
                    size: "0x100".to_string(),
                    name: Some("main".to_string()),
                    segment: Some(".text".to_string()),
                    prototype: Some("int main()".to_string()),
                    flags: "0x1".to_string(),
                },
                FunctionInfo {
                    index: 1,
                    start_ea: "0x1200".to_string(),
                    end_ea: "0x1300".to_string(),
                    size: "0x100".to_string(),
                    name: Some("helper".to_string()),
                    segment: Some(".text".to_string()),
                    prototype: Some("void helper()".to_string()),
                    flags: "0x2".to_string(),
                },
            ])
        }

        fn get_metadata(&self) -> Result<IdaMetadata> {
            self.check_rich()?;
            Ok(IdaMetadata {
                target: IdaTarget::Binary {
                    path: PathBuf::from(r"C:\sample.exe"),
                },
                ida: Some(IdaInfo {
                    install_dir: PathBuf::from(r"C:\FakeIDA"),
                    version: IdaVersion {
                        major: 9,
                        minor: 3,
                        build: 260327,
                    },
                }),
                segments: 2,
                functions: 2,
                rich_api: super::super::model::IdaRichApiStatus {
                    available: true,
                    direct_bindings: true,
                    ida_version_gate: "IDA Professional 9.3 x64".to_string(),
                    capabilities: super::super::model::DirectIdaCapabilities {
                        names: true,
                        disassembly: true,
                        strings: true,
                        imports: true,
                        exports: true,
                        xrefs: true,
                        basic_blocks: true,
                        comments: true,
                        types: true,
                        decompiler: true,
                    },
                    missing_symbols: Vec::new(),
                    hexrays: Some("available".to_string()),
                    warnings: Vec::new(),
                },
            })
        }

        fn list_strings(&self, _request: PageRequest) -> Result<Vec<StringInfo>> {
            self.check_rich()?;
            Ok(vec![StringInfo {
                index: 0,
                ea: "0x2000".to_string(),
                length: 5,
                string_type: Some("ascii".to_string()),
                value: "hello".to_string(),
            }])
        }

        fn list_imports(&self, _request: PageRequest) -> Result<Vec<ImportInfo>> {
            self.check_rich()?;
            Ok(vec![ImportInfo {
                index: 0,
                ea: "0x3000".to_string(),
                module: Some("kernel32.dll".to_string()),
                name: Some("ExitProcess".to_string()),
                ordinal: None,
            }])
        }

        fn list_exports(&self, _request: PageRequest) -> Result<Vec<ExportInfo>> {
            self.check_rich()?;
            Ok(vec![ExportInfo {
                index: 0,
                ea: "0x1100".to_string(),
                name: Some("main".to_string()),
                ordinal: Some(1),
            }])
        }

        fn lookup_functions(&self, request: LookupFunctionsRequest) -> Result<Vec<FunctionLookup>> {
            self.check_rich()?;
            Ok(request
                .queries
                .into_iter()
                .map(|query| FunctionLookup {
                    query,
                    function: Some(FunctionInfo {
                        index: 0,
                        start_ea: "0x1100".to_string(),
                        end_ea: "0x1200".to_string(),
                        size: "0x100".to_string(),
                        name: Some("main".to_string()),
                        segment: Some(".text".to_string()),
                        prototype: Some("int main()".to_string()),
                        flags: "0x1".to_string(),
                    }),
                    error: None,
                })
                .collect())
        }

        fn disassemble(&self, request: DisassembleRequest) -> Result<Disassembly> {
            self.check_rich()?;
            Ok(Disassembly {
                target: request.target,
                function: None,
                lines: vec![DisassemblyLine {
                    ea: "0x1100".to_string(),
                    text: "ret".to_string(),
                    label: Some("main".to_string()),
                    comments: Vec::new(),
                    refs: Vec::new(),
                }],
                page: PageInfo {
                    offset: request.offset,
                    limit: request.limit.unwrap_or(100),
                    total: 1,
                    returned: 1,
                    next_offset: None,
                },
                error: None,
            })
        }

        fn decompile(&self, request: DecompileRequest) -> Result<DecompileResult> {
            self.check_rich()?;
            Ok(DecompileResult {
                target: request.target,
                function: None,
                code: Some("int main() { return 0; }".to_string()),
                refs: Vec::new(),
                error: None,
            })
        }

        fn list_xrefs(&self, request: ListXrefsRequest) -> Result<XrefsResult> {
            self.check_rich()?;
            Ok(XrefsResult {
                target: request.target,
                xrefs: Vec::new(),
                page: PageInfo {
                    offset: request.offset,
                    limit: request.limit.unwrap_or(100),
                    total: 0,
                    returned: 0,
                    next_offset: None,
                },
                error: None,
            })
        }

        fn list_basic_blocks(&self, request: BasicBlocksRequest) -> Result<BasicBlocksResult> {
            self.check_rich()?;
            Ok(BasicBlocksResult {
                target: request.target,
                function: None,
                blocks: vec![BasicBlockInfo {
                    id: 0,
                    start_ea: "0x1100".to_string(),
                    end_ea: "0x1101".to_string(),
                    successors: Vec::new(),
                    predecessors: Vec::new(),
                }],
                error: None,
            })
        }

        fn rename(&self, request: RenameRequest) -> Result<Vec<MutationItemResult>> {
            self.check_rich()?;
            Ok(request
                .items
                .into_iter()
                .map(|item| MutationItemResult {
                    target: item.target,
                    old: Some("old_name".to_string()),
                    new: Some(item.name),
                    success: true,
                    dry_run: request.dry_run,
                    error: None,
                })
                .collect())
        }

        fn set_comment(&self, request: SetCommentRequest) -> Result<Vec<MutationItemResult>> {
            self.check_rich()?;
            Ok(request
                .items
                .into_iter()
                .map(|item| MutationItemResult {
                    target: item.target,
                    old: None,
                    new: Some(item.comment),
                    success: true,
                    dry_run: false,
                    error: None,
                })
                .collect())
        }

        fn set_type(&self, request: SetTypeRequest) -> Result<Vec<MutationItemResult>> {
            self.check_rich()?;
            Ok(request
                .items
                .into_iter()
                .map(|item| MutationItemResult {
                    target: item.target,
                    old: None,
                    new: Some(item.type_text),
                    success: true,
                    dry_run: request.dry_run,
                    error: None,
                })
                .collect())
        }

        fn has_exited(&self) -> Result<bool> {
            Ok(self.closed.load(Ordering::Relaxed) || self.killed.load(Ordering::Relaxed))
        }

        fn close(&self, save: bool) -> Result<CloseDatabaseResult> {
            self.saved.store(save, Ordering::Relaxed);
            self.closed.store(true, Ordering::Relaxed);
            Ok(CloseDatabaseResult::from_idalib_close(save))
        }

        fn kill(&self, _reason: &str) -> Result<()> {
            self.killed.store(true, Ordering::Relaxed);
            Ok(())
        }
    }

    fn fake_binary(root: &std::path::Path) -> PathBuf {
        let path = root.join("sample.exe");
        std::fs::write(&path, b"MZ").expect("write sample");
        path
    }

    fn fake_ida_install(root: &std::path::Path) -> IdaInstall {
        let install_dir = root.join("ida");
        std::fs::create_dir_all(&install_dir).expect("create ida dir");
        for file_name in ["ida.exe", "ida.dll", "idalib.dll", "ida.hlp"] {
            std::fs::write(install_dir.join(file_name), b"").expect("write ida file");
        }
        super::super::install::validate_ida_install_dir(&install_dir).expect("validate fake ida")
    }

    fn test_root(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("dbgflow-ida-manager-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create root");
        root
    }
}
