use serde_json::Value;
use std::ffi::OsString;
use std::process::Command;

/// Why `HERDR_BIN_PATH` and not `herdr` on `PATH`: hooks inherit the server's
/// environment, which is not the login shell's and may not have herdr on it.
fn binary() -> OsString {
    std::env::var_os("HERDR_BIN_PATH").unwrap_or_else(|| OsString::from("herdr"))
}

pub fn workspace_get(workspace_id: &str) -> Result<Value, String> {
    run_json(&["workspace", "get", workspace_id])
}

pub fn workspace_list() -> Result<Value, String> {
    run_json(&["workspace", "list"])
}

fn run_json(args: &[&str]) -> Result<Value, String> {
    let output = Command::new(binary())
        .args(args)
        .output()
        .map_err(|e| format!("herdr {}: {e}", args.join(" ")))?;

    if !output.status.success() {
        return Err(format!(
            "herdr {} exited with {}: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("herdr {}: unparsable response: {e}", args.join(" ")))
}
