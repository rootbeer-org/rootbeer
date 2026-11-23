use crate::{Error, KEPT, LONGEST_NAME, LONGEST_VALUE, MOST_ATTRIBUTES};
use std::cmp::Ordering;

/// Admits entries only in canonical order. The writer and the reader share
/// it, so neither can make or accept a second encoding of a tree.
#[derive(Default)]
pub(crate) struct Order {
    last: Option<Vec<u8>>,
    /// The directories from the top down to the last one admitted, which
    /// are the only ones a later entry can still be inside.
    directories: Vec<Vec<u8>>,
}

impl Order {
    pub(crate) fn admit(&mut self, path: &[u8], is_directory: bool) -> Result<(), Error> {
        check_path(path)?;
        if let Some(last) = &self.last
            && compare(path, last) != Ordering::Greater
        {
            return Err(malformed(format!(
                "{} comes after {}",
                show(path),
                show(last)
            )));
        }

        let parent = path
            .iter()
            .rposition(|byte| *byte == b'/')
            .and_then(|slash| path.get(..slash))
            .unwrap_or_default();
        while self
            .directories
            .last()
            .is_some_and(|directory| directory.as_slice() != parent)
        {
            self.directories.pop();
        }

        if !parent.is_empty() && self.directories.is_empty() {
            return Err(malformed(format!(
                "{} isn't inside a directory recorded before it",
                show(path)
            )));
        }

        if is_directory {
            self.directories.push(path.to_vec());
        }

        self.last = Some(path.to_vec());
        Ok(())
    }
}

/// Paths sort by their components, so a directory's entries all come before
/// a sibling whose name only extends the directory's.
fn compare(a: &[u8], b: &[u8]) -> Ordering {
    a.split(|byte| *byte == b'/')
        .cmp(b.split(|byte| *byte == b'/'))
}

fn check_path(path: &[u8]) -> Result<(), Error> {
    let is_plain = path
        .split(|byte| *byte == b'/')
        .all(|part| !matches!(part, b"" | b"." | b"..") && !part.contains(&0));

    if !is_plain || !fits(path, LONGEST_NAME) {
        return Err(malformed(format!("{} isn't a plain path", show(path))));
    }

    Ok(())
}

pub(crate) fn check_target(path: &[u8], target: &[u8]) -> Result<(), Error> {
    if target.is_empty() || target.contains(&0) || !fits(target, LONGEST_NAME) {
        return Err(malformed(format!(
            "{} links to {}",
            show(path),
            show(target)
        )));
    }

    Ok(())
}

pub(crate) fn check_attributes(path: &[u8], attributes: &[(String, Vec<u8>)]) -> Result<(), Error> {
    let is_sorted = attributes
        .windows(2)
        .all(|pair| matches!(pair, [(a, _), (b, _)] if a < b));
    let is_kept = attributes.iter().all(|(name, value)| {
        name.starts_with(KEPT) && fits(name.as_bytes(), LONGEST_NAME) && fits(value, LONGEST_VALUE)
    });
    let is_few = u32::try_from(attributes.len()).is_ok_and(|count| count <= MOST_ATTRIBUTES);

    if !is_sorted || !is_kept || !is_few {
        return Err(malformed(format!(
            "{} has attributes other than sorted {KEPT}* ones",
            show(path)
        )));
    }

    Ok(())
}

fn fits(bytes: &[u8], limit: u32) -> bool {
    u32::try_from(bytes.len()).is_ok_and(|length| length <= limit)
}

pub(crate) fn show(path: &[u8]) -> String {
    format!("{:?}", String::from_utf8_lossy(path))
}

pub(crate) fn malformed(reason: String) -> Error {
    Error::Malformed(reason)
}
