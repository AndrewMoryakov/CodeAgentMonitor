use serde_json::{json, Value};

use super::item_tracker::{self, ItemInfo};
use super::types::{
    BridgeState, ClaudeEvent, ContentBlock, ContentBlockDelta,
    ControlRequestData, PendingControlRequest,
};

/// Maps a Claude CLI stream-json event to zero or more Codex JSON-RPC
/// notification messages. Returns a `Vec` because some Claude events
/// expand into multiple Codex notifications (e.g. system init →
/// codex/connected + thread/started).
pub(crate) fn map_event(event: &ClaudeEvent, state: &mut BridgeState) -> Vec<Value> {
    match event {
        ClaudeEvent::System(sys) => map_system(sys, state),
        ClaudeEvent::MessageStart(msg) => map_message_start(msg, state),
        ClaudeEvent::ContentBlockStart(cb) => map_content_block_start(cb, state),
        ClaudeEvent::ContentBlockDelta(cbd) => map_content_block_delta(cbd, state),
        ClaudeEvent::ContentBlockStop(cbs) => map_content_block_stop(cbs, state),
        ClaudeEvent::MessageDelta(md) => map_message_delta(md, state),
        ClaudeEvent::MessageStop(_) => map_message_stop(state),
        ClaudeEvent::Result(res) => map_result(res, state),
        ClaudeEvent::Assistant(a) => map_assistant(a, state),
        ClaudeEvent::StreamEvent(wrapper) => map_stream_event(wrapper, state),
        ClaudeEvent::ControlRequest(cr) => map_control_request(cr, state),
        ClaudeEvent::RateLimitEvent(_) => vec![],
        ClaudeEvent::Unknown => vec![],
    }
}

/// Unwrap a `stream_event` wrapper and re-dispatch the inner event.
fn map_stream_event(
    wrapper: &super::types::StreamEventWrapper,
    state: &mut BridgeState,
) -> Vec<Value> {
    let Some(ref inner) = wrapper.event else {
        return vec![];
    };
    // Try to deserialize the inner event as a ClaudeEvent.
    // The inner event has standard types: message_start, content_block_start, etc.
    match serde_json::from_value::<ClaudeEvent>(inner.clone()) {
        Ok(inner_event) => map_event(&inner_event, state),
        Err(_) => vec![],
    }
}

fn map_system(
    sys: &super::types::SystemEvent,
    state: &mut BridgeState,
) -> Vec<Value> {
    let mut out = Vec::new();

    if let Some(ref model) = sys.model {
        state.model = Some(model.clone());
    }

    // NOTE: codex/connected is NOT emitted here — it is already emitted once
    // in spawn_claude_session(). Emitting it on every turn's system event
    // would trigger reconnectLive() in the frontend, resetting thread state.

    // Emit thread/started (once per session)
    if !state.thread_started {
        state.thread_started = true;
        out.push(json!({
            "method": "thread/started",
            "params": {
                "threadId": state.thread_id,
                "thread": {
                    "id": state.thread_id,
                    "name": "New conversation",
                    "status": "active",
                    "source": "appServer"
                }
            }
        }));
    }

    out
}

fn map_message_start(
    msg: &super::types::MessageStartEvent,
    state: &mut BridgeState,
) -> Vec<Value> {
    let mut out = Vec::new();

    if let Some(ref info) = msg.message {
        if let Some(ref model) = info.model {
            state.model = Some(model.clone());
        }
    }

    // Clear per-block maps to avoid index collisions across messages
    // within the same turn (e.g., tool_use message → tool_result message).
    // tool_items is preserved for cross-message tool correlation.
    state.block_items.clear();
    state.block_item_payloads.clear();
    state.block_tool_use_ids.clear();

    // Emit turn/started if not yet done for this turn
    if !state.turn_started {
        state.turn_started = true;
        out.push(json!({
            "method": "turn/started",
            "params": {
                "threadId": state.thread_id,
                "turnId": state.turn_id
            }
        }));
    }

    out
}

