use bytes::Bytes;
use futures::Stream;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::config::OpenAiApiVersion;

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
    // Track Responses API function calls by output item id.
    response_tool_calls: std::collections::HashMap<String, ToolCallState>,
}

#[derive(Clone, Debug, PartialEq)]
enum BlockType {
    Text,
    Thinking,
    ToolUse,
}

#[derive(Clone, Debug)]
struct ToolCallState {
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
            response_tool_calls: std::collections::HashMap::new(),
        }
    }

    fn process_chunk(&mut self, chunk: &Value) -> Vec<String> {
        if let Some(event_type) = chunk.get("type").and_then(|t| t.as_str())
            && event_type.starts_with("response.")
        {
            return self.process_responses_event(chunk, event_type);
        }
        self.process_chat_completions_chunk(chunk)
    }

    fn process_chat_completions_chunk(&mut self, chunk: &Value) -> Vec<String> {
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

        if let Some(start) = self.emit_message_start() {
            events.push(start);
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

        if let Some(reason) = choice.get("finish_reason").and_then(|f| f.as_str()) {
            self.stop_reason = Some(super::chat_finish_to_anthropic_stop(reason).to_string());
        }

        // Handle reasoning_content (thinking)
        if let Some(reasoning) = delta.get("reasoning_content").and_then(|r| r.as_str())
            && !reasoning.is_empty()
        {
            events.extend(self.emit_content_block(
                BlockType::Thinking,
                "thinking",
                "thinking_delta",
                "thinking",
                reasoning,
            ));
        }

        // Handle text content
        if let Some(content) = delta.get("content").and_then(|c| c.as_str())
            && !content.is_empty()
        {
            events.extend(self.emit_content_block(
                BlockType::Text,
                "text",
                "text_delta",
                "text",
                content,
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

    fn process_responses_event(&mut self, chunk: &Value, event_type: &str) -> Vec<String> {
        let mut events = Vec::new();

        let response = chunk.get("response").unwrap_or(chunk);
        if let Some(id) = response.get("id").and_then(|v| v.as_str()) {
            self.message_id = id.to_string();
        }
        if let Some(model) = response.get("model").and_then(|v| v.as_str())
            && self.model.is_empty()
        {
            self.model = model.to_string();
        }
        if let Some(usage) = response.get("usage") {
            if let Some(it) = usage.get("input_tokens").and_then(|v| v.as_u64()) {
                self.input_tokens = it;
            }
            if let Some(ot) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
                self.output_tokens = ot;
            }
        }

        match event_type {
            "response.created" | "response.in_progress" => {
                if let Some(start) = self.emit_message_start() {
                    events.push(start);
                }
            }
            "response.output_text.delta" => {
                if let Some(start) = self.emit_message_start() {
                    events.push(start);
                }
                if let Some(delta) = chunk.get("delta").and_then(|v| v.as_str())
                    && !delta.is_empty()
                {
                    events.extend(self.emit_content_block(
                        BlockType::Text,
                        "text",
                        "text_delta",
                        "text",
                        delta,
                    ));
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(start) = self.emit_message_start() {
                    events.push(start);
                }
                if let Some(delta) = chunk.get("delta").and_then(|v| v.as_str())
                    && !delta.is_empty()
                {
                    events.extend(self.emit_content_block(
                        BlockType::Thinking,
                        "thinking",
                        "thinking_delta",
                        "thinking",
                        delta,
                    ));
                }
            }
            "response.output_item.added" => {
                if let Some(start) = self.emit_message_start() {
                    events.push(start);
                }
                if let Some(item) = chunk.get("item")
                    && item.get("type").and_then(|v| v.as_str()) == Some("function_call")
                {
                    let item_id = item
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let item_id_key = item_id.clone();
                    let call_id = item
                        .get("call_id")
                        .and_then(|v| v.as_str())
                        .or_else(|| item.get("id").and_then(|v| v.as_str()))
                        .unwrap_or("")
                        .to_string();
                    let name = item
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    events.extend(self.close_current_block());
                    self.current_block_type = Some(BlockType::ToolUse);
                    let content_index = self.content_index;
                    self.response_tool_calls.insert(
                        item_id,
                        ToolCallState {
                            arguments_buffer: String::new(),
                            content_index,
                        },
                    );
                    events.push(self.format_event(
                        "content_block_start",
                        &json!({
                            "type": "content_block_start",
                            "index": content_index,
                            "content_block": {
                                "type": "tool_use",
                                "id": call_id,
                                "name": name,
                                "input": {},
                            }
                        }),
                    ));

                    if let Some(args) = item.get("arguments").and_then(|v| v.as_str())
                        && !args.is_empty()
                    {
                        if let Some(tc) = self.response_tool_calls.get_mut(&item_id_key) {
                            tc.arguments_buffer.push_str(args);
                        }
                        events.push(self.format_event(
                            "content_block_delta",
                            &json!({
                                "type": "content_block_delta",
                                "index": content_index,
                                "delta": {
                                    "type": "input_json_delta",
                                    "partial_json": args,
                                }
                            }),
                        ));
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some(item_id) = chunk.get("item_id").and_then(|v| v.as_str())
                    && let Some(delta) = chunk.get("delta").and_then(|v| v.as_str())
                    && !delta.is_empty()
                    && let Some(tc) = self.response_tool_calls.get_mut(item_id)
                {
                    let content_index = tc.content_index;
                    tc.arguments_buffer.push_str(delta);
                    events.push(self.format_event(
                        "content_block_delta",
                        &json!({
                            "type": "content_block_delta",
                            "index": content_index,
                            "delta": {
                                "type": "input_json_delta",
                                "partial_json": delta,
                            }
                        }),
                    ));
                }
            }
            "response.output_item.done" => {
                // Close the current tool_use block when the item finishes,
                // rather than waiting for the next item or finalize.
                if let Some(item) = chunk.get("item")
                    && item.get("type").and_then(|v| v.as_str()) == Some("function_call")
                {
                    events.extend(self.close_current_block());
                }
            }
            "response.completed" => {
                let status = response
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("completed");
                self.stop_reason =
                    Some(super::response_status_to_anthropic_stop(status).to_string());
                self.response_tool_calls.clear();
            }
            _ => {}
        }

        events
    }

    fn emit_message_start(&mut self) -> Option<String> {
        if self.started {
            return None;
        }
        self.started = true;
        Some(self.format_event(
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
        ))
    }

    /// Open a new content block if needed, then emit a delta event.
    /// Handles the repeated pattern: check block type → close if different → open → delta.
    fn emit_content_block(
        &mut self,
        block_type: BlockType,
        block_type_str: &str,
        delta_type: &str,
        content_key: &str,
        content_value: &str,
    ) -> Vec<String> {
        let mut events = Vec::new();
        if self.current_block_type.as_ref() != Some(&block_type) {
            events.extend(self.close_current_block());
            self.current_block_type = Some(block_type);
            events.push(self.format_event(
                "content_block_start",
                &json!({
                    "type": "content_block_start",
                    "index": self.content_index,
                    "content_block": {
                        "type": block_type_str,
                        block_type_str: "",
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
                    "type": delta_type,
                    content_key: content_value,
                }
            }),
        ));
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

// ─── Anthropic SSE → OpenAI SSE ──────────────────────────────────────────────

/// Convert an Anthropic Messages SSE byte-stream to an OpenAI SSE byte-stream.
///
/// Supports both OpenAI Chat Completions (`chat.completion.chunk`) and the
/// Responses API (`response.*` events). Used when a client speaks OpenAI but
/// the upstream provider (or upstream-normalised pipeline) is Anthropic.
pub fn anthropic_stream_to_openai<S>(
    byte_stream: S,
    target: OpenAiApiVersion,
) -> impl Stream<Item = std::result::Result<Bytes, std::io::Error>> + Send
where
    S: Stream<Item = std::result::Result<Bytes, std::io::Error>> + Send + 'static,
{
    async_stream::stream! {
        let mut state = AnthropicToOpenAiState::new(target);
        let mut buffer = String::new();

        use futures::StreamExt;
        let mut byte_stream = Box::pin(byte_stream);

        const BUFFER_MAX: usize = 1024 * 1024;

        while let Some(chunk_result) = byte_stream.next().await {
            let chunk = match chunk_result {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!("Stream read error: {e}");
                    for ev in state.emit_error() {
                        yield Ok(Bytes::from(ev));
                    }
                    break;
                }
            };

            if buffer.len() + chunk.len() > BUFFER_MAX {
                tracing::warn!("SSE buffer exceeded 1 MB, dropping chunk");
                buffer.clear();
                continue;
            }
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(pos) = buffer.find('\n') {
                let line = buffer[..pos].trim_end_matches('\r').to_string();
                buffer.drain(..=pos);

                if line.is_empty() {
                    continue;
                }
                if let Some(data) = line.strip_prefix("data: ") {
                    if data.trim() == "[DONE]" {
                        continue;
                    }
                    match serde_json::from_str::<Value>(data) {
                        Ok(ev) => {
                            for out in state.process_anthropic_event(&ev) {
                                yield Ok(Bytes::from(out));
                            }
                        }
                        Err(e) => tracing::warn!("Failed to parse Anthropic SSE chunk: {e}"),
                    }
                }
            }
        }

        for ev in state.finalize() {
            yield Ok(Bytes::from(ev));
        }
    }
}

struct AnthropicToOpenAiState {
    target: OpenAiApiVersion,
    id: String,
    model: String,
    created: u64,
    input_tokens: u64,
    output_tokens: u64,
    stop_reason: Option<String>,
    role_emitted: bool,
    finalized: bool,
    // Chat Completions: next tool_calls[].index value.
    next_tool_call_index: usize,
    // Currently-open Anthropic content block's OpenAI tool_calls[] index
    // (Some iff the current block is a tool_use).
    current_tool_call_index: Option<usize>,
    // Responses API: output item counter for output_index fields.
    next_output_index: usize,
    // Responses API: current output item id for tool use streaming.
    current_tool_item_id: Option<String>,
    current_tool_call_id: Option<String>,
    // Responses API: whether we've emitted response.created.
    response_created_emitted: bool,
    // Responses API: index of the currently-open output item (message or
    // reasoning). `Some(_)` is the authoritative "a message-like item is
    // open and awaiting output_item.done".
    current_message_index: Option<usize>,
    current_block_kind: Option<BlockType>,
}

impl AnthropicToOpenAiState {
    fn new(target: OpenAiApiVersion) -> Self {
        Self {
            target,
            id: String::new(),
            model: String::new(),
            created: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            input_tokens: 0,
            output_tokens: 0,
            stop_reason: None,
            role_emitted: false,
            finalized: false,
            next_tool_call_index: 0,
            current_tool_call_index: None,
            next_output_index: 0,
            current_tool_item_id: None,
            current_tool_call_id: None,
            response_created_emitted: false,
            current_message_index: None,
            current_block_kind: None,
        }
    }

    fn is_chat(&self) -> bool {
        matches!(self.target, OpenAiApiVersion::ChatCompletions)
    }

    fn process_anthropic_event(&mut self, ev: &Value) -> Vec<String> {
        let event_type = ev.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match event_type {
            "message_start" => self.on_message_start(ev),
            "content_block_start" => self.on_content_block_start(ev),
            "content_block_delta" => self.on_content_block_delta(ev),
            "content_block_stop" => self.on_content_block_stop(),
            "message_delta" => self.on_message_delta(ev),
            "message_stop" => Vec::new(), // handled in finalize()
            _ => Vec::new(),
        }
    }

    fn on_message_start(&mut self, ev: &Value) -> Vec<String> {
        if let Some(msg) = ev.get("message") {
            if let Some(id) = msg.get("id").and_then(|v| v.as_str()) {
                self.id = id.to_string();
            }
            if let Some(model) = msg.get("model").and_then(|v| v.as_str()) {
                self.model = model.to_string();
            }
            if let Some(usage) = msg.get("usage") {
                if let Some(it) = usage.get("input_tokens").and_then(|v| v.as_u64()) {
                    self.input_tokens = it;
                }
                if let Some(ot) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
                    self.output_tokens = ot;
                }
            }
        }
        if self.id.is_empty() {
            self.id = if self.is_chat() {
                format!("chatcmpl-{}", Uuid::new_v4())
            } else {
                format!("resp_{}", Uuid::new_v4())
            };
        }
        if self.is_chat() {
            self.emit_chat_role_chunk()
        } else {
            self.emit_response_created()
        }
    }

    fn on_content_block_start(&mut self, ev: &Value) -> Vec<String> {
        let block = match ev.get("content_block") {
            Some(b) => b,
            None => return Vec::new(),
        };
        let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match block_type {
            "text" => {
                self.current_block_kind = Some(BlockType::Text);
                if self.is_chat() {
                    Vec::new()
                } else {
                    self.emit_response_message_open()
                }
            }
            "thinking" => {
                self.current_block_kind = Some(BlockType::Thinking);
                if self.is_chat() {
                    Vec::new()
                } else {
                    self.emit_response_reasoning_open()
                }
            }
            "tool_use" => {
                self.current_block_kind = Some(BlockType::ToolUse);
                let id = block
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = block
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if self.is_chat() {
                    self.emit_chat_tool_call_start(&id, &name)
                } else {
                    self.emit_response_tool_call_start(&id, &name)
                }
            }
            _ => Vec::new(),
        }
    }

    fn on_content_block_delta(&mut self, ev: &Value) -> Vec<String> {
        let delta = match ev.get("delta") {
            Some(d) => d,
            None => return Vec::new(),
        };
        let delta_type = delta.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match delta_type {
            "text_delta" => {
                let text = delta.get("text").and_then(|t| t.as_str()).unwrap_or("");
                if text.is_empty() {
                    return Vec::new();
                }
                if self.is_chat() {
                    vec![self.chat_chunk(json!({"content": text}), None)]
                } else {
                    self.emit_response_text_delta(text)
                }
            }
            "thinking_delta" => {
                let text = delta.get("thinking").and_then(|t| t.as_str()).unwrap_or("");
                if text.is_empty() {
                    return Vec::new();
                }
                if self.is_chat() {
                    vec![self.chat_chunk(json!({"reasoning_content": text}), None)]
                } else {
                    self.emit_response_reasoning_delta(text)
                }
            }
            "input_json_delta" => {
                let partial = delta
                    .get("partial_json")
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                if partial.is_empty() {
                    return Vec::new();
                }
                if self.is_chat() {
                    self.emit_chat_tool_arguments(partial)
                } else {
                    self.emit_response_tool_arguments(partial)
                }
            }
            _ => Vec::new(),
        }
    }

    fn on_content_block_stop(&mut self) -> Vec<String> {
        let kind = self.current_block_kind.take();
        self.current_tool_call_index = None;
        if self.is_chat() {
            return Vec::new();
        }
        // Responses API: emit close events for the current block.
        match kind {
            Some(BlockType::ToolUse) => self.emit_response_tool_call_done(),
            Some(BlockType::Text) => self.emit_response_message_close(),
            Some(BlockType::Thinking) => self.emit_response_reasoning_close(),
            None => Vec::new(),
        }
    }

    fn on_message_delta(&mut self, ev: &Value) -> Vec<String> {
        if let Some(delta) = ev.get("delta")
            && let Some(reason) = delta.get("stop_reason").and_then(|r| r.as_str())
        {
            self.stop_reason = Some(reason.to_string());
        }
        if let Some(usage) = ev.get("usage") {
            if let Some(it) = usage.get("input_tokens").and_then(|v| v.as_u64()) {
                self.input_tokens = it;
            }
            if let Some(ot) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
                self.output_tokens = ot;
            }
        }
        Vec::new()
    }

    // ─── Chat Completions emitters ───────────────────────────────────────────

    fn emit_chat_role_chunk(&mut self) -> Vec<String> {
        if self.role_emitted {
            return Vec::new();
        }
        self.role_emitted = true;
        vec![self.chat_chunk(json!({"role": "assistant"}), None)]
    }

    fn emit_chat_tool_call_start(&mut self, id: &str, name: &str) -> Vec<String> {
        let index = self.next_tool_call_index;
        self.next_tool_call_index += 1;
        self.current_tool_call_index = Some(index);
        vec![self.chat_chunk(
            json!({
                "tool_calls": [{
                    "index": index,
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": "" }
                }]
            }),
            None,
        )]
    }

    fn emit_chat_tool_arguments(&mut self, partial: &str) -> Vec<String> {
        let index = match self.current_tool_call_index {
            Some(i) => i,
            None => return Vec::new(),
        };
        vec![self.chat_chunk(
            json!({
                "tool_calls": [{
                    "index": index,
                    "function": { "arguments": partial }
                }]
            }),
            None,
        )]
    }

    fn chat_chunk(&self, delta: Value, finish_reason: Option<&str>) -> String {
        let mut choice = json!({
            "index": 0,
            "delta": delta,
            "finish_reason": Value::Null,
        });
        if let Some(reason) = finish_reason {
            choice["finish_reason"] = Value::String(reason.to_string());
        }
        let chunk = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [choice],
        });
        format!(
            "data: {}\n\n",
            serde_json::to_string(&chunk).unwrap_or_default()
        )
    }

    fn chat_usage_chunk(&self) -> String {
        let chunk = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [],
            "usage": {
                "prompt_tokens": self.input_tokens,
                "completion_tokens": self.output_tokens,
                "total_tokens": self.input_tokens + self.output_tokens,
            }
        });
        format!(
            "data: {}\n\n",
            serde_json::to_string(&chunk).unwrap_or_default()
        )
    }

    fn chat_finish_reason(&self) -> &'static str {
        super::anthropic_stop_to_chat_finish(self.stop_reason.as_deref())
    }

    // ─── Responses API emitters ──────────────────────────────────────────────

    fn emit_response_created(&mut self) -> Vec<String> {
        if self.response_created_emitted {
            return Vec::new();
        }
        self.response_created_emitted = true;
        let response = json!({
            "id": self.id,
            "object": "response",
            "created_at": self.created,
            "model": self.model,
            "status": "in_progress",
            "output": [],
        });
        vec![
            format_response_event(
                "response.created",
                &json!({
                    "type": "response.created",
                    "response": response.clone(),
                }),
            ),
            format_response_event(
                "response.in_progress",
                &json!({
                    "type": "response.in_progress",
                    "response": response,
                }),
            ),
        ]
    }

    fn take_message_index(&mut self) -> usize {
        match self.current_message_index {
            Some(i) => i,
            None => {
                let i = self.next_output_index;
                self.next_output_index += 1;
                self.current_message_index = Some(i);
                i
            }
        }
    }

    fn emit_response_message_open(&mut self) -> Vec<String> {
        if self.current_message_index.is_some() {
            return Vec::new();
        }
        let index = self.take_message_index();
        let item = json!({
            "type": "message",
            "id": format!("msg_{}", Uuid::new_v4()),
            "status": "in_progress",
            "role": "assistant",
            "content": [],
        });
        vec![format_response_event(
            "response.output_item.added",
            &json!({
                "type": "response.output_item.added",
                "output_index": index,
                "item": item,
            }),
        )]
    }

    fn emit_response_text_delta(&mut self, text: &str) -> Vec<String> {
        let opens = self.emit_response_message_open();
        let index = self.current_message_index.unwrap_or(0);
        let mut out = opens;
        out.push(format_response_event(
            "response.output_text.delta",
            &json!({
                "type": "response.output_text.delta",
                "output_index": index,
                "content_index": 0,
                "delta": text,
            }),
        ));
        out
    }

    fn emit_response_message_close(&mut self) -> Vec<String> {
        let Some(index) = self.current_message_index.take() else {
            return Vec::new();
        };
        vec![format_response_event(
            "response.output_item.done",
            &json!({
                "type": "response.output_item.done",
                "output_index": index,
                "item": {
                    "type": "message",
                    "role": "assistant",
                    "status": "completed",
                }
            }),
        )]
    }

    fn emit_response_reasoning_open(&mut self) -> Vec<String> {
        // Reasoning items are siblings of messages; close an open message
        // first if needed.
        let mut out = self.emit_response_message_close();
        let index = self.next_output_index;
        self.next_output_index += 1;
        self.current_message_index = Some(index);
        out.push(format_response_event(
            "response.output_item.added",
            &json!({
                "type": "response.output_item.added",
                "output_index": index,
                "item": {
                    "type": "reasoning",
                    "summary": [],
                },
            }),
        ));
        out
    }

    fn emit_response_reasoning_delta(&mut self, text: &str) -> Vec<String> {
        let index = self.current_message_index.unwrap_or(0);
        vec![format_response_event(
            "response.reasoning_summary_text.delta",
            &json!({
                "type": "response.reasoning_summary_text.delta",
                "output_index": index,
                "summary_index": 0,
                "delta": text,
            }),
        )]
    }

    fn emit_response_reasoning_close(&mut self) -> Vec<String> {
        // Reuse message close: same shape, same tracking var.
        self.emit_response_message_close()
    }

    fn emit_response_tool_call_start(&mut self, id: &str, name: &str) -> Vec<String> {
        // Close an open message/reasoning first so the tool call is a sibling.
        let mut out = self.emit_response_message_close();
        let index = self.next_output_index;
        self.next_output_index += 1;
        let item_id = format!("fc_{}", Uuid::new_v4());
        self.current_tool_item_id = Some(item_id.clone());
        self.current_tool_call_id = Some(id.to_string());
        self.current_message_index = Some(index);
        out.push(format_response_event(
            "response.output_item.added",
            &json!({
                "type": "response.output_item.added",
                "output_index": index,
                "item": {
                    "type": "function_call",
                    "id": item_id,
                    "call_id": id,
                    "name": name,
                    "arguments": "",
                    "status": "in_progress",
                },
            }),
        ));
        out
    }

    fn emit_response_tool_arguments(&self, partial: &str) -> Vec<String> {
        let item_id = match self.current_tool_item_id.as_ref() {
            Some(id) => id,
            None => return Vec::new(),
        };
        let output_index = self.current_message_index.unwrap_or(0);
        vec![format_response_event(
            "response.function_call_arguments.delta",
            &json!({
                "type": "response.function_call_arguments.delta",
                "output_index": output_index,
                "item_id": item_id,
                "delta": partial,
            }),
        )]
    }

    fn emit_response_tool_call_done(&mut self) -> Vec<String> {
        let index = match self.current_message_index.take() {
            Some(i) => i,
            None => return Vec::new(),
        };
        let item_id = self.current_tool_item_id.take().unwrap_or_default();
        let call_id = self.current_tool_call_id.take().unwrap_or_default();
        vec![format_response_event(
            "response.output_item.done",
            &json!({
                "type": "response.output_item.done",
                "output_index": index,
                "item": {
                    "type": "function_call",
                    "id": item_id,
                    "call_id": call_id,
                    "status": "completed",
                },
            }),
        )]
    }

    fn response_status(&self) -> &'static str {
        super::anthropic_stop_to_response_status(self.stop_reason.as_deref())
    }

    // ─── finalize ────────────────────────────────────────────────────────────

    fn finalize(&mut self) -> Vec<String> {
        if self.finalized {
            return Vec::new();
        }
        self.finalized = true;
        let mut out = Vec::new();

        // Close any dangling open block (defensive: normally already closed
        // via content_block_stop).
        if self.current_message_index.is_some() && !self.is_chat() {
            out.extend(self.emit_response_message_close());
        }

        if self.is_chat() {
            let finish = self.chat_finish_reason();
            out.push(self.chat_chunk(json!({}), Some(finish)));
            out.push(self.chat_usage_chunk());
            out.push("data: [DONE]\n\n".to_string());
        } else {
            let response = json!({
                "id": self.id,
                "object": "response",
                "created_at": self.created,
                "model": self.model,
                "status": self.response_status(),
                "usage": {
                    "input_tokens": self.input_tokens,
                    "output_tokens": self.output_tokens,
                    "total_tokens": self.input_tokens + self.output_tokens,
                },
            });
            out.push(format_response_event(
                "response.completed",
                &json!({
                    "type": "response.completed",
                    "response": response,
                }),
            ));
        }
        out
    }

    fn emit_error(&self) -> Vec<String> {
        if self.is_chat() {
            vec!["data: [DONE]\n\n".to_string()]
        } else {
            vec![format_response_event(
                "response.failed",
                &json!({
                    "type": "response.failed",
                    "response": {"id": self.id, "status": "failed"}
                }),
            )]
        }
    }
}

