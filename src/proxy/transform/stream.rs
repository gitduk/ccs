use bytes::Bytes;
use futures::Stream;
use serde_json::{json, Value};
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
        if let Some(reasoning) = delta.get("reasoning_content").and_then(|r| r.as_str()) {
            if !reasoning.is_empty() {
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
        }

        // Handle text content
        if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
            if !content.is_empty() {
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
                    if let Some(args) = func.get("arguments").and_then(|a| a.as_str()) {
                        if !args.is_empty() {
                            if let Some(tc_state) = self.tool_calls.get_mut(&tc_index) {
                                tc_state.arguments_buffer.push_str(args);
                            }
                            events.push(self.format_event(
                                "content_block_delta",
                                &json!({
                                    "type": "content_block_delta",
                                    "index": self.content_index,
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
