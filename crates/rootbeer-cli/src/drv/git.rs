use std::collections::BTreeMap;
use std::io::Read;

// Gerrit hosts like go.googlesource.com advertise their review refs too.
const LIMIT: u64 = 64 << 20;
const SERVICE: &[u8] = b"# service=git-upload-pack";

/// A tag and the commit it names, through the annotated tag when it is one.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Tag {
    pub(super) name: String,
    pub(super) commit: String,
}

/// Every tag a repository advertises over git's smart HTTP protocol. A URL
/// without `.git` that isn't found is tried again with it.
pub(super) fn tags(agent: &ureq::Agent, repository: &str) -> Result<Vec<Tag>, String> {
    let advertise = |repository: &str| {
        agent
            .get(format!("{repository}/info/refs?service=git-upload-pack"))
            .call()
    };

    let mut response = advertise(repository);
    if matches!(response, Err(ureq::Error::StatusCode(404))) && !repository.ends_with(".git") {
        response = advertise(&format!("{repository}.git"));
    }

    let body = response
        .map_err(|error| format!("{repository}: {error}"))?
        .into_body();
    parse(body.into_with_config().limit(LIMIT).reader())
        .map_err(|error| format!("{repository}: {error}"))
}

/// The tags in a ref advertisement.
fn parse(mut reader: impl Read) -> Result<Vec<Tag>, String> {
    let service = packet(&mut reader)?;
    let service = service
        .as_deref()
        .map(|line| line.strip_suffix(b"\n").unwrap_or(line));
    if service != Some(SERVICE) {
        return Err("it isn't a smart HTTP git server".into());
    }

    if packet(&mut reader)?.is_some() {
        return Err("the service announcement isn't followed by a flush".into());
    }

    let mut direct = BTreeMap::new();
    let mut peeled = BTreeMap::new();
    while let Some(payload) = packet(&mut reader)? {
        let line = String::from_utf8_lossy(&payload);
        let line = line.strip_suffix('\n').unwrap_or(&line);

        // The first ref carries the server's capabilities after a NUL.
        let line = line.split('\0').next().unwrap_or_default();
        let (id, name) = line
            .split_once(' ')
            .filter(|(id, _)| is_object_id(id))
            .ok_or_else(|| format!("the ref line `{line}` is malformed"))?;

        let Some(tag) = name.strip_prefix("refs/tags/") else {
            continue;
        };

        match tag.strip_suffix("^{}") {
            Some(tag) => peeled.insert(tag.to_string(), id.to_string()),
            None => direct.insert(tag.to_string(), id.to_string()),
        };
    }

    let tags = direct
        .into_iter()
        .map(|(name, id)| Tag {
            commit: peeled.remove(&name).unwrap_or(id),
            name,
        })
        .collect();
    Ok(tags)
}

/// One pkt-line's payload, or none for a flush.
fn packet(reader: &mut impl Read) -> Result<Option<Vec<u8>>, String> {
    let mut length = [0; 4];
    reader
        .read_exact(&mut length)
        .map_err(|error| format!("the ref advertisement ends early: {error}"))?;

    let hex = String::from_utf8_lossy(&length);
    let length = Some(&hex)
        .filter(|hex| hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .and_then(|hex| usize::from_str_radix(hex, 16).ok())
        .ok_or_else(|| format!("`{hex}` isn't a pkt-line length"))?;
    if length == 0 {
        return Ok(None);
    }

    let size = length
        .checked_sub(4)
        .ok_or_else(|| format!("{length} isn't a pkt-line length"))?;
    let mut payload = vec![0; size];
    reader
        .read_exact(&mut payload)
        .map_err(|error| format!("the ref advertisement ends early: {error}"))?;
    Ok(Some(payload))
}

fn is_object_id(id: &str) -> bool {
    matches!(id.len(), 40 | 64)
        && id
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Trimmed from github.com/jqlang/jq.git, with jq-1.0 annotated.
    const ADVERTISEMENT: &[u8] = b"001e# service=git-upload-pack\n0000\
015b175b8e3c510ecf3b58debf866663a25e216da053 HEAD\0multi_ack thin-pack side-band side-band-64k ofs-delta shallow deepen-since deepen-not deepen-relative no-progress include-tag multi_ack_detailed allow-tip-sha1-in-want allow-reachable-sha1-in-want no-done symref=HEAD:refs/heads/master filter object-format=sha1 agent=git/github-41cc7379a75f-Linux\n\
003f175b8e3c510ecf3b58debf866663a25e216da053 refs/heads/master\n\
003e46d1ce2667253f1a34cd389b6d00c0288ab0276f refs/tags/1.6rc2\n\
003ec5d847e2d546456e44469a2a9159a3cb1959befa refs/tags/jq-1.0\n\
0041c53e001973b5d6d749cae20f739eaa3b674a1d92 refs/tags/jq-1.0^{}\n\
004071c2ab509a8628dbbad4bc7b3f98a64aa90d3297 refs/tags/jq-1.7.1\n\
004034f7186b86743a083a589741b6cea95293524108 refs/tags/jq-1.8.2\n\
0000";

    fn tag(name: &str, commit: &str) -> Tag {
        Tag {
            name: name.into(),
            commit: commit.into(),
        }
    }

    #[test]
    fn tags_name_the_commit_an_annotated_tag_peels_to() {
        assert_eq!(
            parse(ADVERTISEMENT).unwrap(),
            [
                tag("1.6rc2", "46d1ce2667253f1a34cd389b6d00c0288ab0276f"),
                tag("jq-1.0", "c53e001973b5d6d749cae20f739eaa3b674a1d92"),
                tag("jq-1.7.1", "71c2ab509a8628dbbad4bc7b3f98a64aa90d3297"),
                tag("jq-1.8.2", "34f7186b86743a083a589741b6cea95293524108"),
            ]
        );

        // Receivers treat a pkt-line the same with or without its newline.
        let bare = [b"001d# service=git-upload-pack0000", &ADVERTISEMENT[34..]].concat();
        assert_eq!(parse(bare.as_slice()), parse(ADVERTISEMENT));
    }

    #[test]
    fn a_malformed_or_truncated_advertisement_is_refused() {
        let malformed = [&ADVERTISEMENT[..34], b"0013not a ref line\n0000"].concat();
        assert_eq!(
            parse(malformed.as_slice()).unwrap_err(),
            "the ref line `not a ref line` is malformed"
        );

        let truncated = &ADVERTISEMENT[..ADVERTISEMENT.len() - 10];
        assert!(parse(truncated).unwrap_err().contains("ends early"));

        assert_eq!(
            parse(b"<!DOCTYPE html>".as_slice()).unwrap_err(),
            "`<!DO` isn't a pkt-line length"
        );
        assert_eq!(
            parse(b"0010# service=x\n0000".as_slice()).unwrap_err(),
            "it isn't a smart HTTP git server"
        );
    }
}
