/// Filter `models` by case-insensitive contains of `filter`, return all matches.
pub fn filter_suggestions<'a>(models: &'a [String], filter: &str) -> Vec<&'a str> {
    let f = filter.to_lowercase();
    models
        .iter()
        .filter(|m| f.is_empty() || m.to_lowercase().contains(&f))
        .map(|s| s.as_str())
        .collect()
}
