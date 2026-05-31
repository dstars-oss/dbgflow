use dbgflow_core::session::SessionManager;
use dbgflow_mcp::tools::{ToolService, CREATE_SESSION};

fn main() {
    let service = ToolService::new(SessionManager::with_mock_backend());
    let output = serde_json::json!({
        "server": "dbgflow-mcp",
        "status": "ready",
        "tools": service.tool_descriptors(),
        "default_tool": CREATE_SESSION,
    });

    println!(
        "{}",
        serde_json::to_string_pretty(&output).expect("serialize startup output")
    );
}
