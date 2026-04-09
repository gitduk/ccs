use serde_json::{Value, json};

use crate::config::Provider;
use crate::error::{AppError, Result};

// Default model names
const DEFAULT_MODEL: &str = "claude-sonnet-4-20250514";

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

/// Convert an Anthropic Messages API request to OpenAI format.
/// Automatically detects and uses the appropriate format based on provider configuration.
pub fn anthropic_to_openai_request(req: &Value, provider: &Provider) -> Result<Value> {
    let is_responses = provider.uses_responses_api();

    let model = req
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or(DEFAULT_MODEL);
    let mapped_model = provider.map_model(model);

    let mut converted_msgs: Vec<Value> = Vec::new();

    let system_text = extract_system_text(req);

    if !is_responses && !system_text.is_empty() {
        // Chat Completions: system prompt as first message
        converted_msgs.push(json!({"role": "system", "content": system_text}));
    }

    // Convert messages using the schema expected by the configured upstream API.
    if let Some(msgs) = req.get("messages").and_then(|m| m.as_array()) {
        for msg in msgs {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let converted = convert_message(role, msg, is_responses)?;
            converted_msgs.extend(converted);
        }
    }

    let is_stream = req.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);

    // Responses API uses "input"; Chat Completions uses "messages"
    let msg_field = if is_responses { "input" } else { "messages" };
    let mut result = json!({
        "model": mapped_model,
        msg_field: converted_msgs,
        "stream": is_stream,
    });

    // Responses API: system prompt goes to top-level "instructions" field
    if is_responses && !system_text.is_empty() {
        result["instructions"] = Value::String(system_text);
    }

    copy_common_parameters(req, &mut result, is_responses);

    // Tools
    if let Some(tools) = req.get("tools").and_then(|t| t.as_array()) {
        let openai_tools = convert_tools_to_openai(tools, is_responses);
        if !openai_tools.is_empty() {
            result["tools"] = json!(openai_tools);
        }
    }

    if let Some(tool_choice) = req.get("tool_choice") {
        result["tool_choice"] = convert_tool_choice_to_openai(tool_choice, is_responses);
    }

    // Thinking → reasoning
    if let Some(thinking) = req.get("thinking")
        && let Some(enabled) = thinking.get("enabled").and_then(|e| e.as_bool())
        && enabled
    {
        // Responses API sets reasoning_effort unconditionally;
        // Chat Completions only sets it when budget_tokens is present.
        if let Some(budget) = thinking.get("budget_tokens") {
            result["reasoning_effort"] = json!("high");
            if is_responses {
                if result.get("max_output_tokens").is_none() {
                    result["max_output_tokens"] = budget.clone();
                }
            } else {
                result["max_completion_tokens"] = budget.clone();
            }
        } else if is_responses {
            result["reasoning_effort"] = json!("high");
        }
    }

    // Chat Completions needs stream_options for usage reporting
    if !is_responses && is_stream {
        result["stream_options"] = json!({"include_usage": true});
    }

    Ok(result)
}

/// Copy common parameters (token limit, temperature, top_p, stop) to result.
fn copy_common_parameters(req: &Value, result: &mut Value, is_responses: bool) {
    // max_tokens -> max_output_tokens for Responses API
    if let Some(max_tokens) = req.get("max_tokens") {
        if is_responses {
            result["max_output_tokens"] = max_tokens.clone();
        } else {
            result["max_tokens"] = max_tokens.clone();
        }
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

/// Convert tool definitions to OpenAI format.
fn convert_tools_to_openai(tools: &[Value], is_responses: bool) -> Vec<Value> {
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
            if is_responses {
                close_object_schemas(&mut input_schema);
                Some(json!({
                    "type": "function",
                    "name": name,
                    "description": description,
                    "parameters": input_schema,
                    "strict": false,
                }))
            } else {
                Some(json!({
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": description,
                        "parameters": input_schema,
                    }
                }))
            }
        })
        .collect()
}

