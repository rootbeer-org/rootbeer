use rootbeer_tree::Writer;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use tempfile::TempDir;

/// The tree an archive holds, gathered in memory as its entries arrive in
/// whatever order the archive keeps them. Contents wait in a scratch
/// directory, so nothing an archive names is ever a path on disk. Writing it
/// out gives the one rootbeer-tree stream for it.
pub(crate) struct Tree {
    scratch: TempDir,
    /// Each entry by its path's components, which sort the way the stream
    /// must list them.
    nodes: BTreeMap<Vec<String>, Node>,
    spilled: u64,
}

#[derive(Clone)]
enum Node {
    Directory,
    Symlink(String),
    File {
        is_executable: bool,
        attributes: BTreeMap<String, Vec<u8>>,
        contents: PathBuf,
        size: u64,
    },
}

impl Tree {
    pub(crate) fn new() -> Result<Tree, String> {
        Ok(Tree {
            scratch: tempfile::tempdir().map_err(|error| error.to_string())?,
            nodes: BTreeMap::new(),
            spilled: 0,
        })
    }

    /// A directory, which may already be there. Archives often leave a
    /// directory out and name only what's inside it, so `place` makes those.
    pub(crate) fn directory(&mut self, path: &str) -> Result<(), String> {
        let components = components(path)?;
        if components.is_empty() {
            return Ok(());
        }

        self.place(&components, path)?;
        match self.nodes.get(&components) {
            None => {
                self.nodes.insert(components, Node::Directory);
                Ok(())
            }
            Some(Node::Directory) => Ok(()),
            Some(_) => Err(format!("{path} is in the archive twice")),
        }
    }

    pub(crate) fn symlink(&mut self, path: &str, target: &str) -> Result<(), String> {
        self.insert(path, Node::Symlink(target.to_string()))
    }

    /// A file whose contents `fill` writes, returning how many bytes it
    /// holds. Only whether `mode` is executable is kept.
    pub(crate) fn file_with(
        &mut self,
        path: &str,
        mode: u32,
        fill: impl FnOnce(&mut File) -> Result<(), String>,
    ) -> Result<u64, String> {
        self.spilled = self.spilled.saturating_add(1);
        let contents = self.scratch.path().join(self.spilled.to_string());
        let at = |error: io::Error| format!("{path}: {error}");
        let mut file = File::create_new(&contents).map_err(at)?;
        fill(&mut file)?;
        let size = file.metadata().map_err(at)?.len();

        let node = Node::File {
            is_executable: mode & 0o111 != 0,
            attributes: BTreeMap::new(),
            contents,
            size,
        };
        self.insert(path, node)?;
        Ok(size)
    }

    pub(crate) fn file(
        &mut self,
        path: &str,
        mode: u32,
        contents: &mut dyn Read,
    ) -> Result<u64, String> {
        self.file_with(path, mode, |file| {
            io::copy(contents, file)
                .map(drop)
                .map_err(|error| format!("{path}: {error}"))
        })
    }

    /// A second name for a file the tree already holds. The original is found
    /// by its own path, never through a symlink.
    pub(crate) fn hard_link(&mut self, path: &str, original: &str) -> Result<(), String> {
        let found = self.nodes.get(&components(original)?);
        let Some(file @ Node::File { .. }) = found.cloned() else {
            return Err(format!(
                "{path} links to {original}, which isn't a file it holds"
            ));
        };

        self.insert(path, file)
    }

    /// Code signature attributes for a file the tree holds. Anything else is
    /// skipped, since a tree records attributes only on files and only these.
    pub(crate) fn set_attributes(
        &mut self,
        path: &str,
        attributes: &[(String, Vec<u8>)],
    ) -> Result<(), String> {
        let Some(Node::File {
            attributes: kept, ..
        }) = self.nodes.get_mut(&components(path)?)
        else {
            return Ok(());
        };

        let signatures = attributes
            .iter()
            .filter(|(name, _)| name.starts_with(rootbeer_tree::KEPT));
        kept.extend(signatures.cloned());
        Ok(())
    }

    /// The one package at the top of the tree, as a disk image holding an
    /// installer keeps it.
    pub(crate) fn only_package(&self) -> Result<File, String> {
        let packages = self
            .nodes
            .iter()
            .filter_map(|(components, node)| match (components.as_slice(), node) {
                ([name], Node::File { contents, .. }) if name.ends_with(".pkg") => Some(contents),
                _ => None,
            })
            .collect::<Vec<_>>();

        let [package] = packages.as_slice() else {
            return Err(format!(
                "expected one package in the image, found {}",
                packages.len()
            ));
        };

        File::open(package).map_err(|error| format!("{}: {error}", package.display()))
    }

