use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

/// The directory an archive unpacks into. Every entry lands inside it, and
/// nothing is written through a symlink an earlier entry made.
pub(crate) struct Tree {
    root: PathBuf,
}

impl Tree {
    pub(crate) fn new(root: &Path) -> Result<Tree, String> {
        fs::create_dir_all(root).map_err(|error| format!("{}: {error}", root.display()))?;
        Ok(Tree {
            root: root.to_path_buf(),
        })
    }

    pub(crate) fn directory(&self, path: &str, mode: u32) -> Result<(), String> {
        let Some(path) = self.place(path)? else {
            return Ok(());
        };

        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Err(format!("{} is in the archive twice", path.display())),
            Err(_) => fs::DirBuilder::new()
                .mode(0o755)
                .create(&path)
                .map_err(|error| format!("{}: {error}", path.display()))?,
        }

        // The owner keeps write access so later entries can land inside.
        fs::set_permissions(&path, fs::Permissions::from_mode(mode & 0o755 | 0o700))
            .map_err(|error| format!("{}: {error}", path.display()))
    }

    pub(crate) fn file(
        &self,
        path: &str,
        mode: u32,
        contents: &mut dyn Read,
    ) -> Result<u64, String> {
        let mut file = self.create(path, mode)?;
        io::copy(contents, &mut file).map_err(|error| format!("{path}: {error}"))
    }

    pub(crate) fn create(&self, path: &str, mode: u32) -> Result<File, String> {
        let Some(path) = self.place(path)? else {
            return Err("an archive entry has no name".into());
        };

        let at = |error: io::Error| format!("{}: {error}", path.display());
        let file = File::create_new(&path).map_err(at)?;
        file.set_permissions(fs::Permissions::from_mode(mode & 0o755))
            .map_err(at)?;
        Ok(file)
    }

    pub(crate) fn symlink(&self, path: &str, target: &str) -> Result<(), String> {
        let Some(path) = self.place(path)? else {
            return Err("an archive entry has no name".into());
        };

        symlink(target, &path).map_err(|error| format!("{}: {error}", path.display()))
    }

    // The original must be a file this tree already holds, reached without a
    // symlink, so a link can't reach outside it.
    pub(crate) fn hard_link(&self, path: &str, original: &str) -> Result<(), String> {
        let Some(found) = self.inside(original)? else {
            return Err("an archive entry has no name".into());
        };

        let mut directory = self.root.clone();
        let relative = found
            .strip_prefix(&self.root)
            .unwrap_or(&found)
            .to_path_buf();
        for component in relative.components() {
            directory.push(component);
            let metadata = fs::symlink_metadata(&directory);
            let is_followable = match &metadata {
                Ok(metadata) if directory == found => metadata.is_file(),
                Ok(metadata) => metadata.is_dir(),
                Err(_) => false,
            };

            if !is_followable {
                return Err(format!(
                    "{path} links to {original}, which isn't a file it holds"
                ));
            }
        }

        let Some(path) = self.place(path)? else {
            return Err("an archive entry has no name".into());
        };

        fs::hard_link(&found, &path).map_err(|error| format!("{}: {error}", path.display()))
    }

    /// Where `path` lands, with each directory above it made. None is the
    /// root itself.
    fn place(&self, path: &str) -> Result<Option<PathBuf>, String> {
        let Some(placed) = self.inside(path)? else {
            return Ok(None);
        };

        let mut directory = self.root.clone();
        let parents = placed
            .strip_prefix(&self.root)
            .ok()
            .and_then(Path::parent)
            .into_iter()
            .flat_map(Path::components);

        for component in parents {
            directory.push(component);
            match fs::symlink_metadata(&directory) {
                Ok(metadata) if metadata.is_dir() => {}
                Ok(_) => {
                    return Err(format!(
                        "{path} lands under {}, which isn't a directory",
                        directory.display()
                    ));
                }
                Err(_) => fs::DirBuilder::new()
                    .mode(0o755)
                    .create(&directory)
                    .map_err(|error| format!("{}: {error}", directory.display()))?,
            }
        }

        Ok(Some(placed))
    }

    /// `path` joined to the root, ignoring leading `/` and `./` since
    /// archives write both. A path that climbs out is refused.
    fn inside(&self, path: &str) -> Result<Option<PathBuf>, String> {
        let mut placed = self.root.clone();
        let mut is_root = true;
        for part in path.split('/') {
            match part {
                "" | "." => {}
                ".." => return Err(format!("{path} leaves the directory it unpacks into")),
                part => {
                    placed.push(part);
                    is_root = false;
                }
            }
        }

        Ok((!is_root).then_some(placed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_stay_inside_and_never_follow_a_symlink() {
        let directory = tempfile::tempdir().unwrap();
        let tree = Tree::new(directory.path()).unwrap();

        tree.file("./a/b/file", 0o4777, &mut "hi".as_bytes())
            .unwrap();
        let mode = fs::metadata(directory.path().join("a/b/file"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o7777, 0o755);

        assert!(tree.file("../escape", 0o644, &mut "".as_bytes()).is_err());
        assert!(
            tree.file("a/../../escape", 0o644, &mut "".as_bytes())
                .is_err()
        );

        tree.symlink("link", "/tmp").unwrap();
        let error = tree
            .file("link/planted", 0o644, &mut "".as_bytes())
            .unwrap_err();
        assert!(error.contains("isn't a directory"), "{error}");
        let error = tree.hard_link("stolen", "link/passwd").unwrap_err();
        assert!(error.contains("isn't a file it holds"), "{error}");
        tree.hard_link("again", "a/b/file").unwrap();

        assert!(tree.file("a/b/file", 0o644, &mut "".as_bytes()).is_err());
        tree.directory("a/b", 0o700).unwrap();
        assert!(tree.directory("a/b/file", 0o755).is_err());
    }
}
