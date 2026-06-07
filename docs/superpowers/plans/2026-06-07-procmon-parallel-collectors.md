# Procmon Parallel Collectors Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add parallel `run_profile` collectors and an optional Procmon collector configured only through `--sysinternals-dir`.

**Architecture:** Extend the core profile model from one collector to `collectors[]`, then make `ProfileManager` start/stop/export multiple collectors around the same launch target. Runtime configuration owns Sysinternals discovery; core Procmon support receives an already-resolved tool directory and refuses Procmon requests when the capability is unavailable.

**Tech Stack:** Rust 2021, serde/serde_json, Windows service runtime, PowerShell 5.1 install script, Sysinternals Process Monitor.

---

## File Structure

- Modify `crates/dbgflow-core/src/profile/model.rs`: collector enum, Procmon filter config, collector results, and multi-collector `RunProfile`.
- Modify `crates/dbgflow-core/src/profile/collector.rs`: factory interface and collector result support for collector-specific output directories.
- Modify `crates/dbgflow-core/src/profile/manager.rs`: normalize collector lifecycle into start all, launch target, stop all, write collector results.
- Modify `crates/dbgflow-core/src/profile/native_etw.rs`: adapt factory to collector-specific `collectors/native_etw` output.
- Create `crates/dbgflow-core/src/profile/procmon.rs`: Sysinternals Procmon resolution and minimal Procmon collector.
- Modify `crates/dbgflow-core/src/profile/mod.rs`: export Procmon config/types.
- Modify `crates/dbgflow-core/src/artifacts/manager.rs`: create `collectors/<name>` directories and artifact kinds for collector outputs.
- Modify `crates/dbgflow-core/tests/profile_lifecycle.rs`: unit coverage for multi-collector lifecycle and error behavior.
- Create `crates/dbgflow-core/tests/procmon_profile.rs`: ignored Windows integration test for real Procmon.
- Modify `crates/dbgflow-mcp/src/runtime.rs`: parse and carry `--sysinternals-dir` in http, service run, and service install configs.
- Modify `crates/dbgflow-mcp/src/mcp.rs`: build `ProfileManager` with profile runtime options.
- Modify `crates/dbgflow-mcp/src/tools.rs`: MCP schema and request decoding for `collector` or `collectors`.
- Modify `crates/dbgflow-mcp/src/service.rs`: include `--sysinternals-dir` in installed service launch arguments.
- Modify `scripts/install-service.ps1`: add `-SysinternalsDir`, auto-detect candidates, prompt before writing service config.
- Modify `README.md`, `README.zh-CN.md`, and `GOALS.md`: document parallel collectors and Sysinternals constraints.

---

### Task 1: Core Collector Model

**Files:**
- Modify: `crates/dbgflow-core/src/profile/model.rs`
- Modify: `crates/dbgflow-core/src/profile/mod.rs`
- Test: `crates/dbgflow-core/tests/profile_lifecycle.rs`

- [ ] **Step 1: Write failing tests for collector config shape**

Add tests in `crates/dbgflow-core/tests/profile_lifecycle.rs`:

```rust
#[test]
fn default_run_profile_collectors_is_native_etw_system_overview() {
    let request = RunProfile {
        target: ProfileTarget::Launch {
            executable: std::env::current_exe().expect("current exe"),
            args: Vec::new(),
        },
        timeout_ms: 1000,
        collectors: Vec::new(),
    }
    .with_default_collectors();

    assert_eq!(request.collectors.len(), 1);
    assert!(matches!(
        request.collectors[0],
        ProfileCollectorConfig::NativeEtw {
            preset: ProfilePreset::SystemOverview
        }
    ));
}

#[test]
fn procmon_collector_config_defaults_to_no_stacks_and_empty_filters() {
    let config = ProfileCollectorConfig::Procmon {
        capture_stacks: false,
        filters: Default::default(),
    };

    assert_eq!(config.kind(), ProfileCollectorKind::Procmon);
    let ProfileCollectorConfig::Procmon {
        capture_stacks,
        filters,
    } = config else {
        panic!("expected procmon config");
    };
    assert!(!capture_stacks);
    assert!(filters.operations.is_empty());
    assert!(filters.paths.is_empty());
}
```

- [ ] **Step 2: Run the failing tests**

Run:

```powershell
cargo test -p dbgflow-core --test profile_lifecycle default_run_profile_collectors_is_native_etw_system_overview procmon_collector_config_defaults_to_no_stacks_and_empty_filters
```

Expected: fail because `RunProfile.collectors`, `ProfileCollectorConfig::Procmon`, and `kind()` do not exist.

- [ ] **Step 3: Implement model types**

Replace the single-collector model in `crates/dbgflow-core/src/profile/model.rs` with:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunProfile {
    pub target: ProfileTarget,
    pub timeout_ms: u64,
    #[serde(default)]
    pub collectors: Vec<ProfileCollectorConfig>,
}

