use serde_json::{json, Value};

pub(crate) const DEFAULT_REMOTE_HOST: &str = "127.0.0.1:4732";
pub(crate) const DISCONNECTED_MESSAGE: &str = "remote backend disconnected";

pub(crate) enum IncomingMessage {
    Response {
        id: u64,
        payload: Result<Value, String>,
    },
    Notification {
        method: String,
        params: Value,
    },
}

pub(crate) fn build_request_line(id: u64, method: &str, params: Value) -> Result<String, String> {
    let request = json!({
        "id": id,
        "method": method,
        "params": params,
    });
    serde_json::to_string(&request).map_err(|err| err.to_string())
}

pub(crate) fn parse_incoming_line(line: &str) -> Option<IncomingMessage> {
    let message: Value = serde_json::from_str(line).ok()?;

    if let Some(id) = message.get("id").and_then(|value| value.as_u64()) {
        if let Some(error) = message.get("error") {
            let error_message = error
                .get("message")
                .and_then(|value| value.as_str())
                .unwrap_or("remote error")
                .to_string();
            return Some(IncomingMessage::Response {
                id,
                payload: Err(error_message),
            });
        }

        let result = message.get("result").cloned().unwrap_or(Value::Null);
        return Some(IncomingMessage::Response {
            id,
            payload: Ok(result),
        });
    }

    let method = message
        .get("method")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    if method.is_empty() {
        return None;
    }
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    Some(IncomingMessage::Notification {
        method: method.to_string(),
        params,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── build_request_line ───────────────────────────────────────

    #[test]
    fn build_request_line_produces_valid_json() {
        let line = build_request_line(42, "list_threads", json!({"workspaceId": "ws1"})).unwrap();
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["id"], 42);
        assert_eq!(parsed["method"], "list_threads");
        assert_eq!(parsed["params"]["workspaceId"], "ws1");
    }

    #[test]
    fn build_request_line_null_params() {
        let line = build_request_line(1, "ping", Value::Null).unwrap();
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["method"], "ping");
        assert!(parsed["params"].is_null());
    }

    // ── parse_incoming_line: responses ───────────────────────────

    #[test]
    fn parse_response_with_result() {
        let line = r#"{"id":42,"result":{"threads":[]}}"#;
        let msg = parse_incoming_line(line).unwrap();
        match msg {
            IncomingMessage::Response { id, payload } => {
                assert_eq!(id, 42);
                let val = payload.unwrap();
                assert!(val["threads"].is_array());
            }
            _ => panic!("expected Response"),
        }
    }

    #[test]
    fn parse_response_without_result_returns_null() {
        let line = r#"{"id":7}"#;
        let msg = parse_incoming_line(line).unwrap();
        match msg {
            IncomingMessage::Response { id, payload } => {
                assert_eq!(id, 7);
                assert_eq!(payload.unwrap(), Value::Null);
            }
            _ => panic!("expected Response"),
        }
    }

    #[test]
    fn parse_response_with_error() {
        let line = r#"{"id":5,"error":{"message":"not found","code":-1}}"#;
        let msg = parse_incoming_line(line).unwrap();
        match msg {
            IncomingMessage::Response { id, payload } => {
                assert_eq!(id, 5);
                assert_eq!(payload.unwrap_err(), "not found");
            }
            _ => panic!("expected Response"),
        }
    }

    #[test]
    fn parse_response_with_error_missing_message_field() {
        let line = r#"{"id":5,"error":{"code":-1}}"#;
        let msg = parse_incoming_line(line).unwrap();
        match msg {
            IncomingMessage::Response { id, payload } => {
                assert_eq!(id, 5);
                assert_eq!(payload.unwrap_err(), "remote error");
            }
            _ => panic!("expected Response"),
        }
    }

    // ── parse_incoming_line: notifications ───────────────────────

    #[test]
    fn parse_notification() {
        let line = r#"{"method":"app-server-event","params":{"data":"test"}}"#;
        let msg = parse_incoming_line(line).unwrap();
        match msg {
            IncomingMessage::Notification { method, params } => {
                assert_eq!(method, "app-server-event");
                assert_eq!(params["data"], "test");
            }
            _ => panic!("expected Notification"),
        }
    }

    #[test]
    fn parse_notification_without_params() {
        let line = r#"{"method":"terminal-exit"}"#;
        let msg = parse_incoming_line(line).unwrap();
        match msg {
            IncomingMessage::Notification { method, params } => {
                assert_eq!(method, "terminal-exit");
                assert!(params.is_null());
            }
            _ => panic!("expected Notification"),
        }
    }

    // ── parse_incoming_line: edge cases ──────────────────────────

    #[test]
    fn parse_invalid_json_returns_none() {
        assert!(parse_incoming_line("not json").is_none());
        assert!(parse_incoming_line("").is_none());
    }

    #[test]
    fn parse_json_without_id_or_method_returns_none() {
        assert!(parse_incoming_line(r#"{"data":"orphan"}"#).is_none());
    }

    #[test]
    fn parse_json_with_empty_method_returns_none() {
        assert!(parse_incoming_line(r#"{"method":""}"#).is_none());
    }
}
