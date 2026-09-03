//! Conversions between the Anthropic canonical form and Google Gemini's
//! native Interactions API (`POST /v1beta/interactions`).
//!
//! The proxy normalises every client request to the Anthropic canonical form
//! first (see `crate::proxy::handler`); this module then translates that form
//! to Gemini's stateless `input`-steps wire format on the way up, and Gemini
//! interaction/SSE payloads back to the canonical form on the way down.
//!
//! Gemini specifics encoded here (per the public Interactions API docs):
//! - stateless mode (`store: false`, full history resent every call), because
//!   a proxy cannot assume the client keeps `previous_interaction_id`;
//! - steps: `user_input`, `model_output`, `thought`, `function_call`,
//!   `function_result`;
//! - streaming uses the same endpoint with `?alt=sse` and `"stream": true`,
//!   emitting `interaction.created` / `step.start` / `step.delta` /
//!   `step.stop` / `interaction.completed` SSE events;
//! - auth is the raw API key in an `x-goog-api-key` header (handled in
//!   `Provider::auth_header`).

use serde_json::{Value, json};
use uuid::Uuid;

use super::request::extract_system_text;
use super::stream::sse_event;
use crate::config::Provider;
use crate::error::{AppError, Result};
/// Anthropic tool ids carry no name, but Gemini `function_result` steps need
/// the called function's name to pair with a `function_call`. The pairing is
/// recoverable from the conversation itself: every `tool_use` block Claude
/// echoes back sits in an earlier assistant message of the same request, so a
/// single scan of `messages` builds the id -> name map.
type ToolIdMap = std::collections::HashMap<String, String>;

fn model_id(req: &Value) -> &str {
    req.get("model").and_then(|m| m.as_str()).unwrap_or("")
}



/// Content blocks of an Anthropic message (string content becomes one text).
fn message_blocks(msg: &Value) -> Vec<Value> {
    match msg.get("content") {
        Some(Value::String(s)) => vec![json!({"type": "text", "text": s})],
        Some(Value::Array(blocks)) => blocks.clone(),
        _ => vec![],
    }
}

/// Build a Gemini `model_output` step carrying `parts` (text/image content).
fn model_output_step(parts: &[Value]) -> Value {
    json!({"type": "model_output", "content": parts})
}

fn user_input_step(parts: &[Value]) -> Value {
    json!({"type": "user_input", "content": parts})
}

/// Convert an Anthropic-canonical request body (model already resolved via
/// routes/mappings where applicable) into a Gemini Interactions request.
///
/// The request may be either the original client JSON or a model-rewritten
/// copy — the provider's routes/model_map are applied here exactly once,
/// mirroring `transform::to_openai`'s responsibility for OpenAI providers.
pub fn anthropic_to_gemini_request(req: &Value, provider: &Provider) -> Result<Value> {
    // Blocks of one kind gather until a different-kind block or the end of
    // the message forces them out as a single step.
    fn flush_parts(
        parts: &mut Vec<Value>,
        out: &mut Vec<Value>,
        build: impl FnOnce(&[Value]) -> Value,
    ) {
        if parts.is_empty() {
            return;
        }
        out.push(build(parts));
        parts.clear();
    }

    let (model, _) = provider.resolve_model(model_id(req));
    let system_text = extract_system_text(req);

    let mut input: Vec<Value> = Vec::new();
    let mut id_to_name: ToolIdMap = ToolIdMap::new();

    if let Some(messages) = req.get("messages").and_then(|m| m.as_array()) {
        for msg in messages {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let blocks = message_blocks(msg);
            if role == "assistant" {
                let mut text_parts: Vec<Value> = Vec::new();
                // Gemini only accepts a replayed `function_call` when the
                // thought step that preceded it — carrying its real signature
                // — is replayed too. Without one the call makes the whole
                // request 400, so the tool round degrades instead: the call
                // is dropped and only its `function_result` goes upstream.
                let mut had_signed_thought = false;
                for block in blocks {
                    match block.get("type").and_then(|t| t.as_str()) {
                        Some("text") => {
                            if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                                text_parts.push(json!({"type": "text", "text": t}));
                            }
                        }
                        Some("tool_use") => {
                            flush_parts(&mut text_parts, &mut input, model_output_step);
                            let id = block
                                .get("id")
                                .and_then(|i| i.as_str())
                                .unwrap_or_default()
                                .to_string();
                            let name = block
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or_default()
                                .to_string();
                            // The name pairing is still recorded so the
                            // client's `tool_result` can become a
                            // `function_result` further down.
                            if !id.is_empty() && !name.is_empty() {
                                id_to_name.insert(id.clone(), name.clone());
                            }
                            if !had_signed_thought {
                                // Signature-less history (e.g. an OpenAI-format
                                // client): replaying the call would 400; the
                                // later `function_result` still delivers the
                                // result to the model (verified live).
                                tracing::warn!(
                                    "tool round replayed without a signed thought; \
                                     function_call '{name}' omitted"
                                );
                                continue;
                            }
                            let arguments = block
                                .get("input")
                                .cloned()
                                .unwrap_or_else(|| json!({}));
                            input.push(json!({
                                "type": "function_call",
                                "id": id,
                                "name": name,
                                "arguments": arguments,
                            }));
                        }
                        // Claude's thinking blocks in history replay as
                        // Gemini thought steps. The signature is mandatory:
                        // Gemini validates it cryptographically, so a block
                        // without one (e.g. from an interrupted stream) is
                        // dropped rather than sent as an invalid step.
                        Some("thinking") => {
                            flush_parts(&mut text_parts, &mut input, model_output_step);
                            if let Some(sig) = block
                                .get("signature")
                                .and_then(|s| s.as_str())
                                .filter(|s| !s.is_empty())
                            {
                                had_signed_thought = true;
                                input.push(json!({"type": "thought", "signature": sig}));
                            } else {
                                tracing::warn!("dropped signature-less thinking block from Gemini history");
                            }
                        }
                        _ => {}
                    }
                }
                flush_parts(&mut text_parts, &mut input, model_output_step);
            } else {
                // User message: plain content and images gather into one
                // `user_input` step; `tool_result` blocks become separate
                // `function_result` steps in content order.
                let mut user_parts: Vec<Value> = Vec::new();
                for block in blocks {
                    match block.get("type").and_then(|t| t.as_str()) {
                        Some("text") => {
                            if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                                user_parts.push(json!({"type": "text", "text": t}));
                            }
                        }
                        Some("image") => {
                            if let Some(img) = anthropic_image_to_gemini(&block) {
                                user_parts.push(img);
                            }
                        }
                        Some("tool_result") => {
                            flush_parts(&mut user_parts, &mut input, user_input_step);
                            input.push(tool_result_to_function_result(&block, &id_to_name)?);
                        }
                        _ => {}
                    }
                }
                flush_parts(&mut user_parts, &mut input, user_input_step);
            }
        }
    }

    if input.is_empty() {
        // Anthropic requests always carry at least one user message; guard
        // anyway so an empty history never produces an empty `input` array.
        input.push(user_input_step(&[json!({"type": "text", "text": ""})]));
    }

    let mut out = json!({
        "model": model,
        "input": input,
        "store": false,
    });
    if !system_text.is_empty() {
        out["system_instruction"] = json!(system_text);
    }

    // Tools: Anthropic `input_schema` is already a JSON Schema object, which
    // is exactly what Gemini's `parameters` field expects.
    if let Some(tools) = req.get("tools").and_then(|t| t.as_array()) {
        let mut converted = Vec::new();
        for tool in tools {
            let name = tool.get("name").and_then(|n| n.as_str()).unwrap_or("");
            if name.is_empty() {
                continue;
            }
            let mut fn_tool = json!({"type": "function", "name": name});
            if let Some(desc) = tool.get("description").and_then(|d| d.as_str()) {
                fn_tool["description"] = json!(desc);
            }
            let params = tool
                .get("input_schema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            fn_tool["parameters"] = params;
            converted.push(fn_tool);
        }
        if !converted.is_empty() {
            out["tools"] = json!(converted);
        }
    }

    // Sampling config: pass temperature through; map Anthropic thinking to
    // Gemini's effort level + readable summaries so reasoning can be relayed.
    let mut gen_cfg = serde_json::Map::new();
    if let Some(t) = req.get("temperature") {
        gen_cfg.insert("temperature".to_string(), t.clone());
    }
    let thinking_requested = req
        .get("thinking")
        .and_then(|th| th.get("type"))
        .and_then(|t| t.as_str())
        .is_some_and(|t| matches!(t, "enabled" | "adaptive"));
    if thinking_requested {
        gen_cfg.insert("thinking_level".to_string(), json!("high"));
        gen_cfg.insert("thinking_summaries".to_string(), json!("auto"));
    }
    if !gen_cfg.is_empty() {
        out["generation_config"] = Value::Object(gen_cfg);
    }

    let is_stream = req.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    if is_stream {
        out["stream"] = json!(true);
    }

    Ok(out)
}

