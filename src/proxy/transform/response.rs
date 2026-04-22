use serde_json::{Value, json};
use uuid::Uuid;

use crate::config::OpenAiApiVersion;
use crate::error::{AppError, Result};

/// Convert OpenAI Chat Completion response to Anthropic Messages response.
pub fn openai_to_anthropic_response(resp: &Value) -> Result<Value> {
    if resp.get("choices").is_some() {
        return openai_chat_to_anthropic(resp);
    }
    openai_responses_to_anthropic(resp)
}

fn openai_chat_to_anthropic(resp: &Value) -> Result<Value> {
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

    let finish_reason = choice
        .get("finish_reason")
        .and_then(|f| f.as_str())
        .unwrap_or("stop");
    let stop_reason = super::chat_finish_to_anthropic_stop(finish_reason);

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

fn openai_responses_to_anthropic(resp: &Value) -> Result<Value> {
    let output = resp
        .get("output")
        .and_then(|v| v.as_array())
        .ok_or_else(|| AppError::Transform("No output in OpenAI Responses response".into()))?;

    let mut content_blocks: Vec<Value> = Vec::new();

    for item in output {
        match item.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "message" => {
                if let Some(parts) = item.get("content").and_then(|c| c.as_array()) {
                    for part in parts {
                        let part_type = part.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        if (part_type == "output_text" || part_type == "text")
                            && let Some(text) = part.get("text").and_then(|t| t.as_str())
                            && !text.is_empty()
                        {
                            content_blocks.push(json!({
                                "type": "text",
                                "text": text,
                            }));
                        }
                    }
                }
            }
            "function_call" => {
                let id = item
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .or_else(|| item.get("id").and_then(|v| v.as_str()))
                    .unwrap_or("");
                let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let args_str = item
                    .get("arguments")
                    .and_then(|a| a.as_str())
                    .unwrap_or("{}");
                let input: Value = serde_json::from_str(args_str).unwrap_or_else(|e| {
                    tracing::warn!(
                        "Failed to parse function_call arguments for '{name}': {e}; using empty input"
                    );
                    json!({})
                });
                content_blocks.push(json!({
                    "type": "tool_use",
                    "id": id,
                    "name": name,
                    "input": input,
                }));
            }
            _ => {}
        }
    }

    if content_blocks.is_empty() {
        content_blocks.push(json!({"type": "text", "text": ""}));
    }

    let status = resp
        .get("status")
        .and_then(|s| s.as_str())
        .unwrap_or("completed");
    let stop_reason = super::response_status_to_anthropic_stop(status);

    let usage = resp.get("usage");
    let input_tokens = usage
        .and_then(|u| u.get("input_tokens"))
        .and_then(|t| t.as_u64())
        .unwrap_or(0);
    let output_tokens = usage
        .and_then(|u| u.get("output_tokens"))
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

/// Convert an Anthropic Messages API response into the OpenAI shape the
/// client expects. Dispatches on the client-facing API variant (Chat
/// Completions vs Responses).
pub fn anthropic_to_openai_response(resp: &Value, api_version: OpenAiApiVersion) -> Result<Value> {
    match api_version {
        OpenAiApiVersion::ChatCompletions => anthropic_to_openai_chat(resp),
        OpenAiApiVersion::Responses => anthropic_to_openai_responses(resp),
    }
}

fn anthropic_to_openai_chat(resp: &Value) -> Result<Value> {
    let content_blocks = resp
        .get("content")
        .and_then(|c| c.as_array())
        .ok_or_else(|| AppError::Transform("No content in Anthropic response".into()))?;

    let mut text_content = String::new();
    let mut reasoning_content = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();

    for block in content_blocks {
        match block.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "text" => {
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    text_content.push_str(text);
                }
            }
            "thinking" => {
                if let Some(text) = block.get("thinking").and_then(|t| t.as_str()) {
                    reasoning_content.push_str(text);
                }
            }
            "tool_use" => {
                let id = block.get("id").and_then(|i| i.as_str()).unwrap_or("");
                let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let input = block.get("input").cloned().unwrap_or(json!({}));
                let arguments = serde_json::to_string(&input).map_err(|e| {
                    AppError::Transform(format!("Failed to serialize tool input: {e}"))
                })?;
                tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": arguments,
                    }
                }));
            }
            _ => {}
        }
    }

    let mut message = json!({ "role": "assistant" });
    if text_content.is_empty() {
        message["content"] = Value::Null;
    } else {
        message["content"] = json!(text_content);
    }
    if !reasoning_content.is_empty() {
        message["reasoning_content"] = json!(reasoning_content);
    }
    if !tool_calls.is_empty() {
        message["tool_calls"] = json!(tool_calls);
    }

    let stop_reason = resp.get("stop_reason").and_then(|s| s.as_str());
    let finish_reason = super::anthropic_stop_to_chat_finish(stop_reason);

    let usage = resp.get("usage");
    let input_tokens = usage
        .and_then(|u| u.get("input_tokens"))
        .and_then(|t| t.as_u64())
        .unwrap_or(0);
    let output_tokens = usage
        .and_then(|u| u.get("output_tokens"))
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
        .unwrap_or_else(|| format!("chatcmpl-{}", Uuid::new_v4()));
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    Ok(json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason,
        }],
        "usage": {
            "prompt_tokens": input_tokens,
            "completion_tokens": output_tokens,
            "total_tokens": input_tokens + output_tokens,
        }
    }))
}

