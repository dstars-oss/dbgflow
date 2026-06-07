# Native ETW `run_profile` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a launch-only `run_profile` MCP tool that records a native ETW `.etl` trace in one automatically-cleaned-up profile job.

**Architecture:** Add a new `dbgflow-core::profile` subsystem separate from `DebugBackend`. `ProfileManager` validates the launch target, enforces one active native ETW job at a time, creates profile artifacts, runs a collector, launches the target, waits for target exit or timeout, stops collection, and returns artifact references. `dbgflow-mcp::tools` exposes this through one thin `run_profile` facade.

**Tech Stack:** Rust 2021, `serde`, `serde_json`, `uuid`, `thiserror`, existing `windows` crate, Windows ETW controller APIs (`StartTraceW`, `EnableTraceEx2`, `ControlTraceW`), existing cargo test/fmt workflow.

---

## File Structure

- Create: `crates/dbgflow-core/src/profile/mod.rs`
  - Public profile API exports and module wiring.
- Create: `crates/dbgflow-core/src/profile/id.rs`
  - `ProfileId` newtype matching `SessionId` style.
- Create: `crates/dbgflow-core/src/profile/model.rs`
  - Request/result/state structs: `RunProfile`, `ProfileTarget`, `ProfileCollectorConfig`, `ProfileResult`, `ProfileArtifacts`, `ProfileStatus`, `ProfileCompletionReason`.
- Create: `crates/dbgflow-core/src/profile/collector.rs`
  - `ProfileCollector` trait, `CollectorStart`, `CollectorStop`, `CollectorFactory`, test fake collector.
- Create: `crates/dbgflow-core/src/profile/target.rs`
  - Launch target validation and target process running.
- Create: `crates/dbgflow-core/src/profile/manager.rs`
  - `ProfileManager` orchestration, concurrency guard, artifact writes.
- Create: `crates/dbgflow-core/src/profile/native_etw.rs`
  - Windows native ETW collector plus non-Windows unsupported stub.
- Modify: `crates/dbgflow-core/src/lib.rs`
  - Export `profile`.
- Modify: `crates/dbgflow-core/src/artifacts/manager.rs`
  - Add profile artifact helpers and `ArtifactKind` variants.
- Modify: `crates/dbgflow-core/Cargo.toml`
  - Add Windows ETW feature flags to the `windows` dependency.
- Create: `crates/dbgflow-core/tests/profile_lifecycle.rs`
  - Unit-style lifecycle tests using fake collector and fake target runner.
- Create: `crates/dbgflow-core/tests/native_etw_profile.rs`
  - Windows-only ignored integration smoke test for real ETW.
- Modify: `crates/dbgflow-mcp/src/tools.rs`
  - Add `run_profile` descriptor, request decoding, and facade method.
- Modify: `crates/dbgflow-mcp/src/service.rs` if needed
  - Construct `ToolService` with a `ProfileManager` if constructor changes require it.
- Create or modify: `crates/dbgflow-mcp/tests/...`
  - Add MCP-facing schema/call tests if existing test layout already covers tools; otherwise add focused unit tests under `tools.rs`.
- Modify: `README.md`, `README.zh-CN.md`, `GOALS.md`
  - Document `run_profile`, V1 limits, ETL artifact semantics, and roadmap status.

---

### Task 1: Add Profile Artifact Support

**Files:**
- Modify: `crates/dbgflow-core/src/artifacts/manager.rs`
- Test: `crates/dbgflow-core/src/artifacts/manager.rs`

- [ ] **Step 1: Write failing artifact tests**

Add this test module at the end of `crates/dbgflow-core/src/artifacts/manager.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::ProfileId;
    use serde_json::json;

    #[test]
    fn profile_artifacts_are_initialized_under_profiles_directory() {
        let root = std::env::temp_dir().join(format!(
            "dbgflow-profile-artifacts-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let artifacts = ArtifactManager::new(&root);
        let profile_id = ProfileId::new();

        let dir = artifacts
            .initialize_profile_artifacts(profile_id)
            .expect("initialize profile artifacts");

        assert_eq!(dir, root.join("profiles").join(profile_id.to_string()));
        assert!(dir.join("events.jsonl").is_file());
        assert!(dir.join("target").is_dir());
        assert_eq!(
            artifacts.profile_trace_path(profile_id),
            dir.join("trace.etl")
        );
        assert_eq!(
            artifacts.profile_metadata_path(profile_id),
            dir.join("profile.json")
        );
    }

    #[test]
    fn profile_event_and_metadata_are_written() {
        let root = std::env::temp_dir().join(format!(
            "dbgflow-profile-event-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let artifacts = ArtifactManager::new(&root);
        let profile_id = ProfileId::new();
        artifacts
            .initialize_profile_artifacts(profile_id)
            .expect("initialize profile artifacts");

        artifacts
            .append_profile_event(
                profile_id,
                &ProfileArtifactEvent {
                    timestamp_unix_ms: 1,
                    event: "profile_created".to_string(),
                    profile_id: profile_id.to_string(),
                    artifact_path: None,
                    error: None,
                    fields: Map::new(),
                },
            )
            .expect("append profile event");
        artifacts
            .write_profile_metadata(profile_id, &json!({"status": "completed"}))
            .expect("write metadata");

        let dir = root.join("profiles").join(profile_id.to_string());
        let events = std::fs::read_to_string(dir.join("events.jsonl")).expect("read events");
        assert!(events.contains("profile_created"));
        let metadata = std::fs::read_to_string(dir.join("profile.json")).expect("read metadata");
        assert!(metadata.contains("completed"));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run:

```powershell
cargo test -p dbgflow-core profile_artifacts_are_initialized_under_profiles_directory profile_event_and_metadata_are_written
```

Expected: FAIL because `crate::profile`, `ProfileId`, `initialize_profile_artifacts`, `profile_trace_path`, `profile_metadata_path`, `append_profile_event`, `write_profile_metadata`, and `ProfileArtifactEvent` do not exist.

- [ ] **Step 3: Add `ProfileId` and profile module export**

Create `crates/dbgflow-core/src/profile/mod.rs`:

```rust
pub mod id;

pub use id::ProfileId;
```

Create `crates/dbgflow-core/src/profile/id.rs`:

```rust
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProfileId(Uuid);

impl ProfileId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ProfileId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ProfileId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for ProfileId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(value).map(Self)
    }
}
```

Modify `crates/dbgflow-core/src/lib.rs`:

```rust
pub mod artifacts;
pub mod backend;
pub mod error;
pub mod logging;
pub mod profile;
pub mod proxy;
pub mod session;

pub use error::{DbgFlowError, Result};
```

- [ ] **Step 4: Add profile artifact helpers**

Modify imports at the top of `crates/dbgflow-core/src/artifacts/manager.rs`:

```rust
use crate::profile::ProfileId;
use crate::session::SessionId;
```

Add these methods inside `impl ArtifactManager`:

```rust
pub fn ensure_profile_dir(&self, profile_id: ProfileId) -> Result<PathBuf> {
    let _guard = self
        .lock
        .lock()
        .map_err(|_| DbgFlowError::Artifact("artifact manager lock poisoned".to_string()))?;
    self.ensure_profile_dir_unlocked(profile_id)
}

pub fn initialize_profile_artifacts(&self, profile_id: ProfileId) -> Result<PathBuf> {
    let _guard = self
        .lock
        .lock()
        .map_err(|_| DbgFlowError::Artifact("artifact manager lock poisoned".to_string()))?;
    let dir = self.ensure_profile_dir_unlocked(profile_id)?;
    fs::create_dir_all(dir.join("target"))
        .map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
    touch(&dir.join("events.jsonl"))?;
    Ok(dir)
}

pub fn profile_trace_path(&self, profile_id: ProfileId) -> PathBuf {
    self.root
        .join("profiles")
        .join(profile_id.to_string())
        .join("trace.etl")
}

pub fn profile_metadata_path(&self, profile_id: ProfileId) -> PathBuf {
    self.root
        .join("profiles")
        .join(profile_id.to_string())
        .join("profile.json")
}

pub fn profile_stdout_path(&self, profile_id: ProfileId) -> PathBuf {
    self.root
        .join("profiles")
        .join(profile_id.to_string())
        .join("target")
        .join("stdout.txt")
}

pub fn profile_stderr_path(&self, profile_id: ProfileId) -> PathBuf {
    self.root
        .join("profiles")
        .join(profile_id.to_string())
        .join("target")
        .join("stderr.txt")
}

pub fn append_profile_event(
    &self,
    profile_id: ProfileId,
    event: &ProfileArtifactEvent,
) -> Result<()> {
    let _guard = self
        .lock
        .lock()
        .map_err(|_| DbgFlowError::Artifact("artifact manager lock poisoned".to_string()))?;
    let profile_dir = self.ensure_profile_dir_unlocked(profile_id)?;
    let line = serde_json::to_string(event)
        .map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
    append_jsonl(&profile_dir.join("events.jsonl"), &line)
}

pub fn write_profile_metadata(
    &self,
    profile_id: ProfileId,
    metadata: &Value,
) -> Result<ArtifactRef> {
    let _guard = self
        .lock
        .lock()
        .map_err(|_| DbgFlowError::Artifact("artifact manager lock poisoned".to_string()))?;
    let profile_dir = self.ensure_profile_dir_unlocked(profile_id)?;
    let metadata_path = profile_dir.join("profile.json");
    let text = serde_json::to_string_pretty(metadata)
        .map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
    fs::write(&metadata_path, text)
        .map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
    Ok(ArtifactRef {
        kind: ArtifactKind::ProfileMetadata,
        path: metadata_path,
    })
}

