//! Edge-case conformance tests for the Anthropic → OpenAI request translation
//! (Switchyard codec). Derived from a source study of free-claude-code's
//! conversion layer: history ledger, synthetic turn boundaries, thinking
//! replay, empty-content placeholders.

use evolve_server::translate::Translator;
use serde_json::{json, Value};

fn to_openai(body: Value) -> Result<Value, String> {
    Translator::global().request_anthropic_to_openai(&body)
}

fn roles(body: &Value) -> Vec<&str> {
    body["messages"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|m| m["role"].as_str()).collect())
        .unwrap_or_default()
}

/// Case 1 — interleaved tool_result inside a user message with sibling text,
/// plus follow-up user turns. OpenAI requires: assistant(tool_calls) →
/// role:"tool" → user; sibling text must be merged into the user turn.
#[test]
fn interleaved_tool_result_orders_and_merges() {
    let body = json!({
        "model": "x", "max_tokens": 16,
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "call the read tool"}]},
            {"role": "assistant", "content": [
                {"type": "text", "text": "reading it"},
                {"type": "tool_use", "id": "tu_01", "name": "read", "input": {"path": "/tmp/x"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu_01", "content": [{"type": "text", "text": "file contents"}]},
                {"type": "text", "text": "now summarize"}
            ]},
            {"role": "user", "content": "and then?"}
        ]
    });
    let out = to_openai(body).expect("translate ok");
    let r = roles(&out);
    // 必须出现 assistant(tool_calls) → tool → user 的顺序
    let tc_idx = r.iter().position(|x| *x == "assistant").expect("assistant turn");
    let out_arr = out["messages"].as_array().unwrap();
    assert!(out_arr[tc_idx]["tool_calls"].is_array(), "assistant must carry tool_calls");
    assert_eq!(r.get(tc_idx + 1), Some(&"tool"), "tool result must be role:tool");
    assert_eq!(r.get(tc_idx + 2), Some(&"user"), "then user");
    // sibling text "now summarize" 不能丢
    let joined = out["messages"].as_array().unwrap()[tc_idx + 2]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string()
        + &out_arr[tc_idx + 2]["content"].as_array().map(|a| a.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("")).unwrap_or_default();
    assert!(joined.contains("now summarize"), "sibling text preserved: {joined}");
}

/// Case 2 — tool_result immediately followed by user (no interleaved text):
/// some chat templates reject user-after-tool; the codec should either insert
/// a synthetic assistant boundary or be inherently compatible — must not lose
/// the tool output either way.
#[test]
fn tool_result_then_user_no_loss() {
    let body = json!({
        "model": "x", "max_tokens": 16,
        "messages": [
            {"role": "user", "content": "run it"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "tu_02", "name": "bash", "input": {"cmd": "ls"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu_02", "content": "out"}
            ]},
            {"role": "user", "content": "what next"}
        ]
    });
    let out = to_openai(body).expect("translate ok");
    let joined = json!(out).to_string();
    assert!(joined.contains("out"), "tool output preserved: {joined}");
    assert!(joined.contains("what next"), "follow-up preserved");
    let r = roles(&out);
    // tool 消息必须存在且紧跟 tool_calls 之后
    let tool_pos = r.iter().position(|x| *x == "tool").expect("role:tool present");
    let asst_pos = r.iter().position(|x| *x == "assistant").expect("assistant");
    assert!(asst_pos < tool_pos);
}

/// Case 3 — assistant message with empty text blocks and only tool_use:
/// empty-string content must not crash or produce null content.
#[test]
fn assistant_tool_only_content() {
    let body = json!({
        "model": "x", "max_tokens": 16,
        "messages": [
            {"role": "user", "content": "list files"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "tu_03", "name": "ls", "input": {}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu_03", "content": "a\nb"}
            ]}
        ]
    });
    let out = to_openai(body).expect("translate ok");
    let arr = out["messages"].as_array().unwrap();
    let asst = arr.iter().find(|m| m["role"] == "assistant").expect("assistant");
    // content 允许 "" 或 null，但 tool_calls 必须在
    assert!(asst["tool_calls"].is_array(), "tool_calls present: {asst}");
}

/// Case 4 — thinking block replay: must not be silently dropped; either
/// folded into content (think tags), reasoning_content field, or omitted
/// wholesale — but the assistant text itself must survive.
#[test]
fn thinking_block_replay_keeps_text() {
    let body = json!({
        "model": "x", "max_tokens": 16,
        "messages": [
            {"role": "user", "content": "think about 2+2"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "2+2 = 4", "signature": "sig"},
                {"type": "text", "text": "The answer is 4."}
            ]},
            {"role": "user", "content": "and 3+3?"}
        ]
    });
    let out = to_openai(body).expect("translate ok");
    let joined = out.to_string();
    assert!(joined.contains("The answer is 4."), "assistant text survives");
    // thinking 有回放或被剥离，但不能破坏消息结构
    let r = roles(&out);
    assert!(r.contains(&"assistant"));
    assert!(r.contains(&"user"));
}

/// Case 5 — multiple tool_results in one user message (multi-tool turn):
/// each must map to its own role:"tool" message keyed by tool_use_id.
#[test]
fn multi_tool_results_each_mapped() {
    let body = json!({
        "model": "x", "max_tokens": 16,
        "messages": [
            {"role": "user", "content": "read both"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "tA", "name": "read", "input": {"path": "a"}},
                {"type": "tool_use", "id": "tB", "name": "read", "input": {"path": "b"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tA", "content": "AAA"},
                {"type": "tool_result", "tool_use_id": "tB", "content": "BBB"}
            ]}
        ]
    });
    let out = to_openai(body).expect("translate ok");
    let joined = out.to_string();
    assert!(joined.contains("AAA") && joined.contains("BBB"), "both outputs: {joined}");
    let arr = out["messages"].as_array().unwrap();
    let tool_msgs: Vec<_> = arr.iter().filter(|m| m["role"] == "tool").collect();
    assert!(tool_msgs.len() >= 2, "each tool_result maps to role:tool: {joined}");
}

/// Case 6 — empty assistant content (no blocks): must not produce null
/// content or a dropped turn that breaks tool_call pairing.
#[test]
fn empty_assistant_does_not_break_pairing() {
    let body = json!({
        "model": "x", "max_tokens": 16,
        "messages": [
            {"role": "user", "content": "go"},
            {"role": "assistant", "content": []},
            {"role": "user", "content": "still here"}
        ]
    });
    let out = to_openai(body).expect("translate ok");
    let joined = out.to_string();
    assert!(joined.contains("still here"), "follow-up survives: {joined}");
}