impl RunProfile {
    pub fn with_default_collectors(mut self) -> Self {
        if self.collectors.is_empty() {
            self.collectors.push(ProfileCollectorConfig::default());
        }
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProfileCollectorConfig {
    NativeEtw { preset: ProfilePreset },
    Procmon {
        #[serde(default)]
        capture_stacks: bool,
        #[serde(default)]
        filters: ProcmonFilterConfig,
    },
}

impl ProfileCollectorConfig {
    pub fn kind(&self) -> ProfileCollectorKind {
        match self {
            Self::NativeEtw { .. } => ProfileCollectorKind::NativeEtw,
            Self::Procmon { .. } => ProfileCollectorKind::Procmon,
        }
    }

    pub fn artifact_name(&self) -> &'static str {
        match self {
            Self::NativeEtw { .. } => "native_etw",
            Self::Procmon { .. } => "procmon",
        }
    }
}

impl Default for ProfileCollectorConfig {
    fn default() -> Self {
        Self::NativeEtw {
            preset: ProfilePreset::SystemOverview,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProcmonFilterConfig {
    #[serde(default)]
    pub operations: Vec<String>,
    #[serde(default)]
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileCollectorKind {
    NativeEtw,
    Procmon,
}
```

Add collector result types:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileCollectorStatus {
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileCollectorResult {
    pub kind: ProfileCollectorKind,
    pub name: String,
    pub status: ProfileCollectorStatus,
    pub artifacts: Vec<ArtifactRef>,
    pub warnings: Vec<String>,
    pub error: Option<String>,
}
```

Extend `ProfileResult`:

```rust
pub struct ProfileResult {
    pub profile_id: ProfileId,
    pub status: ProfileStatus,
    pub completion_reason: ProfileCompletionReason,
    pub target_pid: Option<u32>,
    pub target_exit_code: Option<i32>,
    pub duration_ms: u128,
    pub artifacts: ProfileArtifacts,
    pub collector_results: Vec<ProfileCollectorResult>,
    pub warnings: Vec<String>,
    pub error: Option<String>,
}
```

Change `ProfileArtifacts.trace` to an optional legacy convenience field:

```rust
pub struct ProfileArtifacts {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace: Option<ArtifactRef>,
    pub profile: ArtifactRef,
    pub events: ArtifactRef,
    pub stdout: ArtifactRef,
    pub stderr: ArtifactRef,
}
```

- [ ] **Step 4: Run model tests**

Run:

```powershell
cargo test -p dbgflow-core --test profile_lifecycle default_run_profile_collectors_is_native_etw_system_overview procmon_collector_config_defaults_to_no_stacks_and_empty_filters
```

Expected: pass.

- [ ] **Step 5: Commit**

```powershell
git add crates/dbgflow-core/src/profile/model.rs crates/dbgflow-core/src/profile/mod.rs crates/dbgflow-core/tests/profile_lifecycle.rs
git commit -m "Add profile collector model for procmon"
```

### Task 2: Artifact Layout for Collector Directories

**Files:**
- Modify: `crates/dbgflow-core/src/artifacts/manager.rs`
- Test: `crates/dbgflow-core/src/artifacts/manager.rs`

- [ ] **Step 1: Write failing artifact layout test**

Add to artifact manager tests:

```rust
#[test]
fn profile_collector_artifacts_are_under_named_collector_directories() {
    let root = std::env::temp_dir().join(format!(
        "dbgflow-profile-collector-artifacts-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let artifacts = ArtifactManager::new(&root);
    let profile_id = ProfileId::new();

    let dir = artifacts
        .profile_collector_dir(profile_id, "procmon")
        .expect("collector dir");

    assert_eq!(
        dir,
        root.join("profiles")
            .join(profile_id.to_string())
            .join("collectors")
            .join("procmon")
    );
    assert!(dir.is_dir());
    assert_eq!(
        artifacts.profile_collector_artifact_path(profile_id, "procmon", "capture.pml"),
        dir.join("capture.pml")
    );
}
```

- [ ] **Step 2: Run the failing test**

Run:

```powershell
cargo test -p dbgflow-core artifacts::manager::tests::profile_collector_artifacts_are_under_named_collector_directories
```

Expected: fail because the new artifact APIs do not exist.

- [ ] **Step 3: Implement collector artifact APIs**

Add artifact kinds:

```rust
ProfileCollectorTrace,
ProfileCollectorSummary,
ProfileCollectorEvents,
```

Add methods:

```rust
pub fn profile_collector_dir(
    &self,
    profile_id: ProfileId,
    collector_name: &str,
) -> Result<PathBuf> {
    if collector_name.is_empty()
        || collector_name
            .chars()
            .any(|ch| matches!(ch, '/' | '\\') || ch.is_control())
    {
        return Err(DbgFlowError::Artifact(
            "profile collector artifact name is invalid".to_string(),
        ));
    }
    let _guard = self
        .lock
        .lock()
        .map_err(|_| DbgFlowError::Artifact("artifact manager lock poisoned".to_string()))?;
    let dir = self
        .ensure_profile_dir_unlocked(profile_id)?
        .join("collectors")
        .join(collector_name);
    fs::create_dir_all(&dir).map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
    Ok(dir)
}

pub fn profile_collector_artifact_path(
    &self,
    profile_id: ProfileId,
    collector_name: &str,
    file_name: &str,
) -> PathBuf {
    self.root
        .join("profiles")
        .join(profile_id.to_string())
        .join("collectors")
        .join(collector_name)
        .join(file_name)
}
```

Update `initialize_profile_artifacts` to create `collectors/`.

- [ ] **Step 4: Run artifact tests**

Run:

```powershell
cargo test -p dbgflow-core artifacts::manager::tests::profile_collector_artifacts_are_under_named_collector_directories
```

Expected: pass.

- [ ] **Step 5: Commit**

```powershell
git add crates/dbgflow-core/src/artifacts/manager.rs
git commit -m "Add profile collector artifact directories"
```

### Task 3: Parallel Collector Lifecycle

**Files:**
- Modify: `crates/dbgflow-core/src/profile/collector.rs`
- Modify: `crates/dbgflow-core/src/profile/manager.rs`
- Modify: `crates/dbgflow-core/src/profile/native_etw.rs`
- Test: `crates/dbgflow-core/tests/profile_lifecycle.rs`

- [ ] **Step 1: Write failing lifecycle tests**

Add tests:

```rust
#[test]
fn run_profile_starts_and_stops_collectors_in_order() {
    let root = test_profile_root("multi-collector");
    let collector_state = Arc::new(Mutex::new(Vec::new()));
    let manager = ProfileManager::with_components(
        &root,
        Arc::new(TestCollectorFactory {
            state: collector_state.clone(),
            fail_start_for: None,
            fail_stop_for: None,
        }),
        Arc::new(TestTargetRunner {
            exit: TargetExit::Exited {
                pid: 1234,
                exit_code: Some(0),
            },
        }),
    );

    let result = manager
        .run_profile(RunProfile {
            target: ProfileTarget::Launch {
                executable: std::env::current_exe().expect("current exe"),
                args: Vec::new(),
            },
            timeout_ms: 1000,
            collectors: vec![
                ProfileCollectorConfig::NativeEtw {
                    preset: ProfilePreset::SystemOverview,
                },
                ProfileCollectorConfig::Procmon {
                    capture_stacks: true,
                    filters: Default::default(),
                },
            ],
        })
        .expect("run profile");

    assert_eq!(result.status, ProfileStatus::Completed);
    assert_eq!(result.collector_results.len(), 2);
    assert_eq!(
        collector_state.lock().expect("state").as_slice(),
        &[
            "start:native_etw".to_string(),
            "start:procmon".to_string(),
            "stop:procmon".to_string(),
            "stop:native_etw".to_string(),
        ]
    );
}

#[test]
fn run_profile_start_failure_stops_already_started_collectors() {
    let root = test_profile_root("multi-start-failure");
    let collector_state = Arc::new(Mutex::new(Vec::new()));
    let manager = ProfileManager::with_components(
        &root,
        Arc::new(TestCollectorFactory {
            state: collector_state.clone(),
            fail_start_for: Some("procmon".to_string()),
            fail_stop_for: None,
        }),
        Arc::new(PanicTargetRunner),
    );

    let error = manager
        .run_profile(RunProfile {
            target: ProfileTarget::Launch {
                executable: std::env::current_exe().expect("current exe"),
                args: Vec::new(),
            },
            timeout_ms: 1000,
            collectors: vec![
                ProfileCollectorConfig::NativeEtw {
                    preset: ProfilePreset::SystemOverview,
                },
                ProfileCollectorConfig::Procmon {
                    capture_stacks: false,
                    filters: Default::default(),
                },
            ],
        })
        .expect_err("collector start fails");

    assert!(error.to_string().contains("collector start failed: procmon"));
    assert_eq!(
        collector_state.lock().expect("state").as_slice(),
        &[
            "start:native_etw".to_string(),
            "start:procmon".to_string(),
            "stop:native_etw".to_string(),
        ]
    );
}
```

- [ ] **Step 2: Run failing lifecycle tests**

Run:

```powershell
cargo test -p dbgflow-core --test profile_lifecycle run_profile_starts_and_stops_collectors_in_order run_profile_start_failure_stops_already_started_collectors
```

Expected: fail because manager and test factories still support only one collector.

- [ ] **Step 3: Update collector trait and factory**

Use this interface in `collector.rs`:

```rust
pub trait ProfileCollector: Send + Sync {
    fn name(&self) -> &str;
    fn kind(&self) -> ProfileCollectorKind;
    fn start(&self) -> Result<CollectorStart>;
    fn stop(&self) -> Result<CollectorStop>;
    fn cleanup(&self) -> Result<()>;
}

pub trait CollectorFactory: Send + Sync {
    fn create(
        &self,
        config: &ProfileCollectorConfig,
        output_dir: &Path,
    ) -> Result<Box<dyn ProfileCollector>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectorStop {
    pub artifacts: Vec<ArtifactRef>,
    pub warnings: Vec<String>,
}
```

- [ ] **Step 4: Refactor manager lifecycle**

In `ProfileManager::run_profile`:

```rust
let request = request.with_default_collectors();
let mut started_collectors = Vec::<Box<dyn ProfileCollector>>::new();

for config in &request.collectors {
    let output_dir = self
        .artifacts
        .profile_collector_dir(profile_id, config.artifact_name())?;
    self.record_event(profile_id, "collector_starting", None, None, collector_fields(config));
    let collector = self.collector_factory.create(config, &output_dir)?;
    match collector.start() {
        Ok(start) => {
            warnings.extend(start.warnings);
            self.record_event(profile_id, "collector_started", Some(output_dir), None, collector_fields(config));
            started_collectors.push(collector);
        }
        Err(error) => {
            stop_started_collectors_in_reverse(&mut started_collectors, profile_id, self);
            return Err(error);
        }
    }
}
```

After target completion, stop in reverse order and convert each stop into `ProfileCollectorResult`. Stop errors should record a failed collector result and add a warning; they should not discard successful collector artifacts.

- [ ] **Step 5: Adapt native ETW collector**

Change `NativeEtwCollectorFactory::create` to receive `output_dir` and write:

```rust
let trace_path = output_dir.join("trace.etl");
```

Change `NativeEtwCollector::stop` to return:

```rust
Ok(CollectorStop {
    artifacts: vec![ArtifactRef {
        kind: ArtifactKind::ProfileCollectorTrace,
        path: self.trace_path.clone(),
    }],
    warnings,
})
```

- [ ] **Step 6: Run lifecycle tests**

Run:

```powershell
cargo test -p dbgflow-core --test profile_lifecycle
```

Expected: pass.

- [ ] **Step 7: Commit**

```powershell
git add crates/dbgflow-core/src/profile/collector.rs crates/dbgflow-core/src/profile/manager.rs crates/dbgflow-core/src/profile/native_etw.rs crates/dbgflow-core/tests/profile_lifecycle.rs
git commit -m "Run profile collectors in parallel lifecycle"
```

### Task 4: MCP Request Normalization and Schema

**Files:**
- Modify: `crates/dbgflow-mcp/src/tools.rs`
- Test: `crates/dbgflow-mcp/src/tools.rs`

- [ ] **Step 1: Write failing MCP decode tests**

Add tests:

```rust
#[test]
fn run_profile_arguments_decode_legacy_collector_to_collectors() {
    let value = json!({
        "target": {
            "kind": "launch",
            "executable": "C:\\Windows\\System32\\cmd.exe"
        },
        "timeout_ms": 1000,
        "collector": {
            "kind": "native_etw",
            "preset": "system_overview"
        }
    });

    let request: RunProfileRequest = decode_arguments(value).expect("decode request");
    assert_eq!(request.collectors.len(), 1);
    assert!(matches!(
        request.collectors[0],
        ProfileCollectorConfig::NativeEtw {
            preset: ProfilePreset::SystemOverview
        }
    ));
}

#[test]
fn run_profile_arguments_decode_procmon_collectors_array() {
    let value = json!({
        "target": {
            "kind": "launch",
            "executable": "C:\\Windows\\System32\\cmd.exe"
        },
        "timeout_ms": 1000,
        "collectors": [
            {
                "kind": "native_etw",
                "preset": "system_overview"
            },
            {
                "kind": "procmon",
                "capture_stacks": true,
                "filters": {
                    "operations": ["CreateFile", "ReadFile"],
                    "paths": ["C:\\data\\large_input.bin"]
                }
            }
        ]
    });

    let request: RunProfileRequest = decode_arguments(value).expect("decode request");
    assert_eq!(request.collectors.len(), 2);
    assert!(matches!(request.collectors[1], ProfileCollectorConfig::Procmon { .. }));
}

#[test]
fn run_profile_arguments_reject_both_collector_forms() {
    let value = json!({
        "target": {
            "kind": "launch",
            "executable": "C:\\Windows\\System32\\cmd.exe"
        },
        "timeout_ms": 1000,
        "collector": {
            "kind": "native_etw",
            "preset": "system_overview"
        },
        "collectors": [
            {
                "kind": "native_etw",
                "preset": "system_overview"
            }
        ]
    });

    let error = decode_arguments::<RunProfileRequest>(value).expect_err("reject both forms");
    assert!(error.to_string().contains("collector and collectors cannot both be set"));
}
```

- [ ] **Step 2: Run failing MCP tests**

Run:

```powershell
cargo test -p dbgflow-mcp tools::tests::run_profile_arguments_decode_legacy_collector_to_collectors tools::tests::run_profile_arguments_decode_procmon_collectors_array tools::tests::run_profile_arguments_reject_both_collector_forms
```

Expected: fail because `RunProfileRequest.collectors` and custom normalization do not exist.

- [ ] **Step 3: Implement request normalization**

Use an internal serde helper:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct RawRunProfileRequest {
    target: dbgflow_core::profile::ProfileTarget,
    timeout_ms: u64,
    collector: Option<ProfileCollectorConfig>,
    #[serde(default)]
    collectors: Vec<ProfileCollectorConfig>,
}

impl<'de> Deserialize<'de> for RunProfileRequest {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawRunProfileRequest::deserialize(deserializer)?;
        if raw.collector.is_some() && !raw.collectors.is_empty() {
            return Err(serde::de::Error::custom(
                "collector and collectors cannot both be set",
            ));
        }
        let collectors = match raw.collector {
            Some(collector) => vec![collector],
            None if raw.collectors.is_empty() => vec![ProfileCollectorConfig::default()],
            None => raw.collectors,
        };
        Ok(Self {
            target: raw.target,
            timeout_ms: raw.timeout_ms,
            collectors,
        })
    }
}
```

Change `RunProfileRequest` and conversion:

```rust
pub struct RunProfileRequest {
    pub target: dbgflow_core::profile::ProfileTarget,
    pub timeout_ms: u64,
    pub collectors: Vec<ProfileCollectorConfig>,
}

impl From<RunProfileRequest> for RunProfile {
    fn from(value: RunProfileRequest) -> Self {
        Self {
            target: value.target,
            timeout_ms: value.timeout_ms,
            collectors: value.collectors,
        }
    }
}
```

- [ ] **Step 4: Update schema**

In `tool_descriptors`, replace the `run_profile` collector schema with `oneOf` support for `collector` or `collectors`. Include Procmon schema:

```json
{
  "kind": { "type": "string", "const": "procmon" },
  "capture_stacks": { "type": "boolean" },
  "filters": {
    "type": "object",
    "properties": {
      "operations": { "type": "array", "items": { "type": "string" } },
      "paths": { "type": "array", "items": { "type": "string" } }
    },
    "additionalProperties": false
  }
}
```

- [ ] **Step 5: Run MCP tests**

Run:

```powershell
cargo test -p dbgflow-mcp tools::tests
```

Expected: pass.

- [ ] **Step 6: Commit**

```powershell
git add crates/dbgflow-mcp/src/tools.rs
git commit -m "Accept parallel profile collectors in MCP schema"
```

### Task 5: Runtime Sysinternals Configuration

**Files:**
- Modify: `crates/dbgflow-core/src/profile/procmon.rs`
- Modify: `crates/dbgflow-core/src/profile/mod.rs`
- Modify: `crates/dbgflow-mcp/src/runtime.rs`
- Modify: `crates/dbgflow-mcp/src/mcp.rs`
- Modify: `crates/dbgflow-mcp/src/service.rs`
- Test: `crates/dbgflow-mcp/src/runtime.rs`

- [ ] **Step 1: Write failing runtime tests**

Add tests:

```rust
#[test]
fn parses_sysinternals_dir_for_http_runtime() {
    let root = std::env::temp_dir().join(format!("dbgflow-sysinternals-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create sysinternals dir");

    let config = parse_options_with_env(
        [
            OsString::from("--data-dir"),
            OsString::from(".\\var"),
            OsString::from("--sysinternals-dir"),
            root.as_os_str().to_os_string(),
        ],
        &env(&[]),
    )
    .expect("parse options");

    assert_eq!(config.sysinternals_dir.as_deref(), Some(root.as_path()));
}

#[test]
fn service_install_launch_arguments_include_sysinternals_dir_when_configured() {
    let config = parse_service_install_options([
        OsString::from("--install-root=C:\\dbgflow"),
        OsString::from("--sysinternals-dir=C:\\Tools\\Sysinternals"),
    ])
    .expect("parse service install options");

    assert!(config
        .service_launch_arguments()
        .windows(2)
        .any(|pair| pair[0] == "--sysinternals-dir"
            && pair[1] == OsString::from("C:\\Tools\\Sysinternals")));
}
```

- [ ] **Step 2: Run failing runtime tests**

Run:

```powershell
cargo test -p dbgflow-mcp runtime::tests::parses_sysinternals_dir_for_http_runtime runtime::tests::service_install_launch_arguments_include_sysinternals_dir_when_configured
```

Expected: fail because `sysinternals_dir` is not parsed or stored.

- [ ] **Step 3: Add core Procmon runtime options**

Create `crates/dbgflow-core/src/profile/procmon.rs` with:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProcmonRuntime {
    sysinternals_dir: Option<PathBuf>,
}

impl ProcmonRuntime {
    pub fn unavailable() -> Self {
        Self {
            sysinternals_dir: None,
        }
    }

    pub fn with_sysinternals_dir(path: PathBuf) -> Self {
        Self {
            sysinternals_dir: Some(path),
        }
    }

    pub fn procmon_exe(&self) -> Result<PathBuf> {
        let dir = self.sysinternals_dir.as_ref().ok_or_else(|| {
            DbgFlowError::Backend(
                "procmon collector requires service --sysinternals-dir".to_string(),
            )
        })?;
        for name in ["Procmon64.exe", "Procmon.exe"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
        Err(DbgFlowError::Backend(format!(
            "procmon collector requires Procmon64.exe or Procmon.exe under {}",
            dir.display()
        )))
    }
}

impl From<Option<PathBuf>> for ProcmonRuntime {
    fn from(value: Option<PathBuf>) -> Self {
        match value {
            Some(path) => Self::with_sysinternals_dir(path),
            None => Self::unavailable(),
        }
    }
}
```

Export `ProcmonRuntime` from `profile/mod.rs`.

- [ ] **Step 4: Parse `--sysinternals-dir`**

Add to `AppConfig`:

```rust
pub sysinternals_dir: Option<PathBuf>,
```

Add to `ServiceInstallConfig`:

```rust
pub sysinternals_dir: Option<PathBuf>,
```

Parse `--sysinternals-dir` and `--sysinternals-dir=<path>` in http, service run, and service install. Validation:

```rust
fn parse_existing_dir(value: &str, option: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(parse_non_empty(value, option)?);
    if !path.is_dir() {
        return Err(format!("{option} must point to an existing directory: {}", path.display()));
    }
    Ok(path)
}
```

- [ ] **Step 5: Inject runtime into server**

Add a server constructor:

```rust
pub fn server_with_data_dir_proxy_sysinternals_and_logger(
    data_dir: impl Into<PathBuf>,
    proxy: ProxyEnvironment,
    sysinternals_dir: Option<PathBuf>,
    logger: Arc<dyn LogSink>,
) -> McpServer {
    let data_dir = data_dir.into();
    let artifact_root = data_dir.join("artifacts");
    let sessions = dbgflow_core::session::SessionManager::with_worker_launcher_proxy_and_logger(
        default_process_worker_launcher(),
        &artifact_root,
        proxy,
        logger,
    );
    let profiles = ProfileManager::with_runtime(
        &artifact_root,
        dbgflow_core::profile::ProcmonRuntime::from(sysinternals_dir),
    );
    McpServer::new(ToolService::with_profiles(sessions, profiles))
}
```

Use this constructor from http and service paths.

- [ ] **Step 6: Run runtime tests**

Run:

```powershell
cargo test -p dbgflow-mcp runtime::tests
```

Expected: pass.

- [ ] **Step 7: Commit**

```powershell
git add crates/dbgflow-core/src/profile/procmon.rs crates/dbgflow-core/src/profile/mod.rs crates/dbgflow-mcp/src/runtime.rs crates/dbgflow-mcp/src/mcp.rs crates/dbgflow-mcp/src/service.rs
git commit -m "Add sysinternals runtime configuration"
```

### Task 6: Procmon Collector Minimal Implementation

**Files:**
- Modify: `crates/dbgflow-core/src/profile/procmon.rs`
- Modify: `crates/dbgflow-core/src/profile/native_etw.rs`
- Modify: `crates/dbgflow-core/src/profile/manager.rs`
- Test: `crates/dbgflow-core/tests/profile_lifecycle.rs`
- Test: `crates/dbgflow-core/tests/procmon_profile.rs`

- [ ] **Step 1: Write failing Procmon resolution tests**

Add unit tests in `procmon.rs`:

```rust
#[test]
fn procmon_runtime_prefers_procmon64() {
    let root = std::env::temp_dir().join(format!("dbgflow-procmon-runtime-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create root");
    std::fs::write(root.join("Procmon.exe"), b"exe").expect("write procmon");
    std::fs::write(root.join("Procmon64.exe"), b"exe").expect("write procmon64");

    let runtime = ProcmonRuntime::with_sysinternals_dir(root.clone());

    assert_eq!(
        runtime.procmon_exe().expect("resolve procmon"),
        root.join("Procmon64.exe")
    );
}

#[test]
fn procmon_runtime_without_sysinternals_dir_is_unavailable() {
    let error = ProcmonRuntime::unavailable()
        .procmon_exe()
        .expect_err("procmon unavailable");

    assert!(error.to_string().contains("--sysinternals-dir"));
}
```

- [ ] **Step 2: Run failing Procmon tests**

Run:

```powershell
cargo test -p dbgflow-core profile::procmon::tests::procmon_runtime_prefers_procmon64 profile::procmon::tests::procmon_runtime_without_sysinternals_dir_is_unavailable
```

Expected: fail until `ProcmonRuntime` is complete.

- [ ] **Step 3: Implement default collector factory dispatch**

Create a factory that owns Procmon runtime:

```rust
pub struct DefaultProfileCollectorFactory {
    procmon: ProcmonRuntime,
}

impl DefaultProfileCollectorFactory {
    pub fn new(procmon: ProcmonRuntime) -> Self {
        Self { procmon }
    }
}

impl CollectorFactory for DefaultProfileCollectorFactory {
    fn create(
        &self,
        config: &ProfileCollectorConfig,
        output_dir: &Path,
    ) -> Result<Box<dyn ProfileCollector>> {
        match config {
            ProfileCollectorConfig::NativeEtw { .. } => {
                NativeEtwCollectorFactory.create(config, output_dir)
            }
            ProfileCollectorConfig::Procmon { .. } => {
                ProcmonCollectorFactory::new(self.procmon.clone()).create(config, output_dir)
            }
        }
    }
}
```

Use `DefaultProfileCollectorFactory` in `ProfileManager::new` and `ProfileManager::with_runtime`.

- [ ] **Step 4: Implement Procmon command runner**

Use a small command abstraction for tests:

```rust
trait ProcmonCommandRunner: Send + Sync {
    fn run(&self, exe: &Path, args: &[OsString]) -> Result<()>;
}

struct ProcessProcmonCommandRunner;

impl ProcmonCommandRunner for ProcessProcmonCommandRunner {
    fn run(&self, exe: &Path, args: &[OsString]) -> Result<()> {
        let output = std::process::Command::new(exe)
            .args(args)
            .output()
            .map_err(|error| DbgFlowError::Backend(format!("start procmon: {error}")))?;
        if output.status.success() {
            return Ok(());
        }
        Err(DbgFlowError::Backend(format!(
            "procmon failed with exit code {:?}: {}{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )))
    }
}
```

- [ ] **Step 5: Implement minimal Procmon collector**

Use these command lines:

```rust
// start
[
    OsString::from("/AcceptEula"),
    OsString::from("/Quiet"),
    OsString::from("/Minimized"),
    OsString::from("/BackingFile"),
    self.pml_path.as_os_str().to_os_string(),
]

// stop
[
    OsString::from("/AcceptEula"),
    OsString::from("/Terminate"),
]
```

Return artifacts:

```rust
vec![
    ArtifactRef {
        kind: ArtifactKind::ProfileCollectorTrace,
        path: self.pml_path.clone(),
    },
    ArtifactRef {
        kind: ArtifactKind::ProfileCollectorSummary,
        path: self.summary_path.clone(),
    },
]
```

Write `summary.json` after stop with requested filters, pml path, capture stack flag, and a note that PML is the authoritative artifact.

- [ ] **Step 6: Add ignored live integration test**

Create `crates/dbgflow-core/tests/procmon_profile.rs`:

```rust
#[cfg(windows)]
#[test]
#[ignore = "requires Sysinternals Procmon and elevated/service-capable local environment"]
fn procmon_run_profile_writes_pml_for_cmd() {
    let sysinternals_dir = std::env::var_os("DBGFLOW_SYSINTERNALS_DIR")
        .map(std::path::PathBuf::from)
        .expect("set DBGFLOW_SYSINTERNALS_DIR to a Sysinternals directory");
    let root = std::env::temp_dir().join(format!("dbgflow-procmon-profile-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create root");

    let manager = ProfileManager::with_runtime(
        &root,
        dbgflow_core::profile::ProcmonRuntime::with_sysinternals_dir(sysinternals_dir),
    );
    let result = manager
        .run_profile(RunProfile {
            target: ProfileTarget::Launch {
                executable: std::path::PathBuf::from("C:\\Windows\\System32\\cmd.exe"),
                args: vec!["/C".to_string(), "echo dbgflow-procmon".to_string()],
            },
            timeout_ms: 10_000,
            collectors: vec![ProfileCollectorConfig::Procmon {
                capture_stacks: false,
                filters: Default::default(),
            }],
        })
        .expect("run profile");

    assert_eq!(result.status, ProfileStatus::Completed);
    let procmon = result
        .collector_results
        .iter()
        .find(|collector| collector.kind == ProfileCollectorKind::Procmon)
        .expect("procmon result");
    assert!(procmon.artifacts.iter().any(|artifact| artifact.path.extension().is_some_and(|ext| ext == "pml")));
}
```

- [ ] **Step 7: Run unit tests**

Run:

```powershell
cargo test -p dbgflow-core --test profile_lifecycle
cargo test -p dbgflow-core profile::procmon::tests
```

Expected: pass.

- [ ] **Step 8: Commit**

```powershell
git add crates/dbgflow-core/src/profile/procmon.rs crates/dbgflow-core/src/profile/native_etw.rs crates/dbgflow-core/src/profile/manager.rs crates/dbgflow-core/tests/profile_lifecycle.rs crates/dbgflow-core/tests/procmon_profile.rs
git commit -m "Add procmon profile collector"
```

### Task 7: Install Script Sysinternals Prompt

**Files:**
- Modify: `scripts/install-service.ps1`
- Test: manual PowerShell dry-run inspection

- [ ] **Step 1: Add helper functions**

Add functions:

```powershell
function Find-SysinternalsDir {
    param([string]$RepoRoot)
    $candidates = @(
        (Join-Path (Split-Path -Parent $RepoRoot) "Sysinternals"),
        "C:\Tools\Sysinternals",
        "C:\Sysinternals",
        "C:\Program Files\Sysinternals"
    )
    foreach ($candidate in $candidates) {
        if (Test-SysinternalsDir -Path $candidate) {
            return (Resolve-Path -LiteralPath $candidate).Path
        }
    }
    return $null
}

function Test-SysinternalsDir {
    param([AllowNull()][string]$Path)
    if ([string]::IsNullOrWhiteSpace($Path)) {
        return $false
    }
    if (-not (Test-Path -LiteralPath $Path -PathType Container)) {
        return $false
    }
    return (
        (Test-Path -LiteralPath (Join-Path $Path "Procmon64.exe") -PathType Leaf) -or
        (Test-Path -LiteralPath (Join-Path $Path "Procmon.exe") -PathType Leaf)
    )
}

function Confirm-SysinternalsDir {
    param([Parameter(Mandatory = $true)][string]$Path)
    $answer = Read-Host "Use Sysinternals directory '$Path' for optional Procmon features? [Y/n]"
    return [string]::IsNullOrWhiteSpace($answer) -or $answer -match '^(y|yes)$'
}
```

- [ ] **Step 2: Add parameter and elevation propagation**

Add parameter:

```powershell
[string]$SysinternalsDir
```

When re-launching elevated, append `-SysinternalsDir` only when the user explicitly supplied it.

- [ ] **Step 3: Resolve optional Sysinternals dir**

Before building `$arguments`:

```powershell
$resolvedSysinternalsDir = $null
if ($PSBoundParameters.ContainsKey("SysinternalsDir")) {
    if (-not (Test-SysinternalsDir -Path $SysinternalsDir)) {
        throw "SysinternalsDir must contain Procmon64.exe or Procmon.exe: $SysinternalsDir"
    }
    $resolvedSysinternalsDir = (Resolve-Path -LiteralPath $SysinternalsDir).Path
}
else {
    $candidateSysinternalsDir = Find-SysinternalsDir -RepoRoot $RepoRoot
    if ($candidateSysinternalsDir -and (Confirm-SysinternalsDir -Path $candidateSysinternalsDir)) {
        $resolvedSysinternalsDir = $candidateSysinternalsDir
    }
}
```

Append service install args:

```powershell
if ($resolvedSysinternalsDir) {
    $arguments += @("--sysinternals-dir", $resolvedSysinternalsDir)
}
```

- [ ] **Step 4: Run syntax check**

Run:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -Command "$null = [scriptblock]::Create((Get-Content .\scripts\install-service.ps1 -Raw)); 'syntax ok'"
```

Expected: prints `syntax ok`.

- [ ] **Step 5: Commit**

```powershell
git add scripts/install-service.ps1
git commit -m "Prompt for sysinternals directory during service install"
```

### Task 8: Documentation, Verification, and Review

**Files:**
- Modify: `README.md`
- Modify: `README.zh-CN.md`
- Modify: `GOALS.md`

- [ ] **Step 1: Update docs**

Document:

```text
run_profile accepts collectors[] for parallel collection.
collector remains accepted for compatibility.
native_etw/system_overview remains the default collector.
procmon is optional and requires service --sysinternals-dir.
The install script detects Sysinternals interactively and skips Procmon features when not configured.
```

- [ ] **Step 2: Run formatting**

Run:

```powershell
cargo fmt --all -- --check
```

Expected: exit code 0.

- [ ] **Step 3: Run tests**

Run:

```powershell
cargo test
```

Expected: exit code 0; ignored Procmon live test is not run by default.

- [ ] **Step 4: Run build**

Run:

```powershell
cargo build -p dbgflow-mcp --release
```

Expected: exit code 0.

- [ ] **Step 5: Optional live Procmon verification**

If `DBGFLOW_SYSINTERNALS_DIR` points to a directory containing `Procmon64.exe` or `Procmon.exe`, run:

```powershell
cargo test -p dbgflow-core --test procmon_profile -- --ignored --nocapture
```

Expected: test passes and writes a `.pml` artifact. If the local machine lacks Procmon or permissions, record that this verification was not run.

- [ ] **Step 6: Commit docs**

```powershell
git add README.md README.zh-CN.md GOALS.md
git commit -m "Document parallel profile collectors"
```

- [ ] **Step 7: Independent review**

Dispatch an independent review subagent or run a separate review pass focused on:

```text
collector lifecycle cleanup on partial failure
Procmon unavailability errors before target launch
no standalone procmon path accepted
service launch args include only --sysinternals-dir
old collector input remains accepted
```

Fix review findings with focused commits.

---

## Self-Review

- Spec coverage: covered multi-collector input, old `collector` compatibility, `--sysinternals-dir` only, install prompt behavior, Procmon unavailability, artifacts, errors, docs, and live verification.
- Placeholder scan: no task contains unresolved placeholders or future-only implementation instructions.
- Type consistency: `ProfileCollectorConfig`, `ProcmonRuntime`, `ProfileCollectorResult`, and `collectors` names are used consistently across tasks.
