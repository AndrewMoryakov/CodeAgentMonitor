use std::fs;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

const RULES_DIR: &str = "rules";
const DEFAULT_RULES_FILE: &str = "default.rules";

pub(crate) fn default_rules_path(codex_home: &Path) -> PathBuf {
    codex_home.join(RULES_DIR).join(DEFAULT_RULES_FILE)
}

pub(crate) fn append_prefix_rule(path: &Path, pattern: &[String]) -> Result<(), String> {
    if pattern.is_empty() {
        return Err("empty command pattern".to_string());
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }

    let _lock = acquire_rules_lock(path)?;
    let existing = fs::read_to_string(path).unwrap_or_default();
    if rule_already_present(&existing, pattern) {
        return Ok(());
    }
    let mut updated = existing;

    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    if !updated.is_empty() {
        updated.push('\n');
    }

    let rule = format_prefix_rule(pattern);
    updated.push_str(&rule);

    if !updated.ends_with('\n') {
        updated.push('\n');
    }

    fs::write(path, updated).map_err(|err| err.to_string())
}

struct RulesFileLock {
    path: PathBuf,
}

impl Drop for RulesFileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn acquire_rules_lock(path: &Path) -> Result<RulesFileLock, String> {
    let lock_path = path.with_extension("lock");
    let deadline = Instant::now() + Duration::from_secs(2);
    let stale_after = Duration::from_secs(30);

    loop {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(_) => return Ok(RulesFileLock { path: lock_path }),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                if is_lock_stale(&lock_path, stale_after) {
                    let _ = fs::remove_file(&lock_path);
                    continue;
                }
                if Instant::now() >= deadline {
                    return Err("timed out waiting for rules file lock".to_string());
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(err) => return Err(err.to_string()),
        }
    }
}

fn is_lock_stale(path: &Path, stale_after: Duration) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    let Ok(age) = SystemTime::now().duration_since(modified) else {
        return false;
    };
    age > stale_after
}

fn format_prefix_rule(pattern: &[String]) -> String {
    let items = format_pattern_list(pattern);
    format!("prefix_rule(\n    pattern = [{items}],\n    decision = \"allow\",\n)\n")
}

fn format_pattern_list(pattern: &[String]) -> String {
    pattern
        .iter()
        .map(|item| format!("\"{}\"", escape_string(item)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn rule_already_present(contents: &str, pattern: &[String]) -> bool {
    let target_pattern = normalize_rule_value(&format!("[{}]", format_pattern_list(pattern)));
    let mut in_rule = false;
    let mut pattern_matches = false;
    let mut decision_allows = false;

    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("prefix_rule(") {
            in_rule = true;
            pattern_matches = false;
            decision_allows = false;
            continue;
        }
        if !in_rule {
            continue;
        }
        if trimmed.starts_with("pattern") {
            if let Some((_, value)) = trimmed.split_once('=') {
                let candidate = value.trim().trim_end_matches(',');
                if normalize_rule_value(candidate) == target_pattern {
                    pattern_matches = true;
                }
            }
        } else if trimmed.starts_with("decision") {
            if let Some((_, value)) = trimmed.split_once('=') {
                let candidate = value.trim().trim_end_matches(',');
                if candidate.contains("\"allow\"") || candidate.contains("'allow'") {
                    decision_allows = true;
                }
            }
        } else if trimmed.starts_with(')') {
            if pattern_matches && decision_allows {
                return true;
            }
            in_rule = false;
        }
    }
    false
}

fn normalize_rule_value(value: &str) -> String {
    value.chars().filter(|ch| !ch.is_whitespace()).collect()
}

/// Check if any `prefix_rule` with `decision = "allow"` matches the given
/// command using prefix semantics: each pattern element must be a prefix of
/// the corresponding command element, and the pattern may be shorter than
/// the command (i.e. `["Bash"]` matches any `["Bash", ...]`).
pub(crate) fn check_prefix_rules(path: &Path, command: &[&str]) -> bool {
    let contents = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    for (pattern, allows) in parse_prefix_rules(&contents) {
        if allows && prefix_matches(&pattern, command) {
            return true;
        }
    }
    false
}

/// Parse all `prefix_rule` blocks, returning `(pattern, is_allow)` pairs.
fn parse_prefix_rules(contents: &str) -> Vec<(Vec<String>, bool)> {
    let mut rules = Vec::new();
    let mut in_rule = false;
    let mut pattern: Vec<String> = Vec::new();
    let mut decision_allows = false;

    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("prefix_rule(") {
            in_rule = true;
            pattern.clear();
            decision_allows = false;
            continue;
        }
        if !in_rule {
            continue;
        }
        if trimmed.starts_with("pattern") {
            if let Some((_, value)) = trimmed.split_once('=') {
                pattern = extract_string_list(value);
            }
        } else if trimmed.starts_with("decision") {
            if let Some((_, value)) = trimmed.split_once('=') {
                let candidate = value.trim().trim_end_matches(',');
                if candidate.contains("\"allow\"") || candidate.contains("'allow'") {
                    decision_allows = true;
                }
            }
        } else if trimmed.starts_with(')') {
            if !pattern.is_empty() {
                rules.push((pattern.clone(), decision_allows));
            }
            in_rule = false;
        }
    }
    rules
}

/// Extract a list of strings from a pattern value like `["Bash", "rm -rf"]`.
fn extract_string_list(value: &str) -> Vec<String> {
    let value = value.trim().trim_end_matches(',');
    // Strip outer brackets
    let inner = value
        .trim()
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(value);
    inner
        .split(',')
        .filter_map(|item| {
            let item = item.trim();
            // Strip surrounding quotes
            let unquoted = item
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .or_else(|| item.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')));
            unquoted.map(|s| unescape_string(s))
        })
        .filter(|s| !s.is_empty())
        .collect()
}

/// Reverse of escape_string: unescape common escape sequences.
fn unescape_string(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('\\') => result.push('\\'),
                Some('"') => result.push('"'),
                Some('n') => result.push('\n'),
                Some('r') => result.push('\r'),
                Some('t') => result.push('\t'),
                Some(other) => {
                    result.push('\\');
                    result.push(other);
                }
                None => result.push('\\'),
            }
        } else {
            result.push(ch);
        }
    }
    result
}

