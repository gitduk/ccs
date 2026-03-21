use serde_json::{json, Value};

use crate::config::Provider;
use crate::error::Result;

// Default model names
const DEFAULT_MODEL: &str = "claude-sonnet-4-20250514";

/// Convert an Anthropic Messages API request to OpenAI Chat Completions format.
pub fn anthropic_to_openai_request(req: &Value, provider: &Provider) -> Result<Value> {
    let model = req
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or(DEFAULT_MODEL);
    let mapped_model = provider.map_model(model);

    let mut messages: Vec<Value> = Vec::new();

    // Convert system message
    if let Some(system) = req.get("system") {
        let system_text = match system {
            Value::String(s) => s.clone(),
            Value::Array(blocks) => blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        if !system_text.is_empty() {
            messages.push(json!({"role": "system", "content": system_text}));
        }
    }

    // Convert messages
    if let Some(msgs) = req.get("messages").and_then(|m| m.as_array()) {
        for msg in msgs {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let converted = convert_message_to_openai(role, msg)?;
            messages.extend(converted);
        }
    }

    let mut result = json!({
        "model": mapped_model,
        "messages": messages,
        "stream": req.get("stream").and_then(|s| s.as_bool()).unwrap_or(false),
    });

    // max_tokens
    if let Some(max_tokens) = req.get("max_tokens") {
        result["max_tokens"] = max_tokens.clone();
    }

    // temperature
    if let Some(temp) = req.get("temperature") {
        result["temperature"] = temp.clone();
    }

    // top_p
    if let Some(top_p) = req.get("top_p") {
        result["top_p"] = top_p.clone();
    }

    // stop_sequences → stop
    if let Some(stop) = req.get("stop_sequences") {
        result["stop"] = stop.clone();
    }

    // tools → OpenAI tools format
    if let Some(tools) = req.get("tools").and_then(|t| t.as_array()) {
        let openai_tools: Vec<Value> = tools
            .iter()
            .filter_map(|tool| {
                let name = tool.get("name")?.as_str()?;
                let description = tool
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("");
                let mut input_schema = tool
                    .get("input_schema")
                    .cloned()
                    .unwrap_or(json!({"type": "object"}));
                clean_schema(&mut input_schema);
                Some(json!({
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": description,
                        "parameters": input_schema,
                    }
                }))
            })
            .collect();
        if !openai_tools.is_empty() {
            result["tools"] = json!(openai_tools);
        }
    }

    // tool_choice
    if let Some(tool_choice) = req.get("tool_choice") {
        if let Some(tc_type) = tool_choice.get("type").and_then(|t| t.as_str()) {
            match tc_type {
                "any" => result["tool_choice"] = json!("required"),
                "auto" => result["tool_choice"] = json!("auto"),
                "none" => result["tool_choice"] = json!("none"),
                "tool" => {
                    if let Some(name) = tool_choice.get("name").and_then(|n| n.as_str()) {
                        result["tool_choice"] = json!({
                            "type": "function",
                            "function": { "name": name }
                        });
                    }
                }
                _ => {}
            }
        }
    }

    // thinking/extended thinking → reasoning
    if let Some(thinking) = req.get("thinking") {
        if let Some(enabled) = thinking.get("enabled").and_then(|e| e.as_bool()) {
            if enabled {
                // OpenAI-compatible providers use different reasoning params
                // Some use "reasoning_effort", some just pass through
                if let Some(budget) = thinking.get("budget_tokens") {
                    result["reasoning_effort"] = json!("high");
                    result["max_completion_tokens"] = budget.clone();
                }
            }
        }
    }

    // Stream options for OpenAI
    if req.get("stream").and_then(|s| s.as_bool()).unwrap_or(false) {
        result["stream_options"] = json!({"include_usage": true});
    }

    Ok(result)
}

/// Convert a single Anthropic message to OpenAI message(s).
fn convert_message_to_openai(role: &str, msg: &Value) -> Result<Vec<Value>> {
    let content = msg.get("content");

    match content {
        Some(Value::String(text)) => {
            let openai_role = match role {
                "assistant" => "assistant",
                _ => "user",
            };
            Ok(vec![json!({"role": openai_role, "content": text})])
        }
        Some(Value::Array(blocks)) => convert_content_blocks_to_openai(role, blocks),
        _ => Ok(vec![json!({"role": role, "content": ""})]),
    }
}

