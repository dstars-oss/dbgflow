use dbgflow_mcp::mcp::{default_server, run_stdio};
use std::io::{self, BufReader};

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();

    if let Err(error) = run_stdio(
        default_server(),
        BufReader::new(stdin.lock()),
        stdout.lock(),
    ) {
        eprintln!("dbgflow-mcp server error: {error}");
        std::process::exit(1);
    }
}
