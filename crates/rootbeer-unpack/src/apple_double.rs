// The entry in an AppleDouble file that holds the Finder's info, which macOS
// follows with the file's extended attributes.
const FINDER_INFO: u32 = 9;
const FINDER_INFO_SIZE: usize = 32;

const MAGIC: &[u8] = &[0x00, 0x05, 0x16, 0x07];
const ATTRIBUTES_MAGIC: &[u8] = b"ATTR";

// A signed file with nowhere to hold its signature, like a jar in an app
// bundle, keeps it in these. No others are kept, since the store holds only
// these.
const KEPT: &str = "com.apple.cs.";

/// The extended attributes an AppleDouble file records for the file beside
/// it, keeping only code signatures.
pub(crate) fn attributes(bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    if bytes.get(..4) != Some(MAGIC) {
        return Err("it isn't an AppleDouble file".into());
    }

    let count = usize::from(u16_at(bytes, 24)?);
    let mut finder_info = None;
    for index in 0..count {
        let at = index
            .checked_mul(12)
            .and_then(|offset| offset.checked_add(26));
        let at = at.ok_or("it lists too many entries")?;
        if u32_at(bytes, at)? == FINDER_INFO {
            finder_info = Some(usize_at(bytes, at.saturating_add(4))?);
        }
    }

    // A file without extended attributes can have no Finder info.
    let Some(finder_info) = finder_info else {
        return Ok(Vec::new());
    };

    let header = finder_info
        .checked_add(FINDER_INFO_SIZE)
        .and_then(|end| end.checked_next_multiple_of(4))
        .ok_or("its Finder info lies past the end")?;
    if bytes.get(header..header.saturating_add(4)) != Some(ATTRIBUTES_MAGIC) {
        return Ok(Vec::new());
    }

    let count = u16_at(bytes, header.saturating_add(34))?;
    let mut at = header.saturating_add(36);
    let mut kept = Vec::new();
    for _ in 0..count {
        let offset = usize_at(bytes, at)?;
        let length = usize_at(bytes, at.saturating_add(4))?;
        let name_length = usize::from(*bytes.get(at.saturating_add(10)).ok_or("it ends early")?);
        let name = bytes
            .get(at.saturating_add(11)..at.saturating_add(11).saturating_add(name_length))
            .ok_or("it ends inside an attribute's name")?;
        let name = name.strip_suffix(&[0]).unwrap_or(name);
        let name = std::str::from_utf8(name).map_err(|_| "an attribute's name isn't UTF-8")?;

        at = at
            .saturating_add(11)
            .saturating_add(name_length)
            .checked_next_multiple_of(4)
            .ok_or("it lists too many attributes")?;
        if !name.starts_with(KEPT) {
            continue;
        }

        let value = bytes
            .get(offset..offset.saturating_add(length))
            .ok_or_else(|| format!("{name} lies past the end"))?;
        kept.push((name.to_string(), value.to_vec()));
    }

    Ok(kept)
}

/// The file an AppleDouble entry describes, beside it or mirrored under
/// `__MACOSX`.
pub(crate) fn described(name: &str) -> Option<String> {
    let name = name.strip_prefix("__MACOSX/").unwrap_or(name);
    let (parent, file) = name.rsplit_once('/').unwrap_or(("", name));
    let file = file.strip_prefix("._").filter(|file| !file.is_empty())?;
    Some(match parent {
        "" => file.to_string(),
        parent => format!("{parent}/{file}"),
    })
}

fn u16_at(bytes: &[u8], at: usize) -> Result<u16, String> {
    let field = bytes.get(at..).and_then(<[u8]>::first_chunk);
    Ok(u16::from_be_bytes(*field.ok_or("it ends early")?))
}

fn u32_at(bytes: &[u8], at: usize) -> Result<u32, String> {
    let field = bytes.get(at..).and_then(<[u8]>::first_chunk);
    Ok(u32::from_be_bytes(*field.ok_or("it ends early")?))
}

fn usize_at(bytes: &[u8], at: usize) -> Result<usize, String> {
    usize::try_from(u32_at(bytes, at)?).map_err(|error| error.to_string())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// An AppleDouble file holding `attributes`, laid out as macOS writes it.
    pub(crate) fn apple_double(attributes: &[(&str, &[u8])]) -> Vec<u8> {
        // The attributes' header starts at 84 and is 36 bytes long.
        const RECORDS: usize = 120;
        let be32 = |value: usize| u32::try_from(value).unwrap().to_be_bytes();
        let record = |name: &str| {
            11usize
                .saturating_add(name.len())
                .saturating_add(1)
                .next_multiple_of(4)
        };

        let mut offset = attributes
            .iter()
            .fold(RECORDS, |end, (name, _)| end.saturating_add(record(name)));
        let mut records = Vec::new();
        let mut data = Vec::new();
        for (name, value) in attributes {
            let mut entry = Vec::new();
            entry.extend_from_slice(&be32(offset));
            entry.extend_from_slice(&be32(value.len()));
            entry.extend_from_slice(&[0, 0, u8::try_from(name.len()).unwrap().saturating_add(1)]);
            entry.extend_from_slice(name.as_bytes());
            entry.resize(record(name), 0);
            records.extend_from_slice(&entry);
            data.extend_from_slice(value);
            offset = offset.saturating_add(value.len());
        }

        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&[0, 2, 0, 0]);
        bytes.extend_from_slice(b"Mac OS X        ");
        bytes.extend_from_slice(&[0, 1]);
        bytes.extend_from_slice(&FINDER_INFO.to_be_bytes());
        bytes.extend_from_slice(&be32(50));
        bytes.extend_from_slice(&be32(0));
        bytes.resize(84, 0);
        bytes.extend_from_slice(ATTRIBUTES_MAGIC);
        bytes.resize(118, 0);
        bytes.extend_from_slice(&u16::try_from(attributes.len()).unwrap().to_be_bytes());
        bytes.extend_from_slice(&records);
        bytes.extend_from_slice(&data);
        bytes
    }

    #[test]
    fn only_code_signatures_are_read() {
        let bytes = apple_double(&[
            ("com.apple.quarantine", b"0081;"),
            ("com.apple.cs.CodeDirectory", b"hashes"),
            ("com.apple.cs.CodeSignature", b"signed"),
        ]);

        assert_eq!(
            attributes(&bytes).unwrap(),
            [
                ("com.apple.cs.CodeDirectory".to_string(), b"hashes".to_vec()),
                ("com.apple.cs.CodeSignature".to_string(), b"signed".to_vec()),
            ]
        );

        let cut = bytes.get(..bytes.len().saturating_sub(3)).unwrap();
        assert!(attributes(cut).is_err());
        assert!(attributes(b"not one").is_err());
    }

    #[test]
    fn entries_describe_the_file_beside_them() {
        assert_eq!(
            described("__MACOSX/Tool.app/Contents/._Info.plist").as_deref(),
            Some("Tool.app/Contents/Info.plist")
        );
        assert_eq!(
            described("Tool.app/._tool").as_deref(),
            Some("Tool.app/tool")
        );
        assert_eq!(described("._Tool.app").as_deref(), Some("Tool.app"));
        assert_eq!(described("__MACOSX/Tool.app/"), None);
    }
}
