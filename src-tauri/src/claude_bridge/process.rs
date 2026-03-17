use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::backend::app_server::{InterceptAction, WorkspaceSession};
use crate::backend::events::{AppServerEvent, EventSink};
use crate::types::WorkspaceEntry;

use super::event_mapper;
use super::history::{discover_models, read_claude_sessions, read_session_items, ClaudeSession};
use super::types::BridgeState;

/// A prompt turn to be processed by the coordinator task.
struct TurnRequest {
    prompt: String,
    /// The thread ID from `turn/start` params (may be a historical session UUID).
    thread_id: String,
    turn_id: String,
    /// Model override from the UI dropdown (e.g. "claude-opus-4-6").
    model: Option<String>,
}

/// How to identify the conversation session when spawning `claude --print`.
enum SessionArg {
    /// First turn of a brand-new conversation — no session flags.
    New,
    /// Continue the most-recent session in the working directory.
    Continue,
    /// Resume a specific session by UUID.
    Resume(String),
}

/// Check that the `claude` CLI binary is available.
#[tauri::command]
pub(crate) async fn check_claude_installation() -> Result<String, String> {
    let result = Command::new("claude")
        .arg("--version")
        .output()
        .await
        .map_err(|e| format!("Claude CLI not found in PATH: {e}"))?;

    if !result.status.success() {
        return Err("Claude CLI --version returned non-zero exit code".to_string());
    }

    let version = String::from_utf8_lossy(&result.stdout).trim().to_string();
    Ok(version)
}

