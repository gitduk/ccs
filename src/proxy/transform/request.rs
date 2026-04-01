use serde_json::{Value, json};

use crate::config::Provider;
use crate::error::{AppError, Result};

// Default model names
const DEFAULT_MODEL: &str = "claude-sonnet-4-20250514";

/// Convert an Anthropic Messages API request to OpenAI format.
/// Automatically detects and uses the appropriate format based on provider configuration.
pub fn anthropic_to_openai_request(req: &Value, provider: &Provider) -> Result<Value> {
    if provider.uses_responses_api() {
        anthropic_to_openai_responses_request(req, provider)
    } else {
        anthropic_to_openai_chat_completions_request(req, provider)
    }
}

/// Extract system text from Anthropic request
fn extract_system_text(req: &Value) -> String {
    req.get("system")
        .map(|system| match system {
            Value::String(s) => s.clone(),
            Value::Array(blocks) => blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        })
        .unwrap_or_default()
}

/// Convert an Anthropic Messages API request to OpenAI Responses API format.
pub fn anthropic_to_openai_responses_request(req: &Value, provider: &Provider) -> Result<Value> {
    let model = req
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or(DEFAULT_MODEL);
    let mapped_model = provider.map_model(model);

    // For Responses API, we need to build input array instead of messages
    let mut input: Vec<Value> = Vec::new();

    // Convert system message using extracted helper
    let system_text = extract_system_text(req);
    if !system_text.is_empty() {
        input.push(json!({"role": "system", "content": system_text}));
    }

    // Convert messages for Responses API (same structure as Chat Completions)
    if let Some(msgs) = req.get("messages").and_then(|m| m.as_array()) {
        for msg in msgs {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let converted = convert_message_to_openai_responses(role, msg)?;
            input.extend(converted);
        }
    }

    let mut result = json!({
        "model": mapped_model,
        "input": input,  // 使用 input 而不是 messages
        "stream": req.get("stream").and_then(|s| s.as_bool()).unwrap_or(false),
    });

    // 复制其他参数
    copy_common_parameters(req, &mut result);

    // tools → OpenAI tools format (Responses API 使用相同的工具格式)
    if let Some(tools) = req.get("tools").and_then(|t| t.as_array()) {
        let openai_tools = convert_tools_to_openai(tools);
        if !openai_tools.is_empty() {
            result["tools"] = json!(openai_tools);
        }
    }

    // tool_choice (Responses API 使用相同的格式)
    if let Some(tool_choice) = req.get("tool_choice") {
        result["tool_choice"] = convert_tool_choice_to_openai(tool_choice);
    }

    // thinking/extended thinking → reasoning (Responses API 特有)
    if let Some(thinking) = req.get("thinking")
        && let Some(enabled) = thinking.get("enabled").and_then(|e| e.as_bool())
        && enabled
    {
        result["reasoning_effort"] = json!("high");
        if let Some(budget) = thinking.get("budget_tokens") {
            result["max_completion_tokens"] = budget.clone();
        }
    }

    Ok(result)
}

/// Convert an Anthropic Messages API request to OpenAI Chat Completions format.
pub fn anthropic_to_openai_chat_completions_request(
    req: &Value,
    provider: &Provider,
) -> Result<Value> {
    let model = req
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or(DEFAULT_MODEL);
    let mapped_model = provider.map_model(model);

    let mut messages: Vec<Value> = Vec::new();

    // Convert system message using extracted helper
    let system_text = extract_system_text(req);
    if !system_text.is_empty() {
        messages.push(json!({"role": "system", "content": system_text}));
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
        "messages": messages,  // Chat Completions 使用 messages
        "stream": req.get("stream").and_then(|s| s.as_bool()).unwrap_or(false),
    });

    // 复制其他参数
    copy_common_parameters(req, &mut result);

    // tools → OpenAI tools format
    if let Some(tools) = req.get("tools").and_then(|t| t.as_array()) {
        let openai_tools = convert_tools_to_openai(tools);
        if !openai_tools.is_empty() {
            result["tools"] = json!(openai_tools);
        }
    }

    // tool_choice
    if let Some(tool_choice) = req.get("tool_choice") {
        result["tool_choice"] = convert_tool_choice_to_openai(tool_choice);
    }

    // thinking/extended thinking → reasoning (Chat Completions 兼容)
    if let Some(thinking) = req.get("thinking")
        && let Some(enabled) = thinking.get("enabled").and_then(|e| e.as_bool())
        && enabled
        && let Some(budget) = thinking.get("budget_tokens")
    {
        result["reasoning_effort"] = json!("high");
        result["max_completion_tokens"] = budget.clone();
    }

    // Stream options for OpenAI Chat Completions
    if req.get("stream").and_then(|s| s.as_bool()).unwrap_or(false) {
        result["stream_options"] = json!({"include_usage": true});
    }

    Ok(result)
}

