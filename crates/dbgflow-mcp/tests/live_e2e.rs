#![cfg(windows)]

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const CHILD_SLEEP_ENV: &str = "DBGFLOW_HTTP_E2E_SLEEP_CHILD";

#[test]
#[ignore = "live HTTP/DbgEng attach E2E is environment-sensitive; run explicitly when validating live debugging"]
fn http_worker_can_attach_to_process_and_eval_queries() {
    let _guard = live_debug_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let target = ChildGuard::spawn_sleep();
    std::thread::sleep(Duration::from_millis(500));

    let mut server = TestServer::start("attach", &[]);
    initialize(&server.addr);

    let created = tool_call(
        &server.addr,
        "create_session",
        json!({
            "target": { "kind": "attach", "pid": target.id() }
        }),
    );
    let session_id = created["id"].as_str().expect("session id").to_string();
    let session = wait_for_session_state(&server.addr, &session_id, "Break");
    assert_eq!(session["state"], "Break");

    let modules = tool_call(
        &server.addr,
        "eval",
        json!({
            "session_id": session_id,
            "command": "lm"
        }),
    );
    assert_nonempty_output_with_artifact(&modules);

    let stacks = tool_call(
        &server.addr,
        "eval",
        json!({
            "session_id": session_id,
            "command": "~* k"
        }),
    );
    assert_nonempty_output_with_artifact(&stacks);

    let resource = resource_read(&server.addr, &session_id);
    assert_eq!(resource["state"], "Break");

    tool_call(
        &server.addr,
        "close_session",
        json!({ "session_id": session_id }),
    );
    let closed = wait_for_session_state(&server.addr, &session_id, "Closed");
    assert_eq!(closed["state"], "Closed");

    assert_session_audit(&server.data_dir, &session_id, &["lm", "~* k"]);
    server.stop();
}

#[test]
#[ignore = "live HTTP/DbgEng launch E2E is environment-sensitive; run explicitly when validating live debugging"]
fn http_worker_can_launch_process_and_continue_to_exit() {
    let _guard = live_debug_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let launch_env = [("DBGFLOW_ENABLE_LAUNCH", "1")];
    let mut server = TestServer::start("launch", &launch_env);
    initialize(&server.addr);

    let created = tool_call(
        &server.addr,
        "create_session",
        json!({
            "target": {
                "kind": "launch",
                "executable": ping_exe(),
                "args": ["127.0.0.1", "-n", "2"]
            }
        }),
    );
    let session_id = created["id"].as_str().expect("session id").to_string();
    let session = wait_for_session_state(&server.addr, &session_id, "Break");
    assert_eq!(session["state"], "Break");

    let continued = tool_call(
        &server.addr,
        "eval",
        json!({
            "session_id": session_id,
            "command": "g"
        }),
    );
    assert!(continued["artifact"]["path"]
        .as_str()
        .map(Path::new)
        .is_some_and(Path::is_file));

    let resource = resource_read(&server.addr, &session_id);
    assert_eq!(resource["state"], "Break");

    tool_call(
        &server.addr,
        "close_session",
        json!({ "session_id": session_id }),
    );
    let closed = wait_for_session_state(&server.addr, &session_id, "Closed");
    assert_eq!(closed["state"], "Closed");

    assert_session_audit(&server.data_dir, &session_id, &["g"]);
    server.stop();
}

#[test]
fn process_child_sleep_entrypoint() {
    if std::env::var_os(CHILD_SLEEP_ENV).is_some() {
        std::thread::sleep(Duration::from_secs(300));
    }
}

struct ChildGuard {
    child: Child,
}

impl ChildGuard {
    fn spawn_sleep() -> Self {
        Self {
            child: spawn_sleep_child(),
        }
    }