/// Convert Anthropic content blocks to OpenAI messages.
fn convert_content_blocks_to_openai(role: &str, blocks: &[Value]) -> Result<Vec<Value>> {
    match role {
        "user" => convert_user_blocks_to_openai(blocks),
        "assistant" => convert_assistant_blocks_to_openai(blocks),
        _ => Ok(vec![json!({"role": role, "content": ""})]),
    }
}

fn convert_user_blocks_to_openai(blocks: &[Value]) -> Result<Vec<Value>> {
    let mut messages = Vec::new();
    let mut content_parts: Vec<Value> = Vec::new();
    let mut tool_results: Vec<Value> = Vec::new();

    for block in blocks {
        let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match block_type {
            "text" => {
                let text = block.get("text").and_then(|t| t.as_str()).unwrap_or("");
                content_parts.push(json!({"type": "text", "text": text}));
            }
            "image" => {
                if let Some(source) = block.get("source") {
                    let media_type = source
                        .get("media_type")
                        .and_then(|m| m.as_str())
                        .unwrap_or("image/png");
                    let data = source.get("data").and_then(|d| d.as_str()).unwrap_or("");
                    content_parts.push(json!({
                        "type": "image_url",
                        "image_url": {
                            "url": format!("data:{media_type};base64,{data}")
                        }
                    }));
                }
            }
            "tool_result" => {
                let tool_use_id = block
                    .get("tool_use_id")
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                let content_text = extract_tool_result_content(block);
                tool_results.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_use_id,
                    "content": content_text,
                }));
            }
            _ => {}
        }
    }

    // Emit user message with content parts if any
    if !content_parts.is_empty() {
        if content_parts.len() == 1
            && content_parts[0].get("type").and_then(|t| t.as_str()) == Some("text")
        {
            // Single text → plain string content
            messages.push(json!({
                "role": "user",
                "content": content_parts[0].get("text").and_then(|t| t.as_str()).unwrap_or("")
            }));
        } else {
            messages.push(json!({"role": "user", "content": content_parts}));
        }
    }

    // Emit tool results
    messages.extend(tool_results);

    if messages.is_empty() {
        messages.push(json!({"role": "user", "content": ""}));
    }

    Ok(messages)
}

fn convert_assistant_blocks_to_openai(blocks: &[Value]) -> Result<Vec<Value>> {
    let mut text_content = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut reasoning_content = String::new();

    for block in blocks {
        let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match block_type {
            "text" => {
                let text = block.get("text").and_then(|t| t.as_str()).unwrap_or("");
                text_content.push_str(text);
            }
            "tool_use" => {
                let id = block.get("id").and_then(|i| i.as_str()).unwrap_or("");
                let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let input = block.get("input").cloned().unwrap_or(json!({}));
                tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": serde_json::to_string(&input).unwrap_or_default(),
                    }
                }));
            }
            "thinking" => {
                let thinking_text = block.get("thinking").and_then(|t| t.as_str()).unwrap_or("");
                reasoning_content.push_str(thinking_text);
            }
            _ => {}
        }
    }

    let mut msg = json!({"role": "assistant"});

    if !text_content.is_empty() {
        msg["content"] = json!(text_content);
    } else {
        msg["content"] = json!(null);
    }

    if !tool_calls.is_empty() {
        msg["tool_calls"] = json!(tool_calls);
    }

    if !reasoning_content.is_empty() {
        msg["reasoning_content"] = json!(reasoning_content);
    }

    Ok(vec![msg])
}

fn extract_tool_result_content(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| {
                if p.get("type").and_then(|t| t.as_str()) == Some("text") {
                    p.get("text").and_then(|t| t.as_str()).map(String::from)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Recursively remove `"format": "uri"` from JSON schemas.
/// Some OpenAI-compatible providers reject this format specifier.
pub fn clean_schema(schema: &mut Value) {
    if let Some(obj) = schema.as_object_mut() {
        if obj.get("format").and_then(|f| f.as_str()) == Some("uri") {
            obj.remove("format");
        }
        if let Some(props) = obj.get_mut("properties") {
            if let Some(props_obj) = props.as_object_mut() {
                for (_key, prop) in props_obj.iter_mut() {
                    clean_schema(prop);
                }
            }
        }
        if let Some(items) = obj.get_mut("items") {
            clean_schema(items);
        }
    }
}
