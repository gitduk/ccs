use bytes::Bytes;
use futures::Stream;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::config::Provider;
use crate::error::{AppError, Result};

// ─── Request: Anthropic → OpenAI ────────────────────────────────────────────

/// Convert an Anthropic Messages API request to OpenAI Chat Completions format.
pub fn anthropic_to_openai_request(req: &Value, provider: &Provider) -> Result<Value> {
    let model = req
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("claude-sonnet-4-20250514");
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
                let description = tool.get("description").and_then(|d| d.as_str()).unwrap_or("");
                let mut input_schema = tool.get("input_schema").cloned().unwrap_or(json!({"type": "object"}));
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
        Some(Value::Array(blocks)) => {
            convert_content_blocks_to_openai(role, blocks)
        }
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
                    let media_type = source.get("media_type").and_then(|m| m.as_str()).unwrap_or("image/png");
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
                let tool_use_id = block.get("tool_use_id").and_then(|t| t.as_str()).unwrap_or("");
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
        if content_parts.len() == 1 && content_parts[0].get("type").and_then(|t| t.as_str()) == Some("text") {
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
fn clean_schema(schema: &mut Value) {
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

// ─── Response: OpenAI → Anthropic (non-streaming) ───────────────────────────

/// Convert OpenAI Chat Completion response to Anthropic Messages response.
pub fn openai_to_anthropic_response(resp: &Value) -> Result<Value> {
    let choice = resp
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .ok_or_else(|| AppError::Transform("No choices in OpenAI response".into()))?;

    let message = choice.get("message").ok_or_else(|| {
        AppError::Transform("No message in choice".into())
    })?;

    let mut content_blocks: Vec<Value> = Vec::new();

    // Handle reasoning/thinking content
    if let Some(reasoning) = message.get("reasoning_content").and_then(|r| r.as_str()) {
        if !reasoning.is_empty() {
            content_blocks.push(json!({
                "type": "thinking",
                "thinking": reasoning,
            }));
        }
    }

    // Handle text content
    if let Some(text) = message.get("content").and_then(|c| c.as_str()) {
        if !text.is_empty() {
            content_blocks.push(json!({
                "type": "text",
                "text": text,
            }));
        }
    }

    // Handle tool calls
    if let Some(tool_calls) = message.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in tool_calls {
            let id = tc.get("id").and_then(|i| i.as_str()).unwrap_or("");
            if let Some(func) = tc.get("function") {
                let name = func.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let args_str = func.get("arguments").and_then(|a| a.as_str()).unwrap_or("{}");
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
    let finish_reason = choice.get("finish_reason").and_then(|f| f.as_str()).unwrap_or("stop");
    let stop_reason = match finish_reason {
        "stop" => "end_turn",
        "length" => "max_tokens",
        "tool_calls" => "tool_use",
        "content_filter" => "end_turn",
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

    let model = resp.get("model").and_then(|m| m.as_str()).unwrap_or("unknown");
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

// ─── Streaming: OpenAI SSE → Anthropic SSE ──────────────────────────────────

/// Convert an OpenAI SSE stream to Anthropic SSE stream.
pub fn openai_stream_to_anthropic(
    response: reqwest::Response,
) -> impl Stream<Item = std::result::Result<Bytes, std::io::Error>> {
    let stream = async_stream::stream! {
        let mut state = StreamState::new();
        let mut buffer = String::new();

        let mut byte_stream = response.bytes_stream();
        use futures::StreamExt;

        while let Some(chunk_result) = byte_stream.next().await {
            let chunk = match chunk_result {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!("Stream read error: {e}");
                    break;
                }
            };

            buffer.push_str(&String::from_utf8_lossy(&chunk));

            // Process complete SSE lines
            while let Some(pos) = buffer.find('\n') {
                let line = buffer[..pos].trim_end_matches('\r').to_string();
                buffer = buffer[pos + 1..].to_string();

                if line.is_empty() {
                    continue;
                }

                if let Some(data) = line.strip_prefix("data: ") {
                    if data.trim() == "[DONE]" {
                        // Emit final events
                        for event in state.finalize() {
                            yield Ok(Bytes::from(event));
                        }
                        continue;
                    }

                    match serde_json::from_str::<Value>(data) {
                        Ok(chunk_json) => {
                            let events = state.process_chunk(&chunk_json);
                            for event in events {
                                yield Ok(Bytes::from(event));
                            }
                        }
                        Err(e) => {
                            tracing::warn!("Failed to parse SSE chunk: {e}");
                        }
                    }
                }
            }
        }

        // Final finalize if stream ended without [DONE]
        if !state.finalized {
            for event in state.finalize() {
                yield Ok(Bytes::from(event));
            }
        }
    };

    stream
}

/// State machine for converting OpenAI stream chunks to Anthropic events.
struct StreamState {
    message_id: String,
    model: String,
    input_tokens: u64,
    output_tokens: u64,
    content_index: usize,
    started: bool,
    finalized: bool,
    current_block_type: Option<BlockType>,
    stop_reason: Option<String>,
    // Track tool call state
    tool_calls: std::collections::HashMap<usize, ToolCallState>,
}

#[derive(Clone, Debug, PartialEq)]
enum BlockType {
    Text,
    Thinking,
    ToolUse,
}

#[derive(Clone, Debug)]
struct ToolCallState {
    #[allow(dead_code)]
    id: String,
    #[allow(dead_code)]
    name: String,
    arguments_buffer: String,
}

impl StreamState {
    fn new() -> Self {
        Self {
            message_id: format!("msg_{}", Uuid::new_v4()),
            model: String::new(),
            input_tokens: 0,
            output_tokens: 0,
            content_index: 0,
            started: false,
            finalized: false,
            current_block_type: None,
            stop_reason: None,
            tool_calls: std::collections::HashMap::new(),
        }
    }

    fn process_chunk(&mut self, chunk: &Value) -> Vec<String> {
        let mut events = Vec::new();

        // Extract model info
        if let Some(model) = chunk.get("model").and_then(|m| m.as_str()) {
            if self.model.is_empty() {
                self.model = model.to_string();
            }
        }

        // Extract usage from chunk
        if let Some(usage) = chunk.get("usage") {
            if let Some(pt) = usage.get("prompt_tokens").and_then(|t| t.as_u64()) {
                self.input_tokens = pt;
            }
            if let Some(ct) = usage.get("completion_tokens").and_then(|t| t.as_u64()) {
                self.output_tokens = ct;
            }
        }

        // Emit message_start on first chunk
        if !self.started {
            self.started = true;
            events.push(self.format_event("message_start", &json!({
                "type": "message_start",
                "message": {
                    "id": self.message_id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {
                        "input_tokens": self.input_tokens,
                        "output_tokens": 0,
                    }
                }
            })));
        }

        let choice = match chunk.get("choices").and_then(|c| c.as_array()).and_then(|c| c.first()) {
            Some(c) => c,
            None => return events,
        };

        let delta = match choice.get("delta") {
            Some(d) => d,
            None => return events,
        };

        // Check finish_reason
        if let Some(reason) = choice.get("finish_reason").and_then(|f| f.as_str()) {
            self.stop_reason = Some(match reason {
                "stop" => "end_turn".to_string(),
                "length" => "max_tokens".to_string(),
                "tool_calls" => "tool_use".to_string(),
                other => other.to_string(),
            });
        }

        // Handle reasoning_content (thinking)
        if let Some(reasoning) = delta.get("reasoning_content").and_then(|r| r.as_str()) {
            if !reasoning.is_empty() {
                if self.current_block_type.as_ref() != Some(&BlockType::Thinking) {
                    // Close previous block if any
                    events.extend(self.close_current_block());
                    self.current_block_type = Some(BlockType::Thinking);
                    events.push(self.format_event("content_block_start", &json!({
                        "type": "content_block_start",
                        "index": self.content_index,
                        "content_block": {
                            "type": "thinking",
                            "thinking": "",
                        }
                    })));
                }
                events.push(self.format_event("content_block_delta", &json!({
                    "type": "content_block_delta",
                    "index": self.content_index,
                    "delta": {
                        "type": "thinking_delta",
                        "thinking": reasoning,
                    }
                })));
            }
        }

        // Handle text content
        if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
            if !content.is_empty() {
                if self.current_block_type.as_ref() != Some(&BlockType::Text) {
                    events.extend(self.close_current_block());
                    self.current_block_type = Some(BlockType::Text);
                    events.push(self.format_event("content_block_start", &json!({
                        "type": "content_block_start",
                        "index": self.content_index,
                        "content_block": {
                            "type": "text",
                            "text": "",
                        }
                    })));
                }
                events.push(self.format_event("content_block_delta", &json!({
                    "type": "content_block_delta",
                    "index": self.content_index,
                    "delta": {
                        "type": "text_delta",
                        "text": content,
                    }
                })));
            }
        }

        // Handle tool calls
        if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
            for tc in tool_calls {
                let tc_index = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;

                // New tool call
                if let Some(func) = tc.get("function") {
                    if let Some(name) = func.get("name").and_then(|n| n.as_str()) {
                        // Close previous block
                        events.extend(self.close_current_block());
                        self.current_block_type = Some(BlockType::ToolUse);

                        let id = tc.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();

                        self.tool_calls.insert(tc_index, ToolCallState {
                            id: id.clone(),
                            name: name.to_string(),
                            arguments_buffer: String::new(),
                        });

                        events.push(self.format_event("content_block_start", &json!({
                            "type": "content_block_start",
                            "index": self.content_index,
                            "content_block": {
                                "type": "tool_use",
                                "id": id,
                                "name": name,
                                "input": {},
                            }
                        })));
                    }

                    // Accumulate arguments
                    if let Some(args) = func.get("arguments").and_then(|a| a.as_str()) {
                        if !args.is_empty() {
                            if let Some(tc_state) = self.tool_calls.get_mut(&tc_index) {
                                tc_state.arguments_buffer.push_str(args);
                            }
                            events.push(self.format_event("content_block_delta", &json!({
                                "type": "content_block_delta",
                                "index": self.content_index,
                                "delta": {
                                    "type": "input_json_delta",
                                    "partial_json": args,
                                }
                            })));
                        }
                    }
                }
            }
        }

        events
    }

    fn close_current_block(&mut self) -> Vec<String> {
        let mut events = Vec::new();
        if self.current_block_type.is_some() {
            events.push(self.format_event("content_block_stop", &json!({
                "type": "content_block_stop",
                "index": self.content_index,
            })));
            self.content_index += 1;
            self.current_block_type = None;
        }
        events
    }

    fn finalize(&mut self) -> Vec<String> {
        if self.finalized {
            return Vec::new();
        }
        self.finalized = true;

        let mut events = Vec::new();

        // Close any open block
        events.extend(self.close_current_block());

        // message_delta with stop_reason and usage
        let stop_reason = self.stop_reason.clone().unwrap_or_else(|| "end_turn".to_string());
        events.push(self.format_event("message_delta", &json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": stop_reason,
                "stop_sequence": null,
            },
            "usage": {
                "output_tokens": self.output_tokens,
            }
        })));

        // message_stop
        events.push(self.format_event("message_stop", &json!({
            "type": "message_stop",
        })));

        events
    }

    fn format_event(&self, event_type: &str, data: &Value) -> String {
        format!("event: {event_type}\ndata: {}\n\n", serde_json::to_string(data).unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn test_provider() -> Provider {
        Provider {
            base_url: "https://api.example.com".to_string(),
            api_key: "test-key".to_string(),
            api_format: crate::config::ApiFormat::OpenAI,
            model_map: HashMap::from([
                ("claude-sonnet-4-20250514".to_string(), "anthropic/claude-sonnet-4-20250514".to_string()),
            ]),
        }
    }

    #[test]
    fn test_basic_request_conversion() {
        let req = json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": "Hello"}
            ],
            "stream": true
        });

        let result = anthropic_to_openai_request(&req, &test_provider()).unwrap();
        assert_eq!(result["model"], "anthropic/claude-sonnet-4-20250514");
        assert_eq!(result["max_tokens"], 1024);
        assert_eq!(result["stream"], true);
        assert_eq!(result["messages"][0]["role"], "user");
        assert_eq!(result["messages"][0]["content"], "Hello");
        assert!(result["stream_options"]["include_usage"].as_bool().unwrap());
    }

    #[test]
    fn test_system_message_conversion() {
        let req = json!({
            "model": "test",
            "system": "You are helpful",
            "messages": [{"role": "user", "content": "Hi"}],
        });

        let result = anthropic_to_openai_request(&req, &test_provider()).unwrap();
        assert_eq!(result["messages"][0]["role"], "system");
        assert_eq!(result["messages"][0]["content"], "You are helpful");
        assert_eq!(result["messages"][1]["role"], "user");
    }

    #[test]
    fn test_system_array_conversion() {
        let req = json!({
            "model": "test",
            "system": [
                {"type": "text", "text": "Part 1"},
                {"type": "text", "text": "Part 2"},
            ],
            "messages": [{"role": "user", "content": "Hi"}],
        });

        let result = anthropic_to_openai_request(&req, &test_provider()).unwrap();
        assert_eq!(result["messages"][0]["content"], "Part 1\nPart 2");
    }

    #[test]
    fn test_tool_use_request_conversion() {
        let req = json!({
            "model": "test",
            "messages": [{
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "toolu_123",
                    "name": "get_weather",
                    "input": {"location": "NYC"}
                }]
            }],
        });

        let result = anthropic_to_openai_request(&req, &test_provider()).unwrap();
        let msg = &result["messages"][0];
        assert_eq!(msg["role"], "assistant");
        assert_eq!(msg["tool_calls"][0]["id"], "toolu_123");
        assert_eq!(msg["tool_calls"][0]["function"]["name"], "get_weather");
    }

    #[test]
    fn test_tool_result_conversion() {
        let req = json!({
            "model": "test",
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_123",
                    "content": "Sunny, 72°F"
                }]
            }],
        });

        let result = anthropic_to_openai_request(&req, &test_provider()).unwrap();
        let msg = &result["messages"][0];
        assert_eq!(msg["role"], "tool");
        assert_eq!(msg["tool_call_id"], "toolu_123");
        assert_eq!(msg["content"], "Sunny, 72°F");
    }

    #[test]
    fn test_openai_response_conversion() {
        let resp = json!({
            "id": "chatcmpl-123",
            "model": "anthropic/claude-sonnet-4-20250514",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "Hello!"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5
            }
        });

        let result = openai_to_anthropic_response(&resp).unwrap();
        assert_eq!(result["type"], "message");
        assert_eq!(result["role"], "assistant");
        assert_eq!(result["content"][0]["type"], "text");
        assert_eq!(result["content"][0]["text"], "Hello!");
        assert_eq!(result["stop_reason"], "end_turn");
        assert_eq!(result["usage"]["input_tokens"], 10);
        assert_eq!(result["usage"]["output_tokens"], 5);
    }

    #[test]
    fn test_openai_tool_call_response() {
        let resp = json!({
            "id": "chatcmpl-456",
            "model": "test",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_abc",
                        "type": "function",
                        "function": {
                            "name": "get_weather",
                            "arguments": "{\"location\":\"NYC\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        });

        let result = openai_to_anthropic_response(&resp).unwrap();
        assert_eq!(result["stop_reason"], "tool_use");
        assert_eq!(result["content"][0]["type"], "tool_use");
        assert_eq!(result["content"][0]["name"], "get_weather");
        assert_eq!(result["content"][0]["input"]["location"], "NYC");
    }

    #[test]
    fn test_stop_reason_mapping() {
        for (openai, anthropic) in [
            ("stop", "end_turn"),
            ("length", "max_tokens"),
            ("tool_calls", "tool_use"),
        ] {
            let resp = json!({
                "id": "test",
                "model": "test",
                "choices": [{"message": {"content": "hi"}, "finish_reason": openai}],
                "usage": {"prompt_tokens": 0, "completion_tokens": 0}
            });
            let result = openai_to_anthropic_response(&resp).unwrap();
            assert_eq!(result["stop_reason"], anthropic, "failed for {openai}");
        }
    }

    #[test]
    fn test_clean_schema() {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "format": "uri"},
                "name": {"type": "string"},
                "nested": {
                    "type": "object",
                    "properties": {
                        "link": {"type": "string", "format": "uri"}
                    }
                }
            }
        });
        clean_schema(&mut schema);
        assert!(schema["properties"]["url"].get("format").is_none());
        assert!(schema["properties"]["nested"]["properties"]["link"].get("format").is_none());
        assert_eq!(schema["properties"]["name"]["type"], "string");
    }

    #[test]
    fn test_stream_state_message_start() {
        let mut state = StreamState::new();
        let chunk = json!({
            "model": "test-model",
            "choices": [{
                "delta": {"content": "Hi"},
                "finish_reason": null
            }]
        });

        let events = state.process_chunk(&chunk);
        // Should have: message_start, content_block_start, content_block_delta
        assert_eq!(events.len(), 3);
        assert!(events[0].contains("message_start"));
        assert!(events[1].contains("content_block_start"));
        assert!(events[2].contains("text_delta"));
    }

    #[test]
    fn test_stream_state_finalize() {
        let mut state = StreamState::new();
        state.started = true;
        state.stop_reason = Some("end_turn".to_string());

        let events = state.finalize();
        // message_delta + message_stop
        assert_eq!(events.len(), 2);
        assert!(events[0].contains("message_delta"));
        assert!(events[0].contains("end_turn"));
        assert!(events[1].contains("message_stop"));
    }

    #[test]
    fn test_thinking_blocks_in_assistant_message() {
        let req = json!({
            "model": "test",
            "messages": [{
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "Let me think..."},
                    {"type": "text", "text": "The answer is 42."}
                ]
            }],
        });

        let result = anthropic_to_openai_request(&req, &test_provider()).unwrap();
        let msg = &result["messages"][0];
        assert_eq!(msg["reasoning_content"], "Let me think...");
        assert_eq!(msg["content"], "The answer is 42.");
    }
}
