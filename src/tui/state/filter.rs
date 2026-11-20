/// Filter `models` by case-insensitive contains of `filter`, return all matches.
pub fn filter_suggestions<'a>(models: &'a [String], filter: &str) -> Vec<&'a str> {
    filter_names(models.iter().map(String::as_str), filter)
}

/// Does `name` contain `filter_lower` (already lower-cased)? Every narrowing
/// list in the TUI — model names, attribute paths — matches by this one rule.
pub(crate) fn matches_filter(name: &str, filter_lower: &str) -> bool {
    filter_lower.is_empty() || name.to_lowercase().contains(filter_lower)
}

/// Case-insensitive substring filter over any stream of names.
fn filter_names<'a>(names: impl Iterator<Item = &'a str>, filter: &str) -> Vec<&'a str> {
    let filter = filter.to_lowercase();
    names.filter(|name| matches_filter(name, &filter)).collect()
}

/// Filter a provider's cached model list by `filter` (unknown provider → empty).
/// Shared by the Routes target field and the Models panel's Test Model pin.
pub fn provider_model_suggestions<'a>(
    catalog: &'a std::collections::HashMap<String, Vec<crate::catalog::CatalogModel>>,
    provider: &str,
    filter: &str,
) -> Vec<&'a str> {
    let models = catalog.get(provider).map(|v| v.as_slice()).unwrap_or(&[]);
    filter_names(models.iter().map(|m| m.name.as_str()), filter)
}

/// Every model known across providers, deduplicated and sorted.
/// Feeds the global-route pattern suggestions; empty when no catalog is cached.
pub fn all_models(
    catalog: &std::collections::HashMap<String, Vec<crate::catalog::CatalogModel>>,
) -> Vec<String> {
    let mut models: Vec<String> = catalog.values().flatten().map(|m| m.name.clone()).collect();
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
                vec![
                    crate::catalog::CatalogModel::name_only("zeta"),
                    crate::catalog::CatalogModel::name_only("shared"),
                ],
            ),
            (
                "b".to_string(),
                vec![
                    crate::catalog::CatalogModel::name_only("shared"),
                    crate::catalog::CatalogModel::name_only("alpha"),
                ],
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