fn ensure_profile_dir_unlocked(&self, profile_id: ProfileId) -> Result<PathBuf> {
    let dir = self.root.join("profiles").join(profile_id.to_string());
    fs::create_dir_all(&dir).map_err(|error| DbgFlowError::Artifact(error.to_string()))?;
    Ok(dir)
}
```

Add variants to `ArtifactKind`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArtifactKind {
    CommandOutput,
    ProfileTrace,
    ProfileMetadata,
    ProfileEvents,
    ProfileStdout,
    ProfileStderr,
}
```

Add this event type near `SessionArtifactEvent`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileArtifactEvent {
    pub timestamp_unix_ms: u128,
    pub event: String,
    pub profile_id: String,
    pub artifact_path: Option<PathBuf>,
    pub error: Option<String>,
    pub fields: Map<String, Value>,
}
```

- [ ] **Step 5: Run artifact tests**

Run:

```powershell
cargo test -p dbgflow-core profile_artifacts_are_initialized_under_profiles_directory profile_event_and_metadata_are_written
```

Expected: PASS.

- [ ] **Step 6: Commit**

```powershell
git add crates/dbgflow-core/src/lib.rs crates/dbgflow-core/src/profile/mod.rs crates/dbgflow-core/src/profile/id.rs crates/dbgflow-core/src/artifacts/manager.rs
git commit -m "feat: add profile artifact support"
```

---

### Task 2: Add Profile Models and Validation

**Files:**
- Create: `crates/dbgflow-core/src/profile/model.rs`
- Create: `crates/dbgflow-core/src/profile/target.rs`
- Modify: `crates/dbgflow-core/src/profile/mod.rs`
- Test: `crates/dbgflow-core/tests/profile_lifecycle.rs`

- [ ] **Step 1: Write failing validation tests**

Create `crates/dbgflow-core/tests/profile_lifecycle.rs`:

```rust
use dbgflow_core::profile::{
    validate_profile_target, ProfileCollectorConfig, ProfileCollectorKind, ProfilePreset,
    ProfileTarget,
};
use std::fs;

#[test]
fn profile_launch_target_rejects_missing_executable() {
    let missing = std::env::temp_dir()
        .join(format!("dbgflow-profile-missing-{}", std::process::id()))
        .join("missing.exe");

    let error = validate_profile_target(ProfileTarget::Launch {
        executable: missing,
        args: Vec::new(),
    })
    .expect_err("reject missing executable");

    assert!(error.to_string().contains("invalid profile launch executable"));
}

