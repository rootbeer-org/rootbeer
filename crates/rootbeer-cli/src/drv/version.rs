//! How versions order, and how a request selects one among them.

use std::cmp::Ordering;

/// Dotted versions compared part by part, numbers numerically.
pub(super) fn compare(a: &str, b: &str) -> Ordering {
    let parts = |version: &str| {
        version
            .split(|byte: char| !byte.is_ascii_alphanumeric())
            .map(|part| part.parse::<u64>().map_err(|_| part.to_string()))
            .collect::<Vec<_>>()
    };

    parts(a).cmp(&parts(b))
}

/// The newest of `versions` that `asked` selects. That's the version itself,
/// the release line it starts, like 24.22.3 for 24, or a range like `^1.2`,
/// `~1.2.3`, or `>=1.2,<2`.
pub(super) fn resolve<'a>(
    versions: impl IntoIterator<Item = &'a str>,
    asked: &str,
) -> Option<&'a str> {
    let range = range(asked);
    let mut newest = None;
    for version in versions {
        if version == asked {
            return Some(version);
        }

        let is_selected = match &range {
            Some(bounds) => numbers(release(version))
                .is_some_and(|numbers| bounds.iter().all(|bound| bound.holds(&numbers))),
            None => version
                .strip_prefix(asked)
                .is_some_and(|rest| rest.starts_with('.')),
        };

        let is_newer = newest.is_none_or(|newest| compare(version, newest) == Ordering::Greater);
        if is_selected && is_newer {
            newest = Some(version);
        }
    }

    newest
}

/// One side of a range, against a version's numbers.
struct Bound {
    order: Ordering,
    is_inclusive: bool,
    numbers: Vec<u64>,
}

impl Bound {
    fn holds(&self, version: &[u64]) -> bool {
        let order = version.cmp(&self.numbers);
        order == self.order || (self.is_inclusive && order == Ordering::Equal)
    }
}

/// The bounds a range names, all of which a version must hold. None when
/// `asked` isn't a range, or isn't one rb can read.
fn range(asked: &str) -> Option<Vec<Bound>> {
    if !asked.starts_with(['^', '~', '<', '>', '=']) && !asked.contains(',') {
        return None;
    }

    let bound = |order, is_inclusive, numbers: Vec<u64>| Bound {
        order,
        is_inclusive,
        numbers: trimmed(numbers),
    };
    let at_least = |numbers| bound(Ordering::Greater, true, numbers);
    let below = |numbers| bound(Ordering::Less, false, numbers);

    let mut bounds = Vec::new();
    for term in asked.split(',').map(str::trim) {
        let split = |operator: &str| term.strip_prefix(operator).and_then(parts);
        if let Some(lowest) = split("^") {
            // A caret holds the leftmost part that isn't zero, so ^0.2.3
            // stays below 0.3.
            let held = lowest
                .iter()
                .position(|&part| part != 0)
                .unwrap_or(lowest.len() - 1);
            bounds.push(below(next(&lowest, held)));
            bounds.push(at_least(lowest));
        } else if let Some(lowest) = split("~") {
            let held = (lowest.len() - 1).min(1);
            bounds.push(below(next(&lowest, held)));
            bounds.push(at_least(lowest));
        } else if let Some(numbers) = split(">=") {
            bounds.push(at_least(numbers));
        } else if let Some(numbers) = split("<=") {
            bounds.push(bound(Ordering::Less, true, numbers));
        } else if let Some(numbers) = split(">") {
            bounds.push(bound(Ordering::Greater, false, numbers));
        } else if let Some(numbers) = split("<") {
            bounds.push(below(numbers));
        } else {
            bounds.push(bound(Ordering::Equal, true, split("=")?));
        }
    }

    Some(bounds)
}

/// The first version past every one that shares `numbers` up to `held`.
fn next(numbers: &[u64], held: usize) -> Vec<u64> {
    let mut next = numbers[..=held].to_vec();
    next[held] += 1;
    next
}

/// A version without its build or prerelease suffix, like 7.1.2 for 7.1.2-35.
fn release(version: &str) -> &str {
    version.split(['-', '+']).next().unwrap_or_default()
}

/// A plain dotted version's numbers, so `1.2` and `1.2.0` are equal.
fn numbers(version: &str) -> Option<Vec<u64>> {
    parts(version).map(trimmed)
}

fn parts(version: &str) -> Option<Vec<u64>> {
    version
        .split('.')
        .map(|part| {
            let is_number = !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
            is_number.then(|| part.parse().ok()).flatten()
        })
        .collect()
}

fn trimmed(mut numbers: Vec<u64>) -> Vec<u64> {
    while numbers.last() == Some(&0) {
        numbers.pop();
    }

    numbers
}

#[cfg(test)]
mod tests {
    use super::*;

    const VERSIONS: [&str; 9] = [
        "0.2.3", "0.2.9", "0.3.0", "1.2.0", "1.2.7", "1.10.1", "2.0.0", "2.4", "7.1.2-35",
    ];

    fn resolved(asked: &str) -> Option<&'static str> {
        resolve(VERSIONS, asked)
    }

    #[test]
    fn a_version_or_the_line_it_starts_is_selected() {
        assert_eq!(resolved("1.2.0"), Some("1.2.0"));
        assert_eq!(resolved("1"), Some("1.10.1"));
        assert_eq!(resolved("1.2"), Some("1.2.7"));
        assert_eq!(resolved("7.1"), Some("7.1.2-35"));

        // A version wins over the line it starts.
        assert_eq!(resolved("2.4"), Some("2.4"));
        assert_eq!(resolved("1.1"), None);
        assert_eq!(resolved("3"), None);
    }

    #[test]
    fn a_range_selects_the_newest_version_within_it() {
        assert_eq!(resolved("^1.2"), Some("1.10.1"));
        assert_eq!(resolved("^0.2.3"), Some("0.2.9"));
        assert_eq!(resolved("^0"), Some("0.3.0"));
        assert_eq!(resolved("~1.2.0"), Some("1.2.7"));
        assert_eq!(resolved("~1"), Some("1.10.1"));

        assert_eq!(resolved(">=1.2"), Some("7.1.2-35"));
        assert_eq!(resolved(">=1.2,<2"), Some("1.10.1"));
        assert_eq!(resolved(">1.2, <=2.0"), Some("2.0.0"));
        assert_eq!(resolved("<1"), Some("0.3.0"));
        assert_eq!(resolved("=2"), Some("2.0.0"));
        assert_eq!(resolved(">7.1.2"), None);

        // What rb can't read selects nothing.
        assert_eq!(resolved(">=one"), None);
        assert_eq!(resolved("1,2"), None);
        assert_eq!(resolved("^"), None);
    }

    #[test]
    fn versions_order_by_their_numbers() {
        assert_eq!(compare("1.10.0", "1.9.0"), Ordering::Greater);
        assert_eq!(compare("2026.2.5+262", "2026.2.5+100"), Ordering::Greater);
    }
}
