# Claude Bridge — Current State

> LLM-oriented reference. Structured for fast context loading: flat key-value facts, explicit enum values, deterministic lookup paths. No prose filler.

## Status: WORKING

Last updated: 2026-03-18
Last commit: `ef30fb3` fix: deduplicate format_model_name, process death notification, cross-platform dummy child, stdin alloc
Tests: 393 passed, 0 failed (`cd src-tauri && CARGO_TARGET_DIR=D:/tmp/cam-target LIBCLANG_PATH=D:/LLVM/bin cargo test -p codex-monitor --lib`)

## Module Boundary

All Claude-specific code lives in `src-tauri/src/claude_bridge/`. Zero modifications to `backend/`, `codex/`, `shared/`, or frontend for Claude features. The `request_interceptor` field on `WorkspaceSession` (defined in `backend/app_server.rs`) is the sole integration point — `None` for Codex, `Some(closure)` for Claude.

## Files

| File | Lines | Role |
|---|---|---|
| `mod.rs` | 5 | Module declarations only |
| `types.rs` | ~837 | ClaudeEvent enum (13 variants), BridgeState, ControlRequestData, PendingControlRequest |
| `event_mapper.rs` | ~647 | Maps ClaudeEvent → Vec<Codex JSON-RPC Value>. Pure function, no I/O |
| `event_mapper_tests.rs` | ~1473 | Tests for event_mapper (extracted from event_mapper.rs) |
| `process.rs` | ~1754 | spawn_claude_session(), stdin_writer_task(), stdout_reader_task(), interceptor, NDJSON builders |
| `item_tracker.rs` | ~711 | ToolCategory enum, classify_tool(), ItemInfo, heuristic classification for MCP/unknown tools |
| `history.rs` | ~695 | read_claude_sessions(), read_session_items(), discover_models(), group_items_into_turns() |
| **Total** | **6122** | |

## Architecture

```
spawn_claude_session() → persistent `claude` process (one per workspace session)
  │
  ├── stdin_writer_task (tokio::spawn)
  │     owner: ChildStdin
  │     input: mpsc::UnboundedReceiver<StdinMessage>
  │     writes: NDJSON lines to claude stdin
  │
  ├── stdout_reader_task (tokio::spawn)
  │     reads: BufReader<ChildStdout>.lines()
  │     parses: serde_json::from_str::<ClaudeEvent>
  │     calls: event_mapper::map_event() under bridge_state lock
  │     emits: AppServerEvent via EventSink
  │
  ├── stderr_reader_task (tokio::spawn)
  │     emits: codex/stderr events
  │
  └── request_interceptor (sync closure, Arc<dyn Fn(Value) -> InterceptAction>)
        captures: stdin_tx (mpsc::UnboundedSender), bridge_state, event_sink, sessions, detected_model
        never returns Forward — always Respond or Drop
        handles: turn/start, turn/steer, turn/interrupt, thread/list, thread/resume, approval responses, all other methods
```

## Shared State

```
bridge_state: Arc<std::sync::Mutex<BridgeState>>
  ├── per-session (preserved across turns):
  │     workspace_id, thread_id, model, thread_started
  │     total_input_tokens, total_output_tokens, total_cost_usd
  │     pending_control_requests: HashMap<u64, PendingControlRequest>
  │     approval_id_counter: u64 (starts at 100_000)
  │     claude_session_id: Option<String>
  │
  └── per-turn (cleared by new_turn() / new_turn_with_id()):
        turn_id, turn_started
        block_items: HashMap<u64, String>
        block_item_payloads: HashMap<u64, Value>
        tool_items: HashMap<String, ItemInfo>
        block_tool_use_ids: HashMap<u64, String>
        last_assistant_msg_id, last_assistant_block_count
        accumulated_text

detected_model: Arc<std::sync::Mutex<Option<String>>>
  written by: stdout_reader_task (from BridgeState after map_event)
  read by: interceptor (for model/list response)

sessions: Arc<std::sync::Mutex<Vec<ClaudeSession>>>
  loaded at: spawn time (read_claude_sessions)
  refreshed: after each result event
  read by: interceptor (thread/list, thread/resume)
```

