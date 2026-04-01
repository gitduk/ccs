use bytes::Bytes;
use futures::Stream;
use serde_json::{Value, json};
use uuid::Uuid;

/// Convert an OpenAI SSE stream to Anthropic SSE stream.
pub fn openai_stream_to_anthropic(
    response: reqwest::Response,
) -> impl Stream<Item = std::result::Result<Bytes, std::io::Error>> {
    let stream = async_stream::stream! {
        let mut state = StreamState::new();
        let mut buffer = String::new();

        let mut byte_stream = response.bytes_stream();
        use futures::StreamExt;

        const BUFFER_MAX: usize = 1024 * 1024; // 1 MB safety cap

        while let Some(chunk_result) = byte_stream.next().await {
            let chunk = match chunk_result {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!("Stream read error: {e}");
                    // Send error event to client before breaking
                    let error_event = format!(
                        "event: error\ndata: {}\n\n",
                        serde_json::json!({"type": "error", "error": {"message": "Stream read error"}})
                    );
                    yield Ok(Bytes::from(error_event));
                    break;
                }
            };

            if buffer.len() + chunk.len() > BUFFER_MAX {
                tracing::warn!("SSE buffer exceeded 1 MB, dropping chunk");
                buffer.clear();
                continue;
            }
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            // Process complete SSE lines
            while let Some(pos) = buffer.find('\n') {
                let line = buffer[..pos].trim_end_matches('\r').to_string();
                buffer.drain(..=pos);

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
    /// content_index assigned to this tool call's content block.
    content_index: usize,
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
        if let Some(model) = chunk.get("model").and_then(|m| m.as_str())
            && self.model.is_empty()
        {
            self.model = model.to_string();
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
            events.push(self.format_event(
                "message_start",
                &json!({
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
                }),
            ));
        }

        let choice = match chunk
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())
        {
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
        if let Some(reasoning) = delta.get("reasoning_content").and_then(|r| r.as_str())
            && !reasoning.is_empty()
        {
            if self.current_block_type.as_ref() != Some(&BlockType::Thinking) {
                // Close previous block if any
                events.extend(self.close_current_block());
                self.current_block_type = Some(BlockType::Thinking);
                events.push(self.format_event(
                    "content_block_start",
                    &json!({
                        "type": "content_block_start",
                        "index": self.content_index,
                        "content_block": {
                            "type": "thinking",
                            "thinking": "",
                        }
                    }),
                ));
            }
            events.push(self.format_event(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": self.content_index,
                    "delta": {
                        "type": "thinking_delta",
                        "thinking": reasoning,
                    }
                }),
            ));
        }

        // Handle text content
        if let Some(content) = delta.get("content").and_then(|c| c.as_str())
            && !content.is_empty()
        {
            if self.current_block_type.as_ref() != Some(&BlockType::Text) {
                events.extend(self.close_current_block());
                self.current_block_type = Some(BlockType::Text);
                events.push(self.format_event(
                    "content_block_start",
                    &json!({
                        "type": "content_block_start",
                        "index": self.content_index,
                        "content_block": {
                            "type": "text",
                            "text": "",
                        }
                    }),
                ));
            }
            events.push(self.format_event(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": self.content_index,
                    "delta": {
                        "type": "text_delta",
                        "text": content,
                    }
                }),
            ));
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

                        let id = tc
                            .get("id")
                            .and_then(|i| i.as_str())
                            .unwrap_or("")
                            .to_string();

                        self.tool_calls.insert(
                            tc_index,
                            ToolCallState {
                                id: id.clone(),
                                name: name.to_string(),
                                arguments_buffer: String::new(),
                                content_index: self.content_index,
                            },
                        );

                        events.push(self.format_event(
                            "content_block_start",
                            &json!({
                                "type": "content_block_start",
                                "index": self.content_index,
                                "content_block": {
                                    "type": "tool_use",
                                    "id": id,
                                    "name": name,
                                    "input": {},
                                }
                            }),
                        ));
                    }

                    // Accumulate arguments
                    if let Some(args) = func.get("arguments").and_then(|a| a.as_str())
                        && !args.is_empty()
                    {
                        let tc_ci = if let Some(tc_state) = self.tool_calls.get_mut(&tc_index) {
                            tc_state.arguments_buffer.push_str(args);
                            tc_state.content_index
                        } else {
                            self.content_index
                        };
                        events.push(self.format_event(
                            "content_block_delta",
                            &json!({
                                "type": "content_block_delta",
                                "index": tc_ci,
                                "delta": {
                                    "type": "input_json_delta",
                                    "partial_json": args,
                                }
                            }),
                        ));
                    }
                }
            }
        }

        events
    }

    fn close_current_block(&mut self) -> Vec<String> {
        let mut events = Vec::new();
        if self.current_block_type.is_some() {
            events.push(self.format_event(
                "content_block_stop",
                &json!({
                    "type": "content_block_stop",
                    "index": self.content_index,
                }),
            ));
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
        let stop_reason = self
            .stop_reason
            .clone()
            .unwrap_or_else(|| "end_turn".to_string());
        events.push(self.format_event(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": stop_reason,
                    "stop_sequence": null,
                },
                "usage": {
                    "input_tokens": self.input_tokens,
                    "output_tokens": self.output_tokens,
                }
            }),
        ));

        // message_stop
        events.push(self.format_event(
            "message_stop",
            &json!({
                "type": "message_stop",
            }),
        ));

        events
    }

    fn format_event(&self, event_type: &str, data: &Value) -> String {
        format!(
            "event: {event_type}\ndata: {}\n\n",
            serde_json::to_string(data).unwrap_or_default()
        )
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    // ─── helpers ─────────────────────────────────────────────────────────────

    /// Parse all SSE events from a list of raw event strings.
    /// Returns Vec<(event_type, data_json)>.
    fn parse_events(events: &[String]) -> Vec<(String, Value)> {
        events
            .iter()
            .filter_map(|raw| {
                let mut etype = String::new();
                let mut data = String::new();
                for line in raw.lines() {
                    if let Some(e) = line.strip_prefix("event: ") {
                        etype = e.to_string();
                    } else if let Some(d) = line.strip_prefix("data: ") {
                        data = d.to_string();
                    }
                }
                if etype.is_empty() {
                    return None;
                }
                let val: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
                Some((etype, val))
            })
            .collect()
    }

    fn event_types(events: &[(String, Value)]) -> Vec<&str> {
        events.iter().map(|(t, _)| t.as_str()).collect()
    }

    // ─── format_event ────────────────────────────────────────────────────────

    #[test]
    fn format_event_produces_sse_format() {
        let state = StreamState::new();
        let raw = state.format_event("message_start", &json!({"type": "message_start"}));
        assert!(raw.starts_with("event: message_start\n"));
        assert!(raw.contains("data: "));
        assert!(raw.ends_with("\n\n"));
    }

    // ─── message_start on first chunk ────────────────────────────────────────

    #[test]
    fn first_chunk_emits_message_start() {
        let mut state = StreamState::new();
        let chunk = json!({
            "model": "gpt-4o",
            "choices": [{"delta": {"content": "Hi"}, "finish_reason": null}]
        });
        let events = parse_events(&state.process_chunk(&chunk));
        assert_eq!(events[0].0, "message_start");
        // Subsequent chunks must not emit another message_start.
        let events2 = parse_events(&state.process_chunk(&chunk));
        assert!(!event_types(&events2).contains(&"message_start"));
    }

    #[test]
    fn message_start_contains_model_name() {
        let mut state = StreamState::new();
        let chunk = json!({
            "model": "gpt-4o-mini",
            "choices": [{"delta": {"content": "Hi"}, "finish_reason": null}]
        });
        let events = parse_events(&state.process_chunk(&chunk));
        let (_, start_data) = events.iter().find(|(t, _)| t == "message_start").unwrap();
        assert_eq!(start_data["message"]["model"], "gpt-4o-mini");
    }

    // ─── text delta ──────────────────────────────────────────────────────────

    #[test]
    fn text_delta_opens_text_block_then_emits_delta() {
        let mut state = StreamState::new();
        let chunk = json!({
            "model": "m",
            "choices": [{"delta": {"content": "Hello"}, "finish_reason": null}]
        });
        let events = parse_events(&state.process_chunk(&chunk));
        let types = event_types(&events);
        assert!(types.contains(&"content_block_start"));
        assert!(types.contains(&"content_block_delta"));

        let (_, delta_data) = events
            .iter()
            .find(|(t, _)| t == "content_block_delta")
            .unwrap();
        assert_eq!(delta_data["delta"]["type"], "text_delta");
        assert_eq!(delta_data["delta"]["text"], "Hello");
    }

    #[test]
    fn text_block_not_reopened_for_consecutive_deltas() {
        let mut state = StreamState::new();
        let chunk = json!({
            "model": "m",
            "choices": [{"delta": {"content": "Hello"}, "finish_reason": null}]
        });
        let first = parse_events(&state.process_chunk(&chunk));
        let second = parse_events(&state.process_chunk(&chunk));

        // Only the first chunk should have content_block_start
        assert!(event_types(&first).contains(&"content_block_start"));
        assert!(!event_types(&second).contains(&"content_block_start"));
        // Both should have deltas
        assert!(event_types(&first).contains(&"content_block_delta"));
        assert!(event_types(&second).contains(&"content_block_delta"));
    }

    // ─── thinking delta ──────────────────────────────────────────────────────

    #[test]
    fn reasoning_content_opens_thinking_block() {
        let mut state = StreamState::new();
        let chunk = json!({
            "model": "m",
            "choices": [{"delta": {"reasoning_content": "Let me think"}, "finish_reason": null}]
        });
        let events = parse_events(&state.process_chunk(&chunk));
        let (_, block_start) = events
            .iter()
            .find(|(t, _)| t == "content_block_start")
            .unwrap();
        assert_eq!(block_start["content_block"]["type"], "thinking");

        let (_, delta) = events
            .iter()
            .find(|(t, _)| t == "content_block_delta")
            .unwrap();
        assert_eq!(delta["delta"]["type"], "thinking_delta");
        assert_eq!(delta["delta"]["thinking"], "Let me think");
    }

    #[test]
    fn thinking_block_closes_when_text_starts() {
        let mut state = StreamState::new();

        // First: thinking chunk
        let c1 = json!({
            "model": "m",
            "choices": [{"delta": {"reasoning_content": "thinking..."}, "finish_reason": null}]
        });
        state.process_chunk(&c1);

        // Then: text chunk — should close thinking and open text
        let c2 = json!({
            "model": "m",
            "choices": [{"delta": {"content": "Answer"}, "finish_reason": null}]
        });
        let events = parse_events(&state.process_chunk(&c2));
        let types = event_types(&events);
        assert!(
            types.contains(&"content_block_stop"),
            "thinking block should be closed"
        );
        assert!(
            types.contains(&"content_block_start"),
            "text block should open"
        );
    }

    // ─── tool call delta ─────────────────────────────────────────────────────

    #[test]
    fn tool_call_opens_tool_use_block() {
        let mut state = StreamState::new();
        let chunk = json!({
            "model": "m",
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call-1",
                        "type": "function",
                        "function": {"name": "search", "arguments": ""}
                    }]
                },
                "finish_reason": null
            }]
        });
        let events = parse_events(&state.process_chunk(&chunk));
        let (_, block_start) = events
            .iter()
            .find(|(t, _)| t == "content_block_start")
            .unwrap();
        assert_eq!(block_start["content_block"]["type"], "tool_use");
        assert_eq!(block_start["content_block"]["name"], "search");
        assert_eq!(block_start["content_block"]["id"], "call-1");
    }

    #[test]
    fn tool_call_arguments_emit_input_json_delta() {
        let mut state = StreamState::new();
        // Open the tool call
        let open = json!({
            "model": "m",
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0, "id": "call-1", "type": "function",
                        "function": {"name": "search", "arguments": ""}
                    }]
                },
                "finish_reason": null
            }]
        });
        state.process_chunk(&open);

        // Send argument fragment
        let args = json!({
            "model": "m",
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "function": {"arguments": "{\"q\":\"rust\"}"}
                    }]
                },
                "finish_reason": null
            }]
        });
        let events = parse_events(&state.process_chunk(&args));
        let (_, delta) = events
            .iter()
            .find(|(t, _)| t == "content_block_delta")
            .unwrap();
        assert_eq!(delta["delta"]["type"], "input_json_delta");
        assert_eq!(delta["delta"]["partial_json"], "{\"q\":\"rust\"}");
    }

    // ─── finish_reason mapping ────────────────────────────────────────────────

    #[test]
    fn finish_reason_stop_maps_to_end_turn_in_finalize() {
        let mut state = StreamState::new();
        let chunk = json!({
            "model": "m",
            "choices": [{"delta": {"content": "Hi"}, "finish_reason": "stop"}]
        });
        state.process_chunk(&chunk);
        let events = parse_events(&state.finalize());
        let (_, delta) = events.iter().find(|(t, _)| t == "message_delta").unwrap();
        assert_eq!(delta["delta"]["stop_reason"], "end_turn");
    }

    #[test]
    fn finish_reason_length_maps_to_max_tokens_in_finalize() {
        let mut state = StreamState::new();
        let chunk = json!({
            "model": "m",
            "choices": [{"delta": {"content": "Hi"}, "finish_reason": "length"}]
        });
        state.process_chunk(&chunk);
        let events = parse_events(&state.finalize());
        let (_, delta) = events.iter().find(|(t, _)| t == "message_delta").unwrap();
        assert_eq!(delta["delta"]["stop_reason"], "max_tokens");
    }

    #[test]
    fn finish_reason_tool_calls_maps_to_tool_use_in_finalize() {
        let mut state = StreamState::new();
        let chunk = json!({
            "model": "m",
            "choices": [{"delta": {}, "finish_reason": "tool_calls"}]
        });
        state.process_chunk(&chunk);
        let events = parse_events(&state.finalize());
        let (_, delta) = events.iter().find(|(t, _)| t == "message_delta").unwrap();
        assert_eq!(delta["delta"]["stop_reason"], "tool_use");
    }

    // ─── finalize ────────────────────────────────────────────────────────────

    #[test]
    fn finalize_emits_message_delta_and_message_stop() {
        let mut state = StreamState::new();
        state.started = true; // skip message_start for simplicity
        let events = parse_events(&state.finalize());
        let types = event_types(&events);
        assert!(types.contains(&"message_delta"));
        assert!(types.contains(&"message_stop"));
    }

    #[test]
    fn finalize_idempotent() {
        let mut state = StreamState::new();
        state.started = true;
        let first = state.finalize();
        let second = state.finalize();
        assert!(!first.is_empty());
        assert!(second.is_empty(), "second finalize should return nothing");
    }

    #[test]
    fn finalize_closes_open_text_block() {
        let mut state = StreamState::new();
        let chunk = json!({
            "model": "m",
            "choices": [{"delta": {"content": "Hi"}, "finish_reason": null}]
        });
        state.process_chunk(&chunk);
        let events = parse_events(&state.finalize());
        let types = event_types(&events);
        assert!(
            types.contains(&"content_block_stop"),
            "open block must be closed on finalize"
        );
    }

    #[test]
    fn finalize_default_stop_reason_is_end_turn() {
        let mut state = StreamState::new();
        state.started = true;
        // No finish_reason chunk sent — should default to end_turn
        let events = parse_events(&state.finalize());
        let (_, delta) = events.iter().find(|(t, _)| t == "message_delta").unwrap();
        assert_eq!(delta["delta"]["stop_reason"], "end_turn");
    }

    #[test]
    fn finalize_carries_usage_tokens() {
        let mut state = StreamState::new();
        let chunk = json!({
            "model": "m",
            "usage": {"prompt_tokens": 42, "completion_tokens": 17},
            "choices": [{"delta": {}, "finish_reason": "stop"}]
        });
        state.process_chunk(&chunk);
        let events = parse_events(&state.finalize());
        let (_, delta) = events.iter().find(|(t, _)| t == "message_delta").unwrap();
        assert_eq!(delta["usage"]["input_tokens"], 42);
        assert_eq!(delta["usage"]["output_tokens"], 17);
    }
}
