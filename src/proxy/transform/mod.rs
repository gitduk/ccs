mod models;
mod request;
mod response;
mod stream;

pub use models::{anthropic_to_openai_models, openai_to_anthropic_models};
pub use request::{
    anthropic_to_openai_request, map_anthropic_model, openai_to_anthropic_request,
    patch_thinking_history, to_openai,
};
pub use response::{anthropic_to_openai_response, openai_to_anthropic_response};
pub use stream::{anthropic_stream_to_openai, openai_stream_to_anthropic, rewrite_model_in_stream};

use crate::error::{AppError, Result};
use serde_json::Value;

/// Extract `(input, output)` token counts from a response's `usage` object,
/// using the given key names (`input_tokens`/`output_tokens` for Anthropic,
/// `prompt_tokens`/`completion_tokens` for OpenAI chat). Missing keys read 0.
fn usage_tokens(resp: &Value, input_key: &str, output_key: &str) -> (u64, u64) {
    let usage = resp.get("usage");
    let get = |key: &str| {
        usage
            .and_then(|u| u.get(key))
            .and_then(|t| t.as_u64())
            .unwrap_or(0)
    };
    (get(input_key), get(output_key))
}

/// Extract `(id, name, serialized arguments)` from an Anthropic `tool_use`
/// content block, ready to embed in an OpenAI tool/function call.
fn tool_use_parts(block: &Value) -> Result<(&str, &str, String)> {
    let id = block.get("id").and_then(|i| i.as_str()).unwrap_or("");
    let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let input = block
        .get("input")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));
    let arguments = serde_json::to_string(&input)
        .map_err(|e| AppError::Transform(format!("Failed to serialize tool input: {e}")))?;
    Ok((id, name, arguments))
}

/// Map an OpenAI chat `finish_reason` to an Anthropic `stop_reason`.
/// Unknown values pass through unchanged.
fn chat_finish_to_anthropic_stop(reason: &str) -> &str {
    match reason {
        "stop" => "end_turn",
        "length" => "max_tokens",
        "tool_calls" => "tool_use",
        other => other,
    }
}

/// Map an OpenAI Responses API `status` to an Anthropic `stop_reason`.
fn response_status_to_anthropic_stop(status: &str) -> &'static str {
    match status {
        "incomplete" => "max_tokens",
        _ => "end_turn",
    }
}

/// Map an Anthropic `stop_reason` to an OpenAI chat `finish_reason`.
fn anthropic_stop_to_chat_finish(reason: Option<&str>) -> &'static str {
    match reason {
        Some("max_tokens") => "length",
        Some("tool_use") => "tool_calls",
        _ => "stop",
    }
}

/// Map an Anthropic `stop_reason` to an OpenAI Responses API `status`.
fn anthropic_stop_to_response_status(reason: Option<&str>) -> &'static str {
    match reason {
        Some("max_tokens") => "incomplete",
        _ => "completed",
    }
}
