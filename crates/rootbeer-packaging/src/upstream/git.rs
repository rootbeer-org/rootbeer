use std::collections::BTreeMap;

/// Lists only tags, peeled to what they point at, so the server filters out branches and
/// GitHub's pull request refs.
pub(super) const LS_REFS: &[u8] =
    b"0014command=ls-refs\n00010009peel\n001aref-prefix refs/tags/\n0000";

/// Maps each tag in an `ls-refs` response to the commit it points at.
pub(super) fn parse_tags(body: &[u8]) -> Result<BTreeMap<String, String>, String> {
    let mut tags = BTreeMap::new();
    let mut rest = body;
    loop {
        let (length, tail) = rest.split_at_checked(4).ok_or("truncated git response")?;
        let length = std::str::from_utf8(length)
            .ok()
            .and_then(|length| usize::from_str_radix(length, 16).ok())
            .ok_or("invalid git packet length")?;
        if length == 0 {
            return Ok(tags);
        }
        if length < 4 {
            return Err("unexpected git packet before the end of the ref list".into());
        }
        let (line, tail) = tail
            .split_at_checked(length - 4)
            .ok_or("truncated git response")?;
        rest = tail;

        let line = std::str::from_utf8(line)
            .map_err(|_| "git ref is not UTF-8")?
            .trim_end_matches('\n');
        if let Some(error) = line.strip_prefix("ERR ") {
            return Err(format!("git server: {error}"));
        }
        let mut fields = line.split(' ');
        let (Some(object), Some(name)) = (fields.next(), fields.next()) else {
            return Err(format!("invalid git ref `{line}`"));
        };
        let Some(tag) = name.strip_prefix("refs/tags/") else {
            continue;
        };
        let commit = fields
            .find_map(|field| field.strip_prefix("peeled:"))
            .unwrap_or(object);
        if !matches!(commit.len(), 40 | 64) || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("{tag}: invalid object ID `{commit}`"));
        }
        tags.insert(tag.to_string(), commit.to_ascii_lowercase());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(line: &str) -> String {
        format!("{:04x}{line}", line.len() + 4)
    }

    #[test]
    fn annotated_tags_resolve_to_their_peeled_commit() {
        let (tag, commit, light) = ("a".repeat(40), "b".repeat(40), "c".repeat(40));
        let body = [
            packet(&format!("{tag} refs/tags/v1.0 peeled:{commit}\n")),
            packet(&format!("{light} refs/tags/v0.9\n")),
            "0000".into(),
        ]
        .concat();
        let tags = parse_tags(body.as_bytes()).unwrap();
        assert_eq!(tags["v1.0"], commit);
        assert_eq!(tags["v0.9"], light);
    }

    #[test]
    fn truncated_and_error_responses_are_rejected() {
        let line = packet(&format!("{} refs/tags/v1\n", "a".repeat(40)));
        assert!(parse_tags(line.as_bytes()).is_err(), "missing flush");
        assert!(parse_tags(&line.as_bytes()[..20]).is_err());
        let error = parse_tags(format!("{}0000", packet("ERR access denied\n")).as_bytes());
        assert!(error.unwrap_err().contains("access denied"));
        let short = packet("abc refs/tags/v1\n") + "0000";
        assert!(parse_tags(short.as_bytes()).is_err());
    }
}
