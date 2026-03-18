use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::{json, Value};

use super::item_tracker;

#[derive(Debug, Clone)]
pub(crate) struct ClaudeSession {
    pub(crate) session_id: String,
    pub(crate) name: String,
    pub(crate) last_active_ms: i64,
}

/// Encode a workspace path to Claude CLI's project directory naming scheme.
/// Replaces `\`, `/`, `:`, and ` ` with `-`.
/// Example: `Z:\files\projects\LifeBook\Life book` → `Z--files-projects-LifeBook-Life-book`
pub(crate) fn encode_workspace_path(path: &str) -> String {
    path.chars()
        .map(|c| if matches!(c, '\\' | '/' | ':' | ' ') { '-' } else { c })
        .collect()
}

/// Get the Claude CLI projects root directory (`~/.claude/projects`).
pub(crate) fn claude_projects_root() -> Option<PathBuf> {
    crate::codex::home::resolve_home_dir().map(|h| h.join(".claude").join("projects"))
}

/// Extract conversation name from the first user message in a JSONL file.
fn read_session_name_from_jsonl(path: &std::path::Path) -> Option<String> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    for line in reader.lines().take(30) {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let obj: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if obj.get("type").and_then(|v| v.as_str()) != Some("user") {
            continue;
        }
        let content = match obj.get("message").and_then(|m| m.get("content")) {
            Some(c) => c,
            None => continue,
        };
        let text = if let Some(s) = content.as_str() {
            s.to_string()
        } else if let Some(arr) = content.as_array() {
            arr.iter()
                .filter_map(|item| {
                    if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                        item.get("text").and_then(|t| t.as_str()).map(|s| s.to_string())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        } else {
            continue;
        };
        let trimmed = text.trim().to_string();
        if trimmed.is_empty() {
            continue;
        }
        // Truncate long names
        let name = if trimmed.chars().count() > 60 {
            let truncated: String = trimmed.chars().take(59).collect();
            format!("{truncated}…")
        } else {
            trimmed
        };
        return Some(name);
    }
    None
}

/// Read all Claude CLI sessions for the given workspace path, sorted newest first.
pub(crate) fn read_claude_sessions(workspace_path: &str) -> Vec<ClaudeSession> {
    let encoded = encode_workspace_path(workspace_path);
    let root = match claude_projects_root() {
        Some(r) => r,
        None => return Vec::new(),
    };
    let project_dir = root.join(&encoded);
    if !project_dir.is_dir() {
        return Vec::new();
    }

    let entries = match std::fs::read_dir(&project_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut sessions: Vec<ClaudeSession> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                return None;
            }
            let session_id = path.file_stem()?.to_str()?.to_string();
            let last_active_ms = entry
                .metadata()
                .and_then(|m| m.modified())
                .map(|t| {
                    t.duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as i64
                })
                .unwrap_or(0);
            let name = read_session_name_from_jsonl(&path)
                .unwrap_or_else(|| "Conversation".to_string());
            Some(ClaudeSession { session_id, name, last_active_ms })
        })
        .collect();

    sessions.sort_by(|a, b| b.last_active_ms.cmp(&a.last_active_ms));
    sessions
}

/// Extract user text from a JSONL `message.content` field.
fn extract_user_text_from_content(content: &Value) -> String {
    if let Some(s) = content.as_str() {
        return s.trim().to_string();
    }
    if let Some(arr) = content.as_array() {
        let parts: Vec<String> = arr
            .iter()
            .filter_map(|item| {
                let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
                if item_type == "text" {
                    item.get("text").and_then(|t| t.as_str()).map(|s| s.to_string())
                } else {
                    None
                }
            })
            .collect();
        return parts.join("\n").trim().to_string();
    }
    String::new()
}

/// Collect assistant text blocks from pre-grouped JSONL lines sharing the
/// same `message.id`.
fn collect_assistant_text_from_group(lines: &[&Value]) -> String {
    let mut parts = Vec::new();
    for obj in lines {
        let msg = match obj.get("message") {
            Some(m) => m,
            None => continue,
        };
        if let Some(content_arr) = msg.get("content").and_then(|c| c.as_array()) {
            for block in content_arr {
                if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                        let trimmed = text.trim();
                        if !trimmed.is_empty() {
                            parts.push(trimmed.to_string());
                        }
                    }
                }
            }
        }
    }
    parts.join("\n\n")
}

/// Extract text from a tool_result content value (string or array of text blocks).
fn extract_tool_result_text_from_content(content: &Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    if let Some(arr) = content.as_array() {
        let texts: Vec<String> = arr
            .iter()
            .filter_map(|item| {
                if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                    item.get("text").and_then(|t| t.as_str()).map(|s| s.to_string())
                } else {
                    None
                }
            })
            .collect();
        return texts.join("\n");
    }
    String::new()
}

