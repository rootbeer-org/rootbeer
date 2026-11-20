use std::cmp::Ordering;

/// Dotted versions compared part by part, numbers numerically.
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    let parts = |version: &str| {
        version
            .split(|byte: char| !byte.is_ascii_alphanumeric())
            .map(|part| part.parse::<u64>().map_err(|_| part.to_string()))
            .collect::<Vec<_>>()
    };

    parts(a).cmp(&parts(b))
}