/// Check if `pattern` is a prefix-match for `command`.
/// Each pattern element must be a prefix of the corresponding command element.
fn prefix_matches(pattern: &[String], command: &[&str]) -> bool {
    if pattern.is_empty() || pattern.len() > command.len() {
        return false;
    }
    for (p, c) in pattern.iter().zip(command.iter()) {
        if !c.starts_with(p.as_str()) {
            return false;
        }
    }
    true
}

fn escape_string(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn prefix_matches_exact() {
        let pattern = vec!["Bash".to_string(), "rm -rf /tmp".to_string()];
        assert!(prefix_matches(&pattern, &["Bash", "rm -rf /tmp"]));
    }

    #[test]
    fn prefix_matches_shorter_pattern() {
        let pattern = vec!["Bash".to_string()];
        assert!(prefix_matches(&pattern, &["Bash", "rm -rf /tmp"]));
    }

    #[test]
    fn prefix_matches_element_prefix() {
        let pattern = vec!["Bash".to_string(), "rm".to_string()];
        assert!(prefix_matches(&pattern, &["Bash", "rm -rf /tmp"]));
    }

    #[test]
    fn prefix_matches_rejects_mismatch() {
        let pattern = vec!["Bash".to_string(), "ls".to_string()];
        assert!(!prefix_matches(&pattern, &["Bash", "rm -rf /tmp"]));
    }

    #[test]
    fn prefix_matches_rejects_empty_pattern() {
        let pattern: Vec<String> = vec![];
        assert!(!prefix_matches(&pattern, &["Bash", "rm"]));
    }

    #[test]
    fn prefix_matches_rejects_longer_pattern() {
        let pattern = vec!["Bash".to_string(), "rm".to_string(), "extra".to_string()];
        assert!(!prefix_matches(&pattern, &["Bash", "rm"]));
    }

    #[test]
    fn parse_prefix_rules_single_allow() {
        let content = r#"
prefix_rule(
    pattern = ["Bash", "rm -rf /tmp"],
    decision = "allow",
)
"#;
        let rules = parse_prefix_rules(content);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].0, vec!["Bash", "rm -rf /tmp"]);
        assert!(rules[0].1);
    }

    #[test]
    fn parse_prefix_rules_deny_not_matched() {
        let content = r#"
prefix_rule(
    pattern = ["Bash"],
    decision = "deny",
)
"#;
        let rules = parse_prefix_rules(content);
        assert_eq!(rules.len(), 1);
        assert!(!rules[0].1); // not allow
    }

    #[test]
    fn parse_prefix_rules_multiple() {
        let content = r#"
prefix_rule(
    pattern = ["Bash"],
    decision = "allow",
)

prefix_rule(
    pattern = ["Write", "/src/main.rs"],
    decision = "allow",
)
"#;
        let rules = parse_prefix_rules(content);
        assert_eq!(rules.len(), 2);
    }

    #[test]
    fn extract_string_list_basic() {
        let items = extract_string_list(r#"["Bash", "rm -rf /tmp"]"#);
        assert_eq!(items, vec!["Bash", "rm -rf /tmp"]);
    }

    #[test]
    fn extract_string_list_single() {
        let items = extract_string_list(r#"["Bash"]"#);
        assert_eq!(items, vec!["Bash"]);
    }

    #[test]
    fn extract_string_list_with_escaped_quotes() {
        let items = extract_string_list(r#"["echo \"hello\""]"#);
        assert_eq!(items, vec![r#"echo "hello""#]);
    }

    #[test]
    fn check_prefix_rules_from_file() {
        let dir = std::env::temp_dir().join("rules_test");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("test.rules");
        let mut f = fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"prefix_rule(
    pattern = ["Bash", "git status"],
    decision = "allow",
)"#
        )
        .unwrap();

        assert!(check_prefix_rules(&path, &["Bash", "git status"]));
        assert!(check_prefix_rules(&path, &["Bash", "git status --short"]));
        assert!(!check_prefix_rules(&path, &["Bash", "rm -rf /"]));
        assert!(!check_prefix_rules(&path, &["Write", "something"]));

        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&dir);
    }

    #[test]
    fn check_prefix_rules_missing_file_returns_false() {
        let path = std::path::PathBuf::from("/nonexistent/rules.txt");
        assert!(!check_prefix_rules(&path, &["Bash", "ls"]));
    }
}