/// Read conversation items from a Claude CLI JSONL session file.
/// Returns items in the format expected by the frontend:
/// `[{type: "userMessage", id, content}, {type: "agentMessage", id, text}, ...]`
///
/// Two-pass approach:
/// - Pass 1: Build a HashMap of tool_use_id → result_text from tool_result entries.
/// - Pass 2: Emit userMessage, agentMessage, commandExecution, and fileChange items.
pub(crate) fn read_session_items(workspace_path: &str, session_id: &str) -> Vec<Value> {
    let encoded = encode_workspace_path(workspace_path);
    let root = match claude_projects_root() {
        Some(r) => r,
        None => return Vec::new(),
    };
    let jsonl_path = root.join(&encoded).join(format!("{session_id}.jsonl"));
    if !jsonl_path.is_file() {
        return Vec::new();
    }

    // Parse all lines up front.
    let parsed: Vec<Value> = {
        use std::io::BufRead;
        let file = match std::fs::File::open(&jsonl_path) {
            Ok(f) => f,
            Err(_) => return Vec::new(),
        };
        std::io::BufReader::new(file)
            .lines()
            .filter_map(|line| {
                let line = line.ok()?;
                let line = line.trim().to_string();
                if line.is_empty() {
                    return None;
                }
                serde_json::from_str::<Value>(&line).ok()
            })
            .collect()
    };

    // ── Pass 1: collect tool results ──────────────────────────────
    let mut tool_results: HashMap<String, String> = HashMap::new();
    for obj in &parsed {
        let msg = match obj.get("message") {
            Some(m) if m.is_object() => m,
            _ => continue,
        };
        let content_arr = match msg.get("content").and_then(|c| c.as_array()) {
            Some(arr) => arr,
            None => continue,
        };
        for block in content_arr {
            if block.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
                continue;
            }
            if let Some(tuid) = block.get("tool_use_id").and_then(|v| v.as_str()) {
                let text = block
                    .get("content")
                    .map(|c| extract_tool_result_text_from_content(c))
                    .unwrap_or_default();
                tool_results.insert(tuid.to_string(), text);
            }
        }
    }

    // ── Pass 1.5: group assistant lines by message.id ──────────
    let mut assistant_lines: HashMap<String, Vec<&Value>> = HashMap::new();
    for obj in &parsed {
        if obj.get("type").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        let msg = match obj.get("message") {
            Some(m) if m.is_object() => m,
            _ => continue,
        };
        let msg_id = msg.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if !msg_id.is_empty() {
            assistant_lines.entry(msg_id.to_string()).or_default().push(obj);
        }
    }

    // ── Pass 2: build items ───────────────────────────────────────
    let mut items = Vec::new();
    let mut seen_assistant_ids = std::collections::HashSet::new();
    let mut seen_tool_use_ids = std::collections::HashSet::new();
    let mut item_counter: u64 = 0;

    for obj in &parsed {
        let record_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let msg = match obj.get("message") {
            Some(m) if m.is_object() => m,
            _ => continue,
        };
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");

        match (record_type, role) {
            ("user", "user") => {
                // Only emit actual user text messages, skip tool_result entries.
                let content = match msg.get("content") {
                    Some(c) => c,
                    None => continue,
                };
                // If content is an array, check if it contains actual text (not just tool_results).
                if let Some(arr) = content.as_array() {
                    let has_text = arr.iter().any(|block| {
                        block.get("type").and_then(|t| t.as_str()) == Some("text")
                    });
                    let only_tool_results = arr.iter().all(|block| {
                        block.get("type").and_then(|t| t.as_str()) == Some("tool_result")
                    });
                    if !has_text && only_tool_results {
                        continue;
                    }
                }
                let text = extract_user_text_from_content(content);
                if text.is_empty() {
                    continue;
                }
                item_counter += 1;
                items.push(json!({
                    "type": "userMessage",
                    "id": format!("user-{item_counter}"),
                    "content": [{ "type": "text", "text": text }]
                }));
            }
            ("assistant", "assistant") => {
                let msg_id = msg.get("id").and_then(|v| v.as_str()).unwrap_or("");
                if msg_id.is_empty() || seen_assistant_ids.contains(msg_id) {
                    continue;
                }
                seen_assistant_ids.insert(msg_id.to_string());

                let group = assistant_lines
                    .get(msg_id)
                    .map(|v| v.as_slice())
                    .unwrap_or(&[]);

                // Aggregate all text blocks for this message ID.
                let full_text = collect_assistant_text_from_group(group);
                if !full_text.is_empty() {
                    item_counter += 1;
                    items.push(json!({
                        "type": "agentMessage",
                        "id": format!("assistant-{item_counter}"),
                        "text": full_text
                    }));
                }

                // Scan tool_use blocks from grouped lines only.
                for line_obj in group {
                    let line_msg = match line_obj.get("message") {
                        Some(m) => m,
                        None => continue,
                    };
                    let content_arr = match line_msg.get("content").and_then(|c| c.as_array()) {
                        Some(arr) => arr,
                        None => continue,
                    };
                    for block in content_arr {
                        if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                            continue;
                        }
                        let tool_name = match block.get("name").and_then(|v| v.as_str()) {
                            Some(n) => n,
                            None => continue,
                        };
                        let tool_use_id = block.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        // Skip duplicates: same tool_use block can appear in multiple
                        // JSONL lines sharing the same message.id.
                        if !seen_tool_use_ids.insert(tool_use_id.to_string()) {
                            continue;
                        }
                        let input = block.get("input").cloned().unwrap_or(Value::Null);
                        let result_text = tool_results
                            .get(tool_use_id)
                            .cloned()
                            .unwrap_or_default();

                        let category = item_tracker::classify_tool(tool_name);
                        item_counter += 1;
                        let item_id = format!("tool-{item_counter}");

                        match category {
                            item_tracker::ToolCategory::FileChange => {
                                let path = item_tracker::extract_file_path(tool_name, &input)
                                    .unwrap_or_default();
                                let kind = item_tracker::infer_change_kind(tool_name);
                                let mut change = json!({
                                    "path": path,
                                    "kind": kind,
                                });
                                if !result_text.is_empty() {
                                    change["diff"] = json!(result_text);
                                }
                                items.push(json!({
                                    "type": "fileChange",
                                    "id": item_id,
                                    "status": "completed",
                                    "changes": [change]
                                }));
                            }
                            _ => {
                                // commandExecution for Bash, Read, Grep, Glob, and others
                                let command = item_tracker::extract_command(tool_name, &input)
                                    .unwrap_or_else(|| tool_name.to_string());
                                items.push(json!({
                                    "type": "commandExecution",
                                    "id": item_id,
                                    "command": command,
                                    "status": "completed",
                                    "aggregatedOutput": result_text,
                                    "cwd": ""
                                }));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    items
}

/// Scan all JSONL session files under `~/.claude/projects/` to discover
/// model IDs actually used. Returns `Vec<(model_id, display_name)>`
/// deduplicated, most-recently-used first. No hardcoded model list.
pub(crate) fn discover_models(workspace_path: &str) -> Vec<(String, String)> {
    let root = match claude_projects_root() {
        Some(r) => r,
        None => return Vec::new(),
    };

    // Scan projects to find every model the user has access to.
    // Current workspace is checked first; remaining dirs are capped to
    // avoid long blocking scans when there are many projects.
    const MAX_PROJECT_DIRS: usize = 20;
    const MAX_FILES_PER_DIR: usize = 5;
    const MAX_LINES_PER_FILE: usize = 10;

    let encoded = encode_workspace_path(workspace_path);
    let current_project = root.join(&encoded);

    let mut all_dirs: Vec<PathBuf> = Vec::new();
    if current_project.is_dir() {
        all_dirs.push(current_project.clone());
    }
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() && p != current_project {
                all_dirs.push(p);
                if all_dirs.len() >= MAX_PROJECT_DIRS {
                    break;
                }
            }
        }
    }

    let mut seen_order: Vec<String> = Vec::new();
    let mut seen_set = std::collections::HashSet::new();

    for project_dir in &all_dirs {
        let entries = match std::fs::read_dir(project_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        let mut paths: Vec<_> = entries
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) == Some("jsonl") {
                    let mtime = e.metadata().and_then(|m| m.modified()).ok();
                    Some((p, mtime))
                } else {
                    None
                }
            })
            .collect();
        // Newest first.
        paths.sort_by(|a, b| b.1.cmp(&a.1));

        // Sample first few lines of each file (model appears early).
        for (path, _) in paths.iter().take(MAX_FILES_PER_DIR) {
            use std::io::BufRead;
            let file = match std::fs::File::open(path) {
                Ok(f) => f,
                Err(_) => continue,
            };
            for line in std::io::BufReader::new(file).lines().take(MAX_LINES_PER_FILE) {
                let line = match line {
                    Ok(l) => l,
                    Err(_) => break,
                };
                let obj: Value = match serde_json::from_str(line.trim()) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(model) = obj
                    .get("message")
                    .and_then(|m| m.get("model"))
                    .and_then(|v| v.as_str())
                {
                    let model = model.trim();
                    if !model.is_empty()
                        && !model.starts_with('<')
                        && seen_set.insert(model.to_string())
                    {
                        seen_order.push(model.to_string());
                    }
                }
            }
        }
    }

    seen_order
        .into_iter()
        .map(|id| {
            let display = format_model_name(&id);
            (id, display)
        })
        .collect()
}

/// Format a model ID into a human-readable name.
/// `claude-opus-4-6` → `Claude Opus 4 6`, strips date suffixes like `-20251001`.
pub(crate) fn format_model_name(id: &str) -> String {
    let base = if let Some(pos) = id.rfind('-') {
        let suffix = &id[pos + 1..];
        if suffix.len() == 8 && suffix.chars().all(|c| c.is_ascii_digit()) {
            &id[..pos]
        } else {
            id
        }
    } else {
        id
    };
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn encodes_windows_path() {
        assert_eq!(
            encode_workspace_path(r"D:\Projects\MyRepo"),
            "D--Projects-MyRepo"
        );
    }

    #[test]
    fn encodes_unix_path() {
        assert_eq!(encode_workspace_path("/home/user/project"), "-home-user-project");
    }

    #[test]
    fn encodes_drive_letter_colon() {
        assert_eq!(encode_workspace_path("C:\\Users\\AndrewM"), "C--Users-AndrewM");
    }

    #[test]
    fn encodes_spaces_as_dashes() {
        assert_eq!(
            encode_workspace_path(r"Z:\files\projects\LifeBook\Life book"),
            "Z--files-projects-LifeBook-Life-book"
        );
    }

    // ── format_model_name tests ──────────────────────────────────

    #[test]
    fn format_model_name_strips_date_suffix() {
        assert_eq!(format_model_name("claude-sonnet-4-5-20251001"), "Claude Sonnet 4 5");
    }

    #[test]
    fn format_model_name_preserves_no_date() {
        assert_eq!(format_model_name("claude-opus-4-6"), "Claude Opus 4 6");
    }

    #[test]
    fn format_model_name_single_segment() {
        assert_eq!(format_model_name("gpt4"), "Gpt4");
    }

    #[test]
    fn format_model_name_with_non_date_suffix() {
        assert_eq!(format_model_name("claude-sonnet-4-beta"), "Claude Sonnet 4 Beta");
    }

    #[test]
    fn format_model_name_empty() {
        assert_eq!(format_model_name(""), "");
    }

    // ── extract_user_text_from_content tests ─────────────────────

    #[test]
    fn extract_user_text_string_content() {
        let content = json!("Hello world");
        assert_eq!(extract_user_text_from_content(&content), "Hello world");
    }

    #[test]
    fn extract_user_text_array_content() {
        let content = json!([
            { "type": "text", "text": "First" },
            { "type": "image", "url": "http://example.com" },
            { "type": "text", "text": "Second" }
        ]);
        assert_eq!(extract_user_text_from_content(&content), "First\nSecond");
    }

    #[test]
    fn extract_user_text_empty_array() {
        let content = json!([{ "type": "tool_result", "tool_use_id": "abc" }]);
        assert_eq!(extract_user_text_from_content(&content), "");
    }

    // ── collect_assistant_text_from_group tests ──────────────────

    #[test]
    fn collect_assistant_text_aggregates_across_lines() {
        let line1 = json!({
            "type": "assistant",
            "message": {
                "id": "msg_1",
                "role": "assistant",
                "content": [{ "type": "text", "text": "Hello" }]
            }
        });
        let line2 = json!({
            "type": "assistant",
            "message": {
                "id": "msg_1",
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "World" },
                    { "type": "tool_use", "id": "tu_1", "name": "Bash", "input": {} }
                ]
            }
        });
        let group: Vec<&Value> = vec![&line1, &line2];
        assert_eq!(collect_assistant_text_from_group(&group), "Hello\n\nWorld");
    }

    #[test]
    fn collect_assistant_text_skips_empty_and_whitespace() {
        let line = json!({
            "type": "assistant",
            "message": {
                "id": "msg_1",
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "   " },
                    { "type": "text", "text": "Real text" }
                ]
            }
        });
        let group: Vec<&Value> = vec![&line];
        assert_eq!(collect_assistant_text_from_group(&group), "Real text");
    }

    // ── extract_tool_result_text_from_content tests ──────────────

    #[test]
    fn extract_tool_result_text_string() {
        assert_eq!(
            extract_tool_result_text_from_content(&json!("output text")),
            "output text"
        );
    }

    #[test]
    fn extract_tool_result_text_array() {
        let content = json!([
            { "type": "text", "text": "line 1" },
            { "type": "text", "text": "line 2" }
        ]);
        assert_eq!(extract_tool_result_text_from_content(&content), "line 1\nline 2");
    }

}
