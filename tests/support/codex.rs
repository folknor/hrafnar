//! Shared synthetic Codex wire representations. Literal protocol expectations
//! stay in decoder tests; process stubs and unit tests use these same builders.
#![allow(dead_code)]
use serde_json::{Value, json};

pub fn metadata(id: &str) -> String {
    json!({"type":"session_meta", "payload":{"id":id}}).to_string()
}

pub fn event(top: Option<&str>, kind: &str) -> String {
    let mut value = json!({"payload":{"type":kind}});
    if let Some(top) = top {
        value["type"] = json!(top);
    }
    value.to_string()
}

/// Observed through Codex 0.147: phase is on event_msg/agent_message.
pub fn legacy_message(text: &str, phase: &str) -> String {
    json!({"type":"event_msg", "payload":{"type":"agent_message", "message":text, "phase":phase}})
        .to_string()
}

/// Codex 0.157: phase is on a response_item assistant message.
pub fn assistant_message(text: &str, phase: &str) -> String {
    json!({"type":"response_item", "payload":{"type":"message", "role":"assistant", "phase":phase,
        "content":[{"type":"output_text", "text":text}]}})
    .to_string()
}

pub fn timestamp(record: &str, timestamp: &str) -> String {
    let mut value: Value = serde_json::from_str(record).expect("fixture JSON");
    value["timestamp"] = json!(timestamp);
    value.to_string()
}