/// Copy common parameters (max_tokens, temperature, top_p, stop) to result
fn copy_common_parameters(req: &Value, result: &mut Value) {
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
}

/// Convert tool definitions to OpenAI format
fn convert_tools_to_openai(tools: &[Value]) -> Vec<Value> {
    tools
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
        .collect()
}

/// Convert tool choice to OpenAI format
fn convert_tool_choice_to_openai(tool_choice: &Value) -> Value {
    if let Some(tc_type) = tool_choice.get("type").and_then(|t| t.as_str()) {
        match tc_type {
            "any" => json!("required"),
            "auto" => json!("auto"),
            "none" => json!("none"),
            "tool" => {
                if let Some(name) = tool_choice.get("name").and_then(|n| n.as_str()) {
                    json!({
                        "type": "function",
                        "function": { "name": name }
                    })
                } else {
                    json!("auto")
                }
            }
            _ => json!("auto"),
        }
    } else {
        json!("auto")
    }
}

/// Convert a single Anthropic message to OpenAI Responses API message(s).
/// Key difference: uses "call_id" instead of "tool_call_id" for tool results.
fn convert_message_to_openai_responses(role: &str, msg: &Value) -> Result<Vec<Value>> {
    let content = msg.get("content");

    match content {
        Some(Value::String(text)) => {
            let openai_role = match role {
                "assistant" => "assistant",
                _ => "user",
            };
            Ok(vec![json!({"role": openai_role, "content": text})])
        }
        Some(Value::Array(blocks)) => convert_content_blocks_to_openai_responses(role, blocks),
        _ => Ok(vec![json!({"role": role, "content": ""})]),
    }
}

/// Convert Anthropic content blocks to OpenAI messages.
/// Uses "call_id" vs "tool_call_id" based on API version.
fn convert_content_blocks_to_openai(
    role: &str,
    blocks: &[Value],
    use_call_id: bool,
) -> Result<Vec<Value>> {
    match role {
        "user" => convert_user_blocks_to_openai(blocks, use_call_id),
        "assistant" => convert_assistant_blocks_to_openai(blocks), // Assistant blocks are the same for both APIs
        _ => Ok(vec![json!({"role": role, "content": ""})]),
    }
}

/// Convert Anthropic content blocks to OpenAI Responses API messages.
/// Uses "call_id" instead of "tool_call_id" for tool results.
fn convert_content_blocks_to_openai_responses(role: &str, blocks: &[Value]) -> Result<Vec<Value>> {
    convert_content_blocks_to_openai(role, blocks, true) // use call_id
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
        Some(Value::Array(blocks)) => convert_content_blocks_to_openai_original(role, blocks),
        _ => Ok(vec![json!({"role": role, "content": ""})]),
    }
}

/// Convert Anthropic content blocks to OpenAI messages.
fn convert_content_blocks_to_openai_original(role: &str, blocks: &[Value]) -> Result<Vec<Value>> {
    convert_content_blocks_to_openai(role, blocks, false) // use tool_call_id
}