fn map_content_block_start(
    cb: &super::types::ContentBlockEvent,
    state: &mut BridgeState,
) -> Vec<Value> {
    let mut out = Vec::new();
    let Some(ref block) = cb.content_block else {
        return out;
    };

    match block {
        ContentBlock::Text { .. } => {
            let item_id = state.next_item();
            state.block_items.insert(cb.index, item_id.clone());
            state.block_item_payloads.insert(
                cb.index,
                json!({
                    "id": item_id,
                    "type": "agentMessage",
                    "status": "in_progress",
                    "text": "",
                }),
            );
            out.push(json!({
                "method": "item/started",
                "params": {
                    "threadId": state.thread_id,
                    "turnId": state.turn_id,
                    "item": {
                        "id": item_id,
                        "type": "agentMessage",
                        "status": "in_progress"
                    }
                }
            }));
        }
        ContentBlock::Thinking { .. } => {
            let item_id = state.next_item();
            state.block_items.insert(cb.index, item_id.clone());
            state.block_item_payloads.insert(
                cb.index,
                json!({
                    "id": item_id,
                    "type": "reasoning",
                    "status": "in_progress",
                    "summary": "",
                    "content": "",
                }),
            );
            out.push(json!({
                "method": "item/started",
                "params": {
                    "threadId": state.thread_id,
                    "turnId": state.turn_id,
                    "item": {
                        "id": item_id,
                        "type": "reasoning",
                        "status": "in_progress"
                    }
                }
            }));
        }
        ContentBlock::ToolUse { id, name, .. } => {
            let item_id = state.next_item();
            state.block_items.insert(cb.index, item_id.clone());

            let category = item_tracker::classify_tool(name);
            let info = ItemInfo {
                item_id: item_id.clone(),
                tool_use_id: id.clone(),
                tool_name: name.clone(),
                category,
                accumulated_input_json: String::new(),
                aggregated_output: String::new(),
            };

            let event = item_tracker::build_item_started(
                &info,
                &state.thread_id,
                &state.turn_id,
            );

            state.block_tool_use_ids.insert(cb.index, id.clone());
            state.tool_items.insert(id.clone(), info);

            out.push(event);
        }
        ContentBlock::ToolResult {
            tool_use_id,
            content,
        } => {
            // Map tool result content to output delta for the original item.
            // Also register in block_tool_use_ids so content_block_stop emits
            // a final item/completed with the full aggregated output.
            if let Some(ref tuid) = tool_use_id {
                state.block_tool_use_ids.insert(cb.index, tuid.clone());

                let result_text = extract_tool_result_text(content.as_ref());
                if !result_text.is_empty() {
                    if let Some(info) = state.tool_items.get_mut(tuid) {
                        info.aggregated_output.push_str(&result_text);
                        out.push(item_tracker::build_output_delta(
                            info,
                            &state.thread_id,
                            &state.turn_id,
                            &result_text,
                        ));
                    }
                }
            }
        }
        _ => {}
    }

    out
}

/// Extract text from a tool result content value.
fn extract_tool_result_text(content: Option<&Value>) -> String {
    let Some(content) = content else {
        return String::new();
    };
    // Content can be a string directly
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    // Or an array of content blocks
    if let Some(arr) = content.as_array() {
        let texts: Vec<&str> = arr
            .iter()
            .filter_map(|item| {
                if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                    item.get("text").and_then(|t| t.as_str())
                } else {
                    None
                }
            })
            .collect();
        return texts.join("\n");
    }
    String::new()
}

fn map_content_block_delta(
    cbd: &super::types::ContentBlockDeltaEvent,
    state: &mut BridgeState,
) -> Vec<Value> {
    let mut out = Vec::new();
    let Some(ref delta) = cbd.delta else {
        return out;
    };
    let item_id = match state.block_items.get(&cbd.index) {
        Some(id) => id.clone(),
        None => return out,
    };

    match delta {
        ContentBlockDelta::TextDelta { text } => {
            // Cap accumulation: only first ~50 chars are used for thread naming.
            if state.accumulated_text.len() < 50 {
                state.accumulated_text.push_str(text);
            }
            if let Some(item) = state.block_item_payloads.get_mut(&cbd.index) {
                let current = item
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                item["text"] = json!(format!("{current}{text}"));
            }
            out.push(json!({
                "method": "item/agentMessage/delta",
                "params": {
                    "threadId": state.thread_id,
                    "turnId": state.turn_id,
                    "itemId": item_id,
                    "delta": text
                }
            }));
        }
        ContentBlockDelta::ThinkingDelta { thinking } => {
            if let Some(item) = state.block_item_payloads.get_mut(&cbd.index) {
                let current = item
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                item["content"] = json!(format!("{current}{thinking}"));
            }
            out.push(json!({
                "method": "item/reasoning/textDelta",
                "params": {
                    "threadId": state.thread_id,
                    "turnId": state.turn_id,
                    "itemId": item_id,
                    "delta": thinking
                }
            }));
        }
        ContentBlockDelta::InputJsonDelta { partial_json } => {
            // Accumulate input JSON in the item tracker
            if let Some(tool_use_id) = state.block_tool_use_ids.get(&cbd.index) {
                if let Some(info) = state.tool_items.get_mut(tool_use_id) {
                    info.accumulated_input_json.push_str(partial_json);
                    out.push(item_tracker::build_output_delta(
                        info,
                        &state.thread_id,
                        &state.turn_id,
                        partial_json,
                    ));
                    return out;
                }
            }
            // Fallback if no tool tracking info found
            out.push(json!({
                "method": "item/commandExecution/outputDelta",
                "params": {
                    "threadId": state.thread_id,
                    "turnId": state.turn_id,
                    "itemId": item_id,
                    "delta": partial_json
                }
            }));
        }
        ContentBlockDelta::Other => {}
    }

    out
}

