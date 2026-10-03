//! Version marker for the on-disk layout under [`crate::root_dir`], so a newer
//! rootbeer migrates an older layout instead of guessing at it.

use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// v2: owned by root and written through the setuid `rb-store` helper.
pub const VERSION: u32 = 2;
const FILE: &str = "layout.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layout {
    pub version: u32,
    /// The installed `rb-store`, recorded by the setup that installed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub helper: Option<Helper>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Helper {
    pub release: u32,
    pub protocols: Vec<u32>,
}

/// Returns the recorded layout, or `None` when no marker exists yet.
pub fn read(root: &Path) -> io::Result<Option<Layout>> {
    let bytes = match fs::read(root.join(FILE)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };

    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(io::Error::other)
}

/// Records [`VERSION`] atomically, without a helper.
pub fn write(root: &Path) -> io::Result<()> {
    let layout = Layout {
        version: VERSION,
        helper: None,
    };
    let json = serde_json::to_string(&layout).map_err(io::Error::other)?;
    let tmp = root.join(format!(".{FILE}.{}", std::process::id()));
    fs::write(&tmp, format!("{json}\n"))?;
    fs::rename(tmp, root.join(FILE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_the_current_version() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(read(root.path()).unwrap(), None);

        write(root.path()).unwrap();
        assert_eq!(read(root.path()).unwrap().unwrap().version, VERSION);
    }

    #[test]
    fn reads_markers_with_and_without_a_helper() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join(FILE), r#"{"version":1}"#).unwrap();
        assert_eq!(read(root.path()).unwrap().unwrap().helper, None);

        let marker = r#"{"version":2,"helper":{"release":3,"protocols":[1,2]}}"#;
        fs::write(root.path().join(FILE), marker).unwrap();
        let helper = read(root.path()).unwrap().unwrap().helper.unwrap();
        assert_eq!((helper.release, helper.protocols), (3, vec![1, 2]));
    }
}