#[test]
fn profile_launch_target_canonicalizes_existing_executable_and_rejects_nul_args() {
    let root = std::env::temp_dir().join(format!(
        "dbgflow-profile-target-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("create root");
    let executable = root.join("target.exe");
    fs::write(&executable, b"not a real exe").expect("write target");

    let target = validate_profile_target(ProfileTarget::Launch {
        executable: executable.clone(),
        args: vec!["--case".to_string(), "1".to_string()],
    })
    .expect("validate target");

    let ProfileTarget::Launch {
        executable: validated,
        args,
    } = target;
    assert_eq!(validated, executable.canonicalize().expect("canonicalize"));
    assert_eq!(args, vec!["--case".to_string(), "1".to_string()]);

    let error = validate_profile_target(ProfileTarget::Launch {
        executable,
        args: vec!["bad\0arg".to_string()],
    })
    .expect_err("reject nul argument");
    assert!(error.to_string().contains("NUL"));
}

#[test]
fn default_profile_collector_is_native_etw_system_overview() {
    let config = ProfileCollectorConfig::default();

    assert_eq!(config.kind, ProfileCollectorKind::NativeEtw);
    assert_eq!(config.preset, ProfilePreset::SystemOverview);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run:

```powershell
cargo test -p dbgflow-core profile_launch_target_rejects_missing_executable profile_launch_target_canonicalizes_existing_executable_and_rejects_nul_args default_profile_collector_is_native_etw_system_overview
```

Expected: FAIL because profile model and validation functions do not exist.

- [ ] **Step 3: Add profile model types**

Create `crates/dbgflow-core/src/profile/model.rs`:

```rust
use super::ProfileId;
use crate::artifacts::ArtifactRef;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunProfile {
    pub target: ProfileTarget,
    pub timeout_ms: u64,
    pub collector: ProfileCollectorConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProfileTarget {
    Launch { executable: PathBuf, args: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileCollectorConfig {
    pub kind: ProfileCollectorKind,
    pub preset: ProfilePreset,
}

impl Default for ProfileCollectorConfig {
    fn default() -> Self {
        Self {
            kind: ProfileCollectorKind::NativeEtw,
            preset: ProfilePreset::SystemOverview,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileCollectorKind {
    NativeEtw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfilePreset {
    SystemOverview,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileStatus {
    Completed,
    TimedOut,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileCompletionReason {
    TargetExited,
    Timeout,
    TargetLaunchFailed,
    CollectorError,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileArtifacts {
    pub trace: ArtifactRef,
    pub profile: ArtifactRef,
    pub events: ArtifactRef,
    pub stdout: ArtifactRef,
    pub stderr: ArtifactRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileResult {
    pub profile_id: ProfileId,
    pub status: ProfileStatus,
    pub completion_reason: ProfileCompletionReason,
    pub target_pid: Option<u32>,
    pub target_exit_code: Option<i32>,
    pub duration_ms: u128,
    pub artifacts: ProfileArtifacts,
    pub warnings: Vec<String>,
    pub error: Option<String>,
}
```

- [ ] **Step 4: Add target validation**

Create `crates/dbgflow-core/src/profile/target.rs`:

```rust
use super::ProfileTarget;
use crate::{DbgFlowError, Result};
use std::path::Path;

pub fn validate_profile_target(target: ProfileTarget) -> Result<ProfileTarget> {
    match target {
        ProfileTarget::Launch { executable, args } => {
            validate_launch_target(&executable, args)
        }
    }
}

fn validate_launch_target(executable: &Path, args: Vec<String>) -> Result<ProfileTarget> {
    let executable = executable
        .canonicalize()
        .map_err(|error| DbgFlowError::Backend(format!("invalid profile launch executable: {error}")))?;
    if !executable.is_file() {
        return Err(DbgFlowError::Backend(format!(
            "profile launch executable is not a file: {}",
            executable.display()
        )));
    }
    if args.iter().any(|arg| arg.contains('\0')) {
        return Err(DbgFlowError::Backend(
            "profile launch arguments must not contain NUL bytes".to_string(),
        ));
    }
    Ok(ProfileTarget::Launch { executable, args })
}
```

Modify `crates/dbgflow-core/src/profile/mod.rs`:

```rust
pub mod id;
pub mod model;
pub mod target;

pub use id::ProfileId;
pub use model::{
    ProfileArtifacts, ProfileCollectorConfig, ProfileCollectorKind, ProfileCompletionReason,
    ProfilePreset, ProfileResult, ProfileStatus, ProfileTarget, RunProfile,
};
pub use target::validate_profile_target;
```

- [ ] **Step 5: Run validation tests**

Run:

```powershell
cargo test -p dbgflow-core profile_launch_target_rejects_missing_executable profile_launch_target_canonicalizes_existing_executable_and_rejects_nul_args default_profile_collector_is_native_etw_system_overview
```

Expected: PASS.

- [ ] **Step 6: Commit**

```powershell
git add crates/dbgflow-core/src/profile/mod.rs crates/dbgflow-core/src/profile/model.rs crates/dbgflow-core/src/profile/target.rs crates/dbgflow-core/tests/profile_lifecycle.rs
git commit -m "feat: add profile request model"
```

---

### Task 3: Add Collector and Target Runner Abstractions

**Files:**
- Create: `crates/dbgflow-core/src/profile/collector.rs`
- Modify: `crates/dbgflow-core/src/profile/target.rs`
- Modify: `crates/dbgflow-core/src/profile/mod.rs`
- Test: `crates/dbgflow-core/tests/profile_lifecycle.rs`

- [ ] **Step 1: Write failing fake collector and target runner tests**

Append to `crates/dbgflow-core/tests/profile_lifecycle.rs`:

```rust
use dbgflow_core::profile::{
    CollectorFactory, CollectorStart, CollectorStop, ProfileCollector, ProfileCollectorConfig,
    TargetExit, TargetRunner,
};
use dbgflow_core::Result;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[test]
fn fake_collector_records_start_and_stop_calls() {
    let state = Arc::new(Mutex::new(Vec::new()));
    let collector = TestCollector {
        state: state.clone(),
        fail_start: false,
        fail_stop: false,
    };
    let output_dir = std::env::temp_dir();

    let started = collector.start(&output_dir).expect("start collector");
    assert_eq!(started.warnings, Vec::<String>::new());
    let stopped = collector.stop().expect("stop collector");
    assert_eq!(stopped.warnings, Vec::<String>::new());

    assert_eq!(
        state.lock().expect("state").as_slice(),
        &["start".to_string(), "stop".to_string()]
    );
}

#[test]
fn target_runner_returns_exit_or_timeout_without_killing_target() {
    let runner = TestTargetRunner {
        exit: TargetExit::TimedOut { pid: 42 },
    };
    let exit = runner
        .launch_and_wait(
            &ProfileTarget::Launch {
                executable: std::env::current_exe().expect("current exe"),
                args: Vec::new(),
            },
            Duration::from_millis(1),
            Path::new("stdout.txt"),
            Path::new("stderr.txt"),
        )
        .expect("launch target");

    assert_eq!(exit, TargetExit::TimedOut { pid: 42 });
}

struct TestCollector {
    state: Arc<Mutex<Vec<String>>>,
    fail_start: bool,
    fail_stop: bool,
}

impl ProfileCollector for TestCollector {
    fn start(&self, _output_dir: &Path) -> Result<CollectorStart> {
        self.state.lock().expect("state").push("start".to_string());
        if self.fail_start {
            return Err(dbgflow_core::DbgFlowError::Backend("collector start failed".to_string()));
        }
        Ok(CollectorStart {
            warnings: Vec::new(),
        })
    }

    fn stop(&self) -> Result<CollectorStop> {
        self.state.lock().expect("state").push("stop".to_string());
        if self.fail_stop {
            return Err(dbgflow_core::DbgFlowError::Backend("collector stop failed".to_string()));
        }
        Ok(CollectorStop {
            warnings: Vec::new(),
        })
    }

    fn cleanup(&self) -> Result<()> {
        self.state.lock().expect("state").push("cleanup".to_string());
        Ok(())
    }
}

struct TestTargetRunner {
    exit: TargetExit,
}

impl TargetRunner for TestTargetRunner {
    fn launch_and_wait(
        &self,
        _target: &ProfileTarget,
        _timeout: Duration,
        _stdout_path: &Path,
        _stderr_path: &Path,
    ) -> Result<TargetExit> {
        Ok(self.exit)
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run:

```powershell
cargo test -p dbgflow-core fake_collector_records_start_and_stop_calls target_runner_returns_exit_or_timeout_without_killing_target
```

Expected: FAIL because collector and target runner traits do not exist.

- [ ] **Step 3: Add collector abstraction**

Create `crates/dbgflow-core/src/profile/collector.rs`:

```rust
use super::ProfileCollectorConfig;
use crate::Result;
use std::path::Path;

pub trait ProfileCollector: Send + Sync {
    fn start(&self, output_dir: &Path) -> Result<CollectorStart>;
    fn stop(&self) -> Result<CollectorStop>;
    fn cleanup(&self) -> Result<()>;
}

pub trait CollectorFactory: Send + Sync {
    fn create(&self, config: &ProfileCollectorConfig, trace_path: &Path)
        -> Result<Box<dyn ProfileCollector>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectorStart {
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectorStop {
    pub warnings: Vec<String>,
}
```

- [ ] **Step 4: Add target runner abstraction and real process runner**

Append to `crates/dbgflow-core/src/profile/target.rs`:

```rust
use std::fs::File;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetExit {
    Exited { pid: u32, exit_code: Option<i32> },
    TimedOut { pid: u32 },
}

pub trait TargetRunner: Send + Sync {
    fn launch_and_wait(
        &self,
        target: &ProfileTarget,
        timeout: Duration,
        stdout_path: &Path,
        stderr_path: &Path,
    ) -> Result<TargetExit>;
}

#[derive(Debug, Default)]
pub struct ProcessTargetRunner;

impl TargetRunner for ProcessTargetRunner {
    fn launch_and_wait(
        &self,
        target: &ProfileTarget,
        timeout: Duration,
        stdout_path: &Path,
        stderr_path: &Path,
    ) -> Result<TargetExit> {
        let ProfileTarget::Launch { executable, args } = target;
        let stdout = File::create(stdout_path)
            .map_err(|error| DbgFlowError::Artifact(format!("create target stdout failed: {error}")))?;
        let stderr = File::create(stderr_path)
            .map_err(|error| DbgFlowError::Artifact(format!("create target stderr failed: {error}")))?;
        let mut child = Command::new(executable)
            .args(args)
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .map_err(|error| DbgFlowError::Backend(format!("launch profile target failed: {error}")))?;
        let pid = child.id();
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = child
                .try_wait()
                .map_err(|error| DbgFlowError::Backend(format!("poll profile target failed: {error}")))?
            {
                return Ok(TargetExit::Exited {
                    pid,
                    exit_code: exit_code(status),
                });
            }
            if Instant::now() >= deadline {
                return Ok(TargetExit::TimedOut { pid });
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

fn exit_code(status: ExitStatus) -> Option<i32> {
    status.code()
}
```

Remove any duplicate `use std::path::Path;` import conflicts by merging imports at the top of `target.rs`:

```rust
use std::path::{Path, PathBuf};
```

If `PathBuf` is unused after implementation, remove it.

- [ ] **Step 5: Export traits**

Modify `crates/dbgflow-core/src/profile/mod.rs`:

```rust
pub mod collector;
pub mod id;
pub mod model;
pub mod target;

pub use collector::{CollectorFactory, CollectorStart, CollectorStop, ProfileCollector};
pub use id::ProfileId;
pub use model::{
    ProfileArtifacts, ProfileCollectorConfig, ProfileCollectorKind, ProfileCompletionReason,
    ProfilePreset, ProfileResult, ProfileStatus, ProfileTarget, RunProfile,
};
pub use target::{validate_profile_target, ProcessTargetRunner, TargetExit, TargetRunner};
```

- [ ] **Step 6: Run abstraction tests**

Run:

```powershell
cargo test -p dbgflow-core fake_collector_records_start_and_stop_calls target_runner_returns_exit_or_timeout_without_killing_target
```

Expected: PASS.

- [ ] **Step 7: Commit**

```powershell
git add crates/dbgflow-core/src/profile/collector.rs crates/dbgflow-core/src/profile/target.rs crates/dbgflow-core/src/profile/mod.rs crates/dbgflow-core/tests/profile_lifecycle.rs
git commit -m "feat: add profile collector abstractions"
```

---

### Task 4: Implement ProfileManager Orchestration with Fake Collector

**Files:**
- Create: `crates/dbgflow-core/src/profile/manager.rs`
- Modify: `crates/dbgflow-core/src/profile/mod.rs`
- Test: `crates/dbgflow-core/tests/profile_lifecycle.rs`

- [ ] **Step 1: Write failing lifecycle tests**

Append to `crates/dbgflow-core/tests/profile_lifecycle.rs`:

```rust
use dbgflow_core::artifacts::ArtifactKind;
use dbgflow_core::profile::{
    ProfileCompletionReason, ProfileManager, ProfileStatus, RunProfile,
};

#[test]
fn run_profile_starts_collector_launches_target_stops_collector_and_writes_artifacts() {
    let root = test_profile_root("completed");
    let collector_state = Arc::new(Mutex::new(Vec::new()));
    let manager = ProfileManager::with_components(
        &root,
        Arc::new(TestCollectorFactory {
            state: collector_state.clone(),
            fail_start: false,
            fail_stop: false,
        }),
        Arc::new(TestTargetRunner {
            exit: TargetExit::Exited {
                pid: 1234,
                exit_code: Some(7),
            },
        }),
    );

    let request = RunProfile {
        target: ProfileTarget::Launch {
            executable: std::env::current_exe().expect("current exe"),
            args: Vec::new(),
        },
        timeout_ms: 1000,
        collector: ProfileCollectorConfig::default(),
    };

    let result = manager.run_profile(request).expect("run profile");

    assert_eq!(result.status, ProfileStatus::Completed);
    assert_eq!(result.completion_reason, ProfileCompletionReason::TargetExited);
    assert_eq!(result.target_pid, Some(1234));
    assert_eq!(result.target_exit_code, Some(7));
    assert_eq!(result.artifacts.trace.kind, ArtifactKind::ProfileTrace);
    assert!(result.artifacts.profile.path.is_file());
    assert!(result.artifacts.events.path.is_file());
    assert!(result.artifacts.stdout.path.is_file());
    assert!(result.artifacts.stderr.path.is_file());
    assert_eq!(
        collector_state.lock().expect("state").as_slice(),
        &["start".to_string(), "stop".to_string()]
    );

    let metadata =
        fs::read_to_string(&result.artifacts.profile.path).expect("read profile metadata");
    assert!(metadata.contains("target_exited"));
    let events = fs::read_to_string(&result.artifacts.events.path).expect("read events");
    assert!(events.contains("collector_started"));
    assert!(events.contains("target_started"));
    assert!(events.contains("collector_stopped"));
    assert!(events.contains("profile_completed"));
}

#[test]
fn run_profile_timeout_stops_collector_without_target_exit_code() {
    let root = test_profile_root("timed-out");
    let manager = ProfileManager::with_components(
        &root,
        Arc::new(TestCollectorFactory::default()),
        Arc::new(TestTargetRunner {
            exit: TargetExit::TimedOut { pid: 88 },
        }),
    );

    let result = manager
        .run_profile(RunProfile {
            target: ProfileTarget::Launch {
                executable: std::env::current_exe().expect("current exe"),
                args: Vec::new(),
            },
            timeout_ms: 1,
            collector: ProfileCollectorConfig::default(),
        })
        .expect("run profile");

    assert_eq!(result.status, ProfileStatus::TimedOut);
    assert_eq!(result.completion_reason, ProfileCompletionReason::Timeout);
    assert_eq!(result.target_pid, Some(88));
    assert_eq!(result.target_exit_code, None);
}

#[test]
fn run_profile_collector_start_failure_does_not_launch_target() {
    let root = test_profile_root("collector-start-failure");
    let manager = ProfileManager::with_components(
        &root,
        Arc::new(TestCollectorFactory {
            state: Arc::new(Mutex::new(Vec::new())),
            fail_start: true,
            fail_stop: false,
        }),
        Arc::new(PanicTargetRunner),
    );

    let error = manager
        .run_profile(RunProfile {
            target: ProfileTarget::Launch {
                executable: std::env::current_exe().expect("current exe"),
                args: Vec::new(),
            },
            timeout_ms: 1,
            collector: ProfileCollectorConfig::default(),
        })
        .expect_err("collector start fails");

    assert!(error.to_string().contains("collector start failed"));
}

fn test_profile_root(name: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("dbgflow-profile-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("create root");
    root
}

#[derive(Default)]
struct TestCollectorFactory {
    state: Arc<Mutex<Vec<String>>>,
    fail_start: bool,
    fail_stop: bool,
}

impl CollectorFactory for TestCollectorFactory {
    fn create(
        &self,
        _config: &ProfileCollectorConfig,
        _trace_path: &Path,
    ) -> Result<Box<dyn ProfileCollector>> {
        Ok(Box::new(TestCollector {
            state: self.state.clone(),
            fail_start: self.fail_start,
            fail_stop: self.fail_stop,
        }))
    }
}

struct PanicTargetRunner;

impl TargetRunner for PanicTargetRunner {
    fn launch_and_wait(
        &self,
        _target: &ProfileTarget,
        _timeout: Duration,
        _stdout_path: &Path,
        _stderr_path: &Path,
    ) -> Result<TargetExit> {
        panic!("target runner must not be called when collector start fails");
    }
}
```

If duplicate imports appear, consolidate the test file imports at the top.

- [ ] **Step 2: Run lifecycle tests to verify they fail**

Run:

```powershell
cargo test -p dbgflow-core run_profile_starts_collector_launches_target_stops_collector_and_writes_artifacts run_profile_timeout_stops_collector_without_target_exit_code run_profile_collector_start_failure_does_not_launch_target
```

Expected: FAIL because `ProfileManager` does not exist.

- [ ] **Step 3: Implement ProfileManager**

Create `crates/dbgflow-core/src/profile/manager.rs`:

```rust
use super::{
    validate_profile_target, CollectorFactory, ProcessTargetRunner, ProfileArtifacts,
    ProfileCollectorKind, ProfileCompletionReason, ProfileId, ProfileResult, ProfileStatus,
    ProfileTarget, RunProfile, TargetExit, TargetRunner,
};
use crate::artifacts::{ArtifactKind, ArtifactManager, ArtifactRef, ProfileArtifactEvent};
use crate::{DbgFlowError, Result};
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone)]
pub struct ProfileManager {
    artifacts: ArtifactManager,
    collector_factory: Arc<dyn CollectorFactory>,
    target_runner: Arc<dyn TargetRunner>,
    active_job: Arc<Mutex<Option<ProfileId>>>,
}

impl ProfileManager {
    pub fn new(artifact_root: impl Into<PathBuf>) -> Self {
        Self::with_components(
            artifact_root,
            Arc::new(super::native_etw::NativeEtwCollectorFactory),
            Arc::new(ProcessTargetRunner),
        )
    }

    pub fn with_components(
        artifact_root: impl Into<PathBuf>,
        collector_factory: Arc<dyn CollectorFactory>,
        target_runner: Arc<dyn TargetRunner>,
    ) -> Self {
        Self {
            artifacts: ArtifactManager::new(artifact_root),
            collector_factory,
            target_runner,
            active_job: Arc::new(Mutex::new(None)),
        }
    }

    pub fn run_profile(&self, mut request: RunProfile) -> Result<ProfileResult> {
        request.target = validate_profile_target(request.target)?;
        if request.timeout_ms == 0 {
            return Err(DbgFlowError::Backend(
                "profile timeout_ms must be greater than zero".to_string(),
            ));
        }
        if request.collector.kind != ProfileCollectorKind::NativeEtw {
            return Err(DbgFlowError::Backend(
                "unsupported profile collector kind".to_string(),
            ));
        }

        let profile_id = ProfileId::new();
        {
            let mut active = self
                .active_job
                .lock()
                .map_err(|_| DbgFlowError::Backend("profile active job lock poisoned".to_string()))?;
            if let Some(active_id) = *active {
                return Err(DbgFlowError::Backend(format!(
                    "another profile job is already active: {active_id}"
                )));
            }
            *active = Some(profile_id);
        }
        let active_guard = ActiveProfileGuard {
            active_job: self.active_job.clone(),
            profile_id,
        };

        let started = Instant::now();
        let started_at = now_unix_ms();
        let profile_dir = self.artifacts.initialize_profile_artifacts(profile_id)?;
        self.record_event(profile_id, "profile_created", None, None, Map::new());

        let trace_path = self.artifacts.profile_trace_path(profile_id);
        let stdout_path = self.artifacts.profile_stdout_path(profile_id);
        let stderr_path = self.artifacts.profile_stderr_path(profile_id);
        let events_artifact = ArtifactRef {
            kind: ArtifactKind::ProfileEvents,
            path: profile_dir.join("events.jsonl"),
        };
        let trace_artifact = ArtifactRef {
            kind: ArtifactKind::ProfileTrace,
            path: trace_path.clone(),
        };
        let stdout_artifact = ArtifactRef {
            kind: ArtifactKind::ProfileStdout,
            path: stdout_path.clone(),
        };
        let stderr_artifact = ArtifactRef {
            kind: ArtifactKind::ProfileStderr,
            path: stderr_path.clone(),
        };

        self.record_event(profile_id, "collector_starting", None, None, Map::new());
        let collector = self.collector_factory.create(&request.collector, &trace_path)?;
        let mut warnings = collector.start(&profile_dir)?.warnings;
        self.record_event(
            profile_id,
            "collector_started",
            Some(trace_path.clone()),
            None,
            Map::new(),
        );

        self.record_event(profile_id, "target_launching", None, None, Map::new());
        let target_exit = self.target_runner.launch_and_wait(
            &request.target,
            Duration::from_millis(request.timeout_ms),
            &stdout_path,
            &stderr_path,
        );

        let mut stop_error = None;
        self.record_event(profile_id, "collector_stopping", None, None, Map::new());
        match collector.stop() {
            Ok(stop) => {
                warnings.extend(stop.warnings);
                self.record_event(profile_id, "collector_stopped", None, None, Map::new());
            }
            Err(error) => {
                stop_error = Some(error.to_string());
                self.record_event(
                    profile_id,
                    "profile_error",
                    None,
                    stop_error.clone(),
                    Map::new(),
                );
            }
        }

        let duration_ms = started.elapsed().as_millis();
        let (status, completion_reason, target_pid, target_exit_code, error) = match target_exit {
            Ok(TargetExit::Exited { pid, exit_code }) => {
                self.record_target_started(profile_id, pid);
                self.record_event(profile_id, "target_exited", None, None, Map::new());
                (
                    ProfileStatus::Completed,
                    ProfileCompletionReason::TargetExited,
                    Some(pid),
                    exit_code,
                    stop_error,
                )
            }
            Ok(TargetExit::TimedOut { pid }) => {
                self.record_target_started(profile_id, pid);
                self.record_event(profile_id, "timeout_reached", None, None, Map::new());
                (
                    ProfileStatus::TimedOut,
                    ProfileCompletionReason::Timeout,
                    Some(pid),
                    None,
                    stop_error,
                )
            }
            Err(error) => {
                let error = error.to_string();
                self.record_event(
                    profile_id,
                    "profile_error",
                    None,
                    Some(error.clone()),
                    Map::new(),
                );
                (
                    ProfileStatus::Failed,
                    ProfileCompletionReason::TargetLaunchFailed,
                    None,
                    None,
                    Some(stop_error.unwrap_or(error)),
                )
            }
        };

        let metadata_artifact = self.write_metadata(
            profile_id,
            &request,
            status,
            completion_reason,
            target_pid,
            target_exit_code,
            started_at,
            duration_ms,
            &trace_artifact,
            &warnings,
            error.clone(),
        )?;

        let result = ProfileResult {
            profile_id,
            status,
            completion_reason,
            target_pid,
            target_exit_code,
            duration_ms,
            artifacts: ProfileArtifacts {
                trace: trace_artifact,
                profile: metadata_artifact,
                events: events_artifact,
                stdout: stdout_artifact,
                stderr: stderr_artifact,
            },
            warnings,
            error,
        };
        self.record_event(profile_id, "profile_completed", None, None, Map::new());
        drop(active_guard);
        Ok(result)
    }

    fn write_metadata(
        &self,
        profile_id: ProfileId,
        request: &RunProfile,
        status: ProfileStatus,
        completion_reason: ProfileCompletionReason,
        target_pid: Option<u32>,
        target_exit_code: Option<i32>,
        started_at_unix_ms: u128,
        duration_ms: u128,
        trace_artifact: &ArtifactRef,
        warnings: &[String],
        error: Option<String>,
    ) -> Result<ArtifactRef> {
        let metadata = json!({
            "profile_id": profile_id.to_string(),
            "target": request.target,
            "target_pid": target_pid,
            "start_time_unix_ms": started_at_unix_ms,
            "end_time_unix_ms": now_unix_ms(),
            "duration_ms": duration_ms,
            "timeout_ms": request.timeout_ms,
            "status": status,
            "completion_reason": completion_reason,
            "target_exit_code": target_exit_code,
            "collector": request.collector,
            "trace": trace_artifact.path,
            "warnings": warnings,
            "error": error,
        });
        self.artifacts.write_profile_metadata(profile_id, &metadata)
    }

    fn record_target_started(&self, profile_id: ProfileId, pid: u32) {
        let mut fields = Map::new();
        fields.insert("pid".to_string(), Value::Number(pid.into()));
        self.record_event(profile_id, "target_started", None, None, fields);
    }

    fn record_event(
        &self,
        profile_id: ProfileId,
        event: &str,
        artifact_path: Option<PathBuf>,
        error: Option<String>,
        fields: Map<String, Value>,
    ) {
        let _ = self.artifacts.append_profile_event(
            profile_id,
            &ProfileArtifactEvent {
                timestamp_unix_ms: now_unix_ms(),
                event: event.to_string(),
                profile_id: profile_id.to_string(),
                artifact_path,
                error,
                fields,
            },
        );
    }
}

struct ActiveProfileGuard {
    active_job: Arc<Mutex<Option<ProfileId>>>,
    profile_id: ProfileId,
}

impl Drop for ActiveProfileGuard {
    fn drop(&mut self) {
        if let Ok(mut active) = self.active_job.lock() {
            if *active == Some(self.profile_id) {
                *active = None;
            }
        }
    }
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}
```

Temporarily create `crates/dbgflow-core/src/profile/native_etw.rs` with a stub factory so `ProfileManager::new` compiles:

```rust
use super::{
    CollectorFactory, ProfileCollector, ProfileCollectorConfig, ProfileCollectorKind, ProfilePreset,
};
use crate::{DbgFlowError, Result};
use std::path::Path;

#[derive(Debug, Default)]
pub struct NativeEtwCollectorFactory;

impl CollectorFactory for NativeEtwCollectorFactory {
    fn create(
        &self,
        config: &ProfileCollectorConfig,
        _trace_path: &Path,
    ) -> Result<Box<dyn ProfileCollector>> {
        if config.kind != ProfileCollectorKind::NativeEtw
            || config.preset != ProfilePreset::SystemOverview
        {
            return Err(DbgFlowError::Backend(
                "unsupported native ETW profile collector configuration".to_string(),
            ));
        }
        Err(DbgFlowError::Backend(
            "native ETW collector is not implemented yet".to_string(),
        ))
    }
}
```

Modify `crates/dbgflow-core/src/profile/mod.rs`:

```rust
pub mod collector;
pub mod id;
pub mod manager;
pub mod model;
pub mod native_etw;
pub mod target;

pub use collector::{CollectorFactory, CollectorStart, CollectorStop, ProfileCollector};
pub use id::ProfileId;
pub use manager::ProfileManager;
pub use model::{
    ProfileArtifacts, ProfileCollectorConfig, ProfileCollectorKind, ProfileCompletionReason,
    ProfilePreset, ProfileResult, ProfileStatus, ProfileTarget, RunProfile,
};
pub use target::{validate_profile_target, ProcessTargetRunner, TargetExit, TargetRunner};
```

- [ ] **Step 4: Run lifecycle tests**

Run:

```powershell
cargo test -p dbgflow-core run_profile_starts_collector_launches_target_stops_collector_and_writes_artifacts run_profile_timeout_stops_collector_without_target_exit_code run_profile_collector_start_failure_does_not_launch_target
```

Expected: PASS.

- [ ] **Step 5: Commit**

```powershell
git add crates/dbgflow-core/src/profile/manager.rs crates/dbgflow-core/src/profile/native_etw.rs crates/dbgflow-core/src/profile/mod.rs crates/dbgflow-core/tests/profile_lifecycle.rs
git commit -m "feat: orchestrate profile jobs"
```

---

### Task 5: Add Concurrency Guard and Failure Path Coverage

**Files:**
- Modify: `crates/dbgflow-core/tests/profile_lifecycle.rs`
- Modify: `crates/dbgflow-core/src/profile/manager.rs`

- [ ] **Step 1: Write failing concurrency and cleanup tests**

Append to `crates/dbgflow-core/tests/profile_lifecycle.rs`:

```rust
use std::sync::Condvar;

#[test]
fn run_profile_rejects_concurrent_profile_job() {
    let root = test_profile_root("concurrent");
    let blocker = Arc::new((Mutex::new(BlockingTargetState::default()), Condvar::new()));
    let manager = ProfileManager::with_components(
        &root,
        Arc::new(TestCollectorFactory::default()),
        Arc::new(BlockingTargetRunner {
            blocker: blocker.clone(),
        }),
    );
    let request = RunProfile {
        target: ProfileTarget::Launch {
            executable: std::env::current_exe().expect("current exe"),
            args: Vec::new(),
        },
        timeout_ms: 1000,
        collector: ProfileCollectorConfig::default(),
    };
    let first_manager = manager.clone();
    let first_request = request.clone();
    let first = std::thread::spawn(move || first_manager.run_profile(first_request));

    wait_until_blocking_runner_started(&blocker);
    let error = manager
        .run_profile(request)
        .expect_err("second profile should be rejected");
    assert!(error.to_string().contains("already active"));

    release_blocking_runner(&blocker);
    first.join().expect("first thread").expect("first profile");
}

#[test]
fn target_launch_failure_after_collector_start_returns_failed_result_and_stops_collector() {
    let root = test_profile_root("target-launch-failure");
    let collector_state = Arc::new(Mutex::new(Vec::new()));
    let manager = ProfileManager::with_components(
        &root,
        Arc::new(TestCollectorFactory {
            state: collector_state.clone(),
            fail_start: false,
            fail_stop: false,
        }),
        Arc::new(FailingTargetRunner),
    );

    let result = manager
        .run_profile(RunProfile {
            target: ProfileTarget::Launch {
                executable: std::env::current_exe().expect("current exe"),
                args: Vec::new(),
            },
            timeout_ms: 1000,
            collector: ProfileCollectorConfig::default(),
        })
        .expect("failed profile result");

    assert_eq!(result.status, ProfileStatus::Failed);
    assert_eq!(
        result.completion_reason,
        ProfileCompletionReason::TargetLaunchFailed
    );
    assert!(result.error.as_deref().is_some_and(|error| error.contains("target failed")));
    assert_eq!(
        collector_state.lock().expect("state").as_slice(),
        &["start".to_string(), "stop".to_string()]
    );
}

struct BlockingTargetRunner {
    blocker: Arc<(Mutex<BlockingTargetState>, Condvar)>,
}

impl TargetRunner for BlockingTargetRunner {
    fn launch_and_wait(
        &self,
        _target: &ProfileTarget,
        _timeout: Duration,
        _stdout_path: &Path,
        _stderr_path: &Path,
    ) -> Result<TargetExit> {
        let (lock, cvar) = &*self.blocker;
        let mut state = lock.lock().expect("blocker lock");
        state.started = true;
        cvar.notify_all();
        while !state.released {
            state = cvar.wait(state).expect("blocker wait");
        }
        Ok(TargetExit::Exited {
            pid: 1,
            exit_code: Some(0),
        })
    }
}

#[derive(Default)]
struct BlockingTargetState {
    started: bool,
    released: bool,
}

struct FailingTargetRunner;

impl TargetRunner for FailingTargetRunner {
    fn launch_and_wait(
        &self,
        _target: &ProfileTarget,
        _timeout: Duration,
        _stdout_path: &Path,
        _stderr_path: &Path,
    ) -> Result<TargetExit> {
        Err(dbgflow_core::DbgFlowError::Backend("target failed".to_string()))
    }
}

fn wait_until_blocking_runner_started(blocker: &Arc<(Mutex<BlockingTargetState>, Condvar)>) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let (lock, cvar) = &**blocker;
    let mut state = lock.lock().expect("blocker lock");
    while !state.started {
        assert!(std::time::Instant::now() < deadline, "runner did not start");
        let (next, _) = cvar
            .wait_timeout(state, Duration::from_millis(10))
            .expect("wait");
        state = next;
    }
}

fn release_blocking_runner(blocker: &Arc<(Mutex<BlockingTargetState>, Condvar)>) {
    let (lock, cvar) = &**blocker;
    let mut state = lock.lock().expect("blocker lock");
    state.released = true;
    cvar.notify_all();
}
```

- [ ] **Step 2: Run tests**

Run:

```powershell
cargo test -p dbgflow-core run_profile_rejects_concurrent_profile_job target_launch_failure_after_collector_start_returns_failed_result_and_stops_collector
```

Expected: `target_launch_failure_after_collector_start_returns_failed_result_and_stops_collector` should PASS from Task 4. `run_profile_rejects_concurrent_profile_job` should PASS because `BlockingTargetState.started` confirms the first profile is inside `launch_and_wait` before the second call starts.

- [ ] **Step 3: Ensure active guard clears on all returns**

Append this test and helper to `crates/dbgflow-core/tests/profile_lifecycle.rs`:

```rust
#[test]
fn run_profile_allows_new_job_after_failed_profile() {
    let root = test_profile_root("failure-releases-active");
    let manager = ProfileManager::with_components(
        &root,
        Arc::new(TestCollectorFactory::default()),
        Arc::new(SequenceTargetRunner {
            exits: Mutex::new(vec![
                Err(dbgflow_core::DbgFlowError::Backend("target failed".to_string())),
                Ok(TargetExit::Exited {
                    pid: 99,
                    exit_code: Some(0),
                }),
            ]),
        }),
    );
    let request = RunProfile {
        target: ProfileTarget::Launch {
            executable: std::env::current_exe().expect("current exe"),
            args: Vec::new(),
        },
        timeout_ms: 1000,
        collector: ProfileCollectorConfig::default(),
    };

    let first = manager
        .run_profile(request.clone())
        .expect("first returns failed profile result");
    assert_eq!(first.status, ProfileStatus::Failed);

    let second = manager.run_profile(request).expect("second profile starts");
    assert_eq!(second.status, ProfileStatus::Completed);
    assert_eq!(second.target_pid, Some(99));
}

struct SequenceTargetRunner {
    exits: Mutex<Vec<Result<TargetExit>>>,
}

impl TargetRunner for SequenceTargetRunner {
    fn launch_and_wait(
        &self,
        _target: &ProfileTarget,
        _timeout: Duration,
        _stdout_path: &Path,
        _stderr_path: &Path,
    ) -> Result<TargetExit> {
        let mut exits = self.exits.lock().expect("sequence lock");
        assert!(!exits.is_empty(), "sequence target runner exhausted");
        exits.remove(0)
    }
}
```

Keep `ActiveProfileGuard` in `crates/dbgflow-core/src/profile/manager.rs` created immediately after setting `active_job`; its `Drop` implementation must clear the active job for both normal returns and failed profile results.

- [ ] **Step 4: Run all profile lifecycle tests**

Run:

```powershell
cargo test -p dbgflow-core --test profile_lifecycle
```

Expected: PASS.

- [ ] **Step 5: Commit**

```powershell
git add crates/dbgflow-core/src/profile/manager.rs crates/dbgflow-core/tests/profile_lifecycle.rs
git commit -m "test: cover profile job concurrency"
```

---

### Task 6: Implement Native ETW Collector

**Files:**
- Modify: `crates/dbgflow-core/Cargo.toml`
- Modify: `crates/dbgflow-core/src/profile/native_etw.rs`
- Create: `crates/dbgflow-core/tests/native_etw_profile.rs`

- [ ] **Step 1: Add Windows ETW dependency features**

Modify the Windows `windows` dependency feature list in `crates/dbgflow-core/Cargo.toml` to include:

```toml
"Win32_System_Diagnostics_Etw",
"Win32_System_SystemServices",
```

The resulting target dependency should include both existing debugger/process features and the two ETW features.

- [ ] **Step 2: Write ignored Windows ETW smoke test**

Create `crates/dbgflow-core/tests/native_etw_profile.rs`:

```rust
#![cfg(windows)]

use dbgflow_core::profile::{
    ProfileCollectorConfig, ProfileCompletionReason, ProfileManager, ProfileStatus, ProfileTarget,
    RunProfile,
};

#[test]
#[ignore = "requires local ETW permissions and writes a real ETL trace"]
fn native_etw_run_profile_writes_etl_for_cmd() {
    let root = std::env::temp_dir().join(format!(
        "dbgflow-native-etw-profile-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create root");

    let manager = ProfileManager::new(&root);
    let result = manager
        .run_profile(RunProfile {
            target: ProfileTarget::Launch {
                executable: std::path::PathBuf::from(r"C:\Windows\System32\cmd.exe"),
                args: vec!["/C".to_string(), "echo dbgflow-etw".to_string()],
            },
            timeout_ms: 10_000,
            collector: ProfileCollectorConfig::default(),
        })
        .expect("run profile");

    assert_eq!(result.status, ProfileStatus::Completed);
    assert_eq!(result.completion_reason, ProfileCompletionReason::TargetExited);
    assert!(result.artifacts.trace.path.is_file());
    assert!(
        result.artifacts.trace.path.metadata().expect("trace metadata").len() > 0,
        "expected non-empty ETL"
    );
}
```

- [ ] **Step 3: Run ignored test listing**

Run:

```powershell
cargo test -p dbgflow-core --test native_etw_profile -- --ignored --list
```

Expected: PASS listing `native_etw_run_profile_writes_etl_for_cmd`.

- [ ] **Step 4: Implement non-Windows unsupported collector**

In `crates/dbgflow-core/src/profile/native_etw.rs`, keep a non-Windows implementation:

```rust
#[cfg(not(windows))]
use super::{
    CollectorFactory, ProfileCollector, ProfileCollectorConfig, ProfileCollectorKind, ProfilePreset,
};
#[cfg(not(windows))]
use crate::{DbgFlowError, Result};
#[cfg(not(windows))]
use std::path::Path;

#[cfg(not(windows))]
#[derive(Debug, Default)]
pub struct NativeEtwCollectorFactory;

#[cfg(not(windows))]
impl CollectorFactory for NativeEtwCollectorFactory {
    fn create(
        &self,
        config: &ProfileCollectorConfig,
        _trace_path: &Path,
    ) -> Result<Box<dyn ProfileCollector>> {
        if config.kind != ProfileCollectorKind::NativeEtw
            || config.preset != ProfilePreset::SystemOverview
        {
            return Err(DbgFlowError::Backend(
                "unsupported native ETW profile collector configuration".to_string(),
            ));
        }
        Err(DbgFlowError::Backend(
            "native ETW profiling is only supported on Windows".to_string(),
        ))
    }
}
```

- [ ] **Step 5: Implement Windows native ETW start/stop**

Add this Windows implementation to `crates/dbgflow-core/src/profile/native_etw.rs`. Use the exact imports supported by the installed `windows` crate; if a symbol path differs, search local generated docs or compile errors and adjust only the import path, not the design.

```rust
#[cfg(windows)]
use super::{
    CollectorFactory, CollectorStart, CollectorStop, ProfileCollector, ProfileCollectorConfig,
    ProfileCollectorKind, ProfilePreset,
};
#[cfg(windows)]
use crate::{DbgFlowError, Result};
#[cfg(windows)]
use std::ffi::OsStr;
#[cfg(windows)]
use std::mem::size_of;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(windows)]
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::sync::Mutex;
#[cfg(windows)]
use uuid::Uuid;
#[cfg(windows)]
use windows::Win32::Foundation::{ERROR_SUCCESS, WIN32_ERROR};
#[cfg(windows)]
use windows::Win32::System::Diagnostics::Etw::{
    ControlTraceW, EnableTraceEx2, StartTraceW, EVENT_TRACE_CONTROL_STOP,
    EVENT_TRACE_FLAG_CSWITCH, EVENT_TRACE_FLAG_DISK_FILE_IO, EVENT_TRACE_FLAG_DISK_IO,
    EVENT_TRACE_FLAG_IMAGE_LOAD, EVENT_TRACE_FLAG_PROCESS, EVENT_TRACE_FLAG_PROFILE,
    EVENT_TRACE_FLAG_REGISTRY, EVENT_TRACE_PROPERTIES, EVENT_TRACE_REAL_TIME_MODE,
    EVENT_TRACE_SYSTEM_LOGGER_MODE, EVENT_TRACE_USE_PAGED_MEMORY, EVENT_TRACE_TYPE_START,
    EVENT_TRACE_TYPE_STOP, EVENT_TRACE_TYPE_DC_START, EVENT_TRACE_TYPE_DC_END,
    EVENT_CONTROL_CODE_ENABLE_PROVIDER, PROCESS_TRACE_MODE_EVENT_RECORD,
    WNODE_FLAG_TRACED_GUID,
};
#[cfg(windows)]
use windows::core::{GUID, PCWSTR, PWSTR};

#[cfg(windows)]
#[derive(Debug, Default)]
pub struct NativeEtwCollectorFactory;

#[cfg(windows)]
impl CollectorFactory for NativeEtwCollectorFactory {
    fn create(
        &self,
        config: &ProfileCollectorConfig,
        trace_path: &Path,
    ) -> Result<Box<dyn ProfileCollector>> {
        if config.kind != ProfileCollectorKind::NativeEtw
            || config.preset != ProfilePreset::SystemOverview
        {
            return Err(DbgFlowError::Backend(
                "unsupported native ETW profile collector configuration".to_string(),
            ));
        }
        Ok(Box::new(NativeEtwCollector::new(trace_path.to_path_buf())))
    }
}

#[cfg(windows)]
struct NativeEtwCollector {
    trace_path: PathBuf,
    state: Mutex<NativeEtwState>,
}

#[cfg(windows)]
#[derive(Debug, Default)]
struct NativeEtwState {
    session_name: Option<String>,
}

#[cfg(windows)]
impl NativeEtwCollector {
    fn new(trace_path: PathBuf) -> Self {
        Self {
            trace_path,
            state: Mutex::new(NativeEtwState::default()),
        }
    }
}

#[cfg(windows)]
impl ProfileCollector for NativeEtwCollector {
    fn start(&self, _output_dir: &Path) -> Result<CollectorStart> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| DbgFlowError::Backend("native ETW collector lock poisoned".to_string()))?;
        if state.session_name.is_some() {
            return Err(DbgFlowError::Backend(
                "native ETW collector already started".to_string(),
            ));
        }
        let session_name = format!("dbgflow-profile-{}", Uuid::new_v4());
        start_trace_session(&session_name, &self.trace_path)?;
        state.session_name = Some(session_name);
        Ok(CollectorStart {
            warnings: Vec::new(),
        })
    }

    fn stop(&self) -> Result<CollectorStop> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| DbgFlowError::Backend("native ETW collector lock poisoned".to_string()))?;
        let Some(session_name) = state.session_name.take() else {
            return Ok(CollectorStop {
                warnings: vec!["native ETW collector was not started".to_string()],
            });
        };
        stop_trace_session(&session_name)?;
        Ok(CollectorStop {
            warnings: Vec::new(),
        })
    }

    fn cleanup(&self) -> Result<()> {
        let session_name = self
            .state
            .lock()
            .map_err(|_| DbgFlowError::Backend("native ETW collector lock poisoned".to_string()))?
            .session_name
            .clone();
        if let Some(session_name) = session_name {
            let _ = stop_trace_session(&session_name);
        }
        Ok(())
    }
}

#[cfg(windows)]
fn start_trace_session(session_name: &str, trace_path: &Path) -> Result<()> {
    let mut session_name_w = wide_null(session_name);
    let mut trace_path_w = wide_null(&trace_path.as_os_str().to_string_lossy());
    let properties_size = size_of::<EVENT_TRACE_PROPERTIES>()
        + session_name_w.len() * size_of::<u16>()
        + trace_path_w.len() * size_of::<u16>();
    let mut buffer = vec![0u8; properties_size];
    let properties = buffer.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES;
    unsafe {
        (*properties).Wnode.BufferSize = properties_size as u32;
        (*properties).Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        (*properties).Wnode.ClientContext = 1;
        (*properties).LogFileMode = EVENT_TRACE_SYSTEM_LOGGER_MODE
            | EVENT_TRACE_USE_PAGED_MEMORY;
        (*properties).EnableFlags = EVENT_TRACE_FLAG_PROCESS
            | EVENT_TRACE_FLAG_IMAGE_LOAD
            | EVENT_TRACE_FLAG_PROFILE
            | EVENT_TRACE_FLAG_CSWITCH
            | EVENT_TRACE_FLAG_DISK_IO
            | EVENT_TRACE_FLAG_DISK_FILE_IO
            | EVENT_TRACE_FLAG_REGISTRY;
        (*properties).BufferSize = 1024;
        (*properties).MinimumBuffers = 64;
        (*properties).MaximumBuffers = 256;
        (*properties).LoggerNameOffset = size_of::<EVENT_TRACE_PROPERTIES>() as u32;
        (*properties).LogFileNameOffset =
            (size_of::<EVENT_TRACE_PROPERTIES>() + session_name_w.len() * size_of::<u16>()) as u32;
        copy_wide_to_buffer(&mut buffer, (*properties).LoggerNameOffset as usize, &session_name_w);
        copy_wide_to_buffer(&mut buffer, (*properties).LogFileNameOffset as usize, &trace_path_w);
        let mut handle = 0u64;
        let status = StartTraceW(&mut handle, PCWSTR(session_name_w.as_ptr()), properties);
        if status != ERROR_SUCCESS {
            return Err(etw_error("StartTraceW", status));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn stop_trace_session(session_name: &str) -> Result<()> {
    let mut session_name_w = wide_null(session_name);
    let properties_size = size_of::<EVENT_TRACE_PROPERTIES>() + session_name_w.len() * size_of::<u16>();
    let mut buffer = vec![0u8; properties_size];
    let properties = buffer.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES;
    unsafe {
        (*properties).Wnode.BufferSize = properties_size as u32;
        (*properties).LoggerNameOffset = size_of::<EVENT_TRACE_PROPERTIES>() as u32;
        copy_wide_to_buffer(&mut buffer, (*properties).LoggerNameOffset as usize, &session_name_w);
        let status = ControlTraceW(
            0,
            PCWSTR(session_name_w.as_ptr()),
            properties,
            EVENT_TRACE_CONTROL_STOP,
        );
        if status != ERROR_SUCCESS {
            return Err(etw_error("ControlTraceW stop", status));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn wide_null(value: &str) -> Vec<u16> {
    OsStr::new(value).encode_wide().chain(Some(0)).collect()
}

#[cfg(windows)]
unsafe fn copy_wide_to_buffer(buffer: &mut [u8], byte_offset: usize, value: &[u16]) {
    let destination = buffer.as_mut_ptr().add(byte_offset) as *mut u16;
    std::ptr::copy_nonoverlapping(value.as_ptr(), destination, value.len());
}

#[cfg(windows)]
fn etw_error(operation: &str, status: WIN32_ERROR) -> DbgFlowError {
    DbgFlowError::Backend(format!("{operation} failed with Win32 error {}", status.0))
}
```

After compilation, remove unused ETW imports. Keep the first ETW implementation focused on kernel logger flags and `.etl` output. If stackwalk configuration requires additional API shape work, record a warning in code comments and leave detailed stackwalk expansion to a later task rather than broadening this task.

- [ ] **Step 6: Compile and fix Windows API type mismatches**

Run:

```powershell
cargo test -p dbgflow-core --test profile_lifecycle
```

Expected: PASS. If compilation fails only inside `native_etw.rs`, fix imports, constants, and parameter types to match `windows 0.62.2`.

- [ ] **Step 7: Run ignored ETW smoke test manually**

Run from an elevated/service-equivalent environment:

```powershell
cargo test -p dbgflow-core --test native_etw_profile -- --ignored --nocapture
```

Expected: PASS and a non-empty `trace.etl`. If local privileges are insufficient, record the exact error in the final implementation notes and keep the test ignored.

- [ ] **Step 8: Commit**

```powershell
git add crates/dbgflow-core/Cargo.toml crates/dbgflow-core/src/profile/native_etw.rs crates/dbgflow-core/tests/native_etw_profile.rs
git commit -m "feat: add native etw collector"
```

---

### Task 7: Expose `run_profile` in MCP Tools

**Files:**
- Modify: `crates/dbgflow-mcp/src/tools.rs`
- Modify: `crates/dbgflow-mcp/src/service.rs` if constructor call sites require profile manager injection
- Test: `crates/dbgflow-mcp/src/tools.rs`

- [ ] **Step 1: Write failing tool tests**

Add this test module at the end of `crates/dbgflow-mcp/src/tools.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use dbgflow_core::profile::{ProfileCollectorKind, ProfilePreset};

    #[test]
    fn tool_descriptors_include_run_profile() {
        let service = ToolService::new_for_tests();

        let descriptors = service.tool_descriptors();
        let run_profile = descriptors
            .iter()
            .find(|descriptor| descriptor.name == RUN_PROFILE)
            .expect("run_profile descriptor");

        assert!(run_profile.description.contains("profile"));
        assert_eq!(run_profile.input_schema["type"], "object");
    }

    #[test]
    fn run_profile_arguments_decode_to_launch_target_and_native_etw() {
        let value = json!({
            "target": {
                "kind": "launch",
                "executable": "C:\\Windows\\System32\\cmd.exe",
                "args": ["/C", "echo dbgflow"]
            },
            "timeout_ms": 1000,
            "collector": {
                "kind": "native_etw",
                "preset": "system_overview"
            }
        });

        let request: RunProfileRequest = decode_arguments(value).expect("decode request");
        assert_eq!(request.timeout_ms, 1000);
        assert_eq!(request.collector.kind, ProfileCollectorKind::NativeEtw);
        assert_eq!(request.collector.preset, ProfilePreset::SystemOverview);
    }
}
```

If `ToolService::new_for_tests` does not exist yet, the test should fail.

- [ ] **Step 2: Run tests to verify they fail**

Run:

```powershell
cargo test -p dbgflow-mcp tool_descriptors_include_run_profile run_profile_arguments_decode_to_launch_target_and_native_etw
```

Expected: FAIL because `RUN_PROFILE`, `RunProfileRequest`, and `new_for_tests` do not exist.

- [ ] **Step 3: Add profile manager to ToolService**

Modify imports in `crates/dbgflow-mcp/src/tools.rs`:

```rust
use dbgflow_core::profile::{ProfileCollectorConfig, ProfileManager, ProfileResult, RunProfile};
```

Add constant:

```rust
pub const RUN_PROFILE: &str = "run_profile";
```

Modify `ToolService`:

```rust
#[derive(Clone)]
pub struct ToolService {
    sessions: SessionManager,
    profiles: ProfileManager,
}

impl ToolService {
    pub fn new(sessions: SessionManager) -> Self {
        let profile_root = sessions_artifact_root(&sessions).unwrap_or_else(|| PathBuf::from("artifacts"));
        Self {
            sessions,
            profiles: ProfileManager::new(profile_root),
        }
    }

    pub fn with_profiles(sessions: SessionManager, profiles: ProfileManager) -> Self {
        Self { sessions, profiles }
    }

    #[cfg(test)]
    fn new_for_tests() -> Self {
        let root = std::env::temp_dir().join(format!("dbgflow-mcp-tools-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create root");
        Self {
            sessions: SessionManager::with_artifact_root(&root),
            profiles: ProfileManager::new(&root),
        }
    }
}

fn sessions_artifact_root(_sessions: &SessionManager) -> Option<PathBuf> {
    None
}
```

This temporary `sessions_artifact_root` fallback keeps call sites compiling. In the next task, replace it with explicit profile manager construction from the runtime data directory if service wiring exposes the artifact root.

- [ ] **Step 4: Add descriptor and call dispatch**

Add a descriptor entry in `tool_descriptors()`:

```rust
ToolDescriptor {
    name: RUN_PROFILE,
    description: "Launch a process and record a native ETW profile trace as a standard ETL artifact.",
    input_schema: json!({
        "type": "object",
        "properties": {
            "target": {
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "const": "launch" },
                    "executable": {
                        "type": "string",
                        "description": "Path to a local executable."
                    },
                    "args": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Command-line arguments. Omit for no arguments."
                    }
                },
                "required": ["kind", "executable"],
                "additionalProperties": false
            },
            "timeout_ms": {
                "type": "integer",
                "minimum": 1,
                "description": "Stop collection when the target exits or this timeout expires."
            },
            "collector": {
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "const": "native_etw" },
                    "preset": { "type": "string", "const": "system_overview" }
                },
                "required": ["kind", "preset"],
                "additionalProperties": false
            }
        },
        "required": ["target", "timeout_ms", "collector"],
        "additionalProperties": false
    }),
},
```

Add method:

```rust
pub fn run_profile(&self, request: RunProfileRequest) -> Result<ProfileResult> {
    self.profiles.run_profile(request.into())
}
```

Add dispatch arm:

```rust
RUN_PROFILE => self
    .run_profile(decode_arguments(arguments)?)
    .map_err(ToolCallError::execution)
    .and_then(to_value),
```

- [ ] **Step 5: Add MCP request decoding types**

Add near other request structs:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RunProfileRequest {
    pub target: dbgflow_core::profile::ProfileTarget,
    pub timeout_ms: u64,
    pub collector: ProfileCollectorConfig,
}

impl From<RunProfileRequest> for RunProfile {
    fn from(value: RunProfileRequest) -> Self {
        Self {
            target: value.target,
            timeout_ms: value.timeout_ms,
            collector: value.collector,
        }
    }
}
```

- [ ] **Step 6: Run MCP tool tests**

Run:

```powershell
cargo test -p dbgflow-mcp tool_descriptors_include_run_profile run_profile_arguments_decode_to_launch_target_and_native_etw
```

Expected: PASS.

- [ ] **Step 7: Commit**

```powershell
git add crates/dbgflow-mcp/src/tools.rs
git commit -m "feat: expose run_profile tool"
```

---

### Task 8: Wire ProfileManager to Runtime Artifact Root

**Files:**
- Inspect/Modify: `crates/dbgflow-mcp/src/runtime.rs`
- Inspect/Modify: `crates/dbgflow-mcp/src/service.rs`
- Modify: `crates/dbgflow-mcp/src/tools.rs`
- Test: relevant existing mcp/runtime tests

- [ ] **Step 1: Inspect current service construction**

Run:

```powershell
rg -n "ToolService::new|SessionManager::|artifact|data_dir" crates/dbgflow-mcp/src crates/dbgflow-mcp/tests
```

Expected: identify the exact place where `SessionManager` is constructed from `<data-dir>\artifacts`.

- [ ] **Step 2: Replace fallback profile root with explicit construction**

Where service/runtime currently constructs `ToolService::new(sessions)`, change it to create both managers from the same artifact root:

```rust
let artifact_root = data_dir.join("artifacts");
let sessions = SessionManager::with_default_worker_at_and_logger(
    &artifact_root,
    logger.clone(),
);
let profiles = ProfileManager::new(&artifact_root);
let tools = ToolService::with_profiles(sessions, profiles);
```

If runtime code already constructs `SessionManager` elsewhere, preserve the existing constructor behavior and only add `ProfileManager::new(&artifact_root)` next to it.

Then remove the temporary `sessions_artifact_root` helper from `tools.rs`.

- [ ] **Step 3: Run MCP tests**

Run:

```powershell
cargo test -p dbgflow-mcp
```

Expected: PASS. If live tests are ignored by default, they remain ignored.

- [ ] **Step 4: Commit**

```powershell
git add crates/dbgflow-mcp/src/runtime.rs crates/dbgflow-mcp/src/service.rs crates/dbgflow-mcp/src/tools.rs
git commit -m "feat: wire profile artifacts to runtime data dir"
```

---

### Task 9: Update Documentation and Goals

**Files:**
- Modify: `README.md`
- Modify: `README.zh-CN.md`
- Modify: `GOALS.md`

- [ ] **Step 1: Update English README**

In `README.md`, add `run_profile` to the tool list:

```text
- `run_profile`
```

Add a section after the `set_symbols` paragraph:

```markdown
`run_profile` launches a local executable and records a native ETW profile trace
as a standard `.etl` artifact. V1 supports launch targets only, uses the built-in
`native_etw/system_overview` preset only, and stops collection when the target
exits or when `timeout_ms` expires. Timeout stops collection but does not
terminate the target process by default. The ETL trace, profile metadata,
lifecycle events, and captured target stdout/stderr are written under
`artifacts\profiles\<profile_id>`.

Example profile request:

```json
{
  "target": {
    "kind": "launch",
    "executable": "C:\\Windows\\System32\\cmd.exe",
    "args": ["/C", "echo dbgflow"]
  },
  "timeout_ms": 10000,
  "collector": {
    "kind": "native_etw",
    "preset": "system_overview"
  }
}
```
```

- [ ] **Step 2: Update Chinese README**

In `README.zh-CN.md`, add equivalent Chinese text:

```markdown
`run_profile` 启动本地可执行文件，并直接通过 native ETW 采集标准
`.etl` trace artifact。V1 仅支持 launch target，仅支持内置
`native_etw/system_overview` preset，并在目标进程退出或 `timeout_ms`
到达时停止采集。timeout 默认只停止采集，不终止目标进程。ETL trace、
profile 元数据、生命周期事件以及目标 stdout/stderr 写入
`artifacts\profiles\<profile_id>`。
```

Use the same JSON example as English README.

- [ ] **Step 3: Update GOALS**

In `GOALS.md`, add a new goal or milestone note:

```markdown
### G6. Native ETW Profiling

支持通过 dbgflow 直接编排 native ETW 采样，生成标准 `.etl` artifact，
并与调试 session、artifact、审计链路保持一致。第一版采用一次性
`run_profile` 工具，支持 launch-only、内置 `system_overview` preset、
目标退出或 timeout 自动停止采集。
```

Add a design decision:

```markdown
### D-008: `run_profile` 使用 native ETW 而不是 WPR 命令行

决定：

第一版 profiling 能力直接在代码层面控制 ETW session，输出标准
`.etl`，不通过 `wpr.exe` 命令行编排。

原因：

* 避免把 dbgflow 变成 shell wrapper。
* 保持采集生命周期、artifact、错误和审计由核心层统一控制。
* 为后续 debugger-gated profiling 和更轻量的 provider preset 留出空间。
```

Mark a P3/P4 item as added:

```markdown
* [x] 支持 native ETW launch-only `run_profile` MVP。
* [ ] 支持 debugger-gated profiling。
* [ ] 支持 ETL 后处理和报告生成。
```

- [ ] **Step 4: Run docs-sensitive checks**

Run:

```powershell
cargo fmt --all -- --check
cargo test -p dbgflow-core --test profile_lifecycle
cargo test -p dbgflow-mcp
```

Expected: all PASS. If the ignored native ETW test was not run, say so in the implementation final.

- [ ] **Step 5: Commit**

```powershell
git add README.md README.zh-CN.md GOALS.md
git commit -m "docs: document native etw profiling"
```

---

### Task 10: Final Verification

**Files:**
- All modified files

- [ ] **Step 1: Run formatting**

```powershell
cargo fmt --all -- --check
```

Expected: PASS. If it fails, run `cargo fmt --all`, inspect the diff, then rerun the check.

- [ ] **Step 2: Run core tests**

```powershell
cargo test -p dbgflow-core
```

Expected: PASS, with Windows-only ignored tests remaining ignored unless explicitly requested.

- [ ] **Step 3: Run MCP tests**

```powershell
cargo test -p dbgflow-mcp
```

Expected: PASS.

- [ ] **Step 4: Run real ETW smoke test when privileges allow**

```powershell
cargo test -p dbgflow-core --test native_etw_profile -- --ignored --nocapture
```

Expected: PASS in an elevated or service-equivalent context. If this cannot run because the current shell lacks ETW privileges, capture the failure message and report that the test remains unverified locally.

- [ ] **Step 5: Inspect git status**

```powershell
git status --short
```

Expected: no uncommitted changes except intentionally generated local artifacts under ignored `var/` or temp directories.

---

## Self-Review

- Spec coverage:
  - Single `run_profile` tool: Tasks 7 and 8.
  - Launch-only target: Tasks 2, 4, and 7.
  - Native ETW collector without WPR command line: Task 6.
  - Standard `.etl` primary artifact: Tasks 1, 4, and 6.
  - Target exit or timeout stop condition: Tasks 3 and 4.
  - Timeout does not kill target by default: Tasks 3 and 4.
  - Permission failure returns error: Task 6 via ETW start error.
  - Built-in `system_overview` only: Tasks 2, 6, and 7.
  - Artifact metadata/events/stdout/stderr: Tasks 1 and 4.
  - One active native ETW profile at a time: Task 5.
  - Documentation/GOALS updates: Task 9.
- Placeholder scan: No placeholder markers or unspecified implementation steps are intentionally left in this plan.
- Type consistency:
  - `ProfileId`, `RunProfile`, `ProfileTarget`, `ProfileCollectorConfig`, `ProfileResult`, `ProfileManager`, `ProfileCollector`, and `TargetRunner` are introduced before use by later tasks.
  - MCP `RunProfileRequest` converts into core `RunProfile`.
  - Artifact kinds match `ProfileArtifacts` fields.