    fn id(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.child.try_wait().expect("poll child").is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

struct TestServer {
    addr: SocketAddr,
    data_dir: PathBuf,
    child: Child,
}

impl TestServer {
    fn start(name: &str, envs: &[(&str, &str)]) -> Self {
        let addr = free_loopback_addr();
        let data_dir = test_data_dir(name);
        let exe = std::env::var_os("CARGO_BIN_EXE_dbgflow-mcp")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("dbgflow-mcp.exe"));

        let mut command = Command::new(exe);
        command
            .arg("http")
            .arg("--bind")
            .arg(addr.to_string())
            .arg("--data-dir")
            .arg(&data_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (key, value) in envs {
            command.env(key, value);
        }

        let child = command.spawn().expect("spawn dbgflow-mcp http server");
        let mut server = Self {
            addr,
            data_dir,
            child,
        };
        server.wait_for_health();
        server
    }

    fn wait_for_health(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if self.child.try_wait().expect("poll server").is_some() {
                panic!("dbgflow-mcp server exited before /healthz became ready");
            }
            if http_get_json(&self.addr, "/healthz")
                .ok()
                .is_some_and(|value| value["status"] == "ok")
            {
                return;
            }
            assert!(Instant::now() < deadline, "server did not become healthy");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn stop(&mut self) {
        if self.child.try_wait().expect("poll server").is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        let _ = std::fs::remove_dir_all(&self.data_dir);
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn initialize(addr: &SocketAddr) {
    let response = mcp_request(
        addr,
        json!({
            "jsonrpc": "2.0",
            "id": "initialize",
            "method": "initialize",
            "params": { "protocolVersion": "2025-06-18" }
        }),
    );
    assert_eq!(response["result"]["serverInfo"]["name"], "dbgflow");
}

fn tool_call(addr: &SocketAddr, name: &str, arguments: Value) -> Value {
    let response = mcp_request(
        addr,
        json!({
            "jsonrpc": "2.0",
            "id": next_id(),
            "method": "tools/call",
            "params": {
                "name": name,
                "arguments": arguments
            }
        }),
    );
    if response.get("error").is_some() {
        panic!("tools/call {name} returned protocol error: {response}");
    }
    let result = &response["result"];
    if result["isError"].as_bool().unwrap_or(false) {
        panic!(
            "tools/call {name} returned tool error: {}",
            result["content"][0]["text"].as_str().unwrap_or("")
        );
    }
    serde_json::from_str(
        result["content"][0]["text"]
            .as_str()
            .expect("tool text content"),
    )
    .expect("parse tool JSON text")
}

fn resource_read(addr: &SocketAddr, session_id: &str) -> Value {
    let response = mcp_request(
        addr,
        json!({
            "jsonrpc": "2.0",
            "id": next_id(),
            "method": "resources/read",
            "params": {
                "uri": format!("dbgflow://sessions/{session_id}")
            }
        }),
    );
    if response.get("error").is_some() {
        panic!("resources/read returned error: {response}");
    }
    serde_json::from_str(
        response["result"]["contents"][0]["text"]
            .as_str()
            .expect("resource text"),
    )
    .expect("parse resource JSON")
}

fn wait_for_session_state(addr: &SocketAddr, session_id: &str, state: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(130);
    loop {
        let session = tool_call(
            addr,
            "get_session",
            json!({
                "session_id": session_id
            }),
        );
        if session["state"] == state {
            return session;
        }
        assert!(
            Instant::now() < deadline,
            "session {session_id} did not reach {state}: {session}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn assert_nonempty_output_with_artifact(result: &Value) {
    assert!(
        !result["output"].as_str().unwrap_or("").trim().is_empty(),
        "expected non-empty debugger output: {result}"
    );
    assert!(result["artifact"]["path"]
        .as_str()
        .map(Path::new)
        .is_some_and(Path::is_file));
}

fn assert_session_audit(data_dir: &Path, session_id: &str, commands: &[&str]) {
    let session_dir = data_dir.join("artifacts").join("sessions").join(session_id);
    let transcript =
        std::fs::read_to_string(session_dir.join("transcript.log")).expect("read transcript");
    assert!(transcript.contains("session created"));
    assert!(transcript.contains("worker startup finished"));
    for command in commands {
        assert!(
            transcript.contains(command),
            "missing transcript command {command}"
        );
    }

    let events = read_jsonl(&session_dir.join("events.jsonl"));
    assert!(events
        .iter()
        .any(|event| event["event"] == "session_created"));
    assert!(events
        .iter()
        .any(|event| event["event"] == "worker_startup_finished"));
    assert!(events.iter().any(|event| event["event"] == "eval_finished"));

    let command_records = read_jsonl(&session_dir.join("commands.jsonl"));
    for command in commands {
        assert!(
            command_records
                .iter()
                .any(|record| record["command"] == *command && record["status"] == "Finished"),
            "missing command record {command}"
        );
    }
}

fn mcp_request(addr: &SocketAddr, message: Value) -> Value {
    http_post_json(addr, "/mcp", &message).expect("HTTP MCP response")
}

fn http_post_json(addr: &SocketAddr, path: &str, value: &Value) -> std::io::Result<Value> {
    let body = serde_json::to_vec(value)?;
    let mut stream = TcpStream::connect(addr)?;
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(&body)?;
    stream.flush()?;
    read_json_response(stream, 200)
}

fn http_get_json(addr: &SocketAddr, path: &str) -> std::io::Result<Value> {
    let mut stream = TcpStream::connect(addr)?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )?;
    stream.flush()?;
    read_json_response(stream, 200)
}

fn read_json_response(mut stream: TcpStream, expected_status: u16) -> std::io::Result<Value> {
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    let response = String::from_utf8_lossy(&response);
    let Some((headers, body)) = response.split_once("\r\n\r\n") else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "HTTP response missing body separator",
        ));
    };
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or_default();
    if status != expected_status {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("unexpected HTTP status {status}: {body}"),
        ));
    }
    serde_json::from_str(body).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("parse response JSON: {error}: {body}"),
        )
    })
}

fn read_jsonl(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .expect("read jsonl")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("parse jsonl line"))
        .collect()
}

fn spawn_sleep_child() -> Child {
    Command::new(std::env::current_exe().expect("current test exe"))
        .arg("process_child_sleep_entrypoint")
        .arg("--exact")
        .env(CHILD_SLEEP_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sleep child")
}

fn ping_exe() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("System32")
        .join("ping.exe")
}

fn free_loopback_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral loopback port");
    let addr = listener.local_addr().expect("local addr");
    drop(listener);
    addr
}

fn test_data_dir(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "dbgflow-http-e2e-{name}-{}-{}",
        std::process::id(),
        NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create data dir");
    root
}

fn live_debug_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn next_id() -> u64 {
    NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
}

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(1);
