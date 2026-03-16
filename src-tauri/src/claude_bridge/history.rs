use std::path::PathBuf;

use serde_json::{json, Value};

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
        let line = line.ok()?;
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

/// Collect assistant text blocks for a given message ID, aggregating across
/// multiple JSONL lines that share the same `message.id`.
fn collect_assistant_text(msg_id: &str, lines: &[Value]) -> String {
    let mut parts = Vec::new();
    for obj in lines {
        if obj.get("type").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        let msg = match obj.get("message") {
            Some(m) => m,
            None => continue,
        };
        let this_id = msg.get("id").and_then(|v| v.as_str()).unwrap_or("");
        if this_id != msg_id {
            continue;
        }
        if let Some(content_arr) = msg.get("content").and_then(|c| c.as_array()) {
            for block in content_arr {
                let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                if block_type == "text" {
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

/// Read conversation items from a Claude CLI JSONL session file.
/// Returns items in the format expected by the frontend:
/// `[{type: "userMessage", id, content}, {type: "agentMessage", id, text}, ...]`
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

    let mut items = Vec::new();
    let mut seen_assistant_ids = std::collections::HashSet::new();
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
                // Aggregate all text blocks for this message ID across JSONL lines.
                let full_text = collect_assistant_text(msg_id, &parsed);
                if full_text.is_empty() {
                    continue;
                }
                item_counter += 1;
                items.push(json!({
                    "type": "agentMessage",
                    "id": format!("assistant-{item_counter}"),
                    "text": full_text
                }));
            }
            _ => {}
        }
    }

    items
}

#[cfg(test)]
mod tests {
    use super::encode_workspace_path;

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
}
