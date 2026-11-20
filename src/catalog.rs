//! Upstream model catalog.
//!
//! One provider's `/models` listing: every model's id plus the scalar fields
//! the upstream attached to it, flattened to dotted paths (`pricing.prompt`,
//! `limit.context`) so a column can render any of them by path. Fetched by
//! [`fetch`], stored by `db::replace_catalog`, rendered by the Models panel.

use std::time::Duration;

use serde_json::{Map, Value};

use crate::config::Provider;
use crate::proxy::forwarder;

/// Timeout for one catalog request.
const FETCH_TIMEOUT_SECS: u64 = 10;
/// Largest catalog body read. A broken or hostile upstream must not be able to
/// balloon memory and the stored catalog in one response.
const MAX_CATALOG_BYTES: usize = 8 * 1024 * 1024;
/// Longer strings are dropped: catalog descriptions bloat every row without
/// being renderable in a column.
const MAX_ATTR_STRING_CHARS: usize = 120;
/// Most attributes kept per model: one pathological catalog entry must not
/// bloat every load. Keys arrive sorted, so a cut keeps a stable prefix.
const MAX_ATTRS_PER_MODEL: usize = 48;
/// Deepest attribute path kept, in dotted segments. `cost.text.input.cache.read`
/// (five) is the deepest shape seen; anything further is detail no column shows.
const MAX_ATTR_DEPTH: usize = 5;

/// One catalog entry: the model id a request names, plus the attributes.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogModel {
    pub name: String,
    /// Scalar attributes keyed by dotted path.
    pub attrs: Map<String, Value>,
}

impl CatalogModel {
    /// A row known only by name — the upstream supply of attributes ran out,
    /// or the name came from traffic rather than a catalog.
    pub fn name_only(name: &str) -> Self {
        Self {
            name: name.to_string(),
            attrs: Map::new(),
        }
    }

    /// The attribute at `path`, if the upstream attached one.
    pub fn attr(&self, path: &str) -> Option<&Value> {
        self.attrs.get(path)
    }

    fn from_json(name: &str, entry: &Value) -> Self {
        let mut attrs = Map::new();
        if let Some(map) = entry.as_object() {
            for (key, value) in map {
                // The row already carries its name; repeating it as an
                // attribute would only offer it as a display column.
                if key == "id" || key == "name" {
                    continue;
                }
                if attrs.len() >= MAX_ATTRS_PER_MODEL {
                    break;
                }
                flatten_into(key, 1, value, &mut attrs);
            }
        }
        Self {
            name: name.to_string(),
            attrs,
        }
    }
}

/// A column the Models panel renders from catalog attributes: the user picks
/// the paths and units in the `a` dialog, and the DB keeps them per provider.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelAttr {
    /// Column heading, shown in the `a` dialog's enabled list.
    pub label: String,
    /// Dotted paths into a model's attributes. One renders that value; two
    /// render as `in/out` (`$0.15/$0.6` for a prompt/completion price pair).
    pub paths: Vec<String>,
    /// Multiplier to the displayed unit — `1e6` turns a per-token price into
    /// $/Mtok, `1` keeps a value that already is one.
    pub scale: f64,
    /// Rendered before each value (`$`).
    pub prefix: String,
}

fn flatten_into(path: &str, depth: usize, value: &Value, attrs: &mut Map<String, Value>) {
    match value {
        Value::Bool(_) | Value::Number(_) => {
            attrs.insert(path.to_string(), value.clone());
        }
        Value::String(s) if s.chars().count() <= MAX_ATTR_STRING_CHARS => {
            attrs.insert(path.to_string(), value.clone());
        }
        Value::Object(map) => {
            if depth >= MAX_ATTR_DEPTH {
                return;
            }
            for (key, child) in map {
                flatten_into(&format!("{path}.{key}"), depth + 1, child, attrs);
            }
        }
        Value::String(_) | Value::Null | Value::Array(_) => {}
    }
}

/// Parse a `/models` body: OpenAI/Anthropic catalogs list `data[].id`, Gemini's
/// lists `models[].name` with a `models/` prefix. `None` when the body carries
/// neither shape.
pub fn parse_catalog(json: &Value) -> Option<Vec<CatalogModel>> {
    if let Some(models) = json["models"].as_array() {
        return Some(
            models
                .iter()
                .filter_map(|m| {
                    let name = m["name"].as_str()?;
                    Some(CatalogModel::from_json(
                        name.strip_prefix("models/").unwrap_or(name),
                        m,
                    ))
                })
                .collect(),
        );
    }
    let arr = json["data"].as_array()?;
    Some(
        arr.iter()
            .filter_map(|v| v["id"].as_str().map(|id| CatalogModel::from_json(id, v)))
            .collect(),
    )
}

/// Fetch a provider's catalog. `None` when the request failed, answered
/// non-2xx, or returned a body of neither known shape.
///
/// A `/models` 404 under the configured path retries at the bare
/// scheme+host root: some relays nest the chat endpoint but serve the catalog
/// only at the root.
pub async fn fetch(client: &reqwest::Client, provider: &Provider) -> Option<Vec<CatalogModel>> {
    let api_key = provider.resolve_api_key().ok()?;
    let req = forwarder::models_request(client, provider, &api_key)
        .timeout(Duration::from_secs(FETCH_TIMEOUT_SECS));
    let Ok(r) = req.send().await else {
        return None;
    };

    if r.status() == reqwest::StatusCode::NOT_FOUND
        && let Some(root) = forwarder::root_base_url(&provider.base_url)
    {
        let fallback = forwarder::models_request_at(client, provider, &api_key, &root)
            .timeout(Duration::from_secs(FETCH_TIMEOUT_SECS));
        if let Ok(r2) = fallback.send().await {
            return parse_response(r2).await;
        }
    }

    parse_response(r).await
}

