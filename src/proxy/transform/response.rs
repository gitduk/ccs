use serde_json::{Value, json};
use uuid::Uuid;

use crate::error::{AppError, Result};

/// Convert OpenAI Chat Completion response to Anthropic Messages response.
pub fn openai_to_anthropic_response(resp: &Value) -> Result<Value> {
    let choice = resp
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .ok_or_else(|| AppError::Transform("No choices in OpenAI response".into()))?;

    let message = choice
        .get("message")
        .ok_or_else(|| AppError::Transform("No message in choice".into()))?;

    let mut content_blocks: Vec<Value> = Vec::new();

    // Handle reasoning/thinking content
    if let Some(reasoning) = message.get("reasoning_content").and_then(|r| r.as_str())
        && !reasoning.is_empty()
    {
        content_blocks.push(json!({
            "type": "thinking",
            "thinking": reasoning,
        }));
    }

    // Handle text content
    if let Some(text) = message.get("content").and_then(|c| c.as_str())
        && !text.is_empty()
    {
        content_blocks.push(json!({
            "type": "text",
            "text": text,
        }));
    }

    // Handle tool calls
    if let Some(tool_calls) = message.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in tool_calls {
            let id = tc.get("id").and_then(|i| i.as_str()).unwrap_or("");
            if let Some(func) = tc.get("function") {
                let name = func.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let args_str = func
                    .get("arguments")
                    .and_then(|a| a.as_str())
                    .unwrap_or("{}");
                let input: Value = match serde_json::from_str(args_str) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(
                            "Failed to parse tool arguments for '{name}': {e}; using empty input"
                        );
                        json!({})
                    }
                };
                content_blocks.push(json!({
                    "type": "tool_use",
                    "id": id,
                    "name": name,
                    "input": input,
                }));
            }
        }
    }

    if content_blocks.is_empty() {
        content_blocks.push(json!({"type": "text", "text": ""}));
    }

    // Map stop reason
    let finish_reason = choice
        .get("finish_reason")
        .and_then(|f| f.as_str())
        .unwrap_or("stop");
    let stop_reason = match finish_reason {
        "stop" => "end_turn",
        "length" => "max_tokens",
        "tool_calls" => "tool_use",
        "content_filter" => "content_filter",
        other => other,
    };

    // Usage
    let usage = resp.get("usage");
    let input_tokens = usage
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(|t| t.as_u64())
        .unwrap_or(0);
    let output_tokens = usage
        .and_then(|u| u.get("completion_tokens"))
        .and_then(|t| t.as_u64())
        .unwrap_or(0);

    let model = resp
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("unknown");
    let id = resp
        .get("id")
        .and_then(|i| i.as_str())
        .map(String::from)
        .unwrap_or_else(|| format!("msg_{}", Uuid::new_v4()));

    Ok(json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content_blocks,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
        }
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn simple_text_response(text: &str, finish_reason: &str) -> serde_json::Value {
        json!({
            "id": "chatcmpl-abc",
            "model": "gpt-4o",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": text
                },
                "finish_reason": finish_reason
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 20
            }
        })
    }

    // ─── basic structure ──────────────────────────────────────────────────────

    #[test]
    fn converts_simple_text_response() {
        let resp = simple_text_response("Hello!", "stop");
        let out = openai_to_anthropic_response(&resp).unwrap();

        assert_eq!(out["type"], "message");
        assert_eq!(out["role"], "assistant");
        assert_eq!(out["model"], "gpt-4o");
        assert_eq!(out["content"][0]["type"], "text");
        assert_eq!(out["content"][0]["text"], "Hello!");
    }

    #[test]
    fn preserves_id_from_response() {
        let resp = simple_text_response("Hi", "stop");
        let out = openai_to_anthropic_response(&resp).unwrap();
        assert_eq!(out["id"], "chatcmpl-abc");
    }

    #[test]
    fn generates_id_when_missing() {
        let resp = json!({
            "model": "gpt-4o",
            "choices": [{"message": {"content": "Hi"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        });
        let out = openai_to_anthropic_response(&resp).unwrap();
        let id = out["id"].as_str().unwrap();
        assert!(
            id.starts_with("msg_"),
            "generated id should start with msg_"
        );
    }

    // ─── finish_reason mapping ────────────────────────────────────────────────

    #[test]
    fn finish_reason_stop_maps_to_end_turn() {
        let out = openai_to_anthropic_response(&simple_text_response("Hi", "stop")).unwrap();
        assert_eq!(out["stop_reason"], "end_turn");
    }

    #[test]
    fn finish_reason_length_maps_to_max_tokens() {
        let out = openai_to_anthropic_response(&simple_text_response("Hi", "length")).unwrap();
        assert_eq!(out["stop_reason"], "max_tokens");
    }

    #[test]
    fn finish_reason_tool_calls_maps_to_tool_use() {
        let resp = json!({
            "id": "id",
            "model": "gpt-4o",
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call-1",
                        "type": "function",
                        "function": {"name": "search", "arguments": "{\"q\":\"rust\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 10}
        });
        let out = openai_to_anthropic_response(&resp).unwrap();
        assert_eq!(out["stop_reason"], "tool_use");
    }

    #[test]
    fn unknown_finish_reason_passed_through() {
        let out =
            openai_to_anthropic_response(&simple_text_response("Hi", "content_filter")).unwrap();
        assert_eq!(out["stop_reason"], "content_filter");
    }

    // ─── tool_calls ───────────────────────────────────────────────────────────

    #[test]
    fn tool_calls_converted_to_tool_use_blocks() {
        let resp = json!({
            "id": "id",
            "model": "gpt-4o",
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call-1",
                        "type": "function",
                        "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 10}
        });
        let out = openai_to_anthropic_response(&resp).unwrap();
        let block = &out["content"][0];
        assert_eq!(block["type"], "tool_use");
        assert_eq!(block["id"], "call-1");
        assert_eq!(block["name"], "get_weather");
        assert_eq!(block["input"]["city"], "Paris");
    }

    #[test]
    fn text_and_tool_calls_both_present() {
        let resp = json!({
            "id": "id",
            "model": "gpt-4o",
            "choices": [{
                "message": {
                    "content": "I'll search for that.",
                    "tool_calls": [{
                        "id": "call-1",
                        "type": "function",
                        "function": {"name": "search", "arguments": "{}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 10}
        });
        let out = openai_to_anthropic_response(&resp).unwrap();
        let blocks = out["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[1]["type"], "tool_use");
    }

    // ─── reasoning_content (thinking) ────────────────────────────────────────

    #[test]
    fn reasoning_content_converted_to_thinking_block() {
        let resp = json!({
            "id": "id",
            "model": "gpt-4o",
            "choices": [{
                "message": {
                    "content": "Answer",
                    "reasoning_content": "Let me think..."
                },
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 10}
        });
        let out = openai_to_anthropic_response(&resp).unwrap();
        let blocks = out["content"].as_array().unwrap();
        // thinking block comes first
        assert_eq!(blocks[0]["type"], "thinking");
        assert_eq!(blocks[0]["thinking"], "Let me think...");
        assert_eq!(blocks[1]["type"], "text");
    }

    #[test]
    fn empty_reasoning_content_not_added_as_block() {
        let resp = json!({
            "id": "id",
            "model": "gpt-4o",
            "choices": [{
                "message": {
                    "content": "Answer",
                    "reasoning_content": ""
                },
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        });
        let out = openai_to_anthropic_response(&resp).unwrap();
        let blocks = out["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "text");
    }

    // ─── usage ───────────────────────────────────────────────────────────────

    #[test]
    fn usage_tokens_mapped_correctly() {
        let resp = simple_text_response("Hi", "stop");
        let out = openai_to_anthropic_response(&resp).unwrap();
        assert_eq!(out["usage"]["input_tokens"], 10);
        assert_eq!(out["usage"]["output_tokens"], 20);
    }

    #[test]
    fn missing_usage_defaults_to_zero() {
        let resp = json!({
            "id": "id",
            "model": "m",
            "choices": [{"message": {"content": "Hi"}, "finish_reason": "stop"}]
        });
        let out = openai_to_anthropic_response(&resp).unwrap();
        assert_eq!(out["usage"]["input_tokens"], 0);
        assert_eq!(out["usage"]["output_tokens"], 0);
    }

    // ─── empty content fallback ───────────────────────────────────────────────

    #[test]
    fn empty_content_yields_empty_text_block() {
        let resp = json!({
            "id": "id",
            "model": "m",
            "choices": [{"message": {"content": null}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        });
        let out = openai_to_anthropic_response(&resp).unwrap();
        let blocks = out["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "");
    }

    // ─── error cases ─────────────────────────────────────────────────────────

    #[test]
    fn missing_choices_returns_error() {
        let resp = json!({"id": "id", "model": "m"});
        assert!(openai_to_anthropic_response(&resp).is_err());
    }

    #[test]
    fn empty_choices_array_returns_error() {
        let resp = json!({"id": "id", "model": "m", "choices": []});
        assert!(openai_to_anthropic_response(&resp).is_err());
    }

    #[test]
    fn missing_message_in_choice_returns_error() {
        let resp = json!({
            "id": "id",
            "model": "m",
            "choices": [{"finish_reason": "stop"}]
        });
        assert!(openai_to_anthropic_response(&resp).is_err());
    }
}
