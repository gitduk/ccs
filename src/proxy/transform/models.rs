use serde_json::{Value, json};

/// Convert a Gemini `GET /v1beta/models` response to Anthropic format.
/// Gemini entries use `models/<id>` style names with no `id` field, so the
/// `models/` prefix is stripped to expose the bare model id.
pub fn gemini_to_anthropic_models(gemini: &Value) -> Value {
    let models = gemini["models"].as_array().cloned().unwrap_or_default();
    let data: Vec<Value> = models
        .iter()
        .map(|m| {
            let raw = m["name"].as_str().unwrap_or("").to_string();
            let id = raw.strip_prefix("models/").unwrap_or(&raw).to_string();
            let display = m
                .get("display_name")
                .and_then(|d| d.as_str())
                .unwrap_or(&id)
                .to_string();
            json!({
                "type": "model",
                "id": id,
                "display_name": display,
                "created_at": "1970-01-01T00:00:00Z",
            })
        })
        .collect();
    json!({
        "data": data,
        "has_more": false,
        "first_id": data.first().and_then(|m| m["id"].as_str()).unwrap_or(""),
        "last_id": data.last().and_then(|m| m["id"].as_str()).unwrap_or(""),
    })
}

/// Convert an Anthropic GET /v1/models response to OpenAI format.
pub fn anthropic_to_openai_models(anthropic: &Value) -> Value {
    let models = anthropic["data"].as_array().cloned().unwrap_or_default();
    let data: Vec<Value> = models
        .iter()
        .map(|m| {
            let id = m["id"].as_str().unwrap_or("unknown").to_string();
            json!({
                "id": id,
                "object": "model",
                "created": 0,
                "owned_by": "anthropic",
            })
        })
        .collect();
    json!({
        "object": "list",
        "data": data,
    })
}

/// Convert an OpenAI GET /v1/models response to Anthropic format.
pub fn openai_to_anthropic_models(openai: &Value) -> Value {
    let models = openai["data"].as_array().cloned().unwrap_or_default();
    let data: Vec<Value> = models
        .iter()
        .map(|m| {
            let id = m["id"].as_str().unwrap_or("unknown").to_string();
            json!({
                "type": "model",
                "id": id,
                "display_name": id,
                "created_at": "1970-01-01T00:00:00Z",
            })
        })
        .collect();
    let first_id = data
        .first()
        .and_then(|m| m["id"].as_str())
        .unwrap_or("")
        .to_string();
    let last_id = data
        .last()
        .and_then(|m| m["id"].as_str())
        .unwrap_or("")
        .to_string();
    json!({
        "data": data,
        "has_more": false,
        "first_id": first_id,
        "last_id": last_id,
    })
}