/// Spawn a Claude CLI session that presents the same `WorkspaceSession`
/// interface as the Codex backend. The bridge translates between the
/// Codex JSON-RPC protocol and Claude CLI's stream-json format.
///
/// Architecture: **spawn-per-turn**. Each `turn/start` / `turn/steer` message
/// spawns a fresh `claude --print --output-format stream-json` process.
/// Historical sessions are loaded from `~/.claude/projects/<encoded-path>/`
/// and surfaced in `thread/list`. Selecting a historical thread passes
/// `--resume <session-id>` so Claude resumes the exact conversation.
pub(crate) async fn spawn_claude_session<E: EventSink>(
    entry: WorkspaceEntry,
    _client_version: String,
    event_sink: E,
) -> Result<Arc<WorkspaceSession>, String> {
    let _ = check_claude_installation().await?;

    let thread_id = format!("thread_{}", uuid::Uuid::new_v4());
    let workspace_id = entry.id.clone();
    let workspace_path = entry.path.clone();

    // Channel: interceptor (sync) → coordinator task (async)
    let (prompt_tx, mut prompt_rx) = mpsc::unbounded_channel::<TurnRequest>();

    // Shared session history: loaded at startup, refreshed after each turn.
    let sessions: Arc<std::sync::Mutex<Vec<ClaudeSession>>> = Arc::new(
        std::sync::Mutex::new(read_claude_sessions(&workspace_path)),
    );

    // Maps thread_id → resolved session_id (populated after first new-conv turn).
    let thread_session_map: Arc<std::sync::Mutex<HashMap<String, String>>> =
        Arc::new(std::sync::Mutex::new(HashMap::new()));

    // Shared detected model: updated by coordinator, read by interceptor.
    let detected_model: Arc<std::sync::Mutex<Option<String>>> =
        Arc::new(std::sync::Mutex::new(None));
    let active_interrupts: Arc<std::sync::Mutex<HashMap<String, oneshot::Sender<()>>>> =
        Arc::new(std::sync::Mutex::new(HashMap::new()));

    // Spawn a dummy child process to satisfy WorkspaceSession's type requirements.
    // The dummy's stdin is never written to — all actual work is done per-turn
    // by the coordinator task below.
    let mut dummy_child = Command::new("cmd")
        .args(["/c", "exit", "0"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to spawn dummy process: {e}"))?;
    let dummy_stdin = dummy_child.stdin.take().ok_or("missing dummy stdin")?;

    // Build the request interceptor.
    // turn/start and turn/steer are short-circuited: prompt+threadId go to
    // the coordinator via channel, an immediate ack is returned to the caller.
    let interceptor_thread_id = thread_id.clone();
    let interceptor_workspace_id = workspace_id.clone();
    let interceptor_workspace_path = workspace_path.clone();
    let detected_model_for_interceptor = detected_model.clone();
    let sessions_for_interceptor = sessions.clone();
    let active_interrupts_for_interceptor = active_interrupts.clone();
    let interceptor: Arc<dyn Fn(Value) -> InterceptAction + Send + Sync> =
        Arc::new(move |value: Value| {
            let method = value.get("method").and_then(|v| v.as_str()).unwrap_or("");
            let id = value.get("id").cloned();
            let params = value.get("params").cloned().unwrap_or(Value::Null);

            match method {
                "turn/start" | "turn/steer" => {
                    let text = extract_user_text(&params);
                    if text.is_empty() {
                        return if let Some(id) = id {
                            InterceptAction::Respond(json!({
                                "id": id,
                                "error": { "message": format!("Empty {method} message") }
                            }))
                        } else {
                            InterceptAction::Drop
                        };
                    }
                    let tid = params
                        .get("threadId")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&interceptor_thread_id)
                        .to_string();
                    let model = params
                        .get("model")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    let turn_id = format!("turn_{}", uuid::Uuid::new_v4());
                    let _ = prompt_tx.send(TurnRequest {
                        prompt: text,
                        thread_id: tid.clone(),
                        turn_id: turn_id.clone(),
                        model,
                    });
                    if let Some(id) = id {
                        InterceptAction::Respond(json!({
                            "id": id,
                            "result": {
                                "turn": {
                                    "id": turn_id,
                                    "threadId": tid
                                }
                            }
                        }))
                    } else {
                        InterceptAction::Drop
                    }
                }

                "turn/interrupt" => {
                    let tid = params
                        .get("threadId")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&interceptor_thread_id)
                        .to_string();
                    if let Some(sender) = active_interrupts_for_interceptor
                        .lock()
                        .unwrap()
                        .remove(&tid)
                    {
                        let _ = sender.send(());
                    }
                    if let Some(id) = id {
                        InterceptAction::Respond(json!({
                            "id": id,
                            "result": { "ok": true }
                        }))
                    } else {
                        InterceptAction::Drop
                    }
                }

                "thread/list" => {
                    if let Some(id) = id {
                        let hist = sessions_for_interceptor
                            .lock()
                            .ok()
                            .map(|s| s.clone())
                            .unwrap_or_default();
                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as i64;
                        let mut data = vec![json!({
                            "id": interceptor_thread_id,
                            "name": "New conversation",
                            "preview": "New conversation",
                            "status": "active",
                            "source": "appServer",
                            "createdAt": now_ms,
                            "updatedAt": now_ms,
                            "cwd": interceptor_workspace_path
                        })];
                        for s in &hist {
                            data.push(json!({
                                "id": s.session_id,
                                "name": s.name,
                                "preview": s.name,
                                "status": "idle",
                                "source": "appServer",
                                "createdAt": s.last_active_ms,
                                "updatedAt": s.last_active_ms,
                                "cwd": interceptor_workspace_path
                            }));
                        }
                        InterceptAction::Respond(json!({
                            "id": id,
                            "result": { "data": data }
                        }))
                    } else {
                        InterceptAction::Drop
                    }
                }

                "thread/resume" => {
                    if let Some(id) = id {
                        let tid = params
                            .get("threadId")
                            .and_then(|v| v.as_str())
                            .unwrap_or(&interceptor_thread_id);
                        // Look up the session name for historical threads.
                        let session_name = sessions_for_interceptor
                            .lock()
                            .ok()
                            .and_then(|sessions| {
                                sessions.iter().find(|s| s.session_id == tid).map(|s| s.name.clone())
                            });
                        let preview = session_name.unwrap_or_else(|| "New conversation".to_string());
                        // Load conversation items from the JSONL session file.
                        let items = read_session_items(&interceptor_workspace_path, tid);
                        let turns = if items.is_empty() {
                            json!([])
                        } else {
                            json!([{
                                "id": format!("turn-resume-{tid}"),
                                "status": "completed",
                                "items": items
                            }])
                        };
                        InterceptAction::Respond(json!({
                            "id": id,
                            "result": {
                                "threadId": tid,
                                "thread": {
                                    "id": tid,
                                    "status": "active",
                                    "preview": preview,
                                    "turns": turns
                                }
                            }
                        }))
                    } else {
                        InterceptAction::Drop
                    }
                }

                _ => {
                    let model = detected_model_for_interceptor
                        .lock()
                        .ok()
                        .and_then(|g| g.clone());
                    build_claude_intercept_action(
                        &value,
                        &interceptor_thread_id,
                        &interceptor_workspace_id,
                        model.as_deref(),
                        Some(&interceptor_workspace_path),
                    )
                }
            }
        });

    let session = Arc::new(WorkspaceSession {
        codex_args: None,
        child: Mutex::new(dummy_child),
        stdin: Mutex::new(dummy_stdin),
        pending: Mutex::new(HashMap::new()),
        request_context: Mutex::new(HashMap::new()),
        thread_workspace: Mutex::new(HashMap::new()),
        hidden_thread_ids: Mutex::new(HashSet::new()),
        next_id: AtomicU64::new(1),
        background_thread_callbacks: Mutex::new(HashMap::new()),
        owner_workspace_id: workspace_id.clone(),
        workspace_ids: Mutex::new(HashSet::from([workspace_id.clone()])),
        workspace_roots: Mutex::new(HashMap::from([(
            workspace_id.clone(),
            workspace_path.clone(),
        )])),
        request_interceptor: Some(interceptor),
    });

    // Coordinator task: one `claude --print` process per turn.
    let coord_workspace_id = workspace_id.clone();
    let coord_workspace_path = workspace_path.clone();
    let coord_thread_id = thread_id.clone();
    let coord_event_sink = event_sink.clone();
    let coord_detected_model = detected_model.clone();
    let coord_sessions = sessions.clone();
    let coord_thread_session_map = thread_session_map.clone();
    let coord_active_interrupts = active_interrupts.clone();
    tokio::spawn(async move {
        // Persist thread_started across turns so thread/started is only emitted once.
        let mut thread_started = false;
        while let Some(request) = prompt_rx.recv().await {
            let session_arg = {
                let map = coord_thread_session_map.lock().unwrap();
                match map.get(&request.thread_id) {
                    Some(sid) => SessionArg::Resume(sid.clone()),
                    None => {
                        if request.thread_id == coord_thread_id {
                            SessionArg::New
                        } else {
                            SessionArg::Resume(request.thread_id.clone())
                        }
                    }
                }
            };

            let (interrupt_tx, interrupt_rx) = oneshot::channel();
            coord_active_interrupts
                .lock()
                .unwrap()
                .insert(request.thread_id.clone(), interrupt_tx);

            run_claude_turn(
                &request.prompt,
                &coord_workspace_path,
                &coord_workspace_id,
                &request.thread_id,
                &request.turn_id,
                session_arg,
                request.model.as_deref(),
                &mut thread_started,
                interrupt_rx,
                &coord_event_sink,
                &coord_detected_model,
            )
            .await;

            coord_active_interrupts
                .lock()
                .unwrap()
                .remove(&request.thread_id);

            if !coord_thread_session_map
                .lock()
                .unwrap()
                .contains_key(&request.thread_id)
                && request.thread_id == coord_thread_id
            {
                let fresh = read_claude_sessions(&coord_workspace_path);
                if let Some(newest) = fresh.first() {
                    coord_thread_session_map
                        .lock()
                        .unwrap()
                        .insert(request.thread_id.clone(), newest.session_id.clone());
                }
            }

            *coord_sessions.lock().unwrap() = read_claude_sessions(&coord_workspace_path);
        }
    });

    // Emit codex/connected immediately so the UI knows the session is ready
    event_sink.emit_app_server_event(AppServerEvent {
        workspace_id: workspace_id.clone(),
        message: json!({
            "method": "codex/connected",
            "params": { "workspaceId": workspace_id }
        }),
    });

    Ok(session)
}