fn map_content_block_stop(
    cbs: &super::types::ContentBlockStopEvent,
    state: &mut BridgeState,
) -> Vec<Value> {
    let mut out = Vec::new();

    // Check if this is a tool block — emit enriched item/completed
    if let Some(tool_use_id) = state.block_tool_use_ids.get(&cbs.index) {
        if let Some(info) = state.tool_items.get(tool_use_id) {
            out.push(item_tracker::build_item_completed(
                info,
                &state.thread_id,
                &state.turn_id,
            ));
            return out;
        }
    }

    // Non-tool block: emit simple item/completed
    if let Some(item_id) = state.block_items.get(&cbs.index) {
        let item = state
            .block_item_payloads
            .get(&cbs.index)
            .cloned()
            .map(|mut payload| {
                payload["status"] = json!("completed");
                payload
            })
            .unwrap_or_else(|| {
                json!({
                    "id": item_id,
                    "status": "completed",
                })
            });
        out.push(json!({
            "method": "item/completed",
            "params": {
                "threadId": state.thread_id,
                "turnId": state.turn_id,
                "itemId": item_id,
                "item": item
            }
        }));
    }
    out
}

/// Infer the model context window size from the model name.
fn context_window_for_model(model: Option<&str>) -> u64 {
    match model {
        Some(m) if m.starts_with("claude-haiku") => 200_000,
        Some(m) if m.starts_with("claude-sonnet") => 200_000,
        Some(m) if m.starts_with("claude-opus") => 200_000,
        _ => 200_000,
    }
}

fn map_message_delta(
    md: &super::types::MessageDeltaEvent,
    state: &mut BridgeState,
) -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(ref usage) = md.usage {
        let ctx_window = context_window_for_model(state.model.as_deref());
        out.push(json!({
            "method": "thread/tokenUsage/updated",
            "params": {
                "threadId": state.thread_id,
                "tokenUsage": {
                    "inputTokens": usage.input_tokens,
                    "outputTokens": usage.output_tokens,
                    "cacheCreationInputTokens": usage.cache_creation_input_tokens,
                    "cacheReadInputTokens": usage.cache_read_input_tokens,
                    "modelContextWindow": ctx_window
                }
            }
        }));
    }
    out
}

fn map_message_stop(state: &mut BridgeState) -> Vec<Value> {
    // Message stop doesn't directly map to a single event.
    // Turn completion is handled by the result event.
    let _ = state;
    vec![]
}

