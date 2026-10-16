use super::{Error, io_at};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Bumped when the store's shape changes, so an older rb stops and runs a newer
/// one instead.
pub const LAYOUT: u64 = 1;

/// Bumped when `rb-helper` learns a command, so rb sets the store up again.
pub const HELPER_VERSION: u64 = 1;

const PATH: &str = "var/layout.json";

/// What the last `rb-helper setup` installed. Stores set up before the helper
/// was versioned read as helper 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct Layout {
    pub layout: u64,
    #[serde(default)]
    pub helper: u64,
}

impl Layout {
    pub const CURRENT: Layout = Layout {
        layout: LAYOUT,
        helper: HELPER_VERSION,
    };

    /// None when the store was never set up, or was set up before rb recorded
    /// its layout. Unknown fields are ignored, so a newer rb's file still says
    /// it's newer.
    pub fn read(root: &Path) -> Result<Option<Layout>, Error> {
        let path = root.join(PATH);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io_at(&path)(error)),
        };

        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| Error::Refused(format!("{} isn't a store layout", path.display())))
    }

    pub fn is_current(found: Option<Layout>) -> bool {
        found.is_some_and(|found| found.layout == LAYOUT && found.helper >= HELPER_VERSION)
    }

    pub fn write(root: &Path) -> Result<(), Error> {
        let var = root.join("var");
        let path = root.join(PATH);
        let mut file = tempfile::Builder::new()
            .prefix(".layout.")
            .tempfile_in(&var)
            .map_err(io_at(&var))?;

        let contents = serde_json::to_vec(&Layout::CURRENT)
            .map_err(|error| Error::Refused(error.to_string()))?;
        file.write_all(&contents).map_err(io_at(&path))?;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o644))
            .map_err(io_at(&path))?;
        file.as_file().sync_all().map_err(io_at(&path))?;
        file.persist(&path)
            .map_err(|error| io_at(&path)(error.error))?;
        Ok(())
    }
}