fn convert_user_blocks_to_openai(blocks: &[Value], use_call_id: bool) -> Result<Vec<Value>> {
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
                // Use call_id for Responses API, tool_call_id for Chat Completions API
                let id_field = if use_call_id {
                    "call_id"
                } else {
                    "tool_call_id"
                };
                tool_results.push(json!({
                    "role": "tool",
                    id_field: tool_use_id,
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
                        "arguments": serde_json::to_string(&input).map_err(|e| {
                            AppError::Transform(format!("Failed to serialize tool arguments: {}", e))
                        })?,
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
        if let Some(props) = obj.get_mut("properties")
            && let Some(props_obj) = props.as_object_mut()
        {
            for (_key, prop) in props_obj.iter_mut() {
                clean_schema(prop);
            }
        }
        if let Some(items) = obj.get_mut("items") {
            clean_schema(items);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;

    use super::*;
    use crate::config::{ApiFormat, Provider};

    // ─── helpers ─────────────────────────────────────────────────────────────

    fn provider_chat(api_version: Option<&str>) -> Provider {
        Provider {
            id: "id".into(),
            base_url: "https://api.example.com".into(),
            api_key: "key".into(),
            api_format: ApiFormat::OpenAI,
            model_map: HashMap::new(),
            notes: String::new(),
            routes: Vec::new(),
            enabled: true,
            api_version: api_version.map(String::from),
        }
    }

    fn provider_responses() -> Provider {
        // Default OpenAI provider uses Responses API
        provider_chat(None)
    }

    fn provider_chat_completions() -> Provider {
        provider_chat(Some("chat_completions"))
    }

    fn _provider_anthropic() -> Provider {
        Provider {
            api_format: ApiFormat::Anthropic,
            ..provider_chat(None)
        }
    }

    // ─── clean_schema ─────────────────────────────────────────────────────────

    #[test]
    fn clean_schema_removes_uri_format() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "format": "uri" },
                "name": { "type": "string" }
            }
        });
        clean_schema(&mut schema);
        assert!(schema["properties"]["url"].get("format").is_none());
        assert_eq!(schema["properties"]["name"]["type"], "string");
    }

    #[test]
    fn clean_schema_preserves_non_uri_format() {
        let mut schema = json!({
            "type": "string",
            "format": "date-time"
        });
        clean_schema(&mut schema);
        assert_eq!(schema["format"], "date-time");
    }

    #[test]
    fn clean_schema_recursive_in_items() {
        let mut schema = json!({
            "type": "array",
            "items": { "type": "string", "format": "uri" }
        });
        clean_schema(&mut schema);
        assert!(schema["items"].get("format").is_none());
    }

    // ─── anthropic_to_openai_chat_completions_request ────────────────────────

    #[test]
    fn chat_completions_simple_user_message() {
        let req = json!({
            "model": "claude-sonnet-4-20250514",
            "messages": [{"role": "user", "content": "Hello"}],
            "max_tokens": 100
        });
        let out = anthropic_to_openai_chat_completions_request(&req, &provider_chat_completions())
            .unwrap();
        assert_eq!(out["model"], "claude-sonnet-4-20250514");
        assert_eq!(out["messages"][0]["role"], "user");
        assert_eq!(out["messages"][0]["content"], "Hello");
        assert_eq!(out["max_tokens"], 100);
    }

    #[test]
    fn chat_completions_system_prompt_becomes_first_message() {
        let req = json!({
            "model": "claude-opus-4",
            "system": "You are helpful.",
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 50
        });
        let out = anthropic_to_openai_chat_completions_request(&req, &provider_chat_completions())
            .unwrap();
        assert_eq!(out["messages"][0]["role"], "system");
        assert_eq!(out["messages"][0]["content"], "You are helpful.");
        assert_eq!(out["messages"][1]["role"], "user");
    }

    #[test]
    fn chat_completions_system_as_content_blocks() {
        let req = json!({
            "model": "m",
            "system": [{"type": "text", "text": "line1"}, {"type": "text", "text": "line2"}],
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 10
        });
        let out = anthropic_to_openai_chat_completions_request(&req, &provider_chat_completions())
            .unwrap();
        assert_eq!(out["messages"][0]["content"], "line1\nline2");
    }

    #[test]
    fn chat_completions_model_mapping_applied() {
        let mut p = provider_chat_completions();
        p.model_map.insert(
            "claude-sonnet-4-20250514".into(),
            "openrouter/claude-sonnet-4".into(),
        );
        let req = json!({
            "model": "claude-sonnet-4-20250514",
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 10
        });
        let out = anthropic_to_openai_chat_completions_request(&req, &p).unwrap();
        assert_eq!(out["model"], "openrouter/claude-sonnet-4");
    }

    #[test]
    fn chat_completions_stop_sequences_mapped_to_stop() {
        let req = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 10,
            "stop_sequences": ["END", "STOP"]
        });
        let out = anthropic_to_openai_chat_completions_request(&req, &provider_chat_completions())
            .unwrap();
        assert_eq!(out["stop"], json!(["END", "STOP"]));
    }

    #[test]
    fn chat_completions_streaming_adds_stream_options() {
        let req = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 10,
            "stream": true
        });
        let out = anthropic_to_openai_chat_completions_request(&req, &provider_chat_completions())
            .unwrap();
        assert_eq!(out["stream_options"]["include_usage"], true);
    }

    #[test]
    fn chat_completions_tool_definitions_converted() {
        let req = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 10,
            "tools": [{
                "name": "get_weather",
                "description": "Get weather",
                "input_schema": {
                    "type": "object",
                    "properties": { "city": { "type": "string" } }
                }
            }]
        });
        let out = anthropic_to_openai_chat_completions_request(&req, &provider_chat_completions())
            .unwrap();
        let tool = &out["tools"][0];
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["function"]["name"], "get_weather");
        assert_eq!(tool["function"]["description"], "Get weather");
    }

    #[test]
    fn chat_completions_tool_choice_any_maps_to_required() {
        let req = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 10,
            "tools": [{"name": "t", "description": "", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "any"}
        });
        let out = anthropic_to_openai_chat_completions_request(&req, &provider_chat_completions())
            .unwrap();
        assert_eq!(out["tool_choice"], "required");
    }

    #[test]
    fn chat_completions_tool_choice_specific_tool() {
        let req = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 10,
            "tools": [{"name": "search", "description": "", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "tool", "name": "search"}
        });
        let out = anthropic_to_openai_chat_completions_request(&req, &provider_chat_completions())
            .unwrap();
        assert_eq!(out["tool_choice"]["type"], "function");
        assert_eq!(out["tool_choice"]["function"]["name"], "search");
    }

    #[test]
    fn chat_completions_thinking_maps_to_reasoning_effort() {
        let req = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "Think hard"}],
            "max_tokens": 10,
            "thinking": {"enabled": true, "budget_tokens": 2000}
        });
        let out = anthropic_to_openai_chat_completions_request(&req, &provider_chat_completions())
            .unwrap();
        assert_eq!(out["reasoning_effort"], "high");
        assert_eq!(out["max_completion_tokens"], 2000);
    }

    #[test]
    fn chat_completions_assistant_tool_use_converted() {
        let req = json!({
            "model": "m",
            "messages": [{
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "call-1",
                    "name": "search",
                    "input": {"query": "rust"}
                }]
            }],
            "max_tokens": 10
        });
        let out = anthropic_to_openai_chat_completions_request(&req, &provider_chat_completions())
            .unwrap();
        let msg = &out["messages"][0];
        assert_eq!(msg["role"], "assistant");
        let tc = &msg["tool_calls"][0];
        assert_eq!(tc["id"], "call-1");
        assert_eq!(tc["function"]["name"], "search");
    }

    #[test]
    fn chat_completions_tool_result_uses_tool_call_id() {
        let req = json!({
            "model": "m",
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "call-1",
                    "content": "sunny"
                }]
            }],
            "max_tokens": 10
        });
        let out = anthropic_to_openai_chat_completions_request(&req, &provider_chat_completions())
            .unwrap();
        let msg = &out["messages"][0];
        assert_eq!(msg["role"], "tool");
        assert_eq!(msg["tool_call_id"], "call-1");
        assert_eq!(msg["content"], "sunny");
    }

    // ─── anthropic_to_openai_responses_request ────────────────────────────────

    #[test]
    fn responses_api_uses_input_field() {
        let req = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 10
        });
        let out = anthropic_to_openai_responses_request(&req, &provider_responses()).unwrap();
        assert!(out.get("input").is_some(), "Responses API must use 'input'");
        assert!(out.get("messages").is_none());
    }

    #[test]
    fn responses_api_tool_result_uses_call_id() {
        let req = json!({
            "model": "m",
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "call-42",
                    "content": "rainy"
                }]
            }],
            "max_tokens": 10
        });
        let out = anthropic_to_openai_responses_request(&req, &provider_responses()).unwrap();
        let msg = &out["input"][0];
        assert_eq!(msg["call_id"], "call-42");
        assert!(msg.get("tool_call_id").is_none());
    }

    // ─── anthropic_to_openai_request dispatch ────────────────────────────────

    #[test]
    fn dispatch_routes_to_responses_api_by_default() {
        let req = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 10
        });
        let out = anthropic_to_openai_request(&req, &provider_responses()).unwrap();
        assert!(out.get("input").is_some());
    }

    #[test]
    fn dispatch_routes_to_chat_completions_when_configured() {
        let req = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 10
        });
        let out = anthropic_to_openai_request(&req, &provider_chat_completions()).unwrap();
        assert!(out.get("messages").is_some());
    }
}
