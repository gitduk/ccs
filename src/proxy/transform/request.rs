use serde_json::{Value, json};

use crate::config::{OpenAiApiVersion, Provider};
use crate::error::Result;

// Default model names
const DEFAULT_MODEL: &str = "claude-sonnet-4-20250514";

fn current_model(req: &Value) -> &str {
    req.get("model")
        .and_then(|m| m.as_str())
        .unwrap_or(DEFAULT_MODEL)
}

fn get_mapped_model(req: &Value, provider: &Provider) -> String {
    provider.resolve_model(current_model(req)).0
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

/// Convert an Anthropic Messages API request to OpenAI format.
/// Automatically detects and uses the appropriate format based on provider configuration.
pub fn anthropic_to_openai_request(req: &Value, provider: &Provider) -> Result<Value> {
    to_openai(req, provider, provider.openai_api_version_enum())
}

/// Apply model mapping to an Anthropic-format request body.
/// Returns `None` when no mapping applies, so callers can reuse the original
/// request bytes instead of re-serializing an identical `Value`.
pub fn map_anthropic_model(req: &Value, provider: &Provider) -> Option<Value> {
    let original = current_model(req);
    let (mapped, _) = provider.resolve_model(original);
    if mapped == original {
        return None;
    }
    let mut out = req.clone();
    out["model"] = json!(mapped);
    Some(out)
}

/// Convert an Anthropic Messages API request to OpenAI format using an explicit
/// OpenAI API version for this attempt.
pub fn to_openai(req: &Value, provider: &Provider, api_version: OpenAiApiVersion) -> Result<Value> {
    let is_responses = matches!(api_version, OpenAiApiVersion::Responses);

    let mapped_model = get_mapped_model(req, provider);

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

    if !is_responses && provider.inject_thinking_history {
        inject_missing_reasoning_content(&mut converted_msgs);
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
        let tc = convert_tool_choice_to_openai(tool_choice, is_responses);
        // Skip "auto" — it's the default for OpenAI-compatible APIs.
        // Sending it explicitly triggers a vllm guard that requires
        // --enable-auto-tool-choice and --tool-call-parser to be set.
        if tc != json!("auto") {
            result["tool_choice"] = tc;
        }
    }

    // Thinking → reasoning. The canonical form is Anthropic-native
    // ({"type": "enabled"|"adaptive"}); ingress normalizes to it.
    if let Some(thinking) = req.get("thinking")
        && matches!(
            thinking.get("type").and_then(|t| t.as_str()),
            Some("enabled") | Some("adaptive")
        )
    {
        // Responses API uses reasoning.effort nested object;
        // Chat Completions uses top-level reasoning_effort.
        if let Some(budget) = thinking.get("budget_tokens") {
            if is_responses {
                result["reasoning"] = json!({"effort": "high"});
                if result.get("max_output_tokens").is_none() {
                    result["max_output_tokens"] = budget.clone();
                }
            } else {
                result["reasoning_effort"] = json!("high");
                result["max_completion_tokens"] = budget.clone();
            }
        } else if is_responses {
            result["reasoning"] = json!({"effort": "high"});
        } else {
            // No budget (e.g. {"type": "adaptive"}): still signal reasoning
            // on the Chat Completions path instead of silently dropping it.
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
                let (id, name, arguments) = super::tool_use_parts(block)?;
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
        // Reasoning must precede the function_call it led to: the upstream
        // pairs each function_call with the following function_call_output,
        // and an interleaved reasoning item breaks that pairing.
        if !reasoning_content.is_empty() {
            items.push(json!({
                "type": "reasoning",
                "summary": [{"type": "summary_text", "text": reasoning_content}]
            }));
        }
        items.extend(tool_calls);
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

/// Convert an OpenAI-shaped request (Chat Completions or Responses API) into
/// the Anthropic Messages request shape used internally by the proxy.
///
/// This is the inverse of [`to_openai`], applied at the client boundary when
/// the caller speaks OpenAI but we need to dispatch through the Anthropic
/// internal pipeline.
pub fn openai_to_anthropic_request(req: &Value, api_version: OpenAiApiVersion) -> Result<Value> {
    match api_version {
        OpenAiApiVersion::ChatCompletions => openai_chat_to_anthropic_request(req),
        OpenAiApiVersion::Responses => openai_responses_to_anthropic_request(req),
    }
}

fn openai_chat_to_anthropic_request(req: &Value) -> Result<Value> {
    let mut system_text = String::new();
    let mut anthropic_messages: Vec<Value> = Vec::new();

    if let Some(msgs) = req.get("messages").and_then(|m| m.as_array()) {
        for msg in msgs {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            match role {
                "system" => {
                    let text = extract_openai_message_text(msg);
                    if !system_text.is_empty() && !text.is_empty() {
                        system_text.push('\n');
                    }
                    system_text.push_str(&text);
                }
                "tool" => {
                    let tool_call_id = msg
                        .get("tool_call_id")
                        .and_then(|t| t.as_str())
                        .unwrap_or("");
                    let content = extract_openai_message_text(msg);
                    let tool_result = json!({
                        "type": "tool_result",
                        "tool_use_id": tool_call_id,
                        "content": content,
                    });
                    merge_into_user_message(&mut anthropic_messages, tool_result);
                }
                "user" => {
                    let blocks = openai_user_content_to_anthropic_blocks(msg)?;
                    anthropic_messages.push(json!({"role": "user", "content": blocks}));
                }
                "assistant" => {
                    let blocks = openai_assistant_content_to_anthropic_blocks(msg)?;
                    anthropic_messages.push(json!({"role": "assistant", "content": blocks}));
                }
                _ => {}
            }
        }
    }

    let mut result = json!({
        "model": current_model(req),
        "messages": anthropic_messages,
    });

    if !system_text.is_empty() {
        result["system"] = Value::String(system_text);
    }

    copy_openai_common_params(req, &mut result, /* is_responses = */ false);
    copy_openai_tools(req, &mut result, /* is_responses = */ false);
    copy_openai_tool_choice(req, &mut result, /* is_responses = */ false);
    copy_openai_reasoning_to_thinking(req, &mut result);

    Ok(result)
}

fn openai_responses_to_anthropic_request(req: &Value) -> Result<Value> {
    let mut system_text = req
        .get("instructions")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_default();

    let mut anthropic_messages: Vec<Value> = Vec::new();

    // Track reasoning items: they come as separate `type: "reasoning"` items
    // and should be attached to the next assistant message.
    let mut pending_thinking: Option<String> = None;

    if let Some(items) = req.get("input").and_then(|v| v.as_array()) {
        for item in items {
            let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match item_type {
                "message" => {
                    let role = item.get("role").and_then(|r| r.as_str()).unwrap_or("user");
                    match role {
                        "system" => {
                            let text = extract_responses_message_text(item);
                            if !system_text.is_empty() && !text.is_empty() {
                                system_text.push('\n');
                            }
                            system_text.push_str(&text);
                        }
                        "user" => {
                            let blocks = responses_user_content_to_anthropic_blocks(item)?;
                            anthropic_messages.push(json!({"role": "user", "content": blocks}));
                        }
                        "assistant" => {
                            let mut blocks = responses_assistant_content_to_anthropic_blocks(item)?;
                            if let Some(thinking) = pending_thinking.take() {
                                blocks.insert(0, json!({"type": "thinking", "thinking": thinking}));
                            }
                            anthropic_messages
                                .push(json!({"role": "assistant", "content": blocks}));
                        }
                        _ => {}
                    }
                }
                "function_call" => {
                    let call_id = item
                        .get("call_id")
                        .and_then(|v| v.as_str())
                        .or_else(|| item.get("id").and_then(|v| v.as_str()))
                        .unwrap_or("");
                    let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let arguments = item
                        .get("arguments")
                        .and_then(|a| a.as_str())
                        .unwrap_or("{}");
                    let input: Value = serde_json::from_str(arguments).unwrap_or(json!({}));

                    let block = json!({
                        "type": "tool_use",
                        "id": call_id,
                        "name": name,
                        "input": input,
                    });
                    let blocks = match pending_thinking.take() {
                        // The tool call may start a fresh assistant message if
                        // the previous one was a different role. Fold any
                        // pending thinking into that new assistant message.
                        Some(thinking) => {
                            vec![json!({"type": "thinking", "thinking": thinking}), block]
                        }
                        None => vec![block],
                    };
                    append_to_assistant_message(&mut anthropic_messages, blocks);
                }
                "function_call_output" => {
                    let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
                    let output = item
                        .get("output")
                        .and_then(|v| v.as_str())
                        .map(String::from)
                        .unwrap_or_default();
                    let tool_result = json!({
                        "type": "tool_result",
                        "tool_use_id": call_id,
                        "content": output,
                    });
                    merge_into_user_message(&mut anthropic_messages, tool_result);
                }
                "reasoning" => {
                    // Collect summary text; attach to the next assistant item.
                    if let Some(summary) = item.get("summary").and_then(|s| s.as_array()) {
                        let text: String = summary
                            .iter()
                            .filter_map(|s| s.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join("\n");
                        if !text.is_empty() {
                            pending_thinking = Some(text);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    let mut result = json!({
        "model": current_model(req),
        "messages": anthropic_messages,
    });

    if !system_text.is_empty() {
        result["system"] = Value::String(system_text);
    }

    copy_openai_common_params(req, &mut result, /* is_responses = */ true);
    copy_openai_tools(req, &mut result, /* is_responses = */ true);
    copy_openai_tool_choice(req, &mut result, /* is_responses = */ true);
    copy_openai_reasoning_to_thinking(req, &mut result);

    Ok(result)
}

fn copy_openai_common_params(req: &Value, out: &mut Value, is_responses: bool) {
    // Token limit: max_output_tokens (Responses) / max_completion_tokens (Chat, reasoning)
    // / max_tokens (Chat legacy)
    let max_tokens = if is_responses {
        req.get("max_output_tokens").cloned()
    } else {
        req.get("max_completion_tokens")
            .or_else(|| req.get("max_tokens"))
            .cloned()
    };
    if let Some(mt) = max_tokens {
        out["max_tokens"] = mt;
    }

    if let Some(temp) = req.get("temperature") {
        out["temperature"] = temp.clone();
    }
    if let Some(top_p) = req.get("top_p") {
        out["top_p"] = top_p.clone();
    }
    if let Some(stop) = req.get("stop") {
        let arr = match stop {
            Value::Array(a) => a.clone(),
            Value::String(s) => vec![Value::String(s.clone())],
            _ => Vec::new(),
        };
        if !arr.is_empty() {
            out["stop_sequences"] = Value::Array(arr);
        }
    }
    if let Some(stream) = req.get("stream") {
        out["stream"] = stream.clone();
    }
}

fn copy_openai_tools(req: &Value, out: &mut Value, is_responses: bool) {
    let Some(tools) = req.get("tools").and_then(|t| t.as_array()) else {
        return;
    };
    let converted: Vec<Value> = tools
        .iter()
        .filter_map(|tool| {
            if is_responses {
                // Flat: {type, name, description, parameters, strict}
                let name = tool.get("name")?.as_str()?;
                let description = tool
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("");
                let parameters = tool
                    .get("parameters")
                    .cloned()
                    .unwrap_or(json!({"type": "object"}));
                Some(json!({
                    "name": name,
                    "description": description,
                    "input_schema": parameters,
                }))
            } else {
                let func = tool.get("function")?;
                let name = func.get("name")?.as_str()?;
                let description = func
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("");
                let parameters = func
                    .get("parameters")
                    .cloned()
                    .unwrap_or(json!({"type": "object"}));
                Some(json!({
                    "name": name,
                    "description": description,
                    "input_schema": parameters,
                }))
            }
        })
        .collect();
    if !converted.is_empty() {
        out["tools"] = json!(converted);
    }
}

fn copy_openai_tool_choice(req: &Value, out: &mut Value, is_responses: bool) {
    let Some(tc) = req.get("tool_choice") else {
        return;
    };
    let anthropic_tc = match tc {
        Value::String(s) => match s.as_str() {
            "auto" => json!({"type": "auto"}),
            "required" => json!({"type": "any"}),
            "none" => json!({"type": "none"}),
            _ => json!({"type": "auto"}),
        },
        Value::Object(_) => {
            let name = if is_responses {
                tc.get("name").and_then(|n| n.as_str())
            } else {
                tc.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
            };
            if let Some(n) = name {
                json!({"type": "tool", "name": n})
            } else {
                json!({"type": "auto"})
            }
        }
        _ => json!({"type": "auto"}),
    };
    out["tool_choice"] = anthropic_tc;
}

fn copy_openai_reasoning_to_thinking(req: &Value, out: &mut Value) {
    // reasoning_effort (Chat) or reasoning.effort (Responses) — treat any
    // explicit effort as "thinking enabled". Emit the Anthropic-native
    // encoding so every downstream consumer sees one canonical format.
    let effort = req
        .get("reasoning_effort")
        .or_else(|| req.get("reasoning").and_then(|r| r.get("effort")));
    if let Some(effort) = effort {
        let budget = match effort.as_str() {
            Some("low") | Some("minimal") => 2048,
            Some("high") => 16384,
            _ => 8192, // "medium" and unknown values
        };
        out["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
    }
}

/// Collapse OpenAI chat message content (string | parts array) to plain text.
fn extract_openai_message_text(msg: &Value) -> String {
    match msg.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| {
                let t = p.get("type").and_then(|t| t.as_str()).unwrap_or("");
                if matches!(t, "text" | "input_text" | "output_text") {
                    p.get("text").and_then(|v| v.as_str()).map(String::from)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn extract_responses_message_text(item: &Value) -> String {
    item.get("content")
        .and_then(|c| c.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| {
                    let t = p.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    if matches!(t, "input_text" | "output_text" | "text") {
                        p.get("text").and_then(|v| v.as_str()).map(String::from)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

fn openai_user_content_to_anthropic_blocks(msg: &Value) -> Result<Vec<Value>> {
    let mut blocks = Vec::new();
    match msg.get("content") {
        Some(Value::String(s)) => {
            blocks.push(json!({"type": "text", "text": s}));
        }
        Some(Value::Array(parts)) => {
            for part in parts {
                let pt = part.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match pt {
                    "text" | "input_text" => {
                        let text = part.get("text").and_then(|t| t.as_str()).unwrap_or("");
                        blocks.push(json!({"type": "text", "text": text}));
                    }
                    "image_url" | "input_image" => {
                        if let Some(block) = image_part_to_anthropic(part) {
                            blocks.push(block);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    if blocks.is_empty() {
        blocks.push(json!({"type": "text", "text": ""}));
    }
    Ok(blocks)
}

fn responses_user_content_to_anthropic_blocks(item: &Value) -> Result<Vec<Value>> {
    let mut blocks = Vec::new();
    if let Some(parts) = item.get("content").and_then(|c| c.as_array()) {
        for part in parts {
            let pt = part.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match pt {
                "input_text" | "text" => {
                    let text = part.get("text").and_then(|t| t.as_str()).unwrap_or("");
                    blocks.push(json!({"type": "text", "text": text}));
                }
                "input_image" => {
                    if let Some(block) = image_part_to_anthropic(part) {
                        blocks.push(block);
                    }
                }
                _ => {}
            }
        }
    }
    if blocks.is_empty() {
        blocks.push(json!({"type": "text", "text": ""}));
    }
    Ok(blocks)
}

fn openai_assistant_content_to_anthropic_blocks(msg: &Value) -> Result<Vec<Value>> {
    let mut blocks = Vec::new();

    // Reasoning (thinking) comes first so it appears before text to the model.
    if let Some(reasoning) = msg.get("reasoning_content").and_then(|r| r.as_str())
        && !reasoning.is_empty()
    {
        blocks.push(json!({"type": "thinking", "thinking": reasoning}));
    }

    match msg.get("content") {
        Some(Value::String(s)) if !s.is_empty() => {
            blocks.push(json!({"type": "text", "text": s}));
        }
        Some(Value::Array(parts)) => {
            for part in parts {
                let pt = part.get("type").and_then(|t| t.as_str()).unwrap_or("");
                if matches!(pt, "text" | "output_text") {
                    let text = part.get("text").and_then(|t| t.as_str()).unwrap_or("");
                    if !text.is_empty() {
                        blocks.push(json!({"type": "text", "text": text}));
                    }
                }
            }
        }
        _ => {}
    }

    if let Some(tool_calls) = msg.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in tool_calls {
            let id = tc.get("id").and_then(|i| i.as_str()).unwrap_or("");
            let func = tc.get("function");
            let name = func
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("");
            let args_str = func
                .and_then(|f| f.get("arguments"))
                .and_then(|a| a.as_str())
                .unwrap_or("{}");
            let input: Value = serde_json::from_str(args_str).unwrap_or(json!({}));
            blocks.push(json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input,
            }));
        }
    }

    if blocks.is_empty() {
        blocks.push(json!({"type": "text", "text": ""}));
    }
    Ok(blocks)
}

fn responses_assistant_content_to_anthropic_blocks(item: &Value) -> Result<Vec<Value>> {
    let mut blocks = Vec::new();
    if let Some(parts) = item.get("content").and_then(|c| c.as_array()) {
        for part in parts {
            let pt = part.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if matches!(pt, "output_text" | "text") {
                let text = part.get("text").and_then(|t| t.as_str()).unwrap_or("");
                if !text.is_empty() {
                    blocks.push(json!({"type": "text", "text": text}));
                }
            }
        }
    }
    if blocks.is_empty() {
        blocks.push(json!({"type": "text", "text": ""}));
    }
    Ok(blocks)
}

/// Convert an OpenAI image_url / input_image part to an Anthropic image block.
/// Supports data URLs (base64) and public URLs (passed through as url source).
fn image_part_to_anthropic(part: &Value) -> Option<Value> {
    let url = part
        .get("image_url")
        .and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            Value::Object(o) => o.get("url").and_then(|u| u.as_str()).map(String::from),
            _ => None,
        })
        .or_else(|| part.get("url").and_then(|u| u.as_str()).map(String::from))?;

    if let Some(rest) = url.strip_prefix("data:") {
        // data:<mime>;base64,<data>
        if let Some((header, data)) = rest.split_once(',') {
            let media_type = header.split(';').next().unwrap_or("image/png").to_string();
            return Some(json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": media_type,
                    "data": data,
                }
            }));
        }
    }
    Some(json!({
        "type": "image",
        "source": {"type": "url", "url": url}
    }))
}

/// Append a tool_result block to the last user message, or start a new one.
/// OpenAI models tool responses as separate top-level messages; Anthropic
/// expects them as blocks within a user message.
fn merge_into_user_message(messages: &mut Vec<Value>, block: Value) {
    if let Some(last) = messages.last_mut()
        && last.get("role").and_then(|r| r.as_str()) == Some("user")
        && let Some(content) = last.get_mut("content")
        && let Some(arr) = content.as_array_mut()
    {
        arr.push(block);
        return;
    }
    messages.push(json!({"role": "user", "content": [block]}));
}

/// Responses API emits tool_use as a sibling item. If the previous Anthropic
/// message is already an assistant turn, append to it; otherwise start a new
/// assistant message.
fn append_to_assistant_message(messages: &mut Vec<Value>, blocks: Vec<Value>) {
    if let Some(last) = messages.last_mut()
        && last.get("role").and_then(|r| r.as_str()) == Some("assistant")
        && let Some(content) = last.get_mut("content")
        && let Some(arr) = content.as_array_mut()
    {
        arr.extend(blocks);
        return;
    }
    messages.push(json!({"role": "assistant", "content": blocks}));
}

fn message_has_thinking_block(msg: &Value) -> bool {
    msg.get("content")
        .and_then(|c| c.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("thinking"))
        })
        .unwrap_or(false)
}

fn is_signed_thinking_block(block: &Value) -> bool {
    block.get("type").and_then(|t| t.as_str()) == Some("thinking")
        && block.get("signature").is_some()
}

fn message_signed_thinking_count(msg: &Value) -> usize {
    msg.get("content")
        .and_then(|c| c.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| is_signed_thinking_block(b))
                .count()
        })
        .unwrap_or(0)
}

/// Drop `signature` from all but the first signed thinking block of an
/// assistant message. DeepSeek-compatible providers reject messages with
/// "multiple signatures detected in assistant message content".
fn strip_duplicate_signatures(msg: &mut Value) {
    let Some(blocks) = msg.get_mut("content").and_then(|c| c.as_array_mut()) else {
        return;
    };
    let mut seen_signed = false;
    for block in blocks.iter_mut() {
        if !is_signed_thinking_block(block) {
            continue;
        }
        if seen_signed && let Some(obj) = block.as_object_mut() {
            obj.remove("signature");
        }
        seen_signed = true;
    }
}

/// When `thinking` is enabled, DeepSeek-compatible providers impose two
/// constraints on assistant history that the genuine Anthropic API does not:
/// every assistant turn must carry a thinking block, and at most one thinking
/// block per turn may carry a `signature` ("multiple signatures detected in
/// assistant message content"). Interleaved thinking from a genuine Claude
/// session violates the latter. This function injects an empty thinking block
/// into turns missing one and drops `signature` from all but the first signed
/// block of each turn. Returns `None` if no patching is needed.
///
/// The trigger is not only an explicit `thinking` param: DeepSeek's Anthropic
/// endpoint enforces the same consistency from the history alone — once any
/// assistant turn carries a thinking block, every earlier turn must too. When
/// `strict_history` is set (a provider that enforces this), a mixed history is
/// patched even without an explicit `thinking` param; tolerant providers keep
/// the history-trigger off by passing `false`.
pub fn patch_thinking_history(req: &Value, strict_history: bool) -> Option<Value> {
    // Anthropic-native encoding only — ingress normalizes the OpenAI
    // reasoning_effort form to {"type": "enabled", ...} (see
    // copy_openai_reasoning_to_thinking).
    let thinking_enabled = matches!(
        req.get("thinking")
            .and_then(|t| t.get("type"))
            .and_then(|v| v.as_str()),
        Some("enabled") | Some("adaptive")
    );

    let messages = req.get("messages")?.as_array()?;
    if !thinking_enabled
        && (!strict_history
            || !messages.iter().any(|m| {
                m.get("role").and_then(|r| r.as_str()) == Some("assistant")
                    && message_has_thinking_block(m)
            }))
    {
        return None;
    }

    // The last message may be an assistant prefill for the current turn — the
    // prefill exemption only covers injecting missing thinking blocks;
    // signature dedup applies to every assistant turn.
    let last_idx = messages.len().saturating_sub(1);
    let needs_patch = messages.iter().enumerate().any(|(i, msg)| {
        msg.get("role").and_then(|r| r.as_str()) == Some("assistant")
            && ((i < last_idx && !message_has_thinking_block(msg))
                || message_signed_thinking_count(msg) > 1)
    });

    if !needs_patch {
        return None;
    }

    let mut patched = req.clone();
    let msgs = patched["messages"].as_array_mut()?;
    let msgs_len = msgs.len();
    for (i, msg) in msgs.iter_mut().enumerate() {
        if msg.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }

        strip_duplicate_signatures(msg);

        if i + 1 == msgs_len {
            continue; // skip trailing turn (may be an assistant prefill)
        }
        if message_has_thinking_block(msg) {
            continue;
        }
        // Build new content with thinking block first to avoid O(n) shift.
        let mut new_content = vec![json!({"type": "thinking", "thinking": ""})];
        match msg["content"].take() {
            Value::String(s) => new_content.push(json!({"type": "text", "text": s})),
            Value::Array(arr) => new_content.extend(arr),
            Value::Null => {}
            other => new_content.push(other),
        }
        msg["content"] = json!(new_content);
    }

    Some(patched)
}

/// DeepSeek-family upstreams reject assistant `tool_calls` carrying no
/// `reasoning_content`; an empty string satisfies the check.
fn inject_missing_reasoning_content(msgs: &mut [Value]) {
    for msg in msgs {
        if msg.get("role").and_then(|r| r.as_str()) != Some("assistant")
            || msg.get("reasoning_content").is_some()
        {
            continue;
        }
        let has_tool_calls = msg
            .get("tool_calls")
            .and_then(|t| t.as_array())
            .is_some_and(|calls| !calls.is_empty());
        if has_tool_calls {
            msg["reasoning_content"] = json!("");
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
            routes: Vec::new(),
            enabled: true,
            fallback: true,
            api_version,
            inject_thinking_history: true,
            strict_thinking_history: false,
            quota_command: None,
            port: None,
            test_model: None,
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
    fn explicit_api_version_can_override_provider_default() {
        let req = json!({
            "model": "m",
            "system": "You are helpful.",
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 10,
            "stream": true
        });
        let out = to_openai(
            &req,
            &provider_responses(),
            OpenAiApiVersion::ChatCompletions,
        )
        .unwrap();
        assert!(out.get("messages").is_some());
        assert!(out.get("input").is_none());
        assert_eq!(out["messages"][0]["role"], "system");
        assert_eq!(out["stream_options"]["include_usage"], true);
    }

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
    fn chat_completions_tool_choice_auto_is_omitted() {
        let req = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 10,
            "tools": [{"name": "t", "description": "", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "auto"}
        });
        let out = anthropic_to_openai_request(&req, &provider_chat_completions()).unwrap();
        // "auto" must be omitted so vllm doesn't reject the request when
        // --enable-auto-tool-choice / --tool-call-parser are not set.
        assert!(out.get("tool_choice").is_none());
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
            "thinking": {"type": "enabled", "budget_tokens": 2000}
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
    fn chat_completions_adaptive_thinking_without_budget_sets_effort() {
        let req = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "Think"}],
            "max_tokens": 10,
            "thinking": {"type": "adaptive"}
        });
        let out = anthropic_to_openai_request(&req, &provider_chat_completions()).unwrap();
        assert_eq!(out["reasoning_effort"], "high");
        assert!(out.get("max_completion_tokens").is_none());
    }

    #[test]
    fn responses_api_thinking_budget_prefers_max_output_tokens() {
        let req = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "Think hard"}],
            "thinking": {"type": "enabled", "budget_tokens": 77}
        });
        let out = anthropic_to_openai_request(&req, &provider_responses()).unwrap();
        assert_eq!(out["reasoning"]["effort"], "high");
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

    // ─── openai_to_anthropic_request: ChatCompletions ────────────────────────

    #[test]
    fn openai_chat_to_anthropic_simple_user_message() {
        let req = json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 50
        });
        let out = openai_to_anthropic_request(&req, OpenAiApiVersion::ChatCompletions).unwrap();
        assert_eq!(out["model"], "gpt-4o");
        assert_eq!(out["max_tokens"], 50);
        assert_eq!(out["messages"][0]["role"], "user");
        assert_eq!(out["messages"][0]["content"][0]["type"], "text");
        assert_eq!(out["messages"][0]["content"][0]["text"], "Hi");
        assert!(out.get("system").is_none());
    }

    #[test]
    fn openai_chat_to_anthropic_system_becomes_top_level() {
        let req = json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "system", "content": "You are helpful."},
                {"role": "user", "content": "Hi"}
            ],
            "max_tokens": 10
        });
        let out = openai_to_anthropic_request(&req, OpenAiApiVersion::ChatCompletions).unwrap();
        assert_eq!(out["system"], "You are helpful.");
        // system message does not appear in the Anthropic messages array
        assert_eq!(out["messages"].as_array().unwrap().len(), 1);
        assert_eq!(out["messages"][0]["role"], "user");
    }

    #[test]
    fn openai_chat_to_anthropic_multiple_system_messages_joined() {
        let req = json!({
            "model": "m",
            "messages": [
                {"role": "system", "content": "rule 1"},
                {"role": "system", "content": "rule 2"},
                {"role": "user", "content": "Hi"}
            ]
        });
        let out = openai_to_anthropic_request(&req, OpenAiApiVersion::ChatCompletions).unwrap();
        assert_eq!(out["system"], "rule 1\nrule 2");
    }

    #[test]
    fn openai_chat_to_anthropic_stop_maps_to_stop_sequences() {
        let req = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "Hi"}],
            "stop": ["END", "STOP"]
        });
        let out = openai_to_anthropic_request(&req, OpenAiApiVersion::ChatCompletions).unwrap();
        assert_eq!(out["stop_sequences"], json!(["END", "STOP"]));
    }

    #[test]
    fn openai_chat_to_anthropic_scalar_stop_wraps_into_array() {
        let req = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "Hi"}],
            "stop": "END"
        });
        let out = openai_to_anthropic_request(&req, OpenAiApiVersion::ChatCompletions).unwrap();
        assert_eq!(out["stop_sequences"], json!(["END"]));
    }

    #[test]
    fn openai_chat_to_anthropic_tool_call_and_result_roundtrip() {
        let req = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "What is the weather?"},
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call-1",
                        "type": "function",
                        "function": {
                            "name": "get_weather",
                            "arguments": "{\"city\":\"SF\"}"
                        }
                    }]
                },
                {
                    "role": "tool",
                    "tool_call_id": "call-1",
                    "content": "sunny"
                },
                {"role": "user", "content": "thanks"}
            ]
        });
        let out = openai_to_anthropic_request(&req, OpenAiApiVersion::ChatCompletions).unwrap();
        let msgs = out["messages"].as_array().unwrap();
        // user(text), assistant(tool_use), user(tool_result), user(text) — tool
        // result stays on its own message since the next user message is a
        // separate turn.
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
        assert_eq!(msgs[1]["content"][0]["id"], "call-1");
        assert_eq!(msgs[1]["content"][0]["input"]["city"], "SF");
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[2]["content"][0]["tool_use_id"], "call-1");
        assert_eq!(msgs[2]["content"][0]["content"], "sunny");
    }

    #[test]
    fn openai_chat_to_anthropic_reasoning_content_becomes_thinking_block() {
        let req = json!({
            "model": "m",
            "messages": [{
                "role": "assistant",
                "content": "answer",
                "reasoning_content": "I think..."
            }]
        });
        let out = openai_to_anthropic_request(&req, OpenAiApiVersion::ChatCompletions).unwrap();
        assert_eq!(out["messages"][0]["content"][0]["type"], "thinking");
        assert_eq!(out["messages"][0]["content"][0]["thinking"], "I think...");
        assert_eq!(out["messages"][0]["content"][1]["type"], "text");
        assert_eq!(out["messages"][0]["content"][1]["text"], "answer");
    }

    #[test]
    fn openai_chat_to_anthropic_tools_flat_schema() {
        let req = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "Hi"}],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "search",
                    "description": "web search",
                    "parameters": {
                        "type": "object",
                        "properties": {"q": {"type": "string"}}
                    }
                }
            }]
        });
        let out = openai_to_anthropic_request(&req, OpenAiApiVersion::ChatCompletions).unwrap();
        let tool = &out["tools"][0];
        assert_eq!(tool["name"], "search");
        assert_eq!(tool["description"], "web search");
        assert_eq!(tool["input_schema"]["type"], "object");
    }

    #[test]
    fn openai_chat_to_anthropic_tool_choice_required_becomes_any() {
        let req = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "Hi"}],
            "tool_choice": "required"
        });
        let out = openai_to_anthropic_request(&req, OpenAiApiVersion::ChatCompletions).unwrap();
        assert_eq!(out["tool_choice"]["type"], "any");
    }

    #[test]
    fn openai_chat_to_anthropic_tool_choice_specific_function() {
        let req = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "Hi"}],
            "tool_choice": {"type": "function", "function": {"name": "search"}}
        });
        let out = openai_to_anthropic_request(&req, OpenAiApiVersion::ChatCompletions).unwrap();
        assert_eq!(out["tool_choice"]["type"], "tool");
        assert_eq!(out["tool_choice"]["name"], "search");
    }

    #[test]
    fn openai_chat_to_anthropic_reasoning_effort_enables_thinking() {
        let req = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "Hi"}],
            "reasoning_effort": "high"
        });
        let out = openai_to_anthropic_request(&req, OpenAiApiVersion::ChatCompletions).unwrap();
        assert_eq!(out["thinking"]["type"], "enabled");
        assert!(out["thinking"]["budget_tokens"].as_u64().is_some());
    }

    #[test]
    fn openai_chat_to_anthropic_image_data_url_decoded() {
        let req = json!({
            "model": "m",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "describe"},
                    {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,AAAA"}}
                ]
            }]
        });
        let out = openai_to_anthropic_request(&req, OpenAiApiVersion::ChatCompletions).unwrap();
        let content = out["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["source"]["type"], "base64");
        assert_eq!(content[1]["source"]["media_type"], "image/jpeg");
        assert_eq!(content[1]["source"]["data"], "AAAA");
    }

    #[test]
    fn openai_chat_to_anthropic_max_completion_tokens_preferred() {
        let req = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 10,
            "max_completion_tokens": 20
        });
        let out = openai_to_anthropic_request(&req, OpenAiApiVersion::ChatCompletions).unwrap();
        assert_eq!(out["max_tokens"], 20);
    }

    // ─── openai_to_anthropic_request: Responses ──────────────────────────────

    #[test]
    fn openai_responses_to_anthropic_instructions_become_system() {
        let req = json!({
            "model": "gpt-4.1",
            "instructions": "You are helpful.",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "Hi"}]
            }],
            "max_output_tokens": 30
        });
        let out = openai_to_anthropic_request(&req, OpenAiApiVersion::Responses).unwrap();
        assert_eq!(out["system"], "You are helpful.");
        assert_eq!(out["model"], "gpt-4.1");
        assert_eq!(out["max_tokens"], 30);
        assert_eq!(out["messages"][0]["role"], "user");
        assert_eq!(out["messages"][0]["content"][0]["text"], "Hi");
    }

    #[test]
    fn openai_responses_to_anthropic_function_call_and_output_roundtrip() {
        let req = json!({
            "model": "m",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "call it"}]},
                {"type": "function_call", "call_id": "fc-1", "name": "search", "arguments": "{\"q\":\"rust\"}"},
                {"type": "function_call_output", "call_id": "fc-1", "output": "result"}
            ]
        });
        let out = openai_to_anthropic_request(&req, OpenAiApiVersion::Responses).unwrap();
        let msgs = out["messages"].as_array().unwrap();
        // user, assistant(tool_use), user(tool_result)
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"][0]["type"], "tool_use");
        assert_eq!(msgs[1]["content"][0]["id"], "fc-1");
        assert_eq!(msgs[1]["content"][0]["input"]["q"], "rust");
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[2]["content"][0]["tool_use_id"], "fc-1");
        assert_eq!(msgs[2]["content"][0]["content"], "result");
    }

    #[test]
    fn openai_responses_to_anthropic_reasoning_prepends_thinking_to_next_message() {
        let req = json!({
            "model": "m",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hi"}]},
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "plan"}]},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "answer"}]}
            ]
        });
        let out = openai_to_anthropic_request(&req, OpenAiApiVersion::Responses).unwrap();
        let assistant = &out["messages"][1];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["content"][0]["type"], "thinking");
        assert_eq!(assistant["content"][0]["thinking"], "plan");
        assert_eq!(assistant["content"][1]["type"], "text");
        assert_eq!(assistant["content"][1]["text"], "answer");
    }

    #[test]
    fn openai_responses_to_anthropic_tools_flat_schema_converted() {
        let req = json!({
            "model": "m",
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hi"}]}],
            "tools": [{
                "type": "function",
                "name": "search",
                "description": "web search",
                "parameters": {"type": "object", "properties": {"q": {"type": "string"}}},
                "strict": false
            }]
        });
        let out = openai_to_anthropic_request(&req, OpenAiApiVersion::Responses).unwrap();
        let tool = &out["tools"][0];
        assert_eq!(tool["name"], "search");
        assert_eq!(tool["description"], "web search");
        assert_eq!(tool["input_schema"]["type"], "object");
    }

    #[test]
    fn patch_thinking_history_injects_empty_block_for_missing_turns() {
        let req = json!({
            "model": "m",
            "thinking": {"type": "enabled", "budget_tokens": 1000},
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": "hi"},
                {"role": "user", "content": "follow up"}
            ]
        });
        let patched = patch_thinking_history(&req, true).unwrap();
        let assistant = &patched["messages"][1];
        let content = assistant["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["thinking"], "");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "hi");
    }

    #[test]
    fn patch_thinking_history_recognizes_anthropic_native_type_format() {
        // Anthropic native format uses {"type":"enabled"} or {"type":"adaptive"}, not {"enabled":true}.
        let req = json!({
            "model": "m",
            "thinking": {"type": "adaptive"},
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": "hi"},
                {"role": "user", "content": "follow up"}
            ]
        });
        let patched = patch_thinking_history(&req, true).unwrap();
        let content = patched["messages"][1]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "thinking");
    }

    #[test]
    fn patch_thinking_history_no_op_when_thinking_disabled() {
        let req = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": "hi"}
            ]
        });
        assert!(patch_thinking_history(&req, false).is_none());
    }

    #[test]
    fn patch_thinking_history_ignores_mixed_history_without_strict_flag() {
        // Tolerant providers (ark, genuine Anthropic) keep the history-trigger
        // off: a mixed history with no `thinking` param must stay untouched.
        let req = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "plan"},
                    {"type": "text", "text": "first"}
                ]},
                {"role": "user", "content": "tool result"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "bash", "input": {"command": "ls"}}
                ]},
                {"role": "user", "content": "next"}
            ]
        });
        assert!(patch_thinking_history(&req, false).is_none());
    }

    #[test]
    fn patch_thinking_history_patches_mixed_history_without_thinking_param() {
        // DeepSeek's Anthropic endpoint enforces the every-turn-thinking rule
        // from the history alone: once one assistant turn carries a thinking
        // block, an earlier turn missing one is rejected even when the request
        // does not enable thinking. The missing turn must get an empty block.
        let req = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "plan"},
                    {"type": "text", "text": "first"}
                ]},
                {"role": "user", "content": "tool result"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "bash", "input": {"command": "ls"}}
                ]},
                {"role": "user", "content": "next"}
            ]
        });
        let patched = patch_thinking_history(&req, true).unwrap();
        let missing = &patched["messages"][3]["content"].as_array().unwrap();
        assert_eq!(missing[0]["type"], "thinking");
        assert_eq!(missing[0]["thinking"], "");
        assert_eq!(missing[1]["type"], "tool_use");
        // The turn that already carried thinking is untouched.
        let first = &patched["messages"][1]["content"].as_array().unwrap();
        assert_eq!(first[0]["thinking"], "plan");
    }

    #[test]
    fn patch_thinking_history_no_op_for_trailing_assistant_prefill() {
        // A trailing assistant message with no thinking block is a prefill — must not be patched.
        let req = json!({
            "model": "m",
            "thinking": {"type": "enabled", "budget_tokens": 1000},
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": "Here is"}
            ]
        });
        assert!(patch_thinking_history(&req, true).is_none());
    }

    #[test]
    fn patch_thinking_history_patches_intermediate_not_trailing() {
        let req = json!({
            "model": "m",
            "thinking": {"type": "enabled", "budget_tokens": 1000},
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": "first"},
                {"role": "user", "content": "follow up"},
                {"role": "assistant", "content": "prefill"}
            ]
        });
        let patched = patch_thinking_history(&req, true).unwrap();
        // Intermediate assistant (index 1) gets thinking block.
        let intermediate = &patched["messages"][1]["content"].as_array().unwrap();
        assert_eq!(intermediate[0]["type"], "thinking");
        // Trailing assistant prefill (index 3) is untouched.
        assert_eq!(patched["messages"][3]["content"], "prefill");
    }

    #[test]
    fn patch_thinking_history_no_op_when_blocks_present() {
        let req = json!({
            "model": "m",
            "thinking": {"type": "enabled", "budget_tokens": 1000},
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "plan"},
                    {"type": "text", "text": "hi"}
                ]}
            ]
        });
        assert!(patch_thinking_history(&req, true).is_none());
    }

    #[test]
    fn patch_thinking_history_no_op_for_single_signed_block() {
        // One signed thinking block per assistant message is accepted upstream.
        let req = json!({
            "model": "m",
            "thinking": {"type": "adaptive"},
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "plan", "signature": "sig-a"},
                    {"type": "text", "text": "hi"}
                ]},
                {"role": "user", "content": "follow up"}
            ]
        });
        assert!(patch_thinking_history(&req, true).is_none());
    }

    #[test]
    fn patch_thinking_history_strips_duplicate_signatures() {
        // Interleaved thinking from the genuine Anthropic API produces multiple
        // signed thinking blocks per assistant message; DeepSeek-compatible
        // providers reject "multiple signatures detected in assistant message
        // content". Only the first signature may survive.
        let req = json!({
            "model": "m",
            "thinking": {"type": "adaptive"},
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "", "signature": "sig-a"},
                    {"type": "tool_use", "id": "t1", "name": "f", "input": {}},
                    {"type": "thinking", "thinking": "", "signature": "sig-b"},
                    {"type": "text", "text": "done"}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}
                ]}
            ]
        });
        let patched = patch_thinking_history(&req, true).unwrap();
        let content = patched["messages"][1]["content"].as_array().unwrap();
        assert_eq!(content[0]["signature"], "sig-a");
        assert!(content[2].get("signature").is_none());
        assert_eq!(content[2]["type"], "thinking");
        assert_eq!(content[3]["text"], "done");
    }

    #[test]
    fn patch_thinking_history_strips_duplicate_signatures_in_trailing_message() {
        // Signature dedup applies to the trailing assistant message too — the
        // prefill exemption only covers injecting missing thinking blocks.
        let req = json!({
            "model": "m",
            "thinking": {"type": "adaptive"},
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "a", "signature": "sig-a"},
                    {"type": "thinking", "thinking": "b", "signature": "sig-b"}
                ]}
            ]
        });
        let patched = patch_thinking_history(&req, true).unwrap();
        let content = patched["messages"][1]["content"].as_array().unwrap();
        assert_eq!(content[0]["signature"], "sig-a");
        assert!(content[1].get("signature").is_none());
    }

    // ─── inject_missing_reasoning_content ────────────────────────────────────

    fn tool_loop_request() -> Value {
        json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "what time is it?"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "Let me check."},
                    {"type": "tool_use", "id": "call_1", "name": "get_time", "input": {"tz": "UTC"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_1", "content": "12:00"}
                ]}
            ]
        })
    }

    #[test]
    fn chat_tool_call_without_thinking_gets_empty_reasoning_content() {
        let out = to_openai(
            &tool_loop_request(),
            &provider_chat_completions(),
            OpenAiApiVersion::ChatCompletions,
        )
        .unwrap();
        let assistant = &out["messages"][1];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["reasoning_content"], "");
    }

    #[test]
    fn chat_tool_call_keeps_existing_reasoning_content() {
        let mut req = tool_loop_request();
        req["messages"][1]["content"][0] = json!({"type": "thinking", "thinking": "hmm"});
        let out = to_openai(
            &req,
            &provider_chat_completions(),
            OpenAiApiVersion::ChatCompletions,
        )
        .unwrap();
        assert_eq!(out["messages"][1]["reasoning_content"], "hmm");
    }

    #[test]
    fn chat_text_only_assistant_gets_no_reasoning_content() {
        let req = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "hello"},
                {"role": "user", "content": "again"}
            ]
        });
        let out = to_openai(
            &req,
            &provider_chat_completions(),
            OpenAiApiVersion::ChatCompletions,
        )
        .unwrap();
        assert!(out["messages"][2].get("reasoning_content").is_none());
    }

    #[test]
    fn chat_injection_skipped_when_thinking_history_disabled() {
        let mut provider = provider_chat_completions();
        provider.inject_thinking_history = false;
        let out = to_openai(
            &tool_loop_request(),
            &provider,
            OpenAiApiVersion::ChatCompletions,
        )
        .unwrap();
        assert!(out["messages"][1].get("reasoning_content").is_none());
    }

    #[test]
    fn responses_tool_call_gets_no_reasoning_content() {
        let out = to_openai(
            &tool_loop_request(),
            &provider_responses(),
            OpenAiApiVersion::Responses,
        )
        .unwrap();
        let items = out["input"].as_array().unwrap();
        assert!(items.iter().all(|i| i.get("reasoning_content").is_none()));
    }

    #[test]
    fn openai_responses_to_anthropic_tool_choice_named_function() {
        let req = json!({
            "model": "m",
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hi"}]}],
            "tool_choice": {"type": "function", "name": "search"}
        });
        let out = openai_to_anthropic_request(&req, OpenAiApiVersion::Responses).unwrap();
        assert_eq!(out["tool_choice"]["type"], "tool");
        assert_eq!(out["tool_choice"]["name"], "search");
    }
}