/// Anthropic image block → Gemini `image` content item.
fn anthropic_image_to_gemini(block: &Value) -> Option<Value> {
    let source = block.get("source")?;
    let data = source.get("data").and_then(|d| d.as_str())?;
    let mime = source
        .get("media_type")
        .and_then(|m| m.as_str())
        .unwrap_or("image/png");
    Some(json!({"type": "image", "data": data, "mime_type": mime}))
}

/// Anthropic `tool_result` block → Gemini `function_result` step. Gemini
/// requires the function name, recovered from the id Claude echoes back.
fn tool_result_to_function_result(block: &Value, id_to_name: &ToolIdMap) -> Result<Value> {
    let call_id = block
        .get("tool_use_id")
        .and_then(|i| i.as_str())
        .unwrap_or_default()
        .to_string();
    let name = id_to_name
        .get(&call_id)
        .cloned()
        .unwrap_or_default();
    if name.is_empty() {
        // Without a name the pair cannot be reconstructed; the upstream would
        // reject the step. Log and keep the call id so the failure is debuggable.
        tracing::warn!("tool_result for unknown tool_use_id '{call_id}'; dropped from Gemini request");
        return Err(AppError::Transform(format!(
            "tool_result references unknown tool_use_id '{call_id}'"
        )));
    }
    let text = tool_result_text(block.get("content"))?;
    Ok(json!({
        "type": "function_result",
        "call_id": call_id,
        "name": name,
        "result": [{"type": "text", "text": text}],
    }))
}

/// Flatten Anthropic `tool_result.content` (string or text/image blocks) into
/// a single text payload, mirroring how OpenAI tool messages carry results.
fn tool_result_text(content: Option<&Value>) -> Result<String> {
    match content {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(Value::Array(blocks)) => {
            let mut parts = Vec::new();
            for b in blocks {
                if b.get("type").and_then(|t| t.as_str()) == Some("text")
                    && let Some(t) = b.get("text").and_then(|t| t.as_str())
                {
                    parts.push(t.to_string());
                }
            }
            Ok(parts.join("\n"))
        }
        _ => Ok(serde_json::to_string(content.expect("guarded above")).unwrap_or_default()),
    }
}

// ─── Non-streaming response ────────────────────────────────────────────────