/// Run a single claude turn: spawn `claude --print --output-format stream-json`,
/// deliver the prompt via stdin (EOF signals end of input), stream events back
/// via `event_sink`, then wait for the process to exit.
async fn run_claude_turn<E: EventSink>(
    prompt: &str,
    workspace_path: &str,
    workspace_id: &str,
    thread_id: &str,
    turn_id: &str,
    session_arg: SessionArg,
    model: Option<&str>,
    thread_started: &mut bool,
    mut interrupt_rx: oneshot::Receiver<()>,
    event_sink: &E,
    detected_model: &Arc<std::sync::Mutex<Option<String>>>,
) {
    let mut command = Command::new("claude");
    command.args([
        "--output-format", "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--print",
    ]);
    if let Some(m) = model {
        command.args(["--model", m]);
    }
    match &session_arg {
        SessionArg::New => {}
        SessionArg::Continue => {
            command.arg("--continue");
        }
        SessionArg::Resume(sid) => {
            command.args(["--resume", sid]);
        }
    }
    command.current_dir(workspace_path);
    command.stdin(std::process::Stdio::piped());
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    // Remove the env var that blocks nested Claude CLI sessions
    command.env_remove("CLAUDECODE");

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            event_sink.emit_app_server_event(AppServerEvent {
                workspace_id: workspace_id.to_string(),
                message: json!({
                    "method": "error",
                    "params": {
                        "threadId": thread_id,
                        "turnId": turn_id,
                        "willRetry": false,
                        "error": { "message": format!("Failed to spawn claude: {e}") }
                    }
                }),
            });
            return;
        }
    };

    // Write prompt to stdin and close it (EOF causes claude to start processing)
    if let Some(mut stdin_handle) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        let _ = stdin_handle.write_all(prompt.as_bytes()).await;
        let _ = stdin_handle.write_all(b"\n").await;
        // stdin_handle dropped here → EOF
    }

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // Read stderr concurrently in a spawned task
    let stderr_event_sink = event_sink.clone();
    let stderr_ws_id = workspace_id.to_string();
    let stderr_task = tokio::spawn(async move {
        if let Some(stderr) = stderr {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                stderr_event_sink.emit_app_server_event(AppServerEvent {
                    workspace_id: stderr_ws_id.clone(),
                    message: json!({
                        "method": "codex/stderr",
                        "params": { "message": line }
                    }),
                });
            }
        }
    });

    // Read stdout (stream-json events) in the current task
    let mut bridge_state = BridgeState::new(
        workspace_id.to_string(),
        thread_id.to_string(),
        turn_id.to_string(),
    );
    bridge_state.thread_started = *thread_started;
    let mut turn_completed = false;
    if let Some(stdout) = stdout {
        let mut lines = BufReader::new(stdout).lines();
        loop {
            let next_line = tokio::select! {
                _ = &mut interrupt_rx => {
                    let _ = child.start_kill();
                    event_sink.emit_app_server_event(AppServerEvent {
                        workspace_id: workspace_id.to_string(),
                        message: json!({
                            "method": "thread/status/changed",
                            "params": {
                                "threadId": thread_id,
                                "status": { "type": "idle" }
                            }
                        }),
                    });
                    turn_completed = true; // interrupt handled, no synthetic completion needed
                    break;
                }
                result = lines.next_line() => result,
            };
            let next_line: Result<Option<String>, _> = next_line;
            let Ok(Some(line)) = next_line else {
                break;
            };
            if line.trim().is_empty() {
                continue;
            }
            let claude_event: super::types::ClaudeEvent = match serde_json::from_str(&line) {
                Ok(e) => e,
                Err(err) => {
                    event_sink.emit_app_server_event(AppServerEvent {
                        workspace_id: workspace_id.to_string(),
                        message: json!({
                            "method": "error",
                            "params": {
                                "threadId": thread_id,
                                "turnId": turn_id,
                                "willRetry": false,
                                "error": {
                                    "message": format!("Failed to parse Claude stream event: {err}")
                                }
                            }
                        }),
                    });
                    continue;
                }
            };

            let codex_messages = event_mapper::map_event(&claude_event, &mut bridge_state);

            // Propagate detected model to interceptor
            if let Some(ref model) = bridge_state.model {
                if let Ok(mut guard) = detected_model.lock() {
                    if guard.as_ref() != Some(model) {
                        *guard = Some(model.clone());
                    }
                }
            }

            for message in codex_messages {
                if message.get("method").and_then(|v| v.as_str()) == Some("turn/completed") {
                    turn_completed = true;
                }
                event_sink.emit_app_server_event(AppServerEvent {
                    workspace_id: workspace_id.to_string(),
                    message,
                });
            }
        }
    }

    // Persist thread_started for next turn.
    *thread_started = bridge_state.thread_started;

    // If the turn was started but never completed (crash/EOF), emit a
    // synthetic turn/completed so the UI exits "processing" state.
    if bridge_state.turn_started && !turn_completed {
        event_sink.emit_app_server_event(AppServerEvent {
            workspace_id: workspace_id.to_string(),
            message: json!({
                "method": "turn/completed",
                "params": {
                    "threadId": thread_id,
                    "turnId": turn_id,
                    "status": "error"
                }
            }),
        });
    }

    let _ = stderr_task.await;
    let _ = child.wait().await;
}