/// Convert tool choice to OpenAI format
fn convert_tool_choice_to_openai(tool_choice: &Value, is_responses: bool) -> Value {
    if let Some(tc_type) = tool_choice.get("type").and_then(|t| t.as_str()) {
        match tc_type {
            "any" => json!("required"),
            "auto" => json!("auto"),
            "none" => json!("none"),
            "tool" => {
                if let Some(name) = tool_choice.get("name").and_then(|n| n.as_str()) {
                    if is_responses {
                        json!({
                            "type": "function",
                            "name": name
                        })
                    } else {
                        json!({
                            "type": "function",
                            "function": { "name": name }
                        })
                    }
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

/// Convert a single Anthropic message to OpenAI message(s).
fn convert_message(role: &str, msg: &Value, is_responses: bool) -> Result<Vec<Value>> {
    match msg.get("content") {
        Some(Value::String(text)) => {
            if is_responses {
                let content_type = if role == "assistant" {
                    "output_text"
                } else {
                    "input_text"
                };
                Ok(vec![json!({
                    "type": "message",
                    "role": role,
                    "content": [{"type": content_type, "text": text}]
                })])
            } else {
                let openai_role = if role == "assistant" {
                    "assistant"
                } else {
                    "user"
                };
                Ok(vec![json!({"role": openai_role, "content": text})])
            }
        }
        Some(Value::Array(blocks)) => match role {
            "user" => convert_user_blocks_to_openai(blocks, is_responses),
            "assistant" => convert_assistant_blocks_to_openai(blocks, is_responses),
            _ => {
                if is_responses {
                    Ok(vec![
                        json!({"type": "message", "role": role, "content": []}),
                    ])
                } else {
                    Ok(vec![json!({"role": role, "content": ""})])
                }
            }
        },
        _ => {
            if is_responses {
                Ok(vec![
                    json!({"type": "message", "role": role, "content": []}),
                ])
            } else {
                Ok(vec![json!({"role": role, "content": ""})])
            }
        }
    }
}

fn convert_user_blocks_to_openai(blocks: &[Value], is_responses: bool) -> Result<Vec<Value>> {
    let mut messages = Vec::new();
    let mut content_parts: Vec<Value> = Vec::new();
    let mut tool_results: Vec<Value> = Vec::new();

    for block in blocks {
        let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match block_type {
            "text" => {
                let text = block.get("text").and_then(|t| t.as_str()).unwrap_or("");
                if is_responses {
                    content_parts.push(json!({"type": "input_text", "text": text}));
                } else {
                    content_parts.push(json!({"type": "text", "text": text}));
                }
            }
            "image" => {
                if let Some(source) = block.get("source") {
                    let media_type = source
                        .get("media_type")
                        .and_then(|m| m.as_str())
                        .unwrap_or("image/png");
                    let data = source.get("data").and_then(|d| d.as_str()).unwrap_or("");
                    let data_url = format!("data:{media_type};base64,{data}");
                    if is_responses {
                        content_parts.push(json!({
                            "type": "input_image",
                            "image_url": data_url
                        }));
                    } else {
                        content_parts.push(json!({
                            "type": "image_url",
                            "image_url": {
                                "url": data_url
                            }
                        }));
                    }
                }
            }
            "tool_result" => {
                let tool_use_id = block
                    .get("tool_use_id")
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                let content_text = extract_tool_result_content(block);
                if is_responses {
                    tool_results.push(json!({
                        "type": "function_call_output",
                        "call_id": tool_use_id,
                        "output": content_text,
                    }));
                } else {
                    tool_results.push(json!({
                        "role": "tool",
                        "tool_call_id": tool_use_id,
                        "content": content_text,
                    }));
                }
            }
            _ => {}
        }
    }

    // Emit user message with content parts if any
    if !content_parts.is_empty() {
        if !is_responses
            && content_parts.len() == 1
            && content_parts[0].get("type").and_then(|t| t.as_str()) == Some("text")
        {
            // Single text → plain string content
            messages.push(json!({
                "role": "user",
                "content": content_parts[0].get("text").and_then(|t| t.as_str()).unwrap_or("")
            }));
        } else if is_responses {
            messages.push(json!({"type": "message", "role": "user", "content": content_parts}));
        } else {
            messages.push(json!({"role": "user", "content": content_parts}));
        }
    }

    // Emit tool results
    messages.extend(tool_results);

    if messages.is_empty() {
        if is_responses {
            messages.push(json!({"type": "message", "role": "user", "content": []}));
        } else {
            messages.push(json!({"role": "user", "content": ""}));
        }
    }

    Ok(messages)
}

fn convert_assistant_blocks_to_openai(blocks: &[Value], is_responses: bool) -> Result<Vec<Value>> {
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
                let arguments = serde_json::to_string(&input).map_err(|e| {
                    AppError::Transform(format!("Failed to serialize tool arguments: {}", e))
                })?;
                if is_responses {
                    tool_calls.push(json!({
                        "type": "function_call",
                        "call_id": id,
                        "name": name,
                        "arguments": arguments,
                    }));
                } else {
                    tool_calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": arguments,
                        }
                    }));
                }
            }
            "thinking" => {
                let thinking_text = block.get("thinking").and_then(|t| t.as_str()).unwrap_or("");
                reasoning_content.push_str(thinking_text);
            }
            _ => {}
        }
    }

    if is_responses {
        let mut items = Vec::new();
        if !text_content.is_empty() {
            items.push(json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text_content}]
            }));
        }
        items.extend(tool_calls);
        if !reasoning_content.is_empty() {
            items.push(json!({
                "type": "reasoning",
                "summary": [{"type": "summary_text", "text": reasoning_content}]
            }));
        }
        if items.is_empty() {
            items.push(json!({"type": "message", "role": "assistant", "content": []}));
        }
        Ok(items)
    } else {
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

/// Recursively add `"additionalProperties": false` to object schemas when absent.
/// Some OpenAI-compatible providers require explicit closed object schemas for tools.
pub fn close_object_schemas(schema: &mut Value) {
    if let Some(obj) = schema.as_object_mut() {
        if obj.get("type").and_then(|t| t.as_str()) == Some("object")
            && !obj.contains_key("additionalProperties")
        {
            obj.insert("additionalProperties".to_string(), json!(false));
        }
        if let Some(props) = obj.get_mut("properties")
            && let Some(props_obj) = props.as_object_mut()
        {
            for (_key, prop) in props_obj.iter_mut() {
                close_object_schemas(prop);
            }
        }
        if let Some(items) = obj.get_mut("items") {
            close_object_schemas(items);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;

    use super::*;
    use crate::config::{ApiFormat, OpenAiApiVersion, Provider};

    // ─── helpers ─────────────────────────────────────────────────────────────

    fn provider_chat(api_version: Option<OpenAiApiVersion>) -> Provider {
        Provider {
            id: "id".into(),
            base_url: "https://api.example.com".into(),
            api_key: "key".into(),
            api_format: ApiFormat::OpenAI,
            model_map: HashMap::new(),
            notes: String::new(),
            routes: Vec::new(),
            enabled: true,
            api_version,
            quota: None,
            quota_command: None,
        }
    }

    fn provider_responses() -> Provider {
        // Default OpenAI provider uses Responses API
        provider_chat(None)
    }

    fn provider_chat_completions() -> Provider {
        provider_chat(Some(OpenAiApiVersion::ChatCompletions))
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

    #[test]
    fn close_object_schemas_adds_additional_properties_false_recursively() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "city": { "type": "string" },
                "meta": {
                    "type": "object",
                    "properties": {
                        "country": { "type": "string" }
                    }
                }
            }
        });
        close_object_schemas(&mut schema);
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["meta"]["additionalProperties"], false);
    }

    // ─── anthropic_to_openai_chat_completions_request ────────────────────────

    #[test]
    fn chat_completions_simple_user_message() {
        let req = json!({
            "model": "claude-sonnet-4-20250514",
            "messages": [{"role": "user", "content": "Hello"}],
            "max_tokens": 100
        });
        let out = anthropic_to_openai_request(&req, &provider_chat_completions()).unwrap();
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
        let out = anthropic_to_openai_request(&req, &provider_chat_completions()).unwrap();
        assert_eq!(out["messages"][0]["role"], "system");
        assert_eq!(out["messages"][0]["content"], "You are helpful.");
        assert_eq!(out["messages"][1]["role"], "user");
    }

    #[test]
    fn responses_system_prompt_becomes_instructions_field() {
        let req = json!({
            "model": "claude-opus-4",
            "system": "You are helpful.",
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 50
        });
        let out = anthropic_to_openai_request(&req, &provider_responses()).unwrap();
        assert_eq!(out["instructions"], "You are helpful.");
        // system prompt must NOT appear in the input array
        let input = out["input"].as_array().unwrap();
        assert!(
            input
                .iter()
                .all(|m| m.get("role").and_then(|r| r.as_str()) != Some("system")),
            "Responses API input must not contain role=system"
        );
    }

    #[test]
    fn responses_system_content_blocks_become_instructions() {
        let req = json!({
            "model": "m",
            "system": [{"type": "text", "text": "line1"}, {"type": "text", "text": "line2"}],
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 10
        });
        let out = anthropic_to_openai_request(&req, &provider_responses()).unwrap();
        assert_eq!(out["instructions"], "line1\nline2");
    }

    #[test]
    fn chat_completions_system_as_content_blocks() {
        let req = json!({
            "model": "m",
            "system": [{"type": "text", "text": "line1"}, {"type": "text", "text": "line2"}],
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 10
        });
        let out = anthropic_to_openai_request(&req, &provider_chat_completions()).unwrap();
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
        let out = anthropic_to_openai_request(&req, &p).unwrap();
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
        let out = anthropic_to_openai_request(&req, &provider_chat_completions()).unwrap();
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
        let out = anthropic_to_openai_request(&req, &provider_chat_completions()).unwrap();
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
        let out = anthropic_to_openai_request(&req, &provider_chat_completions()).unwrap();
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
        let out = anthropic_to_openai_request(&req, &provider_chat_completions()).unwrap();
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
        let out = anthropic_to_openai_request(&req, &provider_chat_completions()).unwrap();
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
        let out = anthropic_to_openai_request(&req, &provider_chat_completions()).unwrap();
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
        let out = anthropic_to_openai_request(&req, &provider_chat_completions()).unwrap();
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
        let out = anthropic_to_openai_request(&req, &provider_chat_completions()).unwrap();
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
        let out = anthropic_to_openai_request(&req, &provider_responses()).unwrap();
        assert!(out.get("input").is_some(), "Responses API must use 'input'");
        assert!(out.get("messages").is_none());
        assert_eq!(out["max_output_tokens"], 10);
        assert!(out.get("max_tokens").is_none());
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
        let out = anthropic_to_openai_request(&req, &provider_responses()).unwrap();
        let msg = &out["input"][0];
        assert_eq!(msg["type"], "function_call_output");
        assert_eq!(msg["call_id"], "call-42");
        assert_eq!(msg["output"], "rainy");
        assert!(msg.get("tool_call_id").is_none());
    }

    #[test]
    fn responses_api_tool_definitions_use_flat_function_schema() {
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
        let out = anthropic_to_openai_request(&req, &provider_responses()).unwrap();
        let tool = &out["tools"][0];
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["name"], "get_weather");
        assert_eq!(tool["description"], "Get weather");
        assert!(tool.get("function").is_none());
        assert_eq!(tool["parameters"]["type"], "object");
        assert_eq!(tool["parameters"]["additionalProperties"], false);
        assert_eq!(tool["strict"], false);
    }

    #[test]
    fn responses_api_tool_choice_specific_tool_uses_flat_schema() {
        let req = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 10,
            "tools": [{"name": "search", "description": "", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "tool", "name": "search"}
        });
        let out = anthropic_to_openai_request(&req, &provider_responses()).unwrap();
        assert_eq!(out["tool_choice"]["type"], "function");
        assert_eq!(out["tool_choice"]["name"], "search");
        assert!(out["tool_choice"].get("function").is_none());
    }

    #[test]
    fn responses_api_thinking_budget_prefers_max_output_tokens() {
        let req = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "Think hard"}],
            "thinking": {"enabled": true, "budget_tokens": 77}
        });
        let out = anthropic_to_openai_request(&req, &provider_responses()).unwrap();
        assert_eq!(out["reasoning_effort"], "high");
        assert_eq!(out["max_output_tokens"], 77);
        assert!(out.get("max_completion_tokens").is_none());
    }

    #[test]
    fn responses_api_user_text_uses_input_text_blocks() {
        let req = json!({
            "model": "m",
            "messages": [{
                "role": "user",
                "content": [{"type": "text", "text": "Hi"}]
            }],
            "max_tokens": 10
        });
        let out = anthropic_to_openai_request(&req, &provider_responses()).unwrap();
        assert_eq!(out["input"][0]["type"], "message");
        assert_eq!(out["input"][0]["role"], "user");
        assert_eq!(out["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(out["input"][0]["content"][0]["text"], "Hi");
    }

    #[test]
    fn responses_api_assistant_text_uses_output_text_blocks() {
        let req = json!({
            "model": "m",
            "messages": [{
                "role": "assistant",
                "content": [{"type": "text", "text": "Hello"}]
            }],
            "max_tokens": 10
        });
        let out = anthropic_to_openai_request(&req, &provider_responses()).unwrap();
        assert_eq!(out["input"][0]["type"], "message");
        assert_eq!(out["input"][0]["role"], "assistant");
        assert_eq!(out["input"][0]["content"][0]["type"], "output_text");
        assert_eq!(out["input"][0]["content"][0]["text"], "Hello");
    }

    #[test]
    fn responses_api_assistant_tool_use_becomes_function_call() {
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
        let out = anthropic_to_openai_request(&req, &provider_responses()).unwrap();
        assert_eq!(out["input"][0]["type"], "function_call");
        assert_eq!(out["input"][0]["call_id"], "call-1");
        assert_eq!(out["input"][0]["name"], "search");
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
