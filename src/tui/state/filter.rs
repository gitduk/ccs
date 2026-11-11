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

/// Every model known across providers, deduplicated and sorted.
/// Feeds the global-route pattern suggestions; empty when no catalog is cached.
pub fn all_models(provider_models: &std::collections::HashMap<String, Vec<String>>) -> Vec<String> {
    let mut models: Vec<String> = provider_models.values().flatten().cloned().collect();
    models.sort_unstable();
    models.dedup();
    models
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn all_models_merges_dedupes_and_sorts_every_provider() {
        let catalogs = HashMap::from([
            (
                "a".to_string(),
                vec!["zeta".to_string(), "shared".to_string()],
            ),
            (
                "b".to_string(),
                vec!["shared".to_string(), "alpha".to_string()],
            ),
            ("empty".to_string(), vec![]),
        ]);

        assert_eq!(all_models(&catalogs), vec!["alpha", "shared", "zeta"]);
    }

    #[test]
    fn all_models_of_no_catalogs_is_empty() {
        assert!(all_models(&HashMap::new()).is_empty());
    }
}