fn format_response_event(event_type: &str, data: &Value) -> String {
    format!(
        "event: {event_type}\ndata: {}\n\n",
        serde_json::to_string(data).unwrap_or_default()
    )
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

    // ─── responses stream events ─────────────────────────────────────────────

    #[test]
    fn responses_output_text_delta_emits_text_block() {
        let mut state = StreamState::new();
        let created = json!({
            "type": "response.created",
            "response": {"id": "resp_1", "model": "gpt-4.1"}
        });
        let delta = json!({
            "type": "response.output_text.delta",
            "delta": "hello"
        });
        state.process_chunk(&created);
        let events = parse_events(&state.process_chunk(&delta));
        let (_, data) = events
            .iter()
            .find(|(t, _)| t == "content_block_delta")
            .unwrap();
        assert_eq!(data["delta"]["type"], "text_delta");
        assert_eq!(data["delta"]["text"], "hello");
    }

    #[test]
    fn responses_completed_updates_usage_and_stop_reason() {
        let mut state = StreamState::new();
        let completed = json!({
            "type": "response.completed",
            "response": {
                "status": "completed",
                "usage": {"input_tokens": 9, "output_tokens": 3}
            }
        });
        state.process_chunk(&completed);
        let events = parse_events(&state.finalize());
        let (_, delta) = events.iter().find(|(t, _)| t == "message_delta").unwrap();
        assert_eq!(delta["delta"]["stop_reason"], "end_turn");
        assert_eq!(delta["usage"]["input_tokens"], 9);
        assert_eq!(delta["usage"]["output_tokens"], 3);
    }

    #[test]
    fn responses_multiple_tool_calls_get_distinct_indices() {
        let mut state = StreamState::new();
        state.process_chunk(&json!({
            "type": "response.created",
            "response": {"id": "resp_tc", "model": "gpt-5.1-codex"}
        }));
        // First tool call
        let events1 = state.process_chunk(&json!({
            "type": "response.output_item.added",
            "item": {
                "type": "function_call",
                "id": "item_1",
                "call_id": "call_1",
                "name": "read_file"
            }
        }));
        let parsed1 = parse_events(&events1);
        let start1 = parsed1
            .iter()
            .find(|(t, _)| t == "content_block_start")
            .unwrap();
        let idx1 = start1.1["index"].as_u64().unwrap();

        // Second tool call — close_current_block closes the first, then opens the second
        let events2 = state.process_chunk(&json!({
            "type": "response.output_item.added",
            "item": {
                "type": "function_call",
                "id": "item_2",
                "call_id": "call_2",
                "name": "write_file"
            }
        }));
        let parsed2 = parse_events(&events2);
        // Second tool call should emit: content_block_stop(0), content_block_start(1)
        let stop = parsed2
            .iter()
            .find(|(t, _)| t == "content_block_stop")
            .expect("second tool call must close the first block");
        assert_eq!(stop.1["index"].as_u64().unwrap(), idx1);
        let start2 = parsed2
            .iter()
            .find(|(t, _)| t == "content_block_start")
            .unwrap();
        let idx2 = start2.1["index"].as_u64().unwrap();
        assert_eq!(idx2, idx1 + 1, "second tool_call must have next index");
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

    // ─── anthropic_stream_to_openai tests ────────────────────────────────────
    //
    // We exercise the state machine directly (parsing + emission) instead of
    // driving a reqwest::Response — this keeps tests fast and deterministic.

    use crate::config::OpenAiApiVersion;

    fn drive_anthropic_events(target: OpenAiApiVersion, events: &[Value]) -> Vec<String> {
        let mut state = AnthropicToOpenAiState::new(target);
        let mut out = Vec::new();
        for ev in events {
            out.extend(state.process_anthropic_event(ev));
        }
        out.extend(state.finalize());
        out
    }

    /// Extract each SSE frame's parsed `data:` JSON (skipping `[DONE]`).
    fn extract_data_json(frames: &[String]) -> Vec<Value> {
        frames
            .iter()
            .flat_map(|frame| frame.lines())
            .filter_map(|line| line.strip_prefix("data: "))
            .filter(|d| d.trim() != "[DONE]")
            .filter_map(|d| serde_json::from_str::<Value>(d).ok())
            .collect()
    }

    // ─── Chat Completions target ─────────────────────────────────────────────

    #[test]
    fn anthropic_to_chat_simple_text_stream() {
        let events = vec![
            json!({"type": "message_start", "message": {"id": "msg_x", "model": "claude-sonnet-4", "usage": {"input_tokens": 5, "output_tokens": 0}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hel"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "lo"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 2}}),
            json!({"type": "message_stop"}),
        ];
        let frames = drive_anthropic_events(OpenAiApiVersion::ChatCompletions, &events);
        let data = extract_data_json(&frames);

        // [role chunk, "Hel" delta, "lo" delta, finish chunk, usage chunk]
        assert_eq!(data.len(), 5);
        assert_eq!(data[0]["object"], "chat.completion.chunk");
        assert_eq!(data[0]["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(data[1]["choices"][0]["delta"]["content"], "Hel");
        assert_eq!(data[2]["choices"][0]["delta"]["content"], "lo");
        assert_eq!(data[3]["choices"][0]["finish_reason"], "stop");
        assert_eq!(data[4]["usage"]["prompt_tokens"], 5);
        assert_eq!(data[4]["usage"]["completion_tokens"], 2);

        // Must end with [DONE]
        assert!(
            frames.iter().any(|f| f.contains("data: [DONE]")),
            "chat stream must terminate with [DONE]"
        );
    }

    #[test]
    fn anthropic_to_chat_max_tokens_becomes_length() {
        let events = vec![
            json!({"type": "message_start", "message": {"id": "msg_x", "model": "m", "usage": {"input_tokens": 1, "output_tokens": 0}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "x"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "max_tokens"}, "usage": {"output_tokens": 1}}),
            json!({"type": "message_stop"}),
        ];
        let data = extract_data_json(&drive_anthropic_events(
            OpenAiApiVersion::ChatCompletions,
            &events,
        ));
        let finish = data
            .iter()
            .find_map(|d| {
                d["choices"]
                    .get(0)
                    .and_then(|c| c.get("finish_reason"))
                    .and_then(|f| f.as_str())
            })
            .unwrap();
        assert_eq!(finish, "length");
    }

    #[test]
    fn anthropic_to_chat_tool_use_stream() {
        let events = vec![
            json!({"type": "message_start", "message": {"id": "msg_x", "model": "m", "usage": {"input_tokens": 3, "output_tokens": 0}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "search", "input": {}}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"q\":"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "\"rust\"}"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 10}}),
            json!({"type": "message_stop"}),
        ];
        let data = extract_data_json(&drive_anthropic_events(
            OpenAiApiVersion::ChatCompletions,
            &events,
        ));
        // role chunk
        assert_eq!(data[0]["choices"][0]["delta"]["role"], "assistant");
        // tool_call start chunk
        let start = &data[1]["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(start["index"], 0);
        assert_eq!(start["id"], "toolu_1");
        assert_eq!(start["type"], "function");
        assert_eq!(start["function"]["name"], "search");
        // argument deltas
        assert_eq!(
            data[2]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            "{\"q\":"
        );
        assert_eq!(
            data[3]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            "\"rust\"}"
        );
        // finish_reason = tool_calls
        let finish = data
            .iter()
            .find_map(|d| {
                d["choices"]
                    .get(0)
                    .and_then(|c| c.get("finish_reason"))
                    .and_then(|f| f.as_str())
            })
            .unwrap();
        assert_eq!(finish, "tool_calls");
    }

    #[test]
    fn anthropic_to_chat_thinking_becomes_reasoning_content() {
        let events = vec![
            json!({"type": "message_start", "message": {"id": "msg_x", "model": "m", "usage": {"input_tokens": 1, "output_tokens": 0}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "hmm"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "ok"}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 2}}),
            json!({"type": "message_stop"}),
        ];
        let data = extract_data_json(&drive_anthropic_events(
            OpenAiApiVersion::ChatCompletions,
            &events,
        ));
        assert!(
            data.iter()
                .any(|d| d["choices"][0]["delta"]["reasoning_content"] == "hmm"),
            "expected a reasoning_content delta"
        );
        assert!(
            data.iter()
                .any(|d| d["choices"][0]["delta"]["content"] == "ok")
        );
    }

    // ─── Responses API target ────────────────────────────────────────────────

    #[test]
    fn anthropic_to_responses_emits_created_and_completed() {
        let events = vec![
            json!({"type": "message_start", "message": {"id": "msg_x", "model": "m", "usage": {"input_tokens": 4, "output_tokens": 0}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "hi"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 1}}),
            json!({"type": "message_stop"}),
        ];
        let frames = drive_anthropic_events(OpenAiApiVersion::Responses, &events);

        let has_event = |name: &str| {
            frames
                .iter()
                .any(|f| f.starts_with(&format!("event: {name}\n")))
        };
        assert!(has_event("response.created"));
        assert!(has_event("response.in_progress"));
        assert!(has_event("response.output_item.added"));
        assert!(has_event("response.output_text.delta"));
        assert!(has_event("response.output_item.done"));
        assert!(has_event("response.completed"));

        // Final event carries usage + model
        let last = frames
            .iter()
            .rev()
            .find(|f| f.starts_with("event: response.completed\n"))
            .unwrap();
        let data_line = last.lines().find_map(|l| l.strip_prefix("data: ")).unwrap();
        let parsed: Value = serde_json::from_str(data_line).unwrap();
        assert_eq!(parsed["response"]["status"], "completed");
        assert_eq!(parsed["response"]["usage"]["input_tokens"], 4);
        assert_eq!(parsed["response"]["usage"]["output_tokens"], 1);
    }

    #[test]
    fn anthropic_to_responses_tool_use_emits_function_call_events() {
        let events = vec![
            json!({"type": "message_start", "message": {"id": "msg_x", "model": "m", "usage": {"input_tokens": 2, "output_tokens": 0}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "search", "input": {}}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"q\":\"rust\"}"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 5}}),
            json!({"type": "message_stop"}),
        ];
        let frames = drive_anthropic_events(OpenAiApiVersion::Responses, &events);

        let added = frames
            .iter()
            .find(|f| f.starts_with("event: response.output_item.added\n"))
            .and_then(|f| f.lines().find_map(|l| l.strip_prefix("data: ")))
            .and_then(|d| serde_json::from_str::<Value>(d).ok())
            .unwrap();
        assert_eq!(added["item"]["type"], "function_call");
        assert_eq!(added["item"]["call_id"], "toolu_1");
        assert_eq!(added["item"]["name"], "search");

        assert!(
            frames
                .iter()
                .any(|f| f.starts_with("event: response.function_call_arguments.delta\n"))
        );
    }

    #[test]
    fn anthropic_to_responses_max_tokens_sets_incomplete() {
        let events = vec![
            json!({"type": "message_start", "message": {"id": "msg_x", "model": "m", "usage": {"input_tokens": 1, "output_tokens": 0}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "x"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "max_tokens"}, "usage": {"output_tokens": 1}}),
            json!({"type": "message_stop"}),
        ];
        let frames = drive_anthropic_events(OpenAiApiVersion::Responses, &events);
        let completed = frames
            .iter()
            .rev()
            .find(|f| f.starts_with("event: response.completed\n"))
            .and_then(|f| f.lines().find_map(|l| l.strip_prefix("data: ")))
            .and_then(|d| serde_json::from_str::<Value>(d).ok())
            .unwrap();
        assert_eq!(completed["response"]["status"], "incomplete");
    }
}
