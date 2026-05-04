//! Agent endpoints: provider listing and chat.
//!
//! `GET /api/v1/providers` — list configured providers from omw-config TOML.
//! `POST /api/v1/agent/ask` — stream an LLM response via the omw-agent CLI.

use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::process::Stdio;
use tokio::process::Command;

#[derive(Serialize)]
struct ProviderInfo {
    name: String,
    kind: String,
    default_model: Option<String>,
}

#[derive(Serialize)]
struct ProviderListResponse {
    providers: Vec<ProviderInfo>,
}

/// `GET /api/v1/providers` — list configured providers (no secrets).
pub async fn list_providers() -> impl IntoResponse {
    let path = match config_path() {
        Some(p) => p,
        None => return Json(ProviderListResponse { providers: vec![] }).into_response(),
    };
    let config: omw_config::Config = match std::fs::read_to_string(&path) {
        Ok(s) => match toml::from_str(&s) {
            Ok(c) => c,
            Err(_) => return Json(ProviderListResponse { providers: vec![] }).into_response(),
        },
        Err(_) => return Json(ProviderListResponse { providers: vec![] }).into_response(),
    };
    let providers: Vec<ProviderInfo> = config
        .providers
        .into_iter()
        .map(|(id, pc)| ProviderInfo {
            name: id.as_str().to_string(),
            kind: pc.kind_str().to_string(),
            default_model: pc.default_model().map(|s| s.to_string()),
        })
        .collect();
    Json(ProviderListResponse { providers }).into_response()
}

#[derive(Deserialize)]
pub struct AskRequest {
    provider: String,
    #[serde(default)]
    model: Option<String>,
    prompt: String,
}

/// `POST /api/v1/agent/ask` — stream an LLM response via omw-agent CLI.
/// Returns `text/event-stream` with SSE events:
///   `data: {"delta":"Hello"}`  — text chunk
///   `data: {"done":true}`     — stream complete
///   `data: {"error":"msg"}`   — error
pub async fn ask(Json(body): Json<AskRequest>) -> Response {
    // Resolve omw-agent binary relative to the workspace or from PATH.
    let agent_bin = resolve_omw_agent_bin();

    // Resolve node.exe by absolute path (GUI processes have limited PATH).
    let node_exe = resolve_node_exe();
    let mut cmd = Command::new(&node_exe);
    cmd.arg(&agent_bin);
    cmd.arg("ask")
        .arg(&body.prompt)
        .arg("--provider")
        .arg(&body.provider);
    if let Some(ref model) = body.model {
        cmd.arg("--model").arg(model);
    }
    // Forward config path + keychain env so the agent can resolve keys.
    if let Some(config_path) = config_path() {
        cmd.env("OMW_CONFIG", config_path);
    }
    cmd.env(
        "OMW_KEYCHAIN_BACKEND",
        std::env::var("OMW_KEYCHAIN_BACKEND").unwrap_or_else(|_| "auto".to_string()),
    );
    // OMW_KEYCHAIN_HELPER: resolve relative to the same workspace.
    if let Some(helper) = resolve_keychain_helper() {
        cmd.env("OMW_KEYCHAIN_HELPER", helper);
    }
    // Forward PATH so node.exe can be found.
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
    // Windows CSPRNG DLL needs SystemRoot.
    if cfg!(windows) {
        if let Ok(sr) = std::env::var("SystemRoot") {
            cmd.env("SystemRoot", sr);
        }
    }
    let cmd_debug = format!("{:?}", cmd);

    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.stdin(Stdio::null());
    cmd.kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let body = serde_json::json!({"error": format!("spawning omw-agent: {e}"), "cmd": cmd_debug}).to_string();
            return (StatusCode::INTERNAL_SERVER_ERROR, body).into_response();
        }
    };

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

    let stream = async_stream::stream! {
        use tokio::io::AsyncBufReadExt;
        let reader = tokio::io::BufReader::new(stdout);
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.is_empty() { continue; }
            let event = Event::default().data(line);
            yield Ok::<_, std::convert::Infallible>(event);
        }
        // Read stderr for diagnostics or usage record.
        use tokio::io::AsyncReadExt;
        let mut err_buf = String::new();
        let _ = tokio::io::BufReader::new(stderr).read_to_string(&mut err_buf).await;
        let status = child.wait().await;
        let stderr_str = err_buf.trim();
        if !stderr_str.is_empty() {
            let event = Event::default().data(stderr_str.to_string());
            yield Ok(event);
        } else if let Ok(s) = status {
            if !s.success() {
                let event = Event::default()
                    .data(format!(r#"{{"error":"agent exited with code {}","cmd":"{}"}}"#,
                        s.code().unwrap_or(-1), cmd_debug.replace('"', "'")));
                yield Ok(event);
            }
        }
        // Always send done.
        let event = Event::default().data(r#"{"done":true}"#);
        yield Ok(event);
    };

    Sse::new(stream).into_response()
}

