/// Filter `models` by case-insensitive contains of `filter`, return all matches.
pub fn filter_suggestions<'a>(models: &'a [String], filter: &str) -> Vec<&'a str> {
    let f = filter.to_lowercase();
    models
        .iter()
        .filter(|m| f.is_empty() || m.to_lowercase().contains(&f))
        .map(|s| s.as_str())
        .collect()
}

/// Filter a provider's cached model list by `filter` (unknown provider → empty).
/// Shared by the Routes target field and the Models panel's Test Model pin.
pub fn provider_model_suggestions<'a>(
    provider_models: &'a std::collections::HashMap<String, Vec<String>>,
    provider: &str,
    filter: &str,
) -> Vec<&'a str> {
    let models = provider_models
        .get(provider)
        .map(|v| v.as_slice())
        .unwrap_or(&[]);
    filter_suggestions(models, filter)
}