/// Convert a non-streaming Gemini Interactions response (an `Interaction`
/// resource with `steps`) into the Anthropic canonical response form.
pub fn gemini_to_anthropic_response(resp: &Value) -> Result<Value> {
    let steps = resp
        .get("steps")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default();

    let mut content: Vec<Value> = Vec::new();
    let mut last_step_was_tool = false;
    for step in &steps {
        match step.get("type").and_then(|t| t.as_str()) {
            Some("model_output") => {
                if let Some(items) = step.get("content").and_then(|c| c.as_array()) {
                    for item in items {
                        match item.get("type").and_then(|t| t.as_str()) {
                            Some("text") => {
                                let text = item.get("text").and_then(|t| t.as_str()).unwrap_or("");
                                content.push(json!({"type": "text", "text": text}));
                            }
                            Some("image") => {
                                content.push(gemini_image_to_anthropic(item));
                            }
                            _ => {}
                        }
                    }
                }
                last_step_was_tool = false;
            }
            Some("function_call") => {
                let id = step.get("id").and_then(|i| i.as_str()).unwrap_or("");
                let name = step.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let input = step
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                content.push(json!({
                    "type": "tool_use",
                    "id": if id.is_empty() { synthesized_tool_id() } else { id.to_string() },
                    "name": name,
                    "input": input,
                }));
                last_step_was_tool = true;
            }
            // Thought steps carry a signature but no readable text unless
            // summaries were requested. They are still emitted so the client
            // can echo the (signature-carrying) thinking block back verbatim
            // on the next turn — Gemini rejects a `function_call` replay that
            // lacks its matching thought step.
            Some("thought") => {
                let signature = step.get("signature").and_then(|s| s.as_str()).unwrap_or("");
                content.push(json!({
                    "type": "thinking",
                    "thinking": "",
                    "signature": signature,
                }));
            }
            _ => {}
        }
    }
    if content.is_empty() {
        content.push(json!({"type": "text", "text": ""}));
    }

    let stop_reason = if last_step_was_tool { "tool_use" } else { "end_turn" };
    let (input_tokens, output_tokens) = gemini_usage(resp);
    let model = resp
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("gemini")
        .to_string();

    Ok(json!({
        "id": format!("msg_{}", Uuid::new_v4()),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
        }
    }))
}

fn gemini_image_to_anthropic(item: &Value) -> Value {
    let mime = item.get("mime_type").and_then(|m| m.as_str()).unwrap_or("image/png");
    let data = item.get("data").and_then(|d| d.as_str()).unwrap_or("");
    json!({
        "type": "image",
        "source": {"type": "base64", "media_type": mime, "data": data},
    })
}

/// Extract input/output token counts from an interaction's `usage` object.
///
/// Gemini splits generated tokens between `total_output_tokens` and
/// `total_thought_tokens`; Anthropic's `output_tokens` counts both (verified
/// live: `input 8 + output 13 + thought 74 = total 95`), so they are summed.
fn gemini_usage(resp: &Value) -> (u64, u64) {
    let usage = resp.get("usage").unwrap_or(&Value::Null);
    let get = |k: &str| usage.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
    let input = get("total_input_tokens");
    let output = get("total_output_tokens") + get("total_thought_tokens");
    if input != 0 || output != 0 {
        (input, output)
    } else {
        // Fallback when only the aggregate is reported.
        let total = get("total_tokens");
        if total != 0 && output == 0 {
            // Cannot separate the halves without input tokens; report the
            // aggregate on the input side so totals still add up.
            (total, 0)
        } else {
            (0, 0)
        }
    }
}

fn synthesized_tool_id() -> String {
    format!("toolu_{}", Uuid::new_v4())
}

// ─── Streaming response ────────────────────────────────────────────────────

use futures::Stream;

/// Convert a Gemini Interactions SSE response stream into Anthropic-canonical
/// SSE events, ready for `anthropic_stream_to_openai` or direct delivery to an
/// Anthropic client.
///
/// `summaries` requests Gemini's readable thinking summaries (thought_summary
/// deltas). Thought steps themselves — including their signatures — are
/// always forwarded as Anthropic thinking blocks so the client can echo them
/// back verbatim on the next tool-calling turn.
pub fn gemini_stream_to_anthropic(
    response: reqwest::Response,
    summaries: bool,
) -> impl Stream<Item = std::result::Result<bytes::Bytes, std::io::Error>> {
    use futures::StreamExt;
    let mapped = response.bytes_stream().map(|r| r.map_err(std::io::Error::other));
    translate_gemini_sse(mapped, summaries)
}