fn map_result(
    res: &super::types::ResultEvent,
    state: &mut BridgeState,
) -> Vec<Value> {
    let mut out = Vec::new();
    let ctx_window = context_window_for_model(state.model.as_deref());

    // Accumulate totals and emit token usage
    if let Some(ref usage) = res.usage {
        state.total_input_tokens += usage.input_tokens;
        state.total_output_tokens += usage.output_tokens;

        let last_total = usage.input_tokens + usage.output_tokens;
        let grand_total = state.total_input_tokens + state.total_output_tokens;

        out.push(json!({
            "method": "thread/tokenUsage/updated",
            "params": {
                "threadId": state.thread_id,
                "tokenUsage": {
                    "last": {
                        "inputTokens": usage.input_tokens,
                        "outputTokens": usage.output_tokens,
                        "totalTokens": last_total,
                        "cachedInputTokens": usage.cache_read_input_tokens.unwrap_or(0),
                        "reasoningOutputTokens": 0
                    },
                    "total": {
                        "inputTokens": state.total_input_tokens,
                        "outputTokens": state.total_output_tokens,
                        "totalTokens": grand_total,
                        "cachedInputTokens": 0,
                        "reasoningOutputTokens": 0
                    },
                    "modelContextWindow": ctx_window
                }
            }
        }));
    }

    // Accumulate cost
    if let Some(cost) = res.cost_usd {
        state.total_cost_usd += cost;
    }

    // Emit turn/completed with cost and duration
    if state.turn_started {
        if res.is_error {
            out.push(json!({
                "method": "error",
                "params": {
                    "threadId": state.thread_id,
                    "turnId": state.turn_id,
                    "willRetry": false,
                    "error": {
                        "message": res.error.clone().unwrap_or_else(|| "Claude turn failed.".to_string())
                    }
                }
            }));
        }
        out.push(json!({
            "method": "turn/completed",
            "params": {
                "threadId": state.thread_id,
                "turnId": state.turn_id,
                "status": if res.is_error { "error" } else { "completed" },
                "costUsd": res.cost_usd,
                "durationMs": res.duration_ms
            }
        }));
    }

    // Emit rate limits with cumulative cost display
    if state.total_cost_usd > 0.0 {
        out.push(json!({
            "method": "account/rateLimits/updated",
            "params": {
                "rateLimits": {
                    "primary": null,
                    "secondary": null,
                    "credits": {
                        "hasCredits": true,
                        "unlimited": false,
                        "balance": format!("${:.2} spent", state.total_cost_usd)
                    },
                    "planType": "claude-cli"
                }
            }
        }));
    }

    // Auto-name the thread from first ~38 chars of accumulated text (once)
    if !state.thread_named && !state.accumulated_text.is_empty() {
        let name: String = state.accumulated_text.chars().take(38).collect();
        let name = name.trim().to_string();
        if !name.is_empty() {
            state.thread_named = true;
            out.push(json!({
                "method": "thread/name/updated",
                "params": {
                    "threadId": state.thread_id,
                    "threadName": name
                }
            }));
        }
    }

    // Clear unanswered control requests from the completed turn.
    state.pending_control_requests.clear();

    // Prepare for next turn
    state.new_turn();

    out
}

/// Map a `control_request` from Claude CLI to a Codex approval or user-input event.
fn map_control_request(
    cr: &ControlRequestData,
    state: &mut BridgeState,
) -> Vec<Value> {
    let subtype = cr.request.get("subtype").and_then(|v| v.as_str()).unwrap_or("");
    if subtype != "can_use_tool" {
        return vec![];
    }

    let tool_name = cr.request.get("tool_name").and_then(|v| v.as_str()).unwrap_or("unknown");
    let approval_id = state.next_approval_id();

    state.pending_control_requests.insert(approval_id, PendingControlRequest {
        claude_request_id: cr.request_id.clone(),
        tool_name: tool_name.to_string(),
        request: cr.request.clone(),
    });

    if tool_name == "AskUserQuestion" {
        // Emit item/tool/requestUserInput for the frontend modal
        let questions = cr.request.get("input")
            .and_then(|i| i.get("questions"))
            .cloned()
            .unwrap_or(json!([]));
        let item_id = format!("ask_{approval_id}");
        vec![json!({
            "id": approval_id,
            "method": "item/tool/requestUserInput",
            "params": {
                "threadId": state.thread_id,
                "turnId": state.turn_id,
                "itemId": item_id,
                "questions": questions
            }
        })]
    } else {
        // Emit codex/requestApproval for the frontend toast
        let input = cr.request.get("input").cloned().unwrap_or(Value::Null);
        let description = cr.request.get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Build command array: [tool_name, description_or_command]
        let command_str = if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
            cmd.to_string()
        } else {
            description.clone()
        };

        vec![json!({
            "id": approval_id,
            "method": "codex/requestApproval",
            "params": {
                "threadId": state.thread_id,
                "turnId": state.turn_id,
                "command": [tool_name, &command_str],
                "tool": tool_name,
                "input": input,
                "description": description
            }
        })]
    }
}

fn map_assistant(
    a: &super::types::AssistantEvent,
    state: &mut BridgeState,
) -> Vec<Value> {
    // With --include-partial-messages, the real streaming data comes via
    // stream_event wrappers (message_start, content_block_*, message_stop).
    // The top-level "assistant" events are just summary snapshots — we only
    // extract the model from them and ignore the content (already handled
    // by the granular stream_event flow).
    if let Some(ref msg) = a.message {
        if let Some(model) = msg.get("model").and_then(|v| v.as_str()) {
            state.model = Some(model.to_string());
        }
    }
    vec![]
}

#[cfg(test)]
#[path = "event_mapper_tests.rs"]
mod tests;