## StdinMessage Enum

```rust
enum StdinMessage {
    UserMessage { text: String, uuid: String, session_id: String },  // → NDJSON {"type":"user","message":{...},"session_id":"..."}
    ControlResponse(String),                      // → pre-serialized NDJSON control_response
    Interrupt,                                    // → NDJSON {"type":"control_request","request":{"subtype":"interrupt"}}
}
```

## ClaudeEvent Enum (input from CLI stdout)

```
system            → thread/started (once)
message_start     → turn/started
content_block_start(text)      → item/started {type: agentMessage}
content_block_start(thinking)  → item/started {type: reasoning}
content_block_start(tool_use)  → item/started {type: per classify_tool()}
content_block_start(tool_result) → output delta for original tool item
content_block_delta(text)      → item/agentMessage/delta
content_block_delta(thinking)  → item/reasoning/textDelta
content_block_delta(input_json)→ item/commandExecution/outputDelta
content_block_stop             → item/completed
message_delta(usage)           → thread/tokenUsage/updated
message_stop                   → (no-op)
result                         → turn/completed + tokenUsage + rateLimits + thread/name + new_turn()
control_request                → codex/requestApproval OR item/tool/requestUserInput
stream_event                   → unwrap inner → recursive map_event()
rate_limit_event               → (ignored)
unknown                        → (ignored)
```

## Implemented Workflows

### 1. USER_MESSAGE

```
trigger: JSON-RPC {method: "turn/start" | "turn/steer", params: {input: [{type:"message",content:[{type:"input_text",text:"..."}]}]}}
interceptor:
  1. extract_user_text(params) → text
  2. bridge_state.new_turn_with_id(turn_id)
  3. emit item/started {type: userMessage}
  4. stdin_tx.send(UserMessage{text, uuid})
  5. return Respond({turn: {id, threadId}})
stdin_writer_task:
  → {"type":"user","message":{"role":"user","content":"..."},"uuid":"...","parent_tool_use_id":null,"session_id":""}
stdout_reader_task:
  → system → message_start → content_block_start/delta/stop → message_delta → result
  → each mapped to Codex events via event_mapper
```

### 2. TOOL_APPROVAL (permission prompts)

```
trigger: CLI stdout {"type":"control_request","request_id":"...","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{...}}}
stdout_reader_task → event_mapper::map_control_request():
  1. approval_id = state.next_approval_id()  // 100_000, 100_001, ...
  2. state.pending_control_requests.insert(approval_id, PendingControlRequest{...})
  3. emit {id: approval_id, method: "codex/requestApproval", params: {command: [tool, cmd], tool, input, description}}

frontend shows approval toast → user clicks accept/decline

frontend → JSON-RPC {id: 100001, result: {decision: "accept"}}  // or "decline"
interceptor:
  1. value.get("result").is_some() && method.is_empty()  // response detection
  2. pending = state.pending_control_requests.remove(100001)
  3. build_tool_approval_response(pending, result)
     accept → {"type":"control_response","response":{"subtype":"success","request_id":"...","response":{"behavior":"allow"}}}
     decline → {"type":"control_response","response":{"subtype":"success","request_id":"...","response":{"behavior":"deny","message":"User denied this action"}}}
  4. stdin_tx.send(ControlResponse(ndjson))
  5. return Respond({id: 100001, result: {ok: true}})
```

### 3. ASK_USER_QUESTION

```
trigger: CLI stdout control_request with tool_name == "AskUserQuestion"
event_mapper:
  → emit {id: approval_id, method: "item/tool/requestUserInput", params: {questions: [...]}}

frontend shows modal → user answers

frontend → JSON-RPC {id: 100002, result: {answers: [...]}}
interceptor:
  → build_ask_user_response(pending, result)
  → {"type":"control_response","response":{"subtype":"success","request_id":"...","response":{"behavior":"allow","result":{"result":[answers]}}}}
  → stdin_tx.send(ControlResponse(ndjson))
```

