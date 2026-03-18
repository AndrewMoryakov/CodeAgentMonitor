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

// ── group_items_into_turns tests ─────────────────────────────

#[test]
fn group_items_into_turns_empty() {
    let result = group_items_into_turns("t1", vec![]);
    assert_eq!(result, json!([]));
}

#[test]
fn group_items_into_turns_single_turn() {
    let items = vec![
        json!({ "type": "userMessage", "id": "u1" }),
        json!({ "type": "agentMessage", "id": "a1" }),
        json!({ "type": "commandExecution", "id": "c1" }),
    ];
    let turns = group_items_into_turns("t1", items);
    let arr = turns.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["items"].as_array().unwrap().len(), 3);
    assert!(arr[0]["id"].as_str().unwrap().contains("t1"));
}

#[test]
fn group_items_into_turns_multiple_turns() {
    let items = vec![
        json!({ "type": "userMessage", "id": "u1" }),
        json!({ "type": "agentMessage", "id": "a1" }),
        json!({ "type": "userMessage", "id": "u2" }),
        json!({ "type": "agentMessage", "id": "a2" }),
        json!({ "type": "commandExecution", "id": "c1" }),
    ];
    let turns = group_items_into_turns("t1", items);
    let arr = turns.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    // First turn: userMessage + agentMessage
    assert_eq!(arr[0]["items"].as_array().unwrap().len(), 2);
    assert_eq!(arr[0]["items"][0]["id"], "u1");
    // Second turn: userMessage + agentMessage + commandExecution
    assert_eq!(arr[1]["items"].as_array().unwrap().len(), 3);
    assert_eq!(arr[1]["items"][0]["id"], "u2");
}

#[test]
fn group_items_into_turns_leading_agent_items() {
    // Agent items before any user message go into the first turn.
    let items = vec![
        json!({ "type": "agentMessage", "id": "a0" }),
        json!({ "type": "userMessage", "id": "u1" }),
        json!({ "type": "agentMessage", "id": "a1" }),
    ];
    let turns = group_items_into_turns("t1", items);
    let arr = turns.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    // First turn: just the leading agent item
    assert_eq!(arr[0]["items"].as_array().unwrap().len(), 1);
    assert_eq!(arr[0]["items"][0]["id"], "a0");
    // Second turn: user + agent
    assert_eq!(arr[1]["items"].as_array().unwrap().len(), 2);
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

// ── NDJSON message builders ────────────────────────────────────

#[test]
fn build_user_message_produces_valid_ndjson() {
    let msg = build_user_message("Hello world", "uuid-123", "sess_abc");
    let parsed: Value = serde_json::from_str(&msg).unwrap();
    assert_eq!(parsed["type"], "user");
    assert_eq!(parsed["message"]["role"], "user");
    assert_eq!(parsed["message"]["content"], "Hello world");
    assert_eq!(parsed["uuid"], "uuid-123");
    assert_eq!(parsed["session_id"], "sess_abc");
}

#[test]
fn build_user_message_empty_session_id() {
    let msg = build_user_message("Hi", "uuid-456", "");
    let parsed: Value = serde_json::from_str(&msg).unwrap();
    assert_eq!(parsed["session_id"], "");
}

#[test]
fn build_interrupt_request_produces_valid_ndjson() {
    let msg = build_interrupt_request();
    let parsed: Value = serde_json::from_str(&msg).unwrap();
    assert_eq!(parsed["type"], "control_request");
    assert_eq!(parsed["request"]["subtype"], "interrupt");
    assert!(parsed["request_id"].as_str().is_some());
}

#[test]
fn build_tool_approval_allow() {
    let pending = super::super::types::PendingControlRequest {
        claude_request_id: "req_abc".to_string(),
        tool_name: "Bash".to_string(),
        request: json!({"subtype": "can_use_tool", "tool_name": "Bash"}),
    };
    let result = json!({"decision": "accept"});
    let msg = build_tool_approval_response(&pending, &result);
    assert_eq!(msg["type"], "control_response");
    assert_eq!(msg["response"]["request_id"], "req_abc");
    assert_eq!(msg["response"]["response"]["behavior"], "allow");
}

#[test]
fn build_tool_approval_deny() {
    let pending = super::super::types::PendingControlRequest {
        claude_request_id: "req_xyz".to_string(),
        tool_name: "Bash".to_string(),
        request: json!({}),
    };
    let result = json!({"decision": "decline", "message": "Not safe"});
    let msg = build_tool_approval_response(&pending, &result);
    assert_eq!(msg["response"]["response"]["behavior"], "deny");
    assert_eq!(msg["response"]["response"]["message"], "Not safe");
}

#[test]
fn build_tool_approval_deny_default_message() {
    let pending = super::super::types::PendingControlRequest {
        claude_request_id: "req_xyz".to_string(),
        tool_name: "Bash".to_string(),
        request: json!({}),
    };
    let result = json!({"decision": "decline"});
    let msg = build_tool_approval_response(&pending, &result);
    assert_eq!(msg["response"]["response"]["behavior"], "deny");
    assert_eq!(msg["response"]["response"]["message"], "User denied this action");
}

#[test]
fn build_ask_user_response_with_answers() {
    let pending = super::super::types::PendingControlRequest {
        claude_request_id: "req_q".to_string(),
        tool_name: "AskUserQuestion".to_string(),
        request: json!({
            "input": {
                "questions": [{"question": "Which lib?"}]
            }
        }),
    };
    let result = json!({"answers": {"Which lib?": "axios"}});
    let msg = build_ask_user_response(&pending, &result);
    assert_eq!(msg["type"], "control_response");
    assert_eq!(msg["response"]["response"]["behavior"], "allow");
    assert_eq!(msg["response"]["response"]["updatedInput"]["answers"]["Which lib?"], "axios");
    // Original questions preserved
    assert_eq!(msg["response"]["response"]["updatedInput"]["questions"][0]["question"], "Which lib?");
}
