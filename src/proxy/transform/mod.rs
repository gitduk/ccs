mod models;
mod request;
mod response;
mod stream;

pub use models::{anthropic_to_openai_models, openai_to_anthropic_models};
pub use request::{
    anthropic_to_openai_request, map_anthropic_model, openai_to_anthropic_request, to_openai,
};
pub use response::{anthropic_to_openai_response, openai_to_anthropic_response};
pub use stream::{anthropic_stream_to_openai, openai_stream_to_anthropic};

/// Map an OpenAI chat `finish_reason` to an Anthropic `stop_reason`.
/// Unknown values pass through unchanged.
fn chat_finish_to_anthropic_stop(reason: &str) -> &str {
    match reason {
        "stop" => "end_turn",
        "length" => "max_tokens",
        "tool_calls" => "tool_use",
        other => other,
    }
}

/// Map an OpenAI Responses API `status` to an Anthropic `stop_reason`.
fn response_status_to_anthropic_stop(status: &str) -> &'static str {
    match status {
        "incomplete" => "max_tokens",
        _ => "end_turn",
    }
}

/// Map an Anthropic `stop_reason` to an OpenAI chat `finish_reason`.
fn anthropic_stop_to_chat_finish(reason: Option<&str>) -> &'static str {
    match reason {
        Some("max_tokens") => "length",
        Some("tool_use") => "tool_calls",
        _ => "stop",
    }
}

/// Map an Anthropic `stop_reason` to an OpenAI Responses API `status`.
fn anthropic_stop_to_response_status(reason: Option<&str>) -> &'static str {
    match reason {
        Some("max_tokens") => "incomplete",
        _ => "completed",
    }
}