/// Format a model ID like "claude-sonnet-4-20250514" into a display name
/// like "Claude Sonnet 4".
fn format_model_display_name(model_id: &str) -> String {
    // Strip date suffix (e.g. "-20250514")
    let base = if let Some(pos) = model_id.rfind('-') {
        let suffix = &model_id[pos + 1..];
        if suffix.len() == 8 && suffix.chars().all(|c| c.is_ascii_digit()) {
            &model_id[..pos]
        } else {
            model_id
        }
    } else {
        model_id
    };
    // Capitalize each segment
    base.split('-')
        .map(|s| {
            let mut c = s.chars();
            match c.next() {
                None => String::new(),
                Some(first) => {
                    let upper: String = first.to_uppercase().collect();
                    upper + c.as_str()
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Determine how to handle a JSON-RPC message destined for Claude CLI.
fn build_claude_intercept_action(
    value: &Value,
    thread_id: &str,
    _workspace_id: &str,
    detected_model: Option<&str>,
    workspace_path: Option<&str>,
) -> InterceptAction {
    let method = value
        .get("method")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let id = value.get("id").cloned();
    let params = value.get("params").cloned().unwrap_or(Value::Null);

    match method {
        "initialize" => {
            if let Some(id) = id {
                InterceptAction::Respond(json!({
                    "id": id,
                    "result": {
                        "capabilities": { "experimentalApi": true },
                        "serverInfo": {
                            "name": "claude-bridge",
                            "version": "1.0.0"
                        }
                    }
                }))
            } else {
                InterceptAction::Drop
            }
        }

        "initialized" => InterceptAction::Drop,

        "turn/start" | "turn/steer" | "turn/interrupt" => {
            if let Some(id) = id {
                InterceptAction::Respond(json!({
                    "id": id,
                    "error": { "message": "Claude bridge routes this request via the coordinator" }
                }))
            } else {
                InterceptAction::Drop
            }
        }

        "thread/start" => {
            if let Some(id) = id {
                InterceptAction::Respond(json!({
                    "id": id,
                    "result": {
                        "threadId": thread_id,
                        "thread": {
                            "id": thread_id,
                            "name": "New conversation",
                            "status": "active",
                            "source": "appServer"
                        }
                    }
                }))
            } else {
                InterceptAction::Drop
            }
        }

        "thread/resume" => {
            if let Some(id) = id {
                InterceptAction::Respond(json!({
                    "id": id,
                    "result": {
                        "threadId": thread_id,
                        "thread": {
                            "id": thread_id,
                            "status": "active"
                        }
                    }
                }))
            } else {
                InterceptAction::Drop
            }
        }

        "thread/list" => {
            if let Some(id) = id {
                InterceptAction::Respond(json!({
                    "id": id,
                    "result": {
                        "data": [{
                            "id": thread_id,
                            "name": "Claude CLI session",
                            "status": "active",
                            "source": "appServer"
                        }]
                    }
                }))
            } else {
                InterceptAction::Drop
            }
        }

        "model/list" => {
            if let Some(id) = id {
                let models = workspace_path
                    .map(|p| discover_models(p))
                    .unwrap_or_default();
                let detected = detected_model.unwrap_or("");
                let data: Vec<Value> = if models.is_empty() {
                    let fallback_id = if detected.is_empty() { "claude-sonnet-4-6" } else { detected };
                    let display = format_model_display_name(fallback_id);
                    vec![json!({
                        "id": fallback_id,
                        "model": fallback_id,
                        "displayName": display,
                        "name": display,
                        "isDefault": true,
                        "supportedReasoningEfforts": [],
                        "defaultReasoningEffort": null,
                        "description": "Claude CLI"
                    })]
                } else {
                    models.iter().enumerate().map(|(i, (model_id, display_name))| {
                        let is_default = if !detected.is_empty() {
                            model_id == detected
                        } else {
                            i == 0
                        };
                        json!({
                            "id": model_id,
                            "model": model_id,
                            "displayName": display_name,
                            "name": display_name,
                            "isDefault": is_default,
                            "supportedReasoningEfforts": [],
                            "defaultReasoningEffort": null,
                            "description": "Claude CLI"
                        })
                    }).collect()
                };
                InterceptAction::Respond(json!({
                    "id": id,
                    "result": { "data": data }
                }))
            } else {
                InterceptAction::Drop
            }
        }

        "thread/fork" | "thread/archive" | "thread/compact/start"
        | "thread/name/set" | "review/start" => {
            if let Some(id) = id {
                InterceptAction::Respond(json!({
                    "id": id,
                    "error": { "message": "Пока не поддерживается" }
                }))
            } else {
                InterceptAction::Drop
            }
        }

        "skills/list" | "app/list" | "mcpServerStatus/list"
        | "experimentalFeature/list" | "collaborationMode/list" => {
            if let Some(id) = id {
                InterceptAction::Respond(json!({
                    "id": id,
                    "result": { "data": [] }
                }))
            } else {
                InterceptAction::Drop
            }
        }

        "account/read" | "account/rateLimits/read" | "account/login/start"
        | "account/login/cancel" => {
            if let Some(id) = id {
                InterceptAction::Respond(json!({
                    "id": id,
                    "error": { "message": "Пока не поддерживается" }
                }))
            } else {
                InterceptAction::Drop
            }
        }

        _ => {
            if let Some(id) = id {
                InterceptAction::Respond(json!({
                    "id": id,
                    "error": {
                        "message": format!("Method not supported in Claude CLI mode: {method}")
                    }
                }))
            } else {
                InterceptAction::Drop
            }
        }
    }
}

/// Extract user text from turn/start params.
fn extract_user_text(params: &Value) -> String {
    if let Some(input) = params.get("input").and_then(|v| v.as_array()) {
        let texts: Vec<&str> = input
            .iter()
            .filter_map(|item| {
                if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                    item.get("text").and_then(|t| t.as_str())
                } else {
                    None
                }
            })
            .collect();
        if !texts.is_empty() {
            return texts.join("\n");
        }
    }

    if let Some(text) = params.get("text").and_then(|v| v.as_str()) {
        return text.to_string();
    }

    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_user_text_from_input_items() {
        let params = json!({
            "input": [
                { "type": "text", "text": "Hello" },
                { "type": "image", "url": "data:..." }
            ]
        });
        assert_eq!(extract_user_text(&params), "Hello");
    }

    #[test]
    fn extract_user_text_from_text_field() {
        let params = json!({ "text": "Hello world" });
        assert_eq!(extract_user_text(&params), "Hello world");
    }

    #[test]
    fn extract_user_text_empty_when_missing() {
        let params = json!({});
        assert_eq!(extract_user_text(&params), "");
    }

    #[test]
    fn intercept_initialize_responds_immediately() {
        let action =
            build_claude_intercept_action(&json!({"id": 1, "method": "initialize"}), "t1", "w1", None, None);
        match action {
            InterceptAction::Respond(v) => {
                assert_eq!(v["id"], 1);
                assert!(v["result"]["serverInfo"]["name"].as_str().is_some());
            }
            _ => panic!("Expected Respond"),
        }
    }

    #[test]
    fn intercept_turn_start_reports_coordinator_routing() {
        let action = build_claude_intercept_action(
            &json!({
                "id": 2,
                "method": "turn/start",
                "params": {
                    "input": [{ "type": "text", "text": "What is Rust?" }]
                }
            }),
            "t1",
            "w1",
            None,
            None,
        );
        match action {
            InterceptAction::Respond(v) => {
                assert!(v["error"]["message"]
                    .as_str()
                    .unwrap_or("")
                    .contains("coordinator"));
            }
            _ => panic!("Expected Respond"),
        }
    }

    #[test]
    fn intercept_thread_list_responds_with_mock() {
        let action = build_claude_intercept_action(
            &json!({"id": 3, "method": "thread/list"}),
            "thread_abc",
            "ws_1",
            None,
            None,
        );
        match action {
            InterceptAction::Respond(v) => {
                assert_eq!(v["id"], 3);
                let data = v["result"]["data"].as_array().unwrap();
                assert_eq!(data[0]["id"], "thread_abc");
            }
            _ => panic!("Expected Respond"),
        }
    }

    #[test]
    fn intercept_unknown_method_returns_error() {
        let action = build_claude_intercept_action(
            &json!({"id": 4, "method": "some/unknown"}),
            "t1",
            "w1",
            None,
            None,
        );
        match action {
            InterceptAction::Respond(v) => {
                assert!(v["error"]["message"].as_str().is_some());
            }
            _ => panic!("Expected Respond"),
        }
    }

    #[test]
    fn intercept_notification_drops_initialized() {
        let action =
            build_claude_intercept_action(&json!({"method": "initialized"}), "t1", "w1", None, None);
        assert!(matches!(action, InterceptAction::Drop));
    }

    #[test]
    fn intercept_empty_turn_start_returns_error() {
        let action = build_claude_intercept_action(
            &json!({
                "id": 5,
                "method": "turn/start",
                "params": { "input": [] }
            }),
            "t1",
            "w1",
            None,
            None,
        );
        match action {
            InterceptAction::Respond(v) => {
                assert!(v["error"].is_object());
            }
            _ => panic!("Expected Respond with error"),
        }
    }

    #[test]
    fn format_model_display_name_strips_date() {
        assert_eq!(
            format_model_display_name("claude-sonnet-4-20250514"),
            "Claude Sonnet 4"
        );
        assert_eq!(
            format_model_display_name("claude-opus-4-20250514"),
            "Claude Opus 4"
        );
    }

    #[test]
    fn format_model_display_name_without_date() {
        assert_eq!(
            format_model_display_name("claude-haiku-4"),
            "Claude Haiku 4"
        );
    }

    #[test]
    fn intercept_model_list_uses_detected_model() {
        let action = build_claude_intercept_action(
            &json!({"id": 10, "method": "model/list"}),
            "t1",
            "w1",
            Some("claude-opus-4-20250514"),
            None,
        );
        match action {
            InterceptAction::Respond(v) => {
                let data = v["result"]["data"].as_array().unwrap();
                assert_eq!(data[0]["model"], "claude-opus-4-20250514");
                assert_eq!(data[0]["displayName"], "Claude Opus 4");
            }
            _ => panic!("Expected Respond"),
        }
    }

    // ── Phase 5: Additional interceptor tests ─────────────────────

    #[test]
    fn intercept_model_list_fallback_when_no_detected_model() {
        let action = build_claude_intercept_action(
            &json!({"id": 11, "method": "model/list"}),
            "t1",
            "w1",
            None,
            None,
        );
        match action {
            InterceptAction::Respond(v) => {
                let data = v["result"]["data"].as_array().unwrap();
                assert_eq!(data[0]["model"], "claude-sonnet-4-6");
                assert_eq!(data[0]["isDefault"], true);
            }
            _ => panic!("Expected Respond"),
        }
    }

    #[test]
    fn intercept_turn_steer_reports_coordinator_routing() {
        let action = build_claude_intercept_action(
            &json!({
                "id": 20,
                "method": "turn/steer",
                "params": { "text": "Actually, do this instead" }
            }),
            "t1",
            "w1",
            None,
            None,
        );
        match action {
            InterceptAction::Respond(v) => {
                assert!(v["error"]["message"]
                    .as_str()
                    .unwrap_or("")
                    .contains("coordinator"));
            }
            _ => panic!("Expected Respond"),
        }
    }

    #[test]
    fn intercept_turn_steer_empty_returns_error() {
        let action = build_claude_intercept_action(
            &json!({
                "id": 21,
                "method": "turn/steer",
                "params": {}
            }),
            "t1",
            "w1",
            None,
            None,
        );
        match action {
            InterceptAction::Respond(v) => {
                assert!(v["error"]["message"]
                    .as_str()
                    .unwrap_or("")
                    .contains("coordinator"));
            }
            _ => panic!("Expected Respond with error"),
        }
    }

    #[test]
    fn intercept_turn_steer_empty_no_id_drops() {
        let action = build_claude_intercept_action(
            &json!({
                "method": "turn/steer",
                "params": {}
            }),
            "t1",
            "w1",
            None,
            None,
        );
        assert!(matches!(action, InterceptAction::Drop));
    }

    #[test]
    fn intercept_thread_start_responds_with_thread() {
        let action = build_claude_intercept_action(
            &json!({"id": 30, "method": "thread/start"}),
            "thread_xyz",
            "w1",
            None,
            None,
        );
        match action {
            InterceptAction::Respond(v) => {
                assert_eq!(v["id"], 30);
                assert_eq!(v["result"]["threadId"], "thread_xyz");
                assert_eq!(v["result"]["thread"]["status"], "active");
                assert_eq!(v["result"]["thread"]["name"], "New conversation");
            }
            _ => panic!("Expected Respond"),
        }
    }

    #[test]
    fn intercept_thread_resume_responds() {
        let action = build_claude_intercept_action(
            &json!({"id": 31, "method": "thread/resume"}),
            "thread_abc",
            "w1",
            None,
            None,
        );
        match action {
            InterceptAction::Respond(v) => {
                assert_eq!(v["result"]["threadId"], "thread_abc");
                assert_eq!(v["result"]["thread"]["status"], "active");
            }
            _ => panic!("Expected Respond"),
        }
    }

    #[test]
    fn intercept_thread_fork_returns_unsupported_error() {
        let action = build_claude_intercept_action(
            &json!({"id": 32, "method": "thread/fork"}),
            "t1",
            "w1",
            None,
            None,
        );
        match action {
            InterceptAction::Respond(v) => {
                assert_eq!(v["id"], 32);
                assert_eq!(v["error"]["message"], "Пока не поддерживается");
            }
            _ => panic!("Expected Respond"),
        }
    }

    #[test]
    fn intercept_thread_archive_returns_unsupported_error() {
        let action = build_claude_intercept_action(
            &json!({"id": 33, "method": "thread/archive"}),
            "t1",
            "w1",
            None,
            None,
        );
        match action {
            InterceptAction::Respond(v) => {
                assert_eq!(v["error"]["message"], "Пока не поддерживается");
            }
            _ => panic!("Expected Respond"),
        }
    }

    #[test]
    fn intercept_thread_compact_start_returns_unsupported_error() {
        let action = build_claude_intercept_action(
            &json!({"id": 34, "method": "thread/compact/start"}),
            "t1",
            "w1",
            None,
            None,
        );
        match action {
            InterceptAction::Respond(v) => {
                assert_eq!(v["error"]["message"], "Пока не поддерживается");
            }
            _ => panic!("Expected Respond"),
        }
    }

    #[test]
    fn intercept_thread_name_set_returns_unsupported_error() {
        let action = build_claude_intercept_action(
            &json!({"id": 35, "method": "thread/name/set"}),
            "t1",
            "w1",
            None,
            None,
        );
        match action {
            InterceptAction::Respond(v) => {
                assert_eq!(v["error"]["message"], "Пока не поддерживается");
            }
            _ => panic!("Expected Respond"),
        }
    }

    #[test]
    fn intercept_review_start_returns_unsupported_error() {
        let action = build_claude_intercept_action(
            &json!({"id": 36, "method": "review/start"}),
            "t1",
            "w1",
            None,
            None,
        );
        match action {
            InterceptAction::Respond(v) => {
                assert_eq!(v["error"]["message"], "Пока не поддерживается");
            }
            _ => panic!("Expected Respond"),
        }
    }

    #[test]
    fn intercept_skills_list_responds_empty() {
        let action = build_claude_intercept_action(
            &json!({"id": 40, "method": "skills/list"}),
            "t1",
            "w1",
            None,
            None,
        );
        match action {
            InterceptAction::Respond(v) => {
                assert_eq!(v["result"]["data"].as_array().unwrap().len(), 0);
            }
            _ => panic!("Expected Respond"),
        }
    }

    #[test]
    fn intercept_app_list_responds_empty() {
        let action = build_claude_intercept_action(
            &json!({"id": 41, "method": "app/list"}),
            "t1",
            "w1",
            None,
            None,
        );
        match action {
            InterceptAction::Respond(v) => {
                assert_eq!(v["result"]["data"].as_array().unwrap().len(), 0);
            }
            _ => panic!("Expected Respond"),
        }
    }

    #[test]
    fn intercept_mcp_server_status_list_responds_empty() {
        let action = build_claude_intercept_action(
            &json!({"id": 42, "method": "mcpServerStatus/list"}),
            "t1",
            "w1",
            None,
            None,
        );
        match action {
            InterceptAction::Respond(v) => {
                assert_eq!(v["result"]["data"].as_array().unwrap().len(), 0);
            }
            _ => panic!("Expected Respond"),
        }
    }

    #[test]
    fn intercept_experimental_feature_list_responds_empty() {
        let action = build_claude_intercept_action(
            &json!({"id": 43, "method": "experimentalFeature/list"}),
            "t1",
            "w1",
            None,
            None,
        );
        match action {
            InterceptAction::Respond(v) => {
                assert_eq!(v["result"]["data"].as_array().unwrap().len(), 0);
            }
            _ => panic!("Expected Respond"),
        }
    }

    #[test]
    fn intercept_collaboration_mode_list_responds_empty() {
        let action = build_claude_intercept_action(
            &json!({"id": 44, "method": "collaborationMode/list"}),
            "t1",
            "w1",
            None,
            None,
        );
        match action {
            InterceptAction::Respond(v) => {
                assert_eq!(v["result"]["data"].as_array().unwrap().len(), 0);
            }
            _ => panic!("Expected Respond"),
        }
    }

    #[test]
    fn intercept_account_read_returns_unsupported_error() {
        let action = build_claude_intercept_action(
            &json!({"id": 50, "method": "account/read"}),
            "t1",
            "w1",
            None,
            None,
        );
        match action {
            InterceptAction::Respond(v) => {
                assert_eq!(v["id"], 50);
                assert_eq!(v["error"]["message"], "Пока не поддерживается");
            }
            _ => panic!("Expected Respond"),
        }
    }

    #[test]
    fn intercept_account_rate_limits_read_returns_unsupported_error() {
        let action = build_claude_intercept_action(
            &json!({"id": 51, "method": "account/rateLimits/read"}),
            "t1",
            "w1",
            None,
            None,
        );
        match action {
            InterceptAction::Respond(v) => {
                assert_eq!(v["id"], 51);
                assert_eq!(v["error"]["message"], "Пока не поддерживается");
            }
            _ => panic!("Expected Respond"),
        }
    }

    #[test]
    fn intercept_account_login_start_returns_unsupported_error() {
        let action = build_claude_intercept_action(
            &json!({"id": 52, "method": "account/login/start"}),
            "t1",
            "w1",
            None,
            None,
        );
        match action {
            InterceptAction::Respond(v) => {
                assert_eq!(v["id"], 52);
                assert_eq!(v["error"]["message"], "Пока не поддерживается");
            }
            _ => panic!("Expected Respond"),
        }
    }

    #[test]
    fn intercept_turn_interrupt_reports_coordinator_routing() {
        let action = build_claude_intercept_action(
            &json!({"id": 60, "method": "turn/interrupt"}),
            "t1",
            "w1",
            None,
            None,
        );
        match action {
            InterceptAction::Respond(v) => {
                assert_eq!(v["id"], 60);
                assert!(v["error"]["message"]
                    .as_str()
                    .unwrap_or("")
                    .contains("coordinator"));
            }
            _ => panic!("Expected Respond"),
        }
    }

    #[test]
    fn intercept_notification_without_id_drops() {
        for method in &["thread/start", "thread/list", "model/list", "turn/interrupt", "skills/list", "account/read"] {
            let action = build_claude_intercept_action(
                &json!({"method": method}),
                "t1",
                "w1",
                None,
            None,
            );
            assert!(
                matches!(action, InterceptAction::Drop),
                "Expected Drop for notification {method} without id"
            );
        }
    }

    #[test]
    fn intercept_turn_start_empty_input_no_id_drops() {
        let action = build_claude_intercept_action(
            &json!({
                "method": "turn/start",
                "params": { "input": [] }
            }),
            "t1",
            "w1",
            None,
            None,
        );
        assert!(matches!(action, InterceptAction::Drop));
    }

    #[test]
    fn intercept_unknown_method_no_id_drops() {
        let action = build_claude_intercept_action(
            &json!({"method": "completely/unknown"}),
            "t1",
            "w1",
            None,
            None,
        );
        assert!(matches!(action, InterceptAction::Drop));
    }

    #[test]
    fn extract_user_text_multiple_text_items_joined() {
        let params = json!({
            "input": [
                { "type": "text", "text": "Hello" },
                { "type": "text", "text": "World" }
            ]
        });
        assert_eq!(extract_user_text(&params), "Hello\nWorld");
    }

    #[test]
    fn extract_user_text_prefers_input_over_text() {
        let params = json!({
            "input": [{ "type": "text", "text": "from input" }],
            "text": "from text field"
        });
        assert_eq!(extract_user_text(&params), "from input");
    }

    #[test]
    fn format_model_display_name_single_segment() {
        assert_eq!(format_model_display_name("claude"), "Claude");
    }

    #[test]
    fn format_model_display_name_with_non_date_suffix() {
        assert_eq!(
            format_model_display_name("claude-sonnet-4-beta"),
            "Claude Sonnet 4 Beta"
        );
    }

    #[test]
    fn format_model_display_name_empty() {
        assert_eq!(format_model_display_name(""), "");
    }

    #[test]
    fn intercept_turn_start_with_text_field_reports_coordinator_routing() {
        let action = build_claude_intercept_action(
            &json!({
                "id": 70,
                "method": "turn/start",
                "params": { "text": "Simple text" }
            }),
            "t1",
            "w1",
            None,
            None,
        );
        match action {
            InterceptAction::Respond(v) => {
                assert!(v["error"]["message"]
                    .as_str()
                    .unwrap_or("")
                    .contains("coordinator"));
            }
            _ => panic!("Expected Respond"),
        }
    }

    #[test]
    fn intercept_initialize_without_id_drops() {
        let action = build_claude_intercept_action(
            &json!({"method": "initialize"}),
            "t1",
            "w1",
            None,
            None,
        );
        assert!(matches!(action, InterceptAction::Drop));
    }
}