### 4. INTERRUPT

```
frontend → JSON-RPC {method: "turn/interrupt"}
interceptor:
  → stdin_tx.send(Interrupt)
  → return Respond({ok: true})
stdin_writer_task:
  → {"type":"control_request","request_id":"...","request":{"subtype":"interrupt"}}
```

### 5. SESSION_HISTORY (thread/list)

```
interceptor:
  → reads sessions from Arc<Mutex<Vec<ClaudeSession>>>
  → returns current active session + all sessions from ~/.claude/projects/<encoded_path>/sessions/*.jsonl
  → each session: {id, name, preview, status, createdAt, updatedAt, cwd}
```

### 6. SESSION_RESUME (thread/resume)

```
interceptor:
  → read_session_items(workspace_path, threadId)
  → parses JSONL files from Claude's session directory
  → reconstructs items: userMessage, agentMessage, tool calls with input/output
  → returns {threadId, thread: {id, status, preview, turns: [{id, status, items}]}}
```

### 7. CRASH_RECOVERY

```
stdout_reader_task: BufReader.lines() returns None (process exited)
  1. under lock: extract (thread_id, turn_id, turn_started), clear pending_control_requests
  2. drop lock
  3. if turn_started: emit turn/completed {status: "error"}
  4. emit thread/status/changed {type: "idle"}
  5. emit codex/disconnected {reason: "Claude CLI process exited"}
  → UI shows reconnect state (not just silent idle)
  → next user message triggers new spawn via codex/ layer
```

### 8. STREAMING (text, thinking, tool input/output)

```
content_block_start → creates item in block_items + block_item_payloads (or tool_items for tool_use)
content_block_delta → appends to accumulated state, emits delta event
content_block_stop → emits item/completed with final payload
stream_event → unwraps inner event, recursive map_event()

tool lifecycle:
  content_block_start(tool_use) → item_tracker::build_item_started()
  content_block_delta(input_json) → accumulate in ItemInfo.accumulated_input_json + emit delta
  content_block_stop → item_tracker::build_item_completed() with parsed input
  content_block_start(tool_result) → extract text → emit output delta for original tool item
```

### 9. TOKEN_USAGE + COST

```
message_delta(usage) → thread/tokenUsage/updated (per-message granular)
result(usage, cost_usd) →
  state.total_input_tokens += usage.input_tokens
  state.total_output_tokens += usage.output_tokens
  state.total_cost_usd += cost_usd
  emit thread/tokenUsage/updated {last: {...}, total: {...}}
  emit account/rateLimits/updated {credits: {balance: "$X.XX spent"}}
```

## Tool Classification

```
classify_tool(name) → ToolCategory:
  bash|execute_command|shell|run_command|Bash → CommandExecution → "commandExecution"
  write_file|edit_file|str_replace_editor|create_file|Write|Edit|NotebookEdit → FileChange → "fileChange"
  read_file|Read|Glob|Grep|WebFetch|WebSearch → FileRead → "commandExecution"
  * → Other → "commandExecution" (with heuristic reclassification)

Heuristic reclassification for unknown/MCP tools (Other category):
  infer_category_from_input(input):
    has path + has content (old_string/new_string/content) → FileChange
    has command field → CommandExecution
    else → Other

  infer_command_from_input(tool_name, input):
    input.command → command string
    input.file_path or input.path → "tool_name: path"
    input.query or input.pattern → "tool_name: query"
    input.url → "tool_name: url"
    else → None
```

## CLI Spawn Args

```
claude --print --input-format stream-json --output-format stream-json --verbose --include-partial-messages
  cwd: workspace_path
  env_remove: CLAUDECODE
  stdin: piped
  stdout: piped
  stderr: piped
```

## Dummy ChildStdin

WorkspaceSession requires ChildStdin. Real writes go through mpsc channel. Dummy provided by:
```
#[cfg(windows)]   cmd /c exit 0
#[cfg(not(windows))]  true
```
Interceptor never returns `Forward`, so dummy stdin is never written to.

## Frontend Integration Points

