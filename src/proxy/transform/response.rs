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
        && !reasoning.is_empty() {
            content_blocks.push(json!({
                "type": "thinking",
                "thinking": reasoning,
            }));
        }

    // Handle text content
    if let Some(text) = message.get("content").and_then(|c| c.as_str())
        && !text.is_empty() {
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
                let input: Value = serde_json::from_str(args_str).unwrap_or(json!({}));
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
