use serde_json::{Value, json};

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
