use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, Mutex};

use crate::backend::app_server::{InterceptAction, WorkspaceSession};
use crate::backend::events::{AppServerEvent, EventSink};
use crate::shared::process_core::tokio_command;
use crate::types::WorkspaceEntry;

use super::event_mapper;
use super::history::{
    discover_models, format_model_name, read_claude_sessions, read_session_items, ClaudeSession,
};
use super::types::BridgeState;

/// Sentinel error returned when the Claude CLI process has exited unexpectedly.
pub(crate) const PROCESS_EXITED_ERROR: &str = "CLAUDE_PROCESS_EXITED";

/// Messages sent to the stdin writer task.
enum StdinMessage {
    /// A new user prompt for a conversation turn.
    UserMessage {
        text: String,
        uuid: String,
        session_id: String,
    },
    /// A pre-serialized NDJSON line (control_response).
    ControlResponse(String),
    /// Interrupt the current turn.
    Interrupt,
}

/// Check that the `claude` CLI binary is available.
#[tauri::command]
pub(crate) async fn check_claude_installation() -> Result<String, String> {
    let result = tokio_command("claude")
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

/// Spawn a persistent Claude CLI session that presents the same `WorkspaceSession`
/// interface as the Codex backend. The bridge translates between the
/// Codex JSON-RPC protocol and Claude CLI's bidirectional stream-json format.
///
/// Architecture: **persistent process**. One `claude --print --input-format stream-json`
/// process lives for the entire session. User messages, control responses, and
/// interrupts are sent via an mpsc channel to a dedicated stdin writer task.
/// A stdout reader task parses NDJSON events and emits Codex-compatible events.
pub(crate) async fn spawn_claude_session<E: EventSink>(
    entry: WorkspaceEntry,
    _client_version: String,
    event_sink: E,
) -> Result<Arc<WorkspaceSession>, String> {
    let _ = check_claude_installation().await?;

    let thread_id = format!("thread_{}", uuid::Uuid::new_v4());
    let workspace_id = entry.id.clone();
    let workspace_path = entry.path.clone();

    // Shared session history: loaded at startup.
    let sessions: Arc<std::sync::Mutex<Vec<ClaudeSession>>> = Arc::new(
        std::sync::Mutex::new(read_claude_sessions(&workspace_path)),
    );

    // Shared detected model: updated by stdout reader, read by interceptor.
    let detected_model: Arc<std::sync::Mutex<Option<String>>> =
        Arc::new(std::sync::Mutex::new(None));

    // Shared bridge state (protected by std::sync::Mutex for sync interceptor access).
    let bridge_state: Arc<std::sync::Mutex<BridgeState>> = Arc::new(
        std::sync::Mutex::new(BridgeState::new(
            workspace_id.clone(),
            thread_id.clone(),
            format!("turn_{}", uuid::Uuid::new_v4()),
        )),
    );

    // Channel: interceptor → stdin writer task
    let (stdin_tx, stdin_rx) = mpsc::unbounded_channel::<StdinMessage>();

    // Process liveness flag: set by stdout_reader_task on EOF, checked by interceptor.
    let process_exited = Arc::new(AtomicBool::new(false));

    // Resolve rules path for auto-approve checks.
    let rules_path = crate::codex::home::resolve_default_codex_home()
        .map(|home| crate::rules::default_rules_path(&home));

    // Spawn the persistent Claude CLI process.
    let mut child = tokio_command("claude")
        .args([
            "--print",
            "--input-format", "stream-json",
            "--output-format", "stream-json",
            "--verbose",
            "--include-partial-messages",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .current_dir(&workspace_path)
        .env_remove("CLAUDECODE")
        .spawn()
        .map_err(|e| format!("Failed to spawn claude: {e}"))?;

    let child_stdin = child.stdin.take().ok_or("missing claude stdin")?;
    let child_stdout = child.stdout.take().ok_or("missing claude stdout")?;
    let child_stderr = child.stderr.take();

    // Spawn stdin writer task (sole owner of ChildStdin).
    tokio::spawn(stdin_writer_task(child_stdin, stdin_rx));

    // Spawn stderr reader task.
    let stderr_event_sink = event_sink.clone();
    let stderr_ws_id = workspace_id.clone();
    tokio::spawn(async move {
        if let Some(stderr) = child_stderr {
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

    // Spawn stdout reader task.
    let stdout_bridge_state = bridge_state.clone();
    let stdout_event_sink = event_sink.clone();
    let stdout_ws_id = workspace_id.clone();
    let stdout_detected_model = detected_model.clone();
    let stdout_sessions = sessions.clone();
    let stdout_workspace_path = workspace_path.clone();
    let stdout_process_exited = process_exited.clone();
    let stdout_stdin_tx = stdin_tx.clone();
    let stdout_rules_path = rules_path;
    tokio::spawn(stdout_reader_task(
        child_stdout,
        stdout_bridge_state,
        stdout_event_sink,
        stdout_ws_id,
        stdout_detected_model,
        stdout_sessions,
        stdout_workspace_path,
        stdout_process_exited,
        stdout_stdin_tx,
        stdout_rules_path,
    ));

    // Spawn a dummy child process to satisfy WorkspaceSession's type requirements.
    #[cfg(windows)]
    let mut dummy_child = tokio_command("cmd")
        .args(["/c", "exit", "0"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to spawn dummy process: {e}"))?;
    #[cfg(not(windows))]
    let mut dummy_child = tokio_command("true")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to spawn dummy process: {e}"))?;
    let dummy_stdin = dummy_child.stdin.take().ok_or("missing dummy stdin")?;

    // Build the request interceptor.
    let interceptor_thread_id = thread_id.clone();
    let interceptor_workspace_id = workspace_id.clone();
    let interceptor_workspace_path = workspace_path.clone();
    let interceptor_event_sink = event_sink.clone();
    let detected_model_for_interceptor = detected_model.clone();
    let sessions_for_interceptor = sessions.clone();
    let interceptor_bridge_state = bridge_state.clone();
    let interceptor_stdin_tx = stdin_tx.clone();
    let interceptor_process_exited = process_exited.clone();

    let interceptor: Arc<dyn Fn(Value) -> InterceptAction + Send + Sync> =
        Arc::new(move |value: Value| {
            let method = value.get("method").and_then(|v| v.as_str()).unwrap_or("");
            let id = value.get("id").cloned();
            let params = value.get("params").cloned().unwrap_or(Value::Null);

            // ── Handle response messages (approval/userInput answers) ──
            if value.get("result").is_some() && method.is_empty() {
                if let Some(resp_id) = value.get("id").and_then(|v| v.as_u64()) {
                    // Check process liveness FIRST, before consuming pending entry.
                    if interceptor_process_exited.load(Ordering::Acquire) {
                        return InterceptAction::Respond(json!({
                            "id": resp_id,
                            "error": { "message": PROCESS_EXITED_ERROR }
                        }));
                    }

                    // Remove pending entry under lock, then release before serialization.
                    let pending = {
                        let mut bs = interceptor_bridge_state.lock().unwrap();
                        bs.pending_control_requests.remove(&resp_id)
                    };
                    if let Some(pending) = pending {
                        let result_val = value.get("result").cloned().unwrap_or(Value::Null);
                        let ndjson = build_control_response(&pending, &result_val);
                        let _ = interceptor_stdin_tx.send(StdinMessage::ControlResponse(ndjson));
                        return InterceptAction::Respond(json!({
                            "id": resp_id,
                            "result": {"ok": true}
                        }));
                    }
                }
                eprintln!(
                    "claude_bridge: dropping response for unknown approval id={:?} (stale or duplicate)",
                    value.get("id")
                );
                return InterceptAction::Drop;
            }

            match method {
                "turn/start" | "turn/steer" => {
                    // ── Dead process check ──
                    if interceptor_process_exited.load(Ordering::Acquire) {
                        return if let Some(id) = id {
                            InterceptAction::Respond(json!({
                                "id": id,
                                "error": { "message": PROCESS_EXITED_ERROR }
                            }))
                        } else {
                            InterceptAction::Drop
                        };
                    }

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

                    // For turn/steer, interrupt the current turn first so
                    // Claude CLI doesn't receive a user message mid-processing.
                    if method == "turn/steer" {
                        let _ = interceptor_stdin_tx.send(StdinMessage::Interrupt);
                    }

                    let tid = params
                        .get("threadId")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&interceptor_thread_id)
                        .to_string();
                    let turn_id = format!("turn_{}", uuid::Uuid::new_v4());
                    let uuid = uuid::Uuid::new_v4().to_string();

                    // Reset bridge state for new turn and mark turn as started
                    // immediately. This ensures map_result() emits turn/completed
                    // even if Claude errors before sending message_start.
                    let session_id = {
                        let mut bs = interceptor_bridge_state.lock().unwrap();
                        bs.thread_id = tid.clone();
                        bs.new_turn_with_id(turn_id.clone());
                        bs.turn_started = true;
                        bs.claude_session_id.clone().unwrap_or_default()
                    };

                    // Emit turn/started so frontend tracks this turn.
                    interceptor_event_sink.emit_app_server_event(AppServerEvent {
                        workspace_id: interceptor_workspace_id.clone(),
                        message: json!({
                            "method": "turn/started",
                            "params": {
                                "threadId": &tid,
                                "turnId": &turn_id
                            }
                        }),
                    });

                    // Emit user message so it appears in the thread
                    let user_item_id = format!("user_{turn_id}");
                    interceptor_event_sink.emit_app_server_event(AppServerEvent {
                        workspace_id: interceptor_workspace_id.clone(),
                        message: json!({
                            "method": "item/started",
                            "params": {
                                "threadId": &tid,
                                "turnId": &turn_id,
                                "item": {
                                    "id": user_item_id,
                                    "type": "userMessage",
                                    "content": [
                                        { "type": "text", "text": &text }
                                    ]
                                }
                            }
                        }),
                    });

                    // Send user message to the persistent process via channel
                    let _ = interceptor_stdin_tx.send(StdinMessage::UserMessage {
                        text,
                        uuid,
                        session_id,
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
                    if !interceptor_process_exited.load(Ordering::Acquire) {
                        let _ = interceptor_stdin_tx.send(StdinMessage::Interrupt);
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
                        // Refresh sessions from disk so newly created
                        // sessions (e.g. from CLI or VS Code) are visible.
                        let hist = {
                            let mut guard = sessions_for_interceptor
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            *guard = read_claude_sessions(&interceptor_workspace_path);
                            guard.clone()
                        };
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
                        let session_name = sessions_for_interceptor
                            .lock()
                            .ok()
                            .and_then(|sessions| {
                                sessions.iter().find(|s| s.session_id == tid).map(|s| s.name.clone())
                            });
                        let preview = session_name.unwrap_or_else(|| "New conversation".to_string());
                        let items = read_session_items(&interceptor_workspace_path, tid);
                        // Group flat items into turns: each userMessage starts a new turn.
                        let turns = group_items_into_turns(tid, items);
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

    // Emit codex/connected immediately so the UI knows the session is ready.
    event_sink.emit_app_server_event(AppServerEvent {
        workspace_id: workspace_id.clone(),
        message: json!({
            "method": "codex/connected",
            "params": { "workspaceId": workspace_id }
        }),
    });

    Ok(session)
}

/// Stdin writer task: sole owner of ChildStdin. Receives messages via channel
/// and writes them as NDJSON lines.
async fn stdin_writer_task(
    mut stdin: tokio::process::ChildStdin,
    mut rx: mpsc::UnboundedReceiver<StdinMessage>,
) {
    while let Some(msg) = rx.recv().await {
        let line = match msg {
            StdinMessage::UserMessage {
                text,
                uuid,
                session_id,
            } => build_user_message(&text, &uuid, &session_id),
            StdinMessage::ControlResponse(ndjson) => ndjson,
            StdinMessage::Interrupt => build_interrupt_request(),
        };
        let mut buf = line.into_bytes();
        buf.push(b'\n');
        if stdin.write_all(&buf).await.is_err() {
            break;
        }
        if stdin.flush().await.is_err() {
            break;
        }
    }
}

/// Stdout reader task: reads NDJSON lines, parses as ClaudeEvent, maps to
/// Codex events, and emits them via event_sink.
async fn stdout_reader_task<E: EventSink>(
    stdout: tokio::process::ChildStdout,
    bridge_state: Arc<std::sync::Mutex<BridgeState>>,
    event_sink: E,
    workspace_id: String,
    detected_model: Arc<std::sync::Mutex<Option<String>>>,
    sessions: Arc<std::sync::Mutex<Vec<ClaudeSession>>>,
    workspace_path: String,
    process_exited: Arc<AtomicBool>,
    stdin_tx: mpsc::UnboundedSender<StdinMessage>,
    rules_path: Option<std::path::PathBuf>,
) {
    let mut lines = BufReader::new(stdout).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }

        let claude_event: super::types::ClaudeEvent = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(err) => {
                let (tid, tuid) = {
                    let bs = bridge_state.lock().unwrap();
                    (bs.thread_id.clone(), bs.turn_id.clone())
                };
                event_sink.emit_app_server_event(AppServerEvent {
                    workspace_id: workspace_id.clone(),
                    message: json!({
                        "method": "error",
                        "params": {
                            "threadId": tid,
                            "turnId": tuid,
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

        let codex_messages = {
            let mut bs = bridge_state.lock().unwrap();

            // Capture session_id from system init
            if let super::types::ClaudeEvent::System(ref sys) = claude_event {
                if let Some(ref sid) = sys.session_id {
                    bs.claude_session_id = Some(sid.clone());
                }
            }

            let messages = event_mapper::map_event(&claude_event, &mut bs);

            // Propagate detected model to interceptor
            if let Some(ref model) = bs.model {
                if let Ok(mut guard) = detected_model.lock() {
                    if guard.as_ref() != Some(model) {
                        *guard = Some(model.clone());
                    }
                }
            }

            messages
        };

        // Ack non-can_use_tool control_requests so Claude CLI doesn't hang.
        if let super::types::ClaudeEvent::ControlRequest(ref cr) = claude_event {
            let subtype = cr.request.get("subtype").and_then(|v| v.as_str()).unwrap_or("");
            if subtype != "can_use_tool" && !cr.request_id.is_empty() {
                let ack = json!({
                    "type": "control_response",
                    "response": {
                        "subtype": "success",
                        "request_id": cr.request_id,
                        "response": {}
                    }
                });
                let _ = stdin_tx.send(StdinMessage::ControlResponse(
                    serde_json::to_string(&ack).unwrap_or_default(),
                ));
            }
        }

        // Auto-approve tool calls that match remembered prefix rules.
        let codex_messages = if let Some(ref rp) = rules_path {
            let mut filtered = Vec::with_capacity(codex_messages.len());
            for msg in codex_messages {
                if msg.get("method").and_then(|v| v.as_str()) == Some("codex/requestApproval") {
                    if let Some(approval_id) = msg.get("id").and_then(|v| v.as_u64()) {
                        let cmd: Vec<&str> = msg["params"]["command"]
                            .as_array()
                            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
                            .unwrap_or_default();
                        if !cmd.is_empty() && crate::rules::check_prefix_rules(rp, &cmd) {
                            let mut bs = bridge_state.lock().unwrap();
                            if let Some(pending) = bs.pending_control_requests.remove(&approval_id) {
                                let ndjson = build_control_response(
                                    &pending,
                                    &json!({"decision": "accept"}),
                                );
                                let _ = stdin_tx.send(StdinMessage::ControlResponse(ndjson));
                            }
                            continue; // Don't emit — tool proceeds silently
                        }
                    }
                }
                filtered.push(msg);
            }
            filtered
        } else {
            codex_messages
        };

        for message in &codex_messages {
            event_sink.emit_app_server_event(AppServerEvent {
                workspace_id: workspace_id.clone(),
                message: message.clone(),
            });
        }

        // After a result event, refresh session history
        if matches!(claude_event, super::types::ClaudeEvent::Result(_)) {
            if let Ok(mut sess) = sessions.lock() {
                *sess = read_claude_sessions(&workspace_path);
            }
        }
    }

    // Signal process death BEFORE locking bridge_state, so the interceptor
    // rejects new requests immediately instead of accepting a doomed turn.
    process_exited.store(true, Ordering::Release);

    // Process exited — extract state and clean up under lock, then emit events.
    let (thread_id, turn_id, turn_started) = {
        let mut bs = bridge_state.lock().unwrap();
        let state = (bs.thread_id.clone(), bs.turn_id.clone(), bs.turn_started);
        // Clean up pending control requests — process is gone, they can never complete.
        bs.pending_control_requests.clear();
        state
    }; // Lock released here.

    if turn_started {
        event_sink.emit_app_server_event(AppServerEvent {
            workspace_id: workspace_id.clone(),
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

    event_sink.emit_app_server_event(AppServerEvent {
        workspace_id: workspace_id.clone(),
        message: json!({
            "method": "thread/status/changed",
            "params": {
                "threadId": thread_id,
                "status": { "type": "idle" }
            }
        }),
    });

    // Notify frontend that the session is gone so it can show reconnect UI
    // instead of silently failing on the next user message.
    event_sink.emit_app_server_event(AppServerEvent {
        workspace_id,
        message: json!({
            "method": "codex/disconnected",
            "params": {
                "reason": "Claude CLI process exited"
            }
        }),
    });
}

/// Build a Claude NDJSON user message.
fn build_user_message(text: &str, uuid: &str, session_id: &str) -> String {
    let msg = json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": text
        },
        "uuid": uuid,
        "parent_tool_use_id": null,
        "session_id": session_id
    });
    serde_json::to_string(&msg).unwrap_or_default()
}

/// Build a Claude NDJSON interrupt control_request.
fn build_interrupt_request() -> String {
    let msg = json!({
        "type": "control_request",
        "request_id": uuid::Uuid::new_v4().to_string(),
        "request": {
            "subtype": "interrupt"
        }
    });
    serde_json::to_string(&msg).unwrap_or_default()
}

/// Build a Claude NDJSON control_response for a pending control request.
fn build_control_response(
    pending: &super::types::PendingControlRequest,
    result: &Value,
) -> String {
    let msg = if pending.tool_name == "AskUserQuestion" {
        build_ask_user_response(pending, result)
    } else {
        build_tool_approval_response(pending, result)
    };
    serde_json::to_string(&msg).unwrap_or_default()
}

/// Build control_response for tool approval (allow/deny).
///
/// The frontend sends `{"decision": "accept"}` or `{"decision": "decline"}`.
fn build_tool_approval_response(
    pending: &super::types::PendingControlRequest,
    result: &Value,
) -> Value {
    let decision = result.get("decision").and_then(|v| v.as_str()).unwrap_or("accept");
    let denied = decision == "decline";

    if denied {
        let reason = result
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("User denied this action");
        json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": pending.claude_request_id,
                "response": {
                    "behavior": "deny",
                    "message": reason
                }
            }
        })
    } else {
        json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": pending.claude_request_id,
                "response": {
                    "behavior": "allow",
                    "updatedInput": null
                }
            }
        })
    }
}

/// Build control_response for AskUserQuestion.
fn build_ask_user_response(
    pending: &super::types::PendingControlRequest,
    result: &Value,
) -> Value {
    // The frontend sends answers in result.answers: {question: answer}
    let answers = result.get("answers").cloned().unwrap_or(json!({}));

    // Reconstruct the original input with answers merged
    let mut updated_input = pending.request.get("input").cloned().unwrap_or(json!({}));
    updated_input["answers"] = answers;

    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": pending.claude_request_id,
            "response": {
                "behavior": "allow",
                "updatedInput": updated_input
            }
        }
    })
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
    let _params = value.get("params").cloned().unwrap_or(Value::Null);

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

        "model/list" => {
            if let Some(id) = id {
                let models = workspace_path
                    .map(|p| discover_models(p))
                    .unwrap_or_default();
                let detected = detected_model.unwrap_or("");
                let data: Vec<Value> = if models.is_empty() {
                    let fallback_id = if detected.is_empty() { "claude-sonnet-4-6" } else { detected };
                    let display = format_model_name(fallback_id);
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

/// Group a flat list of history items into turns.
/// Each `userMessage` starts a new turn; preceding agent/tool items without
/// a user message go into an initial turn.
fn group_items_into_turns(thread_id: &str, items: Vec<Value>) -> Value {
    if items.is_empty() {
        return json!([]);
    }

    let mut turns: Vec<Value> = Vec::new();
    let mut current_items: Vec<Value> = Vec::new();
    let mut turn_counter: u64 = 0;

    for item in items {
        let is_user_msg = item.get("type").and_then(|v| v.as_str()) == Some("userMessage");
        if is_user_msg && !current_items.is_empty() {
            // Flush accumulated items as a completed turn.
            turn_counter += 1;
            turns.push(json!({
                "id": format!("turn-resume-{thread_id}-{turn_counter}"),
                "status": "completed",
                "items": current_items
            }));
            current_items = Vec::new();
        }
        current_items.push(item);
    }

    // Flush remaining items.
    if !current_items.is_empty() {
        turn_counter += 1;
        turns.push(json!({
            "id": format!("turn-resume-{thread_id}-{turn_counter}"),
            "status": "completed",
            "items": current_items
        }));
    }

    json!(turns)
}

#[cfg(test)]
#[path = "process_tests.rs"]
mod tests;