/// SSE-line → Anthropic-event translation loop, split out of
/// [`gemini_stream_to_anthropic`] so tests can drive it with a byte stream.
fn translate_gemini_sse<S>(
    byte_stream: S,
    summaries: bool,
) -> impl Stream<Item = std::result::Result<bytes::Bytes, std::io::Error>>
where
    S: Stream<Item = std::result::Result<bytes::Bytes, std::io::Error>> + Send + 'static,
{
    let stream = async_stream::stream! {
        let mut state = GeminiStreamState::new(summaries);
        let mut buffer = String::new();
        let mut current_event = String::new();

        let mut byte_stream = Box::pin(byte_stream);
        use futures::StreamExt;

        const BUFFER_MAX: usize = 1024 * 1024; // 1 MB safety cap

        while let Some(chunk_result) = byte_stream.next().await {
            let chunk = match chunk_result {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!("Stream read error: {e}");
                    for ev in state.error_events("Stream read error") {
                        yield Ok(bytes::Bytes::from(ev));
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
                if let Some(event) = line.strip_prefix("event: ") {
                    current_event = event.trim().to_string();
                    continue;
                }
                let Some(data) = line.strip_prefix("data: ") else {
                    continue;
                };
                if data.trim() == "[DONE]" {
                    continue;
                }
                let Ok(event_json) = serde_json::from_str::<Value>(data) else {
                    tracing::warn!("Failed to parse Gemini SSE chunk: {data}");
                    continue;
                };
                let name = event_json
                    .get("event_type")
                    .and_then(|t| t.as_str())
                    .unwrap_or(&current_event)
                    .to_string();
                if name == "error" {
                    let message = event_json["error"]["message"]
                        .as_str()
                        .unwrap_or("Gemini stream error")
                        .to_string();
                    for ev in state.error_events(&message) {
                        yield Ok(bytes::Bytes::from(ev));
                    }
                    break;
                }
                let events = state.process_event(&name, &event_json);
                for ev in events {
                    yield Ok(bytes::Bytes::from(ev));
                }
            }
        }

        for ev in state.finalize() {
            yield Ok(bytes::Bytes::from(ev));
        }
    };

    stream
}

/// Anthropic content block kinds a Gemini step maps to.
#[derive(Clone, Copy, PartialEq)]
enum BlockKind {
    Text,
    Thinking,
    ToolUse,
}

struct OpenBlock {
    kind: BlockKind,
    index: usize,
}

struct GeminiStreamState {
    /// Whether Gemini's readable thinking summaries (thought_summary deltas)
    /// are surfaced. Thought steps themselves — including their signatures —
    /// are always forwarded as Anthropic thinking blocks so the client can
    /// echo them back verbatim on the next tool-calling turn.
    summaries: bool,
    message_id: String,
    model: String,
    input_tokens: u64,
    output_tokens: u64,
    content_index: usize,
    started: bool,
    finalized: bool,
    stop_reason: Option<String>,
    /// `Some(_)` while a Gemini step is open (between step.start/step.stop).
    /// Text blocks open lazily on the first text delta; thought/tool blocks
    /// open on `step.start` because they carry metadata.
    open: Option<OpenBlock>,
    /// Signature received for the current thought step; delivered as an
    /// Anthropic `signature_delta` right before the block closes.
    pending_signature: Option<String>,
    /// The most recent Gemini step was a `function_call` → the interaction
    /// ends with `stop_reason = tool_use`.
    last_step_was_tool: bool,
}

impl GeminiStreamState {
    fn new(summaries: bool) -> Self {
        Self {
            summaries,
            message_id: format!("msg_{}", Uuid::new_v4()),
            model: "gemini".to_string(),
            input_tokens: 0,
            output_tokens: 0,
            content_index: 0,
            started: false,
            finalized: false,
            stop_reason: None,
            open: None,
            pending_signature: None,
            last_step_was_tool: false,
        }
    }

    fn process_event(&mut self, name: &str, data: &Value) -> Vec<String> {
        match name {
            "interaction.created" => {
                if let Some(model) = data["interaction"]["model"].as_str()
                    && !model.is_empty()
                {
                    self.model = model.to_string();
                }
                self.message_start()
            }
            "step.start" => self.on_step_start(data),
            "step.delta" => self.on_step_delta(data),
            "step.stop" => self.on_step_stop(),
            "interaction.completed" | "interaction.failed" => {
                let interaction = data.get("interaction").unwrap_or(data);
                if let Some(model) = interaction.get("model").and_then(|m| m.as_str()) {
                    self.model = model.to_string();
                }
                self.usage_from(interaction);
                self.stop_reason = Some(self.final_stop_reason().to_string());
                self.finalize()
            }
            _ => Vec::new(),
        }
    }

    fn message_start(&mut self) -> Vec<String> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        vec![sse_event(
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
        )]
    }

    fn on_step_start(&mut self, data: &Value) -> Vec<String> {
        let Some(step) = data.get("step") else { return Vec::new() };
        let Some(step_type) = step.get("type").and_then(|t| t.as_str()) else {
            return Vec::new();
        };
        let mut events = self.message_start();

        match step_type {
            // Thought steps always become Anthropic thinking blocks: the
            // signature must reach the client so the next tool-calling turn
            // can be replayed upstream (Gemini validates it cryptographically).
            "thought" => {
                events.extend(self.open_block(BlockKind::Thinking, |index| {
                    sse_event(
                        "content_block_start",
                        &json!({
                            "type": "content_block_start",
                            "index": index,
                            "content_block": {"type": "thinking", "thinking": "", "signature": ""},
                        }),
                    )
                }));
            }
            "model_output" => {
                // Text blocks open lazily on the first text delta so a step
                // that only carries an image never emits an empty block.
                self.last_step_was_tool = false;
            }
            "function_call" => {
                self.last_step_was_tool = true;
                let id = step.get("id").and_then(|i| i.as_str()).unwrap_or("");
                let name = step.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let id = if id.is_empty() { synthesized_tool_id() } else { id.to_string() };
                events.extend(self.open_block(BlockKind::ToolUse, move |index| {
                    sse_event(
                        "content_block_start",
                        &json!({
                            "type": "content_block_start",
                            "index": index,
                            "content_block": {
                                "type": "tool_use",
                                "id": id,
                                "name": name,
                                "input": {},
                            }
                        }),
                    )
                }));
                // The full arguments object may ride along on step.start
                // instead of streaming as arguments_delta deltas.
                if let Some(args) = step.get("arguments")
                    && let Some(args_str) = compact_json(args)
                {
                    events.extend(self.input_json_delta(&args_str));
                }
            }
            _ => {}
        }
        events
    }

    fn on_step_delta(&mut self, data: &Value) -> Vec<String> {
        let mut events = self.message_start();
        let Some(delta) = data.get("delta") else { return events };
        match delta.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(text) = delta.get("text").and_then(|t| t.as_str())
                    && !text.is_empty()
                {
                    events.extend(self.ensure_text_block());
                    if let Some(index) = self.open.as_ref().map(|o| o.index) {
                        events.push(sse_event(
                            "content_block_delta",
                            &json!({
                                "type": "content_block_delta",
                                "index": index,
                                "delta": {"type": "text_delta", "text": text},
                            }),
                        ));
                    }
                }
            }
            Some("thought_summary") if self.summaries => {
                if let Some(text) = delta["content"]["text"].as_str()
                    && !text.is_empty()
                    && self.open.as_ref().is_some_and(|o| o.kind == BlockKind::Thinking)
                {
                    let index = self.open.as_ref().unwrap().index;
                    events.push(sse_event(
                        "content_block_delta",
                        &json!({
                            "type": "content_block_delta",
                            "index": index,
                            "delta": {"type": "thinking_delta", "thinking": text},
                        }),
                    ));
                }
            }
            Some("thought_signature") => {
                if let Some(sig) = delta.get("signature").and_then(|s| s.as_str()) {
                    self.pending_signature = Some(sig.to_string());
                }
            }
            Some("arguments_delta") => {
                if let Some(args) = delta.get("arguments").and_then(|a| a.as_str())
                    && !args.is_empty()
                    && self.open.as_ref().is_some_and(|o| o.kind == BlockKind::ToolUse)
                {
                    events.extend(self.input_json_delta(args));
                }
            }
            _ => {}
        }
        events
    }

    /// Emit one `partial_json` fragment for an open tool_use block, matching
    /// how the OpenAI path forwards its streamed `arguments` payloads.
    fn input_json_delta(&self, args: &str) -> Vec<String> {
        let Some(index) = self.open.as_ref().map(|o| o.index) else {
            return Vec::new();
        };
        vec![sse_event(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "input_json_delta", "partial_json": args},
            }),
        )]
    }

    /// Lazily open a text block, then return its content index.
    fn ensure_text_block(&mut self) -> Vec<String> {
        let mut events = Vec::new();
        if self.open.is_none() {
            events = self.open_block(BlockKind::Text, |index| {
                sse_event(
                    "content_block_start",
                    &json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": {"type": "text", "text": ""},
                    }),
                )
            });
        }
        events
    }

    fn open_block(&mut self, kind: BlockKind, build: impl FnOnce(usize) -> String) -> Vec<String> {
        let mut events = self.close_open_block();
        let index = self.content_index;
        self.open = Some(OpenBlock { kind, index });
        events.push(build(index));
        events
    }

    fn close_open_block(&mut self) -> Vec<String> {
        let mut events = Vec::new();
        if let Some(block) = self.open.take() {
            // Anthropic delivers a thinking block's signature as its final
            // content delta, immediately before the block stops.
            if block.kind == BlockKind::Thinking
                && let Some(sig) = self.pending_signature.take()
            {
                events.push(sse_event(
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": block.index,
                        "delta": {"type": "signature_delta", "signature": sig},
                    }),
                ));
            }
            events.push(sse_event(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": block.index}),
            ));
            self.content_index = block.index + 1;
        }
        events
    }

    fn on_step_stop(&mut self) -> Vec<String> {
        self.close_open_block()
    }

    fn usage_from(&mut self, interaction: &Value) {
        let Some(usage) = interaction.get("usage") else { return };
        if let Some(v) = usage.get("total_input_tokens").and_then(|v| v.as_u64()) {
            self.input_tokens = v;
        }
        if let Some(v) = usage.get("total_output_tokens").and_then(|v| v.as_u64()) {
            // Gemini splits generated tokens between `total_output_tokens`
            // and `total_thought_tokens`; Anthropic's `output_tokens` counts
            // both, mirroring `gemini_usage` for buffered responses.
            let thoughts = usage
                .get("total_thought_tokens")
                .and_then(|t| t.as_u64())
                .unwrap_or(0);
            self.output_tokens = v + thoughts;
        }
    }

    fn final_stop_reason(&self) -> &'static str {
        match self.stop_reason.as_deref() {
            Some("tool_use") => "tool_use",
            _ => {
                if self.last_step_was_tool {
                    "tool_use"
                } else {
                    "end_turn"
                }
            }
        }
    }

    fn finalize(&mut self) -> Vec<String> {
        if self.finalized {
            return Vec::new();
        }
        self.finalized = true;

        let mut events = Vec::new();
        events.extend(self.message_start());
        events.extend(self.close_open_block());

        events.push(sse_event(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": self.final_stop_reason(),
                    "stop_sequence": null,
                },
                "usage": {
                    "input_tokens": self.input_tokens,
                    "output_tokens": self.output_tokens,
                }
            }),
        ));
        events.push(sse_event("message_stop", &json!({"type": "message_stop"})));
        events
    }

    fn error_events(&mut self, message: &str) -> Vec<String> {
        if self.finalized {
            return Vec::new();
        }
        let mut events = Vec::new();
        events.extend(self.close_open_block());
        events.push(sse_event(
            "error",
            &json!({"type": "error", "error": {"type": "api_error", "message": message}}),
        ));
        events
    }
}