async fn parse_response(r: reqwest::Response) -> Option<Vec<CatalogModel>> {
    use futures::StreamExt;

    if !r.status().is_success() {
        return None;
    }
    if r.content_length()
        .is_some_and(|len| len > MAX_CATALOG_BYTES as u64)
    {
        tracing::warn!("Catalog body of {MAX_CATALOG_BYTES}+ bytes refused; keeping the old one");
        return None;
    }

    let mut body = Vec::new();
    let mut stream = r.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.ok()?;
        if body.len() + chunk.len() > MAX_CATALOG_BYTES {
            tracing::warn!("Catalog body over {MAX_CATALOG_BYTES} bytes refused");
            return None;
        }
        body.extend_from_slice(&chunk);
    }

    parse_catalog(&serde_json::from_slice::<Value>(&body).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn nested_scalars_flatten_to_dotted_paths() {
        let body = json!({"data": [{
            "id": "deepseek-v4.1-flash",
            "cost": {"text": {"input": {"price": 0.15}, "output": {"price": 0.6}}},
            "limit": {"context": 1000000}
        }]});
        let models = parse_catalog(&body).unwrap();
        let m = &models[0];

        assert_eq!(m.name, "deepseek-v4.1-flash");
        assert_eq!(m.attr("cost.text.input.price"), Some(&json!(0.15)));
        assert_eq!(m.attr("cost.text.output.price"), Some(&json!(0.6)));
        assert_eq!(m.attr("limit.context"), Some(&json!(1000000)));
    }

    #[test]
    fn arrays_long_strings_and_the_name_key_stay_out() {
        let body = json!({"data": [{
            "id": "m",
            "description": "x".repeat(MAX_ATTR_STRING_CHARS + 1),
            "supported_parameters": ["tools", "stream"],
            "pricing": {"prompt": "0.000002"}
        }]});
        let m = &parse_catalog(&body).unwrap()[0];

        assert_eq!(m.attr("id"), None);
        assert_eq!(m.attr("description"), None);
        assert_eq!(m.attr("supported_parameters"), None);
        assert_eq!(m.attr("pricing.prompt"), Some(&json!("0.000002")));
    }

    #[test]
    fn one_model_keeps_a_bounded_number_of_attributes() {
        let mut entry: serde_json::Map<String, Value> = (0..MAX_ATTRS_PER_MODEL + 12)
            .map(|i| (format!("field{i:03}"), json!(i)))
            .collect();
        entry.insert("id".into(), json!("m"));
        let body = json!({"data": [Value::Object(entry.clone())]});

        let m = &parse_catalog(&body).unwrap()[0];

        assert_eq!(m.attrs.len(), MAX_ATTRS_PER_MODEL);
    }

    #[test]
    fn gemini_catalog_drops_the_models_prefix() {
        let body =
            json!({"models": [{"name": "models/gemini-flash-latest", "inputTokenLimit": 1048576}]});
        let m = &parse_catalog(&body).unwrap()[0];

        assert_eq!(m.name, "gemini-flash-latest");
        assert_eq!(m.attr("inputTokenLimit"), Some(&json!(1048576)));
    }

    #[test]
    fn a_body_of_neither_shape_parses_to_none() {
        assert!(parse_catalog(&json!({"error": "unauthorized"})).is_none());
    }

    #[tokio::test]
    async fn an_oversized_catalog_body_is_refused() {
        use axum::Router;
        use axum::routing::get;

        async fn huge_models() -> Vec<u8> {
            let mut body = br#"{"data":[{"id":"m","blob":""#.to_vec();
            body.extend(std::iter::repeat_n(b'x', MAX_CATALOG_BYTES));
            body.extend_from_slice(br#""}]}"#);
            body
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/models", get(huge_models)))
                .await
                .unwrap();
        });

        let provider = crate::config::test_support::provider(&format!("http://{addr}"));

        assert!(fetch(&reqwest::Client::new(), &provider).await.is_none());
    }

    #[tokio::test]
    async fn fetch_falls_back_to_root_domain_on_404() {
        use axum::Router;
        use axum::extract::Path;
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        use axum::routing::get;

        async fn nested_404(Path(_rest): Path<String>) -> impl IntoResponse {
            StatusCode::NOT_FOUND
        }
        async fn root_models() -> impl IntoResponse {
            axum::Json(json!({
                "data": [{"id": "deepseek-v4-flash", "pricing": {"prompt": "0.0000001"}}]
            }))
        }

        let addr = crate::tester::tests::spawn_test_server(
            Router::new()
                // Two levels deep — proves the fallback goes straight to the
                // root, not just one segment up.
                .route("/{*rest}", get(nested_404))
                .route("/models", get(root_models)),
        )
        .await;

        let provider =
            crate::config::test_support::provider(&format!("http://{addr}/org123/anthropic/v1"));
        let models = fetch(&reqwest::Client::new(), &provider).await.unwrap();

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].name, "deepseek-v4-flash");
        assert_eq!(models[0].attr("pricing.prompt"), Some(&json!("0.0000001")));
    }
}
