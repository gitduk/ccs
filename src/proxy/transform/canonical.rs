use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use uuid::Uuid;
use super::tool_result_text;
use crate::config::OpenAiApiVersion;
use crate::error::{AppError, Result};

pub enum WirePayload<'a> {
    Anthropic(&'a Value),
    OpenAi(&'a Value, OpenAiApiVersion),
    Gemini(&'a Value),
}

fn extract_extra(val: &Value, known: &[&str]) -> Map<String, Value> {
    val.as_object()
        .map(|obj| {
            obj.iter()
                .filter(|(k, _)| !known.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default()
}

fn reasoning_text(item: &Value) -> String {
    if let Some(summary) = item.get("summary").and_then(|s| s.as_array()) {
        let text = summary
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("");
        if !text.is_empty() {
            return text;
        }
    }
    item.get("content")
        .and_then(|c| c.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub thought_tokens: u64,
}

impl Usage {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens + self.cache_write_tokens + self.thought_tokens
    }

    pub fn to_anthropic(&self) -> Value {
        json!({
            "input_tokens": self.input_tokens,
            "output_tokens": self.output_tokens + self.thought_tokens,
            "cache_read_input_tokens": self.cache_read_tokens,
            "cache_creation_input_tokens": self.cache_write_tokens,
        })
    }

    pub fn to_openai(&self, version: OpenAiApiVersion) -> Value {
        match version {
            OpenAiApiVersion::ChatCompletions => {
                let prompt = self.input_tokens + self.cache_read_tokens;
                let completion = self.output_tokens + self.thought_tokens;
                json!({
                    "prompt_tokens": prompt,
                    "completion_tokens": completion,
                    "total_tokens": prompt + completion,
                    "prompt_tokens_details": {
                        "cached_tokens": self.cache_read_tokens,
                    },
                    "completion_tokens_details": {
                        "reasoning_tokens": self.thought_tokens,
                    }
                })
            }
            OpenAiApiVersion::Responses => {
                let input = self.input_tokens + self.cache_read_tokens;
                let output = self.output_tokens + self.thought_tokens;
                json!({
                    "input_tokens": input,
                    "output_tokens": output,
                    "total_tokens": input + output,
                    "input_tokens_details": {
                        "cached_tokens": self.cache_read_tokens,
                    },
                    "output_tokens_details": {
                        "reasoning_tokens": self.thought_tokens,
                    }
                })
            }
        }
    }

    pub fn to_gemini(&self) -> Value {
        json!({
            "total_input_tokens": self.input_tokens,
            "total_output_tokens": self.output_tokens,
            "total_thought_tokens": self.thought_tokens,
            "total_cached_tokens": self.cache_read_tokens,
            "total_tokens": self.total_tokens(),
        })
    }
}

impl<'a> TryFrom<WirePayload<'a>> for Usage {
    type Error = AppError;

    fn try_from(payload: WirePayload<'a>) -> Result<Self> {
        match payload {
            WirePayload::Anthropic(val) => {
                let usage = val.get("usage").unwrap_or(val);
                let get = |k: &str| usage.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
                Ok(Self {
                    input_tokens: get("input_tokens"),
                    output_tokens: get("output_tokens"),
                    cache_read_tokens: get("cache_read_input_tokens"),
                    cache_write_tokens: get("cache_creation_input_tokens"),
                    thought_tokens: 0,
                })
            }
            WirePayload::OpenAi(val, OpenAiApiVersion::ChatCompletions) => {
                let usage = val.get("usage").unwrap_or(val);
                let get = |k: &str| usage.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
                let cached = usage
                    .get("prompt_tokens_details")
                    .and_then(|d| d.get("cached_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let reasoning = usage
                    .get("completion_tokens_details")
                    .and_then(|d| d.get("reasoning_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);

                let raw_prompt = get("prompt_tokens");
                let raw_completion = get("completion_tokens");

                Ok(Self {
                    input_tokens: raw_prompt.saturating_sub(cached),
                    output_tokens: raw_completion.saturating_sub(reasoning),
                    cache_read_tokens: cached,
                    cache_write_tokens: 0,
                    thought_tokens: reasoning,
                })
            }
            WirePayload::OpenAi(val, OpenAiApiVersion::Responses) => {
                let usage = val.get("usage").unwrap_or(val);
                let get = |k: &str| usage.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
                let cached = usage
                    .get("input_tokens_details")
                    .and_then(|d| d.get("cached_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let reasoning = usage
                    .get("output_tokens_details")
                    .and_then(|d| d.get("reasoning_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);

                let raw_input = get("input_tokens");
                let raw_output = get("output_tokens");

                Ok(Self {
                    input_tokens: raw_input.saturating_sub(cached),
                    output_tokens: raw_output.saturating_sub(reasoning),
                    cache_read_tokens: cached,
                    cache_write_tokens: 0,
                    thought_tokens: reasoning,
                })
            }
            WirePayload::Gemini(val) => {
                let usage = val.get("usage").unwrap_or(val);
                let get = |k: &str| usage.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
                let input = get("total_input_tokens");
                let output = get("total_output_tokens");
                let thoughts = get("total_thought_tokens");
                let cached = get("total_cached_tokens");

                if input != 0 || output != 0 || thoughts != 0 {
                    Ok(Self {
                        input_tokens: input,
                        output_tokens: output,
                        cache_read_tokens: cached,
                        cache_write_tokens: 0,
                        thought_tokens: thoughts,
                    })
                } else {
                    let total = get("total_tokens");
                    Ok(Self {
                        input_tokens: total,
                        output_tokens: 0,
                        cache_read_tokens: cached,
                        cache_write_tokens: 0,
                        thought_tokens: 0,
                    })
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub input_schema: Value,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    ToolResult {
        tool_use_id: String,
        content: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    Image {
        source: Value,
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
    #[serde(untagged)]
    Custom {
        #[serde(flatten)]
        extra: Map<String, Value>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: Vec<ContentBlock>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub model: String,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub id: String,
    pub model: String,
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub usage: Usage,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Response {
    pub fn to_anthropic(&self) -> Result<Value> {
        let usage = self.usage.to_anthropic();
        let mut map = Map::new();
        let id = if self.id.is_empty() {
            format!("msg_{}", Uuid::new_v4())
        } else {
            self.id.clone()
        };
        map.insert("id".to_string(), json!(id));
        map.insert("type".to_string(), json!("message"));
        map.insert("role".to_string(), json!("assistant"));
        map.insert("model".to_string(), json!(self.model));
        let content_val = if self.content.is_empty() {
            json!([{"type": "text", "text": ""}])
        } else {
            serde_json::to_value(&self.content).map_err(|e| AppError::Transform(e.to_string()))?
        };
        map.insert("content".to_string(), content_val);
        map.insert("stop_reason".to_string(), json!(self.stop_reason));
        map.insert("stop_sequence".to_string(), Value::Null);
        map.insert("usage".to_string(), usage);
        for (k, v) in &self.extra {
            map.entry(k.clone()).or_insert_with(|| v.clone());
        }
        Ok(Value::Object(map))
    }

    pub fn to_openai(&self, version: OpenAiApiVersion) -> Result<Value> {
        match version {
            OpenAiApiVersion::ChatCompletions => {
                let usage = self.usage.to_openai(version);
                let finish_reason = match self.stop_reason.as_deref() {
                    Some("max_tokens") => "length",
                    Some("tool_use") => "tool_calls",
                    _ => "stop",
                };

                let mut text_parts = Vec::new();
                let mut reasoning_parts = Vec::new();
                let mut tool_calls = Vec::new();

                for block in &self.content {
                    match block {
                        ContentBlock::Text { text, .. } => text_parts.push(text.clone()),
                        ContentBlock::Thinking { thinking, .. } => reasoning_parts.push(thinking.clone()),
                        ContentBlock::ToolUse { id, name, input, .. } => {
                            tool_calls.push(json!({
                                "id": id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": serde_json::to_string(input).unwrap_or_default(),
                                }
                            }));
                        }
                        _ => {}
                    }
                }

                let mut msg = json!({
                    "role": "assistant",
                    "content": if text_parts.is_empty() { Value::Null } else { json!(text_parts.join("")) }
                });
                if !reasoning_parts.is_empty() {
                    msg["reasoning_content"] = json!(reasoning_parts.join(""));
                }
                if !tool_calls.is_empty() {
                    msg["tool_calls"] = json!(tool_calls);
                }

                let id = if self.id.is_empty() {
                    format!("chatcmpl-{}", Uuid::new_v4())
                } else if self.id.starts_with("chatcmpl-") {
                    self.id.clone()
                } else {
                    format!("chatcmpl-{}", self.id)
                };

                let mut map = Map::new();
                map.insert("id".to_string(), json!(id));
                map.insert("object".to_string(), json!("chat.completion"));
                map.insert("model".to_string(), json!(self.model));
                map.insert("choices".to_string(), json!([{
                    "index": 0,
                    "message": msg,
                    "finish_reason": finish_reason,
                }]));
                map.insert("usage".to_string(), usage);
                for (k, v) in &self.extra {
                    map.entry(k.clone()).or_insert_with(|| v.clone());
                }
                Ok(Value::Object(map))
            }
            OpenAiApiVersion::Responses => {
                let usage = self.usage.to_openai(version);
                let status = match self.stop_reason.as_deref() {
                    Some("max_tokens") => "incomplete",
                    _ => "completed",
                };

                let mut output_items: Vec<Value> = Vec::new();
                for block in &self.content {
                    match block {
                        ContentBlock::Text { text, .. } => {
                            output_items.push(json!({
                                "type": "message",
                                "role": "assistant",
                                "content": [{"type": "output_text", "text": text}]
                            }));
                        }
                        ContentBlock::Thinking { thinking, .. } => {
                            output_items.push(json!({
                                "type": "reasoning",
                                "summary": thinking
                            }));
                        }
                        ContentBlock::ToolUse { id, name, input, .. } => {
                            output_items.push(json!({
                                "type": "function_call",
                                "call_id": id,
                                "name": name,
                                "arguments": serde_json::to_string(input).unwrap_or_default(),
                            }));
                        }
                        _ => {}
                    }
                }

                let id = if self.id.is_empty() {
                    format!("resp_{}", Uuid::new_v4())
                } else if self.id.starts_with("resp_") {
                    self.id.clone()
                } else {
                    format!("resp_{}", self.id)
                };

                let mut map = Map::new();
                map.insert("id".to_string(), json!(id));
                map.insert("object".to_string(), json!("response"));
                map.insert("model".to_string(), json!(self.model));
                map.insert("status".to_string(), json!(status));
                map.insert("output".to_string(), json!(output_items));
                map.insert("usage".to_string(), usage);
                for (k, v) in &self.extra {
                    map.entry(k.clone()).or_insert_with(|| v.clone());
                }
                Ok(Value::Object(map))
            }
        }
    }

    pub fn to_gemini(&self) -> Result<Value> {
        let usage = self.usage.to_gemini();
        let mut steps = Vec::new();
        for block in &self.content {
            match block {
                ContentBlock::Text { text, .. } => {
                    steps.push(json!({
                        "type": "model_output",
                        "content": [{"type": "text", "text": text}]
                    }));
                }
                ContentBlock::Thinking { thinking, signature, .. } => {
                    let mut obj = json!({
                        "type": "thought",
                        "content": [{"type": "text", "text": thinking}]
                    });
                    if let Some(sig) = signature {
                        obj["signature"] = json!(sig);
                    }
                    steps.push(obj);
                }
                ContentBlock::ToolUse { id, name, input, .. } => {
                    steps.push(json!({
                        "type": "function_call",
                        "id": id,
                        "name": name,
                        "arguments": input,
                    }));
                }
                _ => {}
            }
        }

        let mut map = Map::new();
        map.insert("id".to_string(), json!(self.id));
        map.insert("model".to_string(), json!(self.model));
        map.insert("status".to_string(), json!(if self.stop_reason.as_deref() == Some("tool_use") { "requires_action" } else { "completed" }));
        map.insert("steps".to_string(), json!(steps));
        map.insert("usage".to_string(), usage);
        for (k, v) in &self.extra {
            map.entry(k.clone()).or_insert_with(|| v.clone());
        }
        Ok(Value::Object(map))
    }
}

impl<'a> TryFrom<WirePayload<'a>> for Response {
    type Error = AppError;

    fn try_from(payload: WirePayload<'a>) -> Result<Self> {
        match payload {
            WirePayload::Anthropic(val) => {
                let usage = Usage::try_from(WirePayload::Anthropic(val))?;
                let id = val.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let model = val.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let stop_reason = val.get("stop_reason").and_then(|v| v.as_str()).map(String::from);

                let mut content = Vec::new();
                if let Some(blocks) = val.get("content").and_then(|v| v.as_array()) {
                    for b in blocks {
                        content.push(serde_json::from_value(b.clone()).map_err(|e| AppError::Transform(e.to_string()))?);
                    }
                }

                let extra = extract_extra(val, &["id", "type", "role", "model", "content", "stop_reason", "stop_sequence", "usage"]);

                Ok(Self {
                    id,
                    model,
                    content,
                    stop_reason,
                    usage,
                    extra,
                })
            }
            WirePayload::OpenAi(val, OpenAiApiVersion::ChatCompletions) => {
                let usage = Usage::try_from(WirePayload::OpenAi(val, OpenAiApiVersion::ChatCompletions))?;
                let id = val.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let model = val.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string();

                let choice = val.get("choices").and_then(|c| c.get(0)).unwrap_or(&Value::Null);
                let finish_reason = choice.get("finish_reason").and_then(|f| f.as_str()).unwrap_or("stop");
                let stop_reason = match finish_reason {
                    "stop" => Some("end_turn".to_string()),
                    "length" => Some("max_tokens".to_string()),
                    "tool_calls" => Some("tool_use".to_string()),
                    other => Some(other.to_string()),
                };

                let mut content = Vec::new();
                let msg = choice.get("message").unwrap_or(&Value::Null);

                if let Some(rc) = msg.get("reasoning_content").and_then(|r| r.as_str()) && !rc.is_empty() {
                    content.push(ContentBlock::Thinking {
                        thinking: rc.to_string(),
                        signature: None,
                        extra: Map::new(),
                    });
                }

                if let Some(text) = msg.get("content").and_then(|c| c.as_str()) && !text.is_empty() {
                    content.push(ContentBlock::Text {
                        text: text.to_string(),
                        extra: Map::new(),
                    });
                }

                if let Some(tcs) = msg.get("tool_calls").and_then(|t| t.as_array()) {
                    for tc in tcs {
                        let call_id = tc.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
                        let fn_obj = tc.get("function").unwrap_or(&Value::Null);
                        let name = fn_obj.get("name").and_then(|n| n.as_str()).unwrap_or_default().to_string();
                        let args_str = fn_obj.get("arguments").and_then(|a| a.as_str()).unwrap_or("{}");
                        let input = serde_json::from_str(args_str).unwrap_or(Value::Object(Map::new()));
                        content.push(ContentBlock::ToolUse {
                            id: call_id,
                            name,
                            input,
                            extra: Map::new(),
                        });
                    }
                }

                let extra = extract_extra(val, &["id", "object", "model", "choices", "usage", "created"]);

                Ok(Self {
                    id,
                    model,
                    content,
                    stop_reason,
                    usage,
                    extra,
                })
            }
            WirePayload::OpenAi(val, OpenAiApiVersion::Responses) => {
                let usage = Usage::try_from(WirePayload::OpenAi(val, OpenAiApiVersion::Responses))?;
                let id = val.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let model = val.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let status = val.get("status").and_then(|s| s.as_str()).unwrap_or("completed");
                let stop_reason = match status {
                    "incomplete" => Some("max_tokens".to_string()),
                    _ => Some("end_turn".to_string()),
                };

                let mut content = Vec::new();
                if let Some(output) = val.get("output").and_then(|o| o.as_array()) {
                    for item in output {
                        let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        match item_type {
                            "message" => {
                                if let Some(blocks) = item.get("content").and_then(|c| c.as_array()) {
                                    for b in blocks {
                                        if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                                            content.push(ContentBlock::Text { text: t.to_string(), extra: Map::new() });
                                        }
                                    }
                                }
                            }
                            "function_call" => {
                                let call_id = item.get("call_id").and_then(|i| i.as_str()).unwrap_or("").to_string();
                                let name = item.get("name").and_then(|n| n.as_str()).unwrap_or_default().to_string();
                                let args_str = item.get("arguments").and_then(|a| a.as_str()).unwrap_or("{}");
                                let input = serde_json::from_str(args_str).unwrap_or(Value::Object(Map::new()));
                                content.push(ContentBlock::ToolUse {
                                    id: call_id,
                                    name,
                                    input,
                                    extra: Map::new(),
                                });
                            }
                            "reasoning" => {
                                let reasoning = reasoning_text(item);
                                if !reasoning.is_empty() {
                                    content.push(ContentBlock::Thinking {
                                        thinking: reasoning,
                                        signature: None,
                                        extra: Map::new(),
                                    });
                                }
                            }
                            _ => {}
                        }
                    }
                }

                let extra = extract_extra(val, &["id", "object", "model", "status", "output", "usage", "created_at"]);

                Ok(Self {
                    id,
                    model,
                    content,
                    stop_reason,
                    usage,
                    extra,
                })
            }
            WirePayload::Gemini(val) => {
                let usage = Usage::try_from(WirePayload::Gemini(val))?;
                let id = val.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let model = val.get("model").and_then(|v| v.as_str()).unwrap_or("").to_string();

                let mut content = Vec::new();
                let mut last_was_tool = false;

                if let Some(steps) = val.get("steps").and_then(|s| s.as_array()) {
                    for step in steps {
                        let step_type = step.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        match step_type {
                            "model_output" => {
                                last_was_tool = false;
                                if let Some(blocks) = step.get("content").and_then(|c| c.as_array()) {
                                    for b in blocks {
                                        if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                                            let text = b.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();
                                            content.push(ContentBlock::Text { text, extra: Map::new() });
                                        }
                                    }
                                }
                            }
                            "thought" => {
                                last_was_tool = false;
                                let signature = step.get("signature").and_then(|s| s.as_str()).map(String::from);
                                let mut thinking = String::new();
                                if let Some(blocks) = step.get("content").and_then(|c| c.as_array()) {
                                    for b in blocks {
                                        if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                                            thinking.push_str(t);
                                        }
                                    }
                                }
                                content.push(ContentBlock::Thinking {
                                    thinking,
                                    signature,
                                    extra: Map::new(),
                                });
                            }
                            "function_call" => {
                                last_was_tool = true;
                                let call_id = step.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
                                let name = step.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                                let input = step.get("arguments").cloned().unwrap_or(Value::Object(Map::new()));
                                content.push(ContentBlock::ToolUse {
                                    id: call_id,
                                    name,
                                    input,
                                    extra: Map::new(),
                                });
                            }
                            _ => {}
                        }
                    }
                }

                let stop_reason = if last_was_tool {
                    Some("tool_use".to_string())
                } else {
                    Some("end_turn".to_string())
                };

                let extra = extract_extra(val, &["id", "model", "status", "steps", "usage"]);

                Ok(Self {
                    id,
                    model,
                    content,
                    stop_reason,
                    usage,
                    extra,
                })
            }
        }
    }
}

impl Request {
    pub fn to_anthropic(&self) -> Result<Value> {
        let mut map = Map::new();
        map.insert("model".to_string(), json!(self.model));
        if let Some(sys) = &self.system {
            map.insert("system".to_string(), json!(sys));
        }
        if let Some(mt) = self.max_tokens {
            map.insert("max_tokens".to_string(), json!(mt));
        }
        if let Some(temp) = self.temperature {
            map.insert("temperature".to_string(), json!(temp));
        }
        if self.stream {
            map.insert("stream".to_string(), json!(true));
        }
        let messages_val = serde_json::to_value(&self.messages).map_err(|e| AppError::Transform(e.to_string()))?;
        map.insert("messages".to_string(), messages_val);
        if !self.tools.is_empty() {
            let tools_val = serde_json::to_value(&self.tools).map_err(|e| AppError::Transform(e.to_string()))?;
            map.insert("tools".to_string(), tools_val);
        }
        for (k, v) in &self.extra {
            map.entry(k.clone()).or_insert_with(|| v.clone());
        }
        Ok(Value::Object(map))
    }

    pub fn to_openai(&self, version: OpenAiApiVersion) -> Result<Value> {
        match version {
            OpenAiApiVersion::ChatCompletions => {
                let mut map = Map::new();
                map.insert("model".to_string(), json!(self.model));
                if let Some(mt) = self.max_tokens {
                    map.insert("max_tokens".to_string(), json!(mt));
                }
                if let Some(temp) = self.temperature {
                    map.insert("temperature".to_string(), json!(temp));
                }
                if self.stream {
                    map.insert("stream".to_string(), json!(true));
                }

                let mut msgs = Vec::new();
                if let Some(sys) = &self.system {
                    msgs.push(json!({"role": "system", "content": sys}));
                }
                for m in &self.messages {
                    let mut text_acc = String::new();
                    let mut tool_calls = Vec::new();
                    let mut had_tool_result = false;
                    for b in &m.content {
                        match b {
                            ContentBlock::Text { text, .. } => text_acc.push_str(text),
                            ContentBlock::ToolUse { id, name, input, .. } => {
                                tool_calls.push(json!({
                                    "id": id,
                                    "type": "function",
                                    "function": {
                                        "name": name,
                                        "arguments": serde_json::to_string(input).unwrap_or_default(),
                                    }
                                }));
                            }
                            ContentBlock::ToolResult { tool_use_id, content, .. } => {
                                had_tool_result = true;
                                msgs.push(json!({
                                    "role": "tool",
                                    "tool_call_id": tool_use_id,
                                    "content": tool_result_text(Some(content))?,
                                }));
                            }
                            _ => {}
                        }
                    }
                    if had_tool_result && !text_acc.is_empty() {
                        // OpenAI requires tool messages to directly follow the
                        // assistant tool_calls message, so any accompanying text
                        // from the same user turn lands after them.
                        msgs.push(json!({
                            "role": m.role,
                            "content": text_acc,
                        }));
                        text_acc = String::new();
                    }
                    if !text_acc.is_empty() || !tool_calls.is_empty() {
                        let mut msg_obj = json!({
                            "role": m.role,
                            "content": if text_acc.is_empty() { Value::Null } else { json!(text_acc) },
                        });
                        if !tool_calls.is_empty() {
                            msg_obj["tool_calls"] = json!(tool_calls);
                        }
                        msgs.push(msg_obj);
                    }
                }
                map.insert("messages".to_string(), json!(msgs));

                if !self.tools.is_empty() {
                    let mut tools_json = Vec::new();
                    for t in &self.tools {
                        tools_json.push(json!({
                            "type": "function",
                            "function": {
                                "name": t.name,
                                "description": t.description,
                                "parameters": t.input_schema,
                            }
                        }));
                    }
                    map.insert("tools".to_string(), json!(tools_json));
                }

                for (k, v) in &self.extra {
                    map.entry(k.clone()).or_insert_with(|| v.clone());
                }
                Ok(Value::Object(map))
            }
            OpenAiApiVersion::Responses => {
                let mut map = Map::new();
                map.insert("model".to_string(), json!(self.model));
                if let Some(sys) = &self.system {
                    map.insert("instructions".to_string(), json!(sys));
                }
                if let Some(mt) = self.max_tokens {
                    map.insert("max_output_tokens".to_string(), json!(mt));
                }
                if let Some(temp) = self.temperature {
                    map.insert("temperature".to_string(), json!(temp));
                }
                if self.stream {
                    map.insert("stream".to_string(), json!(true));
                }

                let mut input_items = Vec::new();
                for m in &self.messages {
                    for b in &m.content {
                        match b {
                            ContentBlock::Text { text, .. } => {
                                input_items.push(json!({
                                    "type": "message",
                                    "role": m.role,
                                    "content": [{"type": "input_text", "text": text}]
                                }));
                            }
                            ContentBlock::ToolUse { id, name, input, .. } => {
                                input_items.push(json!({
                                    "type": "function_call",
                                    "call_id": id,
                                    "name": name,
                                    "arguments": serde_json::to_string(input).unwrap_or_default(),
                                }));
                            }
                            ContentBlock::ToolResult { tool_use_id, content, .. } => {
                                input_items.push(json!({
                                    "type": "function_call_output",
                                    "call_id": tool_use_id,
                                    "output": tool_result_text(Some(content))?,
                                }));
                            }
                            _ => {}
                        }
                    }
                }
                map.insert("input".to_string(), json!(input_items));

                if !self.tools.is_empty() {
                    let mut tools_json = Vec::new();
                    for t in &self.tools {
                        tools_json.push(json!({
                            "type": "function",
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.input_schema,
                        }));
                    }
                    map.insert("tools".to_string(), json!(tools_json));
                }

                for (k, v) in &self.extra {
                    map.entry(k.clone()).or_insert_with(|| v.clone());
                }
                Ok(Value::Object(map))
            }
        }
    }

    pub fn to_gemini(&self) -> Result<Value> {
        let mut map = Map::new();
        map.insert("model".to_string(), json!(self.model));
        if self.stream {
            map.insert("stream".to_string(), json!(true));
        }

        let mut id_to_name = std::collections::HashMap::new();
        for m in &self.messages {
            for b in &m.content {
                if let ContentBlock::ToolUse { id, name, .. } = b {
                    id_to_name.insert(id.clone(), name.clone());
                }
            }
        }

        let mut steps = Vec::new();
        if let Some(sys) = &self.system {
            map.insert("system_instruction".to_string(), json!(sys));
        }

        for m in &self.messages {
            match m.role.as_str() {
                "assistant" => {
                    // Gemini only accepts a replayed `function_call` when the
                    // thought step before it carried its real signature; without
                    // one the whole request 400s, so the call is dropped and only
                    // the later `function_result` is sent (verified live).
                    let mut had_signed_thought = false;
                    for b in &m.content {
                        match b {
                            ContentBlock::Text { text, .. } => {
                                steps.push(json!({
                                    "type": "model_output",
                                    "content": [{"type": "text", "text": text}]
                                }));
                            }
                            ContentBlock::Thinking { thinking, signature, .. } => {
                                let sig = signature
                                    .as_deref()
                                    .filter(|s| !s.is_empty());
                                if sig.is_some() {
                                    had_signed_thought = true;
                                    let mut obj = json!({
                                        "type": "thought",
                                        "content": [{"type": "text", "text": thinking}]
                                    });
                                    obj["signature"] = json!(sig);
                                    steps.push(obj);
                                } else {
                                    // Gemini validates the signature
                                    // cryptographically; a signature-less block
                                    // is dropped rather than sent as an invalid
                                    // step (mirrors gemini.rs).
                                    tracing::warn!("dropped signature-less thinking block from Gemini history");
                                }
                            }
                            ContentBlock::ToolUse { id, name, input, .. } => {
                                if !had_signed_thought {
                                    tracing::warn!(
                                        "tool round replayed without a signed thought; \
                                         function_call '{name}' omitted"
                                    );
                                    continue;
                                }
                                steps.push(json!({
                                    "type": "function_call",
                                    "id": id,
                                    "name": name,
                                    "arguments": input,
                                }));
                            }
                            _ => {}
                        }
                    }
                }
                _ => {
                    let mut text_parts = Vec::new();
                    for b in &m.content {
                        match b {
                            ContentBlock::Text { text, .. } => {
                                text_parts.push(json!({"type": "text", "text": text}));
                            }
                            ContentBlock::ToolResult { tool_use_id, content, .. } => {
                                if !text_parts.is_empty() {
                                    steps.push(json!({
                                        "type": "user_input",
                                        "content": text_parts.clone()
                                    }));
                                    text_parts.clear();
                                }
                                let name = id_to_name.get(tool_use_id).cloned().unwrap_or_default();
                                if name.is_empty() {
                                    return Err(AppError::Transform(format!(
                                        "tool_result references unknown tool_use_id '{tool_use_id}'"
                                    )));
                                }
                                steps.push(json!({
                                    "type": "function_result",
                                    "call_id": tool_use_id,
                                    "name": name,
                                    "result": [{"type": "text", "text": tool_result_text(Some(content))?}],
                                }));
                            }
                            _ => {}
                        }
                    }
                    if !text_parts.is_empty() {
                        steps.push(json!({
                            "type": "user_input",
                            "content": text_parts
                        }));
                    }
                }
            }
        }
        map.insert("input".to_string(), json!(steps));

        if !self.tools.is_empty() {
            let mut decls = Vec::new();
            for t in &self.tools {
                decls.push(json!({
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                }));
            }
            map.insert("tools".to_string(), json!([{"function_declarations": decls}]));
        }

        for (k, v) in &self.extra {
            map.entry(k.clone()).or_insert_with(|| v.clone());
        }
        Ok(Value::Object(map))
    }
}

impl<'a> TryFrom<WirePayload<'a>> for Request {
    type Error = AppError;

    fn try_from(payload: WirePayload<'a>) -> Result<Self> {
        match payload {
            WirePayload::Anthropic(val) => {
                let model = val.get("model").and_then(|m| m.as_str()).unwrap_or("").to_string();
                let max_tokens = val.get("max_tokens").and_then(|m| m.as_u64());
                let temperature = val.get("temperature").and_then(|t| t.as_f64());
                let stream = val.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);

                let system = match val.get("system") {
                    Some(Value::String(s)) => Some(s.clone()),
                    Some(Value::Array(arr)) => Some(
                        arr.iter()
                            .filter_map(|item| item.get("text")?.as_str())
                            .collect::<Vec<_>>()
                            .join("\n\n"),
                    ),
                    _ => None,
                };

                let mut messages = Vec::new();
                if let Some(msgs) = val.get("messages").and_then(|m| m.as_array()) {
                    for m in msgs {
                        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("user").to_string();
                        let mut content = Vec::new();
                        match m.get("content") {
                            Some(Value::String(text)) => {
                                content.push(ContentBlock::Text { text: text.clone(), extra: Map::new() });
                            }
                            Some(Value::Array(blocks)) => {
                                for b in blocks {
                                    content.push(serde_json::from_value(b.clone()).map_err(|e| AppError::Transform(e.to_string()))?);
                                }
                            }
                            _ => {}
                        }
                        let extra = extract_extra(m, &["role", "content"]);
                        messages.push(Message { role, content, extra });
                    }
                }

                let mut tools = Vec::new();
                if let Some(ts) = val.get("tools").and_then(|t| t.as_array()) {
                    for t in ts {
                        tools.push(serde_json::from_value(t.clone()).map_err(|e| AppError::Transform(e.to_string()))?);
                    }
                }

                let extra = extract_extra(val, &["model", "messages", "system", "max_tokens", "temperature", "stream", "tools"]);

                Ok(Self {
                    model,
                    messages,
                    system,
                    max_tokens,
                    temperature,
                    stream,
                    tools,
                    extra,
                })
            }
            WirePayload::OpenAi(val, OpenAiApiVersion::ChatCompletions) => {
                let model = val.get("model").and_then(|m| m.as_str()).unwrap_or("").to_string();
                let max_tokens = val.get("max_tokens")
                    .or_else(|| val.get("max_completion_tokens"))
                    .or_else(|| val.get("max_output_tokens"))
                    .and_then(|m| m.as_u64());
                let temperature = val.get("temperature").and_then(|t| t.as_f64());
                let stream = val.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);

                let mut system_parts = Vec::new();
                let mut messages = Vec::new();

                if let Some(msgs) = val.get("messages").and_then(|m| m.as_array()) {
                    for m in msgs {
                        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("user");
                        if role == "system" {
                            if let Some(s) = m.get("content").and_then(|c| c.as_str()) {
                                system_parts.push(s.to_string());
                            }
                            continue;
                        }

                        let mut content = Vec::new();
                        if let Some(rc) = m.get("reasoning_content").and_then(|r| r.as_str()) && !rc.is_empty() {
                            content.push(ContentBlock::Thinking {
                                thinking: rc.to_string(),
                                signature: None,
                                extra: Map::new(),
                            });
                        }


                        if let Some(tcs) = m.get("tool_calls").and_then(|t| t.as_array()) {
                            for tc in tcs {
                                let id = tc.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
                                let name = tc.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()).unwrap_or("").to_string();
                                let args_str = tc.get("function").and_then(|f| f.get("arguments")).and_then(|a| a.as_str()).unwrap_or("{}");
                                let input = serde_json::from_str(args_str).unwrap_or(Value::Object(Map::new()));
                                content.push(ContentBlock::ToolUse { id, name, input, extra: Map::new() });
                            }
                        }

                        if role == "tool" {
                            let tool_use_id = m.get("tool_call_id").and_then(|i| i.as_str()).unwrap_or("").to_string();
                            let content_val = m.get("content").cloned().unwrap_or(Value::Null);
                            content.push(ContentBlock::ToolResult {
                                tool_use_id,
                                content: content_val,
                                is_error: None,
                                extra: Map::new(),
                            });
                        } else if let Some(text) = m.get("content").and_then(|c| c.as_str()) && !text.is_empty() {
                            content.push(ContentBlock::Text { text: text.to_string(), extra: Map::new() });
                        }

                        let extra = extract_extra(m, &["role", "content", "reasoning_content", "tool_calls", "tool_call_id"]);
                        messages.push(Message { role: role.to_string(), content, extra });
                    }
                }

                let system = if system_parts.is_empty() { None } else { Some(system_parts.join("\n\n")) };

                let mut tools = Vec::new();
                if let Some(ts) = val.get("tools").and_then(|t| t.as_array()) {
                    for t in ts {
                        let fn_obj = t.get("function").unwrap_or(t);
                        let name = fn_obj.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                        let description = fn_obj.get("description").and_then(|d| d.as_str()).map(String::from);
                        let input_schema = fn_obj.get("parameters").cloned().unwrap_or(Value::Object(Map::new()));
                        tools.push(Tool { name, description, input_schema, extra: Map::new() });
                    }
                }

                let extra = extract_extra(val, &["model", "messages", "max_tokens", "max_completion_tokens", "temperature", "stream", "tools"]);
                Ok(Self { model, messages, system, max_tokens, temperature, stream, tools, extra })
            }
            WirePayload::OpenAi(val, OpenAiApiVersion::Responses) => {
                let model = val.get("model").and_then(|m| m.as_str()).unwrap_or("").to_string();
                let system = val.get("instructions").and_then(|i| i.as_str()).map(String::from);
                let max_tokens = val.get("max_output_tokens").or_else(|| val.get("max_tokens")).and_then(|m| m.as_u64());
                let temperature = val.get("temperature").and_then(|t| t.as_f64());
                let stream = val.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);

                let mut messages = Vec::new();
                if let Some(items) = val.get("input").and_then(|i| i.as_array()) {
                    for item in items {
                        let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        match item_type {
                            "message" => {
                                let role = item.get("role").and_then(|r| r.as_str()).unwrap_or("user").to_string();
                                let mut content = Vec::new();
                                if let Some(blocks) = item.get("content").and_then(|c| c.as_array()) {
                                    for b in blocks {
                                        if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                                            content.push(ContentBlock::Text { text: t.to_string(), extra: Map::new() });
                                        }
                                    }
                                }
                                messages.push(Message { role, content, extra: Map::new() });
                            }
                            "function_call" => {
                                let call_id = item.get("call_id").and_then(|i| i.as_str()).unwrap_or("").to_string();
                                let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                                let args_str = item.get("arguments").and_then(|a| a.as_str()).unwrap_or("{}");
                                let input = serde_json::from_str(args_str).unwrap_or(Value::Object(Map::new()));
                                messages.push(Message {
                                    role: "assistant".to_string(),
                                    content: vec![ContentBlock::ToolUse { id: call_id, name, input, extra: Map::new() }],
                                    extra: Map::new(),
                                });
                            }
                            "function_call_output" => {
                                let call_id = item.get("call_id").and_then(|i| i.as_str()).unwrap_or("").to_string();
                                let output = item.get("output").cloned().unwrap_or(Value::Null);
                                messages.push(Message {
                                    role: "user".to_string(),
                                    content: vec![ContentBlock::ToolResult {
                                        tool_use_id: call_id,
                                        content: output,
                                        is_error: None,
                                        extra: Map::new(),
                                    }],
                                    extra: Map::new(),
                                });
                            }
                            "reasoning" => {
                                let reasoning = reasoning_text(item);
                                if !reasoning.is_empty() {
                                    messages.push(Message {
                                        role: "assistant".to_string(),
                                        content: vec![ContentBlock::Thinking {
                                            thinking: reasoning,
                                            signature: None,
                                            extra: Map::new(),
                                        }],
                                        extra: Map::new(),
                                    });
                                }
                            }
                            _ => {}
                        }
                    }
                }

                let mut tools = Vec::new();
                if let Some(ts) = val.get("tools").and_then(|t| t.as_array()) {
                    for t in ts {
                        let name = t.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                        let description = t.get("description").and_then(|d| d.as_str()).map(String::from);
                        let input_schema = t.get("parameters").cloned().unwrap_or(Value::Object(Map::new()));
                        tools.push(Tool { name, description, input_schema, extra: Map::new() });
                    }
                }

                let extra = extract_extra(val, &["model", "instructions", "input", "max_output_tokens", "max_tokens", "temperature", "stream", "tools"]);
                Ok(Self { model, messages, system, max_tokens, temperature, stream, tools, extra })
            }
            WirePayload::Gemini(val) => {
                let model = val.get("model").and_then(|m| m.as_str()).unwrap_or("").to_string();
                let stream = val.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);

                let mut messages = Vec::new();
                if let Some(steps) = val.get("input").and_then(|i| i.as_array()) {
                    for step in steps {
                        let step_type = step.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        match step_type {
                            "user_input" => {
                                let mut content = Vec::new();
                                if let Some(blocks) = step.get("content").and_then(|c| c.as_array()) {
                                    for b in blocks {
                                        if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                                            content.push(ContentBlock::Text { text: t.to_string(), extra: Map::new() });
                                        }
                                    }
                                }
                                messages.push(Message { role: "user".to_string(), content, extra: Map::new() });
                            }
                            "model_output" => {
                                let mut content = Vec::new();
                                if let Some(blocks) = step.get("content").and_then(|c| c.as_array()) {
                                    for b in blocks {
                                        if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                                            content.push(ContentBlock::Text { text: t.to_string(), extra: Map::new() });
                                        }
                                    }
                                }
                                messages.push(Message { role: "assistant".to_string(), content, extra: Map::new() });
                            }
                            "thought" => {
                                let signature = step.get("signature").and_then(|s| s.as_str()).map(String::from);
                                let mut thinking = String::new();
                                if let Some(blocks) = step.get("content").and_then(|c| c.as_array()) {
                                    for b in blocks {
                                        if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                                            thinking.push_str(t);
                                        }
                                    }
                                }
                                messages.push(Message {
                                    role: "assistant".to_string(),
                                    content: vec![ContentBlock::Thinking { thinking, signature, extra: Map::new() }],
                                    extra: Map::new(),
                                });
                            }
                            "function_call" => {
                                let id = step.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
                                let name = step.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                                let input = step.get("arguments").cloned().unwrap_or(Value::Object(Map::new()));
                                messages.push(Message {
                                    role: "assistant".to_string(),
                                    content: vec![ContentBlock::ToolUse { id, name, input, extra: Map::new() }],
                                    extra: Map::new(),
                                });
                            }
                            "function_result" => {
                                let call_id = step.get("call_id").and_then(|i| i.as_str()).unwrap_or("").to_string();
                                let result = step.get("result").cloned().unwrap_or(Value::Null);
                                messages.push(Message {
                                    role: "user".to_string(),
                                    content: vec![ContentBlock::ToolResult {
                                        tool_use_id: call_id,
                                        content: result,
                                        is_error: None,
                                        extra: Map::new(),
                                    }],
                                    extra: Map::new(),
                                });
                            }
                            _ => {}
                        }
                    }
                }

                let mut tools = Vec::new();
                if let Some(ts) = val.get("tools").and_then(|t| t.as_array()) {
                    for t in ts {
                        if let Some(decls) = t.get("function_declarations").and_then(|d| d.as_array()) {
                            for decl in decls {
                                let name = decl.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                                let description = decl.get("description").and_then(|d| d.as_str()).map(String::from);
                                let input_schema = decl.get("parameters").cloned().unwrap_or(Value::Object(Map::new()));
                                tools.push(Tool { name, description, input_schema, extra: Map::new() });
                            }
                        }
                    }
                }

                let extra = extract_extra(val, &["model", "input", "stream", "tools"]);
                Ok(Self { model, messages, system: None, max_tokens: None, temperature: None, stream, tools, extra })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_prompt_deducts_cached_tokens() {
        let openai_usage = json!({
            "prompt_tokens": 100,
            "completion_tokens": 50,
            "total_tokens": 150,
            "prompt_tokens_details": {
                "cached_tokens": 30
            },
            "completion_tokens_details": {
                "reasoning_tokens": 20
            }
        });

        let canonical = Usage::try_from(WirePayload::OpenAi(&openai_usage, OpenAiApiVersion::ChatCompletions)).unwrap();
        assert_eq!(canonical.input_tokens, 70);
        assert_eq!(canonical.cache_read_tokens, 30);
        assert_eq!(canonical.output_tokens, 30);
        assert_eq!(canonical.thought_tokens, 20);

        let back = canonical.to_openai(OpenAiApiVersion::ChatCompletions);
        assert_eq!(back["prompt_tokens"], 100);
        assert_eq!(back["completion_tokens"], 50);
        assert_eq!(back["prompt_tokens_details"]["cached_tokens"], 30);
        assert_eq!(back["completion_tokens_details"]["reasoning_tokens"], 20);
    }

    #[test]
    fn gemini_output_accumulates_thought_for_anthropic() {
        let gemini_usage = json!({
            "total_input_tokens": 8,
            "total_output_tokens": 13,
            "total_thought_tokens": 74,
            "total_cached_tokens": 25,
            "total_tokens": 95
        });

        let canonical = Usage::try_from(WirePayload::Gemini(&gemini_usage)).unwrap();
        assert_eq!(canonical.input_tokens, 8);
        assert_eq!(canonical.output_tokens, 13);
        assert_eq!(canonical.thought_tokens, 74);
        assert_eq!(canonical.cache_read_tokens, 25);

        let anthropic_usage = canonical.to_anthropic();
        assert_eq!(anthropic_usage["input_tokens"], 8);
        assert_eq!(anthropic_usage["output_tokens"], 87);
        assert_eq!(anthropic_usage["cache_read_input_tokens"], 25);
    }

    #[test]
    fn response_preserves_extra_fields() {
        let anthropic_resp = json!({
            "id": "msg_123",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4",
            "content": [{"type": "text", "text": "Hello"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 20,
                "cache_read_input_tokens": 5,
                "cache_creation_input_tokens": 0
            },
            "custom_metadata_field": "preserved",
            "billing_tag": 42
        });

        let canonical = Response::try_from(WirePayload::Anthropic(&anthropic_resp)).unwrap();
        assert_eq!(canonical.extra.get("custom_metadata_field").unwrap(), "preserved");
        assert_eq!(canonical.extra.get("billing_tag").unwrap(), 42);

        let exported = canonical.to_anthropic().unwrap();
        assert_eq!(exported["custom_metadata_field"], "preserved");
        assert_eq!(exported["billing_tag"], 42);
    }

    #[test]
    fn request_roundtrip_with_extra_fields() {
        let anthropic_req = json!({
            "model": "claude-sonnet-4",
            "messages": [
                {"role": "user", "content": "Hello", "metadata_turn": "turn1"}
            ],
            "stream": true,
            "max_tokens": 1024,
            "user_id": "usr_999"
        });

        let canonical = Request::try_from(WirePayload::Anthropic(&anthropic_req)).unwrap();
        assert_eq!(canonical.extra.get("user_id").unwrap(), "usr_999");
        assert_eq!(canonical.messages[0].extra.get("metadata_turn").unwrap(), "turn1");

        let exported = canonical.to_anthropic().unwrap();
        assert_eq!(exported["user_id"], "usr_999");
        assert_eq!(exported["user_id"], "usr_999");
    }

    #[test]
    fn response_to_openai_preserves_thinking_and_generates_id() {
        let resp = Response {
            id: String::new(),
            model: "claude-3-7-sonnet".to_string(),
            content: vec![
                ContentBlock::Thinking { thinking: "Let me think...".to_string(), signature: None, extra: Map::new() },
                ContentBlock::Text { text: "Here is the answer.".to_string(), extra: Map::new() },
            ],
            stop_reason: Some("end_turn".to_string()),
            usage: Usage::default(),
            extra: Map::new(),
        };

        let out = resp.to_openai(OpenAiApiVersion::ChatCompletions).unwrap();
        assert!(out["id"].as_str().unwrap().starts_with("chatcmpl-"));
        assert_ne!(out["id"].as_str().unwrap(), "chatcmpl-");
        let msg = &out["choices"][0]["message"];
        assert_eq!(msg["reasoning_content"], "Let me think...");
        assert_eq!(msg["content"], "Here is the answer.");
    }

    #[test]
    fn response_to_anthropic_empty_content_fallback() {
        let resp = Response {
            id: String::new(),
            model: "claude-3-7-sonnet".to_string(),
            content: Vec::new(),
            stop_reason: Some("end_turn".to_string()),
            usage: Usage::default(),
            extra: Map::new(),
        };

        let out = resp.to_anthropic().unwrap();
        assert!(out["id"].as_str().unwrap().starts_with("msg_"));
        assert_ne!(out["id"].as_str().unwrap(), "msg_");
        assert_eq!(out["content"][0]["type"], "text");
        assert_eq!(out["content"][0]["text"], "");
    }

    #[test]
    fn request_to_gemini_interactions_format() {
        let req = Request {
            model: "gemini-2.5-pro".to_string(),
            messages: vec![
                Message {
                    role: "user".to_string(),
                    content: vec![ContentBlock::Text { text: "Hello".to_string(), extra: Map::new() }],
                    extra: Map::new(),
                },
                Message {
                    role: "assistant".to_string(),
                    content: vec![
                        ContentBlock::Thinking {
                            thinking: "Let me check".to_string(),
                            signature: Some("sig_1".to_string()),
                            extra: Map::new(),
                        },
                        ContentBlock::ToolUse {
                            id: "call_1".to_string(),
                            name: "get_weather".to_string(),
                            input: json!({"city": "Paris"}),
                            extra: Map::new(),
                        },
                    ],
                    extra: Map::new(),
                },
                Message {
                    role: "user".to_string(),
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "call_1".to_string(),
                        content: json!({"temp": 20}),
                        is_error: None,
                        extra: Map::new(),
                    }],
                    extra: Map::new(),
                },
            ],
            system: Some("You are a helpful assistant.".to_string()),
            max_tokens: None,
            temperature: None,
            stream: false,
            tools: vec![Tool {
                name: "get_weather".to_string(),
                description: Some("Get weather".to_string()),
                input_schema: json!({"type": "object"}),
                extra: Map::new(),
            }],
            extra: Map::new(),
        };

        let gemini_json = req.to_gemini().unwrap();
        let gemini_json = req.to_gemini().unwrap();
        assert_eq!(gemini_json["model"], "gemini-2.5-pro");
        assert_eq!(gemini_json["system_instruction"], "You are a helpful assistant.");
        let input = gemini_json["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "user_input");
        assert_eq!(input[0]["content"][0]["text"], "Hello");
        assert_eq!(input[1]["type"], "thought");
        assert_eq!(input[1]["signature"], "sig_1");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["name"], "get_weather");
        assert_eq!(input[3]["type"], "function_result");
        assert_eq!(input[3]["call_id"], "call_1");
        assert_eq!(input[3]["name"], "get_weather");
        assert_eq!(input[3]["result"][0]["text"], "{\"temp\":20}");
        assert_eq!(gemini_json["tools"][0]["function_declarations"][0]["name"], "get_weather");
    }

    #[test]
    fn request_from_openai_and_gemini_wire() {
        let openai_req = json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "system", "content": "Be concise."},
                {"role": "user", "content": "Hi"}
            ],
            "max_tokens": 100
        });

        let from_openai = Request::try_from(WirePayload::OpenAi(&openai_req, OpenAiApiVersion::ChatCompletions)).unwrap();
        assert_eq!(from_openai.model, "gpt-4o");
        assert_eq!(from_openai.system, Some("Be concise.".to_string()));
        assert_eq!(from_openai.messages.len(), 1);
        assert_eq!(from_openai.max_tokens, Some(100));

        let gemini_req = json!({
            "model": "gemini-3.8-flash",
            "input": [
                {"type": "user_input", "content": [{"type": "text", "text": "Hi from user"}]}
            ]
        });

        let from_gemini = Request::try_from(WirePayload::Gemini(&gemini_req)).unwrap();
        assert_eq!(from_gemini.model, "gemini-3.8-flash");
        assert_eq!(from_gemini.messages[0].role, "user");
        assert_eq!(match &from_gemini.messages[0].content[0] {
            ContentBlock::Text { text, .. } => text,
            _ => panic!("Expected text block"),
        }, "Hi from user");
        assert_eq!(match &from_gemini.messages[0].content[0] {
            ContentBlock::Text { text, .. } => text,
            _ => panic!("Expected text block"),
        }, "Hi from user");
    }

    #[test]
    fn openai_tool_role_message_creates_only_tool_result() {
        let openai_req = json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "user", "content": "What is the weather?"},
                {"role": "assistant", "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "get_weather", "arguments": "{}"}}]},
                {"role": "tool", "tool_call_id": "call_1", "content": "Sunny 22C"}
            ],
            "max_output_tokens": 500
        });

        let req = Request::try_from(WirePayload::OpenAi(&openai_req, OpenAiApiVersion::ChatCompletions)).unwrap();
        assert_eq!(req.max_tokens, Some(500));
        assert_eq!(req.messages.len(), 3);
        let tool_msg = &req.messages[2];
        assert_eq!(tool_msg.role, "tool");
        assert_eq!(tool_msg.content.len(), 1);
        match &tool_msg.content[0] {
            ContentBlock::ToolResult { tool_use_id, content, .. } => {
                assert_eq!(tool_use_id, "call_1");
                assert_eq!(content.as_str().unwrap(), "Sunny 22C");
            }
            _ => panic!("Expected ToolResult"),
        }
    }
}