fn anthropic_to_openai_responses(resp: &Value) -> Result<Value> {
    let content_blocks = resp
        .get("content")
        .and_then(|c| c.as_array())
        .ok_or_else(|| AppError::Transform("No content in Anthropic response".into()))?;

    let mut output_items: Vec<Value> = Vec::new();
    let mut message_text_parts: Vec<Value> = Vec::new();
    let mut reasoning_text = String::new();

    let flush_message = |parts: &mut Vec<Value>, out: &mut Vec<Value>| {
        if !parts.is_empty() {
            out.push(json!({
                "type": "message",
                "role": "assistant",
                "content": std::mem::take(parts),
            }));
        }
    };

    for block in content_blocks {
        match block.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "text" => {
                if let Some(text) = block.get("text").and_then(|t| t.as_str())
                    && !text.is_empty()
                {
                    message_text_parts.push(json!({
                        "type": "output_text",
                        "text": text,
                        "annotations": [],
                    }));
                }
            }
            "thinking" => {
                if let Some(text) = block.get("thinking").and_then(|t| t.as_str()) {
                    reasoning_text.push_str(text);
                }
            }
            "tool_use" => {
                // Tool calls are siblings of message items. Emit any pending
                // message parts first so output order matches input order.
                flush_message(&mut message_text_parts, &mut output_items);
                let id = block.get("id").and_then(|i| i.as_str()).unwrap_or("");
                let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let input = block.get("input").cloned().unwrap_or(json!({}));
                let arguments = serde_json::to_string(&input).map_err(|e| {
                    AppError::Transform(format!("Failed to serialize tool input: {e}"))
                })?;
                output_items.push(json!({
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    "arguments": arguments,
                }));
            }
            _ => {}
        }
    }
    flush_message(&mut message_text_parts, &mut output_items);

    if !reasoning_text.is_empty() {
        output_items.insert(
            0,
            json!({
                "type": "reasoning",
                "summary": [{"type": "summary_text", "text": reasoning_text}],
            }),
        );
    }

    let stop_reason = resp.get("stop_reason").and_then(|s| s.as_str());
    let status = super::anthropic_stop_to_response_status(stop_reason);

    let usage = resp.get("usage");
    let input_tokens = usage
        .and_then(|u| u.get("input_tokens"))
        .and_then(|t| t.as_u64())
        .unwrap_or(0);
    let output_tokens = usage
        .and_then(|u| u.get("output_tokens"))
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
        .unwrap_or_else(|| format!("resp_{}", Uuid::new_v4()));
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    Ok(json!({
        "id": id,
        "object": "response",
        "created_at": created,
        "model": model,
        "status": status,
        "output": output_items,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "total_tokens": input_tokens + output_tokens,
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

    // ─── responses api ───────────────────────────────────────────────────────

    #[test]
    fn responses_api_text_output_converted() {
        let resp = json!({
            "id": "resp_123",
            "model": "gpt-4.1",
            "status": "completed",
            "output": [{
                "type": "message",
                "content": [{"type": "output_text", "text": "hello from responses"}]
            }],
            "usage": {"input_tokens": 11, "output_tokens": 22}
        });
        let out = openai_to_anthropic_response(&resp).unwrap();
        assert_eq!(out["id"], "resp_123");
        assert_eq!(out["model"], "gpt-4.1");
        assert_eq!(out["content"][0]["type"], "text");
        assert_eq!(out["content"][0]["text"], "hello from responses");
        assert_eq!(out["usage"]["input_tokens"], 11);
        assert_eq!(out["usage"]["output_tokens"], 22);
        assert_eq!(out["stop_reason"], "end_turn");
    }

    #[test]
    fn responses_api_function_call_converted_to_tool_use() {
        let resp = json!({
            "id": "resp_fc",
            "model": "gpt-4.1",
            "status": "completed",
            "output": [{
                "type": "function_call",
                "call_id": "call_abc",
                "name": "search",
                "arguments": "{\"q\":\"rust\"}"
            }]
        });
        let out = openai_to_anthropic_response(&resp).unwrap();
        assert_eq!(out["content"][0]["type"], "tool_use");
        assert_eq!(out["content"][0]["id"], "call_abc");
        assert_eq!(out["content"][0]["name"], "search");
        assert_eq!(out["content"][0]["input"]["q"], "rust");
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

    // ─── anthropic_to_openai_response: ChatCompletions ───────────────────────

    fn simple_anthropic_response(text: &str, stop_reason: &str) -> Value {
        json!({
            "id": "msg_abc",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4",
            "content": [{"type": "text", "text": text}],
            "stop_reason": stop_reason,
            "stop_sequence": null,
            "usage": {"input_tokens": 5, "output_tokens": 7}
        })
    }

    #[test]
    fn anthropic_to_chat_converts_simple_text() {
        let resp = simple_anthropic_response("hello", "end_turn");
        let out = anthropic_to_openai_response(&resp, OpenAiApiVersion::ChatCompletions).unwrap();
        assert_eq!(out["object"], "chat.completion");
        assert_eq!(out["model"], "claude-sonnet-4");
        assert_eq!(out["choices"][0]["index"], 0);
        assert_eq!(out["choices"][0]["message"]["role"], "assistant");
        assert_eq!(out["choices"][0]["message"]["content"], "hello");
        assert_eq!(out["choices"][0]["finish_reason"], "stop");
        assert_eq!(out["usage"]["prompt_tokens"], 5);
        assert_eq!(out["usage"]["completion_tokens"], 7);
        assert_eq!(out["usage"]["total_tokens"], 12);
    }

    #[test]
    fn anthropic_to_chat_stop_reason_max_tokens_maps_to_length() {
        let resp = simple_anthropic_response("partial", "max_tokens");
        let out = anthropic_to_openai_response(&resp, OpenAiApiVersion::ChatCompletions).unwrap();
        assert_eq!(out["choices"][0]["finish_reason"], "length");
    }

    #[test]
    fn anthropic_to_chat_stop_reason_tool_use_maps_to_tool_calls() {
        let resp = simple_anthropic_response("", "tool_use");
        let out = anthropic_to_openai_response(&resp, OpenAiApiVersion::ChatCompletions).unwrap();
        assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
    }

    #[test]
    fn anthropic_to_chat_stop_sequence_maps_to_stop() {
        let resp = simple_anthropic_response("bye", "stop_sequence");
        let out = anthropic_to_openai_response(&resp, OpenAiApiVersion::ChatCompletions).unwrap();
        assert_eq!(out["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn anthropic_to_chat_tool_use_becomes_tool_calls() {
        let resp = json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "m",
            "content": [{
                "type": "tool_use",
                "id": "toolu_1",
                "name": "search",
                "input": {"q": "rust"}
            }],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        });
        let out = anthropic_to_openai_response(&resp, OpenAiApiVersion::ChatCompletions).unwrap();
        let tc = &out["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(tc["id"], "toolu_1");
        assert_eq!(tc["type"], "function");
        assert_eq!(tc["function"]["name"], "search");
        let args: Value =
            serde_json::from_str(tc["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args, json!({"q": "rust"}));
        assert!(out["choices"][0]["message"]["content"].is_null());
    }

    #[test]
    fn anthropic_to_chat_thinking_becomes_reasoning_content() {
        let resp = json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "m",
            "content": [
                {"type": "thinking", "thinking": "I should..."},
                {"type": "text", "text": "answer"}
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 2}
        });
        let out = anthropic_to_openai_response(&resp, OpenAiApiVersion::ChatCompletions).unwrap();
        assert_eq!(
            out["choices"][0]["message"]["reasoning_content"],
            "I should..."
        );
        assert_eq!(out["choices"][0]["message"]["content"], "answer");
    }

    #[test]
    fn anthropic_to_chat_generates_id_when_missing() {
        let mut resp = simple_anthropic_response("x", "end_turn");
        resp.as_object_mut().unwrap().remove("id");
        let out = anthropic_to_openai_response(&resp, OpenAiApiVersion::ChatCompletions).unwrap();
        let id = out["id"].as_str().unwrap();
        assert!(id.starts_with("chatcmpl-"));
    }

    #[test]
    fn anthropic_to_chat_missing_content_returns_error() {
        let resp = json!({"id": "x", "model": "m", "stop_reason": "end_turn"});
        assert!(anthropic_to_openai_response(&resp, OpenAiApiVersion::ChatCompletions).is_err());
    }

    // ─── anthropic_to_openai_response: Responses ─────────────────────────────

    #[test]
    fn anthropic_to_responses_text_becomes_output_message() {
        let resp = simple_anthropic_response("hello", "end_turn");
        let out = anthropic_to_openai_response(&resp, OpenAiApiVersion::Responses).unwrap();
        assert_eq!(out["object"], "response");
        assert_eq!(out["status"], "completed");
        let item = &out["output"][0];
        assert_eq!(item["type"], "message");
        assert_eq!(item["role"], "assistant");
        assert_eq!(item["content"][0]["type"], "output_text");
        assert_eq!(item["content"][0]["text"], "hello");
    }

    #[test]
    fn anthropic_to_responses_tool_use_becomes_function_call() {
        let resp = json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "m",
            "content": [{
                "type": "tool_use",
                "id": "toolu_1",
                "name": "search",
                "input": {"q": "rust"}
            }],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        });
        let out = anthropic_to_openai_response(&resp, OpenAiApiVersion::Responses).unwrap();
        let item = &out["output"][0];
        assert_eq!(item["type"], "function_call");
        assert_eq!(item["call_id"], "toolu_1");
        assert_eq!(item["name"], "search");
        let args: Value = serde_json::from_str(item["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args, json!({"q": "rust"}));
    }

    #[test]
    fn anthropic_to_responses_max_tokens_sets_incomplete_status() {
        let resp = simple_anthropic_response("partial", "max_tokens");
        let out = anthropic_to_openai_response(&resp, OpenAiApiVersion::Responses).unwrap();
        assert_eq!(out["status"], "incomplete");
    }

    #[test]
    fn anthropic_to_responses_thinking_prepends_reasoning_item() {
        let resp = json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "m",
            "content": [
                {"type": "thinking", "thinking": "plan"},
                {"type": "text", "text": "answer"}
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 2}
        });
        let out = anthropic_to_openai_response(&resp, OpenAiApiVersion::Responses).unwrap();
        assert_eq!(out["output"][0]["type"], "reasoning");
        assert_eq!(out["output"][0]["summary"][0]["text"], "plan");
    }

    #[test]
    fn anthropic_to_responses_preserves_output_order_text_then_tool() {
        let resp = json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "m",
            "content": [
                {"type": "text", "text": "thinking out loud"},
                {"type": "tool_use", "id": "t1", "name": "search", "input": {}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 1, "output_tokens": 2}
        });
        let out = anthropic_to_openai_response(&resp, OpenAiApiVersion::Responses).unwrap();
        assert_eq!(out["output"][0]["type"], "message");
        assert_eq!(out["output"][1]["type"], "function_call");
    }

    #[test]
    fn anthropic_to_responses_generates_id_when_missing() {
        let mut resp = simple_anthropic_response("x", "end_turn");
        resp.as_object_mut().unwrap().remove("id");
        let out = anthropic_to_openai_response(&resp, OpenAiApiVersion::Responses).unwrap();
        let id = out["id"].as_str().unwrap();
        assert!(id.starts_with("resp_"));
    }
}