No frontend changes needed. Existing handlers match:
- `codex/requestApproval` → detected by `method.endsWith("requestApproval")` → approval toast
- `item/tool/requestUserInput` → detected by method match → user input modal
- `respondToServerRequest(id, result)` → Tauri invoke → `respond_to_server_request_core` → `session.send_response(id, result)` → `write_message({id, result})` → interceptor

## Known Bugs Fixed

### Commit `1679ef2` — approval decline, lock scopes, pending cleanup
1. **CRITICAL**: `build_tool_approval_response` checked `result.get("denied")` — field never existed. Frontend sends `{decision: "accept"|"decline"}`. Fixed.
2. **MEDIUM**: Lock held across `emit_app_server_event()` in crash recovery and parse-error paths. Fixed: extract state under lock, drop lock, then emit.
3. **MEDIUM**: `pending_control_requests` never cleared on process exit → memory leak. Fixed: clear in crash recovery.
4. **LOW**: `new_turn()` generated UUID immediately overwritten by caller. Fixed: added `new_turn_with_id(turn_id)`.

### Commit `ec67ccb` — stale result detection, tool_result completion
5. **MEDIUM**: Stale `result` events from previous turns processed as current turn. Fixed: skip `result` when `turn_started == false`.
6. **LOW**: `content_block_stop` for tool_result blocks emitted spurious `item/completed`. Fixed: track tool_result blocks in `block_tool_use_ids`.

### Commit `d0b38ab` — session_id passthrough, deny reason
7. **MEDIUM**: `session_id` from user messages not passed through to Claude CLI. Fixed: added `session_id` field to `StdinMessage::UserMessage`.
8. **LOW**: Decline reason was empty string. Fixed: "User denied this action".

### Commits `b0748fe`, `6950a22` — history/resume fixes
9. **MEDIUM**: `thread/list` returned stale sessions (loaded once at spawn). Fixed: refresh from disk on every `thread/list`.
10. **MEDIUM**: O(n²) `collect_assistant_text` called per-message. Fixed: pre-group by message.id in HashMap.
11. **LOW**: `read_session_name_from_jsonl` aborted on single I/O error. Fixed: continue on error.
12. **MEDIUM**: `thread/resume` returned all items in single turn. Fixed: `group_items_into_turns` splits at userMessage boundaries.
13. **LOW**: `discover_models` could scan unbounded files. Fixed: caps `MAX_PROJECT_DIRS=20`, `MAX_FILES_PER_DIR=5`, `MAX_LINES_PER_FILE=10`.

### Commits `bb38e1d`, `858288f` — item tracker improvements
14. **MEDIUM**: `command` field empty for non-Bash tools (Read, Grep, etc.). Fixed: `extract_command` returns descriptions for all known tools.
15. **LOW**: MCP/unknown tools had no command or category heuristics. Fixed: `infer_command_from_input` + `infer_category_from_input`.

### Commit `ef30fb3` — process lifecycle fixes
16. **MEDIUM**: Duplicate `format_model_display_name` in process.rs (copy of history.rs `format_model_name`). Fixed: removed, reuse `pub(crate) format_model_name`.
17. **MEDIUM**: No notification when CLI process dies between turns. Fixed: emit `codex/disconnected` after `thread/status/changed(idle)`.
18. **LOW**: Dummy child `cmd /c exit 0` Windows-only. Fixed: `#[cfg(windows)]` / `#[cfg(not(windows))]`.
19. **LOW**: `stdin_writer_task` extra String allocation per write. Fixed: `line.into_bytes()` + `push(b'\n')`.

## Not Yet Implemented

- Multi-turn context within persistent process (process persists, but no `--resume` flag — each session is fresh)
- MCP server tool proxying (MCP tools appear as regular tool_use, heuristic classification added but no protocol-level MCP support)
- Process restart on crash (UI now gets `codex/disconnected` event, but auto-restart not implemented — user must send new message)
- Explicit session_id pass-through to `--resume` for continuing Claude sessions across app restarts
- Rate limit event handling (currently ignored)
- Permission mode configuration (always uses Claude CLI default)