fn compact_json(v: &Value) -> Option<String> {
    if v.is_object() && v.as_object().is_some_and(|o| !o.is_empty()) {
        serde_json::to_string(v).ok()
    } else {
        None
    }
}


#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::config::{ApiFormat, Provider};

    fn provider(format: ApiFormat) -> Provider {
        Provider {
            id: "test".into(),
            base_url: "https://generativelanguage.googleapis.com".into(),
            api_key: "test-key".into(),
            api_format: format,
            model_map: Default::default(),
            routes: vec![],
            enabled: true,
            fallback: true,
            api_version: None,
            inject_thinking_history: true,
            strict_thinking_history: false,
            quota_command: None,
            port: None,
            test_model: None,
            max_tokens_cap: None,
        }
    }

    fn g_provider() -> Provider {
        provider(ApiFormat::Gemini)
    }

    /// Parse `event: X\ndata: {...}` strings into (type, payload) pairs.
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

    // ─── request conversion ─────────────────────────────────────────────────

    #[test]
    fn request_converts_basic_messages_system_and_tools() {
        let req = json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "stream": true,
            "system": "You are a helpful assistant.",
            "temperature": 0.3,
            "messages": [
                {"role": "user", "content": "Hi there"},
                {"role": "assistant", "content": [{"type": "text", "text": "Hello!"}]},
                {"role": "user", "content": [{"type": "text", "text": "What time is it?"}]}
            ],
            "tools": [{
                "name": "get_time",
                "description": "Get the current time",
                "input_schema": {"type": "object", "properties": {"tz": {"type": "string"}}}
            }]
        });

        let out = anthropic_to_gemini_request(&req, &g_provider()).unwrap();
        assert_eq!(out["model"], "claude-sonnet-4-20250514");
        assert_eq!(out["store"], false);
        assert_eq!(out["stream"], true);
        assert_eq!(out["system_instruction"], "You are a helpful assistant.");
        assert_eq!(out["generation_config"]["temperature"], 0.3);
        assert!(out["generation_config"].get("thinking_level").is_none());

        let input = out["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "user_input");
        assert_eq!(input[0]["content"][0]["text"], "Hi there");
        assert_eq!(input[1]["type"], "model_output");
        assert_eq!(input[1]["content"][0]["text"], "Hello!");
        assert_eq!(input[2]["type"], "user_input");

        let tool = &out["tools"][0];
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["name"], "get_time");
        assert_eq!(tool["parameters"]["type"], "object");
    }

    #[test]
    fn request_reconstructs_function_result_names_from_history() {
        let req = json!({
            "model": "claude-sonnet-4-20250514",
            "messages": [
                {"role": "user", "content": "What's the weather in London?"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "Let me check."},
                    // The signed thinking block is what makes the replay of
                    // the tool call below acceptable to Gemini.
                    {"type": "thinking", "thinking": "", "signature": "sig-abc"},
                    {"type": "tool_use", "id": "toolu_01ABC", "name": "get_weather", "input": {"city": "London"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_01ABC", "content": "22C sunny"}
                ]}
            ]
        });

        let out = anthropic_to_gemini_request(&req, &g_provider()).unwrap();
        let input = out["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "user_input");
        assert_eq!(input[1]["type"], "model_output");
        let thought = &input[2];
        assert_eq!(thought["type"], "thought");
        assert_eq!(thought["signature"], "sig-abc");
        let call = &input[3];
        assert_eq!(call["type"], "function_call");
        assert_eq!(call["id"], "toolu_01ABC");
        assert_eq!(call["name"], "get_weather");
        assert_eq!(call["arguments"]["city"], "London");
        let result = &input[4];
        assert_eq!(result["type"], "function_result");
        assert_eq!(result["call_id"], "toolu_01ABC");
        // Name recovered from the earlier assistant tool_use block.
        assert_eq!(result["name"], "get_weather");
        assert_eq!(result["result"][0]["text"], "22C sunny");
    }

    #[test]
    fn signatureless_history_downgrades_function_call_to_result_only() {
        // History without a signed thinking block (e.g. an OpenAI-format
        // client) must not replay the function_call — Gemini rejects it with
        // 400 — but the tool result still reaches the model.
        let req = json!({
            "model": "claude-sonnet-4-20250514",
            "messages": [
                {"role": "user", "content": "What's the weather in London?"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_01ABC", "name": "get_weather", "input": {"city": "London"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_01ABC", "content": "22C sunny"}
                ]}
            ]
        });

        let out = anthropic_to_gemini_request(&req, &g_provider()).unwrap();
        let input = out["input"].as_array().unwrap();
        assert!(!input.iter().any(|s| s["type"] == "function_call"));
        assert!(!input.iter().any(|s| s["type"] == "thought"));
        let result = input.iter().find(|s| s["type"] == "function_result").unwrap();
        assert_eq!(result["name"], "get_weather");
        assert_eq!(result["result"][0]["text"], "22C sunny");
    }

    #[test]
    fn request_maps_thinking_to_high_effort() {
        let req = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "think hard"}],
            "thinking": {"type": "enabled", "budget_tokens": 2000}
        });
        let out = anthropic_to_gemini_request(&req, &g_provider()).unwrap();
        assert_eq!(out["generation_config"]["thinking_level"], "high");
        assert_eq!(out["generation_config"]["thinking_summaries"], "auto");
    }

    #[test]
    fn request_unknown_tool_result_is_rejected() {
        let req = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "run it"},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_missing", "content": "x"}
                ]}
            ]
        });
        let err = anthropic_to_gemini_request(&req, &g_provider()).unwrap_err();
        assert!(err.to_string().contains("toolu_missing"));
    }

    // ─── buffered response conversion ───────────────────────────────────────

    #[test]
    fn buffered_response_maps_steps_to_canonical() {
        let resp = json!({
            "id": "v1_Chd",
            "status": "completed",
            "model": "gemini-3.8-flash",
            "usage": {"total_tokens": 40, "total_input_tokens": 10, "total_output_tokens": 30, "total_thought_tokens": 6},
            "steps": [
                {"type": "model_output", "content": [{"type": "text", "text": "It is 22C."}]}
            ]
        });
        let out = gemini_to_anthropic_response(&resp).unwrap();
        assert_eq!(out["type"], "message");
        assert_eq!(out["role"], "assistant");
        assert_eq!(out["model"], "gemini-3.8-flash");
        assert_eq!(out["stop_reason"], "end_turn");
        assert_eq!(out["content"][0]["type"], "text");
        assert_eq!(out["content"][0]["text"], "It is 22C.");
        assert_eq!(out["usage"]["input_tokens"], 10);
        assert_eq!(out["usage"]["output_tokens"], 36);
    }

    #[test]
    fn buffered_response_stop_reason_tool_use_on_trailing_call() {
        let resp = json!({
            "status": "requires_action",
            "model": "gemini-3.8-flash",
            "steps": [
                {"type": "function_call", "id": "call_1", "name": "get_weather", "arguments": {"city": "London"}}
            ]
        });
        let out = gemini_to_anthropic_response(&resp).unwrap();
        assert_eq!(out["stop_reason"], "tool_use");
        let block = &out["content"][0];
        assert_eq!(block["type"], "tool_use");
        assert_eq!(block["id"], "call_1");
        assert_eq!(block["name"], "get_weather");
        assert_eq!(block["input"]["city"], "London");
    }

    #[test]
    fn buffered_response_stop_reason_end_turn_after_tool_loop() {
        let resp = json!({
            "status": "completed",
            "model": "gemini-3.8-flash",
            "steps": [
                {"type": "function_call", "id": "call_1", "name": "f", "arguments": {}},
                {"type": "model_output", "content": [{"type": "text", "text": "done"}]}
            ]
        });
        let out = gemini_to_anthropic_response(&resp).unwrap();
        assert_eq!(out["stop_reason"], "end_turn");
        assert_eq!(out["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn models_list_normalises_gemini_shape() {
        let list = json!({"models": [
            {"name": "models/gemini-3.8-flash", "display_name": "Gemini 3.8 Flash"},
            {"name": "models/gemini-2.5-pro", "display_name": "Gemini 2.5 Pro"}
        ]});
        let out = crate::proxy::transform::gemini_to_anthropic_models(&list);
        let data = out["data"].as_array().unwrap();
        assert_eq!(data[0]["id"], "gemini-3.8-flash");
        assert_eq!(data[0]["display_name"], "Gemini 3.8 Flash");
        assert_eq!(data[1]["id"], "gemini-2.5-pro");
    }

    // ─── streaming conversion ───────────────────────────────────────────────

    #[test]
    fn stream_emits_canonical_events_for_text_and_usage() {
        let mut state = GeminiStreamState::new(false);
        let mut raw = Vec::new();
        raw.extend(state.process_event("interaction.created", &json!({
            "interaction": {"model": "gemini-3.8-flash", "status": "in_progress"},
            "event_type": "interaction.created"
        })));
        raw.extend(state.process_event("step.start", &json!({
            "index": 0, "step": {"type": "model_output"}, "event_type": "step.start"
        })));
        raw.extend(state.process_event("step.delta", &json!({
            "index": 0, "delta": {"type": "text", "text": "Hello "}, "event_type": "step.delta"
        })));
        raw.extend(state.process_event("step.delta", &json!({
            "index": 0, "delta": {"type": "text", "text": "world"}, "event_type": "step.delta"
        })));
        raw.extend(state.process_event("step.stop", &json!({"index": 0, "event_type": "step.stop"})));
        raw.extend(state.process_event("interaction.completed", &json!({
            "interaction": {
                "status": "completed",
                "usage": {"total_tokens": 38, "total_input_tokens": 10, "total_output_tokens": 20, "total_thought_tokens": 8}
            },
            "event_type": "interaction.completed"
        })));

        let events = parse_events(&raw);
        let types: Vec<&str> = events.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(
            types,
            vec![
                "message_start", "content_block_start", "content_block_delta",
                "content_block_delta", "content_block_stop", "message_delta", "message_stop"
            ]
        );
        assert_eq!(events[0].1["message"]["model"], "gemini-3.8-flash");
        assert_eq!(events[2].1["delta"]["text"], "Hello ");
        assert_eq!(events[3].1["delta"]["text"], "world");
        assert_eq!(events[5].1["delta"]["stop_reason"], "end_turn");
        assert_eq!(events[5].1["usage"]["input_tokens"], 10);
        assert_eq!(events[5].1["usage"]["output_tokens"], 28);
    }

    #[test]
    fn stream_maps_function_call_with_streaming_arguments() {
        let mut state = GeminiStreamState::new(false);
        let mut raw = state.process_event("interaction.created", &json!({
            "interaction": {"model": "gemini-3.8-flash", "status": "in_progress"},
            "event_type": "interaction.created"
        }));
        raw.extend(state.process_event("step.start", &json!({
            "index": 0, "step": {"type": "function_call", "id": "call_9", "name": "get_weather", "arguments": {}},
            "event_type": "step.start"
        })));
        raw.extend(state.process_event("step.delta", &json!({
            "index": 0, "delta": {"type": "arguments_delta", "arguments": "{\"city\": \"Paris\"}"},
            "event_type": "step.delta"
        })));
        raw.extend(state.process_event("step.stop", &json!({"index": 0, "event_type": "step.stop"})));
        raw.extend(state.process_event("interaction.completed", &json!({
            "interaction": {"status": "completed", "usage": {"total_output_tokens": 12}},
            "event_type": "interaction.completed"
        })));

        let events = parse_events(&raw);
        let start = events.iter().find(|(t, _)| t == "content_block_start").unwrap();
        assert_eq!(start.1["content_block"]["type"], "tool_use");
        assert_eq!(start.1["content_block"]["id"], "call_9");
        assert_eq!(start.1["content_block"]["name"], "get_weather");
        let delta = events.iter().find(|(t, _)| t == "content_block_delta").unwrap();
        assert_eq!(delta.1["delta"]["type"], "input_json_delta");
        assert_eq!(delta.1["delta"]["partial_json"], "{\"city\": \"Paris\"}");
        let done = events.iter().find(|(t, _)| t == "message_delta").unwrap();
        assert_eq!(done.1["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn stream_surfaces_thought_signature_even_without_summaries() {
        let mut raw = Vec::new();
        let mut state = GeminiStreamState::new(false);
        raw.extend(state.process_event("interaction.created", &json!({
            "interaction": {"model": "gemini-3.8-flash", "status": "in_progress"},
            "event_type": "interaction.created"
        })));
        // Thought step with only a signature — the signature must still reach
        // the client so the next tool-calling turn can be replayed upstream.
        raw.extend(state.process_event("step.start", &json!({
            "index": 0, "step": {"type": "thought"}, "event_type": "step.start"
        })));
        raw.extend(state.process_event("step.delta", &json!({
            "index": 0, "delta": {"type": "thought_signature", "signature": "sig-1"},
            "event_type": "step.delta"
        })));
        raw.extend(state.process_event("step.stop", &json!({"index": 0, "event_type": "step.stop"})));
        raw.extend(state.process_event("interaction.completed", &json!({
            "interaction": {"status": "completed", "usage": {"total_output_tokens": 3}},
            "event_type": "interaction.completed"
        })));

        let events = parse_events(&raw);
        let start = events.iter().find(|(t, _)| t == "content_block_start").unwrap();
        assert_eq!(start.1["content_block"]["type"], "thinking");
        // No summaries were requested, so no readable thinking deltas…
        let text_deltas: Vec<&Value> = events
            .iter()
            .filter(|(t, _)| t == "content_block_delta")
            .map(|(_, d)| d)
            .collect();
        assert_eq!(text_deltas.len(), 1);
        assert_eq!(text_deltas[0]["delta"]["type"], "signature_delta");
        assert_eq!(text_deltas[0]["delta"]["signature"], "sig-1");
    }

    #[test]
    fn stream_forwards_thought_blocks_with_signature_when_requested() {
        let mut raw = Vec::new();
        let mut state = GeminiStreamState::new(true);
        raw.extend(state.process_event("interaction.created", &json!({
            "interaction": {"model": "gemini-3.8-flash", "status": "in_progress"},
            "event_type": "interaction.created"
        })));
        raw.extend(state.process_event("step.start", &json!({
            "index": 0, "step": {"type": "thought"}, "event_type": "step.start"
        })));
        raw.extend(state.process_event("step.delta", &json!({
            "index": 0, "delta": {"type": "thought_summary", "content": {"type": "text", "text": "I need the GCD..."}},
            "event_type": "step.delta"
        })));
        raw.extend(state.process_event("step.delta", &json!({
            "index": 0, "delta": {"type": "thought_signature", "signature": "sig-1"},
            "event_type": "step.delta"
        })));
        raw.extend(state.process_event("step.stop", &json!({"index": 0, "event_type": "step.stop"})));
        raw.extend(state.process_event("interaction.completed", &json!({
            "interaction": {"status": "completed", "usage": {"total_output_tokens": 5}},
            "event_type": "interaction.completed"
        })));

        let events = parse_events(&raw);
        let start = events.iter().find(|(t, _)| t == "content_block_start").unwrap();
        assert_eq!(start.1["content_block"]["type"], "thinking");
        let deltas: Vec<&Value> = events
            .iter()
            .filter(|(t, _)| t == "content_block_delta")
            .map(|(_, d)| d)
            .collect();
        assert_eq!(deltas[0]["delta"]["type"], "thinking_delta");
        assert_eq!(deltas[0]["delta"]["thinking"], "I need the GCD...");
        // Signature is delivered right before the block stops.
        assert_eq!(deltas[1]["delta"]["type"], "signature_delta");
        assert_eq!(deltas[1]["delta"]["signature"], "sig-1");
    }

    #[tokio::test]
    async fn translate_gemini_sse_end_to_end_produces_canonical_events() {
        use bytes::Bytes;
        use futures::{StreamExt, stream};

        // A faithful copy of Gemini's live SSE wire format: event: lines plus
        // data: payloads carrying `event_type`, ending in `done`/[DONE].
        let sse = concat!(
            "event: interaction.created\n",
            "data: {\"interaction\":{\"model\":\"gemini-3.8-flash\",\"status\":\"in_progress\"},\"event_type\":\"interaction.created\"}\n\n",
            "event: step.start\n",
            "data: {\"index\":0,\"step\":{\"type\":\"thought\"},\"event_type\":\"step.start\"}\n\n",
            "event: step.delta\n",
            "data: {\"index\":0,\"delta\":{\"signature\":\"sig-1\",\"type\":\"thought_signature\"},\"event_type\":\"step.delta\"}\n\n",
            "event: step.stop\n",
            "data: {\"index\":0,\"event_type\":\"step.stop\"}\n\n",
            "event: step.start\n",
            "data: {\"index\":1,\"step\":{\"type\":\"model_output\"},\"event_type\":\"step.start\"}\n\n",
            "event: step.delta\n",
            "data: {\"index\":1,\"delta\":{\"text\":\"hello\",\"type\":\"text\"},\"event_type\":\"step.delta\"}\n\n",
            "event: step.stop\n",
            "data: {\"index\":1,\"event_type\":\"step.stop\"}\n\n",
            "event: interaction.completed\n",
            "data: {\"interaction\":{\"status\":\"completed\",\"usage\":{\"total_tokens\":95,\"total_input_tokens\":8,\"total_output_tokens\":13,\"total_thought_tokens\":74}},\"event_type\":\"interaction.completed\"}\n\n",
            "event: done\n",
            "data: [DONE]\n\n",
        );
        let input = stream::iter(vec![Ok::<Bytes, std::io::Error>(Bytes::from(sse))]);
        let stream = translate_gemini_sse(input, false);
        let chunks: Vec<std::result::Result<Bytes, std::io::Error>> = stream.collect().await;
        let out: String = chunks
            .into_iter()
            .map(|r| String::from_utf8_lossy(&r.unwrap()).into_owned())
            .collect();

        assert!(out.contains("\"type\":\"message_start\""));
        // Thought steps surface with their signature (thinking block start +
        // signature_delta) even when summaries were not requested.
        assert!(out.contains("\"type\":\"thinking\""));
        assert!(out.contains("\"type\":\"signature_delta\""));
        assert!(out.contains("\"type\":\"text_delta\""));
        // message_delta usage sums output + thought tokens (13 + 74 = 87).
        assert!(out.contains("\"output_tokens\":87"), "{out}");
        assert!(out.contains("\"input_tokens\":8"), "{out}");
        assert!(!out.contains("total_thought_tokens"), "raw usage leaked: {out}");
    }
}

#[cfg(test)]
mod replay_tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn buffered_response_forwards_thought_step_with_signature() {
        let resp = json!({
            "status": "requires_action",
            "model": "gemini-3.8-flash",
            "steps": [
                {"type": "thought", "signature": "sig-real"},
                {"type": "function_call", "id": "call_1", "name": "f", "arguments": {}}
            ]
        });
        let out = gemini_to_anthropic_response(&resp).unwrap();
        let content = out["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["signature"], "sig-real");
        assert_eq!(content[1]["type"], "tool_use");
        // The tool turn stays replayable: the thinking block above carries the
        // signature Gemini will validate when the client echoes it back.
        assert_eq!(out["stop_reason"], "tool_use");
    }

    #[test]
    fn request_drops_signature_less_thinking_blocks() {
        let req = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "", "signature": ""},
                    {"type": "tool_use", "id": "t1", "name": "f", "input": {}}
                ]}
            ]
        });
        let p = Provider {
            id: "t".into(), base_url: "https://x".into(), api_key: "k".into(),
            api_format: crate::config::ApiFormat::Gemini,
            model_map: Default::default(), routes: vec![], enabled: true, fallback: true,
            api_version: None, inject_thinking_history: true, strict_thinking_history: false,
            quota_command: None, port: None, test_model: None, max_tokens_cap: None,
        };
        let out = anthropic_to_gemini_request(&req, &p).unwrap();
        let steps = out["input"].as_array().unwrap();
        // A signature-less thinking block is dropped, and without the signed
        // thought the tool call cannot be replayed either (Gemini would 400):
        // neither step may appear.
        assert!(!steps.iter().any(|s| s["type"] == "thought"));
        assert!(!steps.iter().any(|s| s["type"] == "function_call"));
    }
}