    /// Writes the tree as its rootbeer-tree stream.
    pub(crate) fn write(&self, out: impl Write) -> Result<(), String> {
        let mut writer = Writer::new(out).map_err(|error| error.to_string())?;
        for (components, node) in &self.nodes {
            let path = components.join("/");
            let path = path.as_bytes();
            let written = match node {
                Node::Directory => writer.directory(path),
                Node::Symlink(target) => writer.symlink(path, target.as_bytes()),
                Node::File {
                    is_executable,
                    attributes,
                    contents,
                    size,
                } => {
                    let mut file = File::open(contents)
                        .map_err(|error| format!("{}: {error}", contents.display()))?;
                    let attributes = attributes
                        .iter()
                        .map(|(name, value)| (name.clone(), value.clone()))
                        .collect::<Vec<_>>();
                    writer.file(path, *is_executable, &attributes, *size, &mut file)
                }
            };

            written.map_err(|error| error.to_string())?;
        }

        writer.finish().map(drop).map_err(|error| error.to_string())
    }

    fn insert(&mut self, path: &str, node: Node) -> Result<(), String> {
        let components = components(path)?;
        if components.is_empty() {
            return Err("an archive entry has no name".into());
        }

        self.place(&components, path)?;
        if self.nodes.contains_key(&components) {
            return Err(format!("{path} is in the archive twice"));
        }

        self.nodes.insert(components, node);
        Ok(())
    }

    /// Makes each directory above `components`, which must be a directory
    /// wherever the archive already named it.
    fn place(&mut self, components: &[String], path: &str) -> Result<(), String> {
        let parents = (1..components.len()).filter_map(|end| components.get(..end));
        for parent in parents {
            match self.nodes.get(parent) {
                None => {
                    self.nodes.insert(parent.to_vec(), Node::Directory);
                }
                Some(Node::Directory) => {}
                Some(_) => {
                    return Err(format!(
                        "{path} lands under {}, which isn't a directory",
                        parent.join("/")
                    ));
                }
            }
        }

        Ok(())
    }
}

/// The components of a path inside the archive, ignoring a leading `/` or
/// `./` since archives write both. A path that climbs out is refused.
fn components(path: &str) -> Result<Vec<String>, String> {
    let mut components = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err(format!("{path} leaves the archive")),
            part if part.contains('\0') => return Err(format!("{path:?} holds a NUL")),
            part => components.push(part.to_string()),
        }
    }

    Ok(components)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use rootbeer_tree::{Kind, Reader};

    /// What a tree writes, as each entry's path and kind.
    pub(crate) fn entries(tree: &Tree) -> Vec<(String, Kind)> {
        let mut stream = Vec::new();
        tree.write(&mut stream).unwrap();
        let mut reader = Reader::new(stream.as_slice()).unwrap();
        let mut entries = Vec::new();
        while let Some(entry) = reader.entry().unwrap() {
            entries.push((String::from_utf8(entry.path).unwrap(), entry.kind));
        }

        entries
    }

    #[test]
    fn entries_in_any_order_write_the_canonical_stream() {
        let mut tree = Tree::new().unwrap();
        tree.file("./b/tool", 0o4755, &mut "#!".as_bytes()).unwrap();
        tree.symlink("a-c", "b").unwrap();
        tree.directory("a/").unwrap();
        tree.hard_link("a/again", "b/tool").unwrap();
        let signature = [
            ("com.apple.cs.CodeSignature".to_string(), b"signed".to_vec()),
            ("com.apple.quarantine".to_string(), b"q".to_vec()),
        ];
        tree.set_attributes("b/tool", &signature).unwrap();

        let tool = |attributes| Kind::File {
            is_executable: true,
            attributes,
            size: 2,
        };
        let signed = vec![("com.apple.cs.CodeSignature".to_string(), b"signed".to_vec())];
        assert_eq!(
            entries(&tree),
            [
                ("a".to_string(), Kind::Directory),
                ("a/again".to_string(), tool(Vec::new())),
                (
                    "a-c".to_string(),
                    Kind::Symlink {
                        target: b"b".to_vec()
                    }
                ),
                ("b".to_string(), Kind::Directory),
                ("b/tool".to_string(), tool(signed)),
            ]
        );
    }

    #[test]
    fn nothing_lands_outside_or_under_what_isnt_a_directory() {
        let mut tree = Tree::new().unwrap();
        tree.file("a/file", 0o644, &mut "".as_bytes()).unwrap();
        tree.symlink("link", "/tmp").unwrap();

        let cases = [
            tree.file("../escape", 0o644, &mut "".as_bytes())
                .unwrap_err(),
            tree.file("a/../../escape", 0o644, &mut "".as_bytes())
                .unwrap_err(),
            tree.file("link/planted", 0o644, &mut "".as_bytes())
                .unwrap_err(),
            tree.file("a/file/inner", 0o644, &mut "".as_bytes())
                .unwrap_err(),
            tree.file("a/file", 0o644, &mut "".as_bytes()).unwrap_err(),
            tree.directory("a/file").unwrap_err(),
            tree.hard_link("stolen", "link/passwd").unwrap_err(),
            tree.symlink("./", "x").unwrap_err(),
        ];
        let expected = [
            "leaves the archive",
            "leaves the archive",
            "isn't a directory",
            "isn't a directory",
            "twice",
            "twice",
            "isn't a file it holds",
            "has no name",
        ];
        for (error, expected) in cases.iter().zip(expected) {
            assert!(error.contains(expected), "{error}");
        }
    }
}