/// Resolve the path to the omw-config TOML file.
fn config_path() -> Option<String> {
    if let Ok(p) = std::env::var("OMW_CONFIG") {
        if !p.is_empty() {
            return Some(p);
        }
    }
    omw_config::config_path()
        .ok()
        .map(|p| p.to_string_lossy().to_string())
}

/// Resolve the node binary with an absolute path.
fn resolve_node_exe() -> String {
    let exe_name = if cfg!(windows) { "node.exe" } else { "node" };
    // Check well-known install locations first (GUI processes have limited PATH).
    let candidates: &[&str] = if cfg!(windows) {
        &[
            r"C:\Program Files\nodejs\node.exe",
            r"C:\Program Files (x86)\nodejs\node.exe",
        ]
    } else {
        &["/usr/local/bin/node", "/usr/bin/node"]
    };
    for c in candidates {
        if std::path::Path::new(c).is_file() {
            return c.to_string();
        }
    }
    // Fall back to PATH lookup.
    exe_name.to_string()
}

/// Resolve the omw-agent bin script path.
fn resolve_omw_agent_bin() -> String {
    // Prefer OMW_AGENT_BIN env var.
    if let Ok(p) = std::env::var("OMW_AGENT_BIN") {
        if !p.is_empty() {
            return p;
        }
    }
    // Resolve relative to the running binary at runtime by walking up to the
    // workspace root, then looking for apps/omw-agent/bin/omw-agent.mjs.
    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent().map(|p| p.to_path_buf());
        while let Some(d) = dir {
            let candidate = d.join("apps").join("omw-agent").join("bin").join("omw-agent.mjs");
            if candidate.exists() {
                return candidate.to_string_lossy().to_string();
            }
            if d.parent().is_none() || d.as_os_str().is_empty() {
                break;
            }
            dir = d.parent().map(|p| p.to_path_buf());
        }
    }
    // Absolute last resort.
    "omw-agent.mjs".to_string()
}

/// Resolve the omw-keychain-helper binary.
fn resolve_keychain_helper() -> Option<String> {
    if let Ok(p) = std::env::var("OMW_KEYCHAIN_HELPER") {
        if !p.is_empty() {
            return Some(p);
        }
    }
    let name = if cfg!(windows) {
        "omw-keychain-helper.exe"
    } else {
        "omw-keychain-helper"
    };
    // Walk up from the running binary to find the helper.
    // Check both the current dir and ancestor/target/debug/ at each level
    // since warp-oss is in vendor/warp-stripped/target/debug/ but the
    // helper is in the umbrella target/debug/.
    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent().map(|p| p.to_path_buf());
        while let Some(d) = dir {
            if d.join(&name).exists() {
                return Some(d.join(&name).to_string_lossy().to_string());
            }
            let umbrella_target = d.join("target").join("debug").join(&name);
            if umbrella_target.exists() {
                return Some(umbrella_target.to_string_lossy().to_string());
            }
            if d.parent().is_none() || d.as_os_str().is_empty() {
                break;
            }
            dir = d.parent().map(|p| p.to_path_buf());
        }
    }
    None
}
