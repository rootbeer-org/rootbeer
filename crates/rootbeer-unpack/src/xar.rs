use bzip2::read::BzDecoder;
use flate2::read::ZlibDecoder;
use quick_xml::events::Event;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};

const MAGIC: &[u8] = b"xar!";

// A package's table of contents lists a few dozen files, so anything near
// this is corrupt.
const LONGEST_TOC: u64 = 64 * 1024 * 1024;

/// A xar archive, the container of a flat package.
pub(crate) struct Xar {
    file: BufReader<File>,
    heap: u64,
    entries: BTreeMap<String, Entry>,
}

struct Entry {
    is_directory: bool,
    data: Option<Data>,
}

#[derive(Default)]
struct Data {
    offset: u64,
    length: u64,
    encoding: String,
}

// A file element as the table of contents is read. Names can come after a
// directory's children, so paths are joined once the whole table is read.
#[derive(Default)]
struct Node {
    parent: Option<usize>,
    name: Option<String>,
    kind: String,
    data: Option<Data>,
}

impl Xar {
    pub(crate) fn open(file: File) -> Result<Xar, String> {
        let mut file = BufReader::new(file);

        let mut header = [0u8; 28];
        file.read_exact(&mut header)
            .map_err(|_| "the package is too short to be a xar archive")?;
        if header.get(..4) != Some(MAGIC) {
            return Err("the package isn't a xar archive".into());
        }

        let size = |start: usize, length: usize| {
            header
                .get(start..start.saturating_add(length))
                .map(|bytes| {
                    bytes
                        .iter()
                        .fold(0u64, |total, byte| total << 8 | u64::from(*byte))
                })
                .unwrap_or_default()
        };
        let (header_size, compressed, expanded) = (size(4, 2), size(8, 8), size(16, 8));
        if expanded > LONGEST_TOC {
            return Err(format!(
                "the package's table of contents is {expanded} bytes"
            ));
        }

        file.seek(SeekFrom::Start(header_size))
            .map_err(|error| error.to_string())?;
        let mut toc = String::new();
        ZlibDecoder::new((&mut file).take(compressed))
            .take(expanded)
            .read_to_string(&mut toc)
            .map_err(|error| format!("the package's table of contents is unreadable ({error})"))?;

        Ok(Xar {
            file,
            heap: header_size
                .checked_add(compressed)
                .ok_or("the package's header is corrupt")?,
            entries: entries(&toc)?,
        })
    }

    pub(crate) fn paths(&self) -> impl Iterator<Item = (&str, bool)> {
        self.entries
            .iter()
            .map(|(path, entry)| (path.as_str(), entry.is_directory))
    }

    pub(crate) fn has(&self, path: &str) -> bool {
        self.entries.contains_key(path)
    }

    /// The contents of the file at `path`, decoded.
    pub(crate) fn read(&mut self, path: &str) -> Result<Box<dyn Read + '_>, String> {
        let data = self
            .entries
            .get(path)
            .and_then(|entry| entry.data.as_ref())
            .ok_or_else(|| format!("the package has no file {path}"))?;

        let start = self
            .heap
            .checked_add(data.offset)
            .ok_or_else(|| format!("{path} lies past the end of the package"))?;
        self.file
            .seek(SeekFrom::Start(start))
            .map_err(|error| error.to_string())?;

        let stored = (&mut self.file).take(data.length);
        match data.encoding.as_str() {
            "application/octet-stream" | "" => Ok(Box::new(stored)),
            // xar names zlib streams gzip.
            "application/x-gzip" => Ok(Box::new(ZlibDecoder::new(stored))),
            "application/x-bzip2" => Ok(Box::new(BzDecoder::new(stored))),
            encoding => Err(format!("{path} is encoded as {encoding}")),
        }
    }
}

fn entries(toc: &str) -> Result<BTreeMap<String, Entry>, String> {
    let mut reader = quick_xml::Reader::from_str(toc);
    let mut nodes: Vec<Node> = Vec::new();
    let mut open: Vec<usize> = Vec::new();
    let mut elements: Vec<String> = Vec::new();

    loop {
        let event = reader
            .read_event()
            .map_err(|error| format!("the package's table of contents is malformed ({error})"))?;
        match event {
            Event::Start(element) => {
                let name = element.name().as_ref().to_string();
                if name == "file" {
                    open.push(nodes.len());
                    nodes.push(Node {
                        parent: open.iter().rev().nth(1).copied(),
                        ..Node::default()
                    });
                }
                elements.push(name);
            }
            Event::Empty(element) => {
                let is_encoding = element.name().as_ref() == "encoding";
                if let (true, Some(node)) = (
                    is_encoding,
                    open.last().and_then(|index| nodes.get_mut(*index)),
                ) {
                    let style = element
                        .try_get_attribute("style")
                        .map_err(|error| error.to_string())?
                        .map(|style| style.value.into_owned());
                    node.data.get_or_insert_with(Data::default).encoding =
                        style.unwrap_or_default();
                }
            }
            Event::Text(text) => {
                let Some(node) = open.last().and_then(|index| nodes.get_mut(*index)) else {
                    continue;
                };

                let text = text.xml10_content().trim().to_string();
                let number = || {
                    text.parse::<u64>()
                        .map_err(|_| format!("the package's table of contents holds {text:?}"))
                };
                let within = |depth: usize| elements.iter().rev().nth(depth).map(String::as_str);
                match (within(0), within(1)) {
                    // Some packagers write a file's name twice.
                    (Some("name"), Some("file")) => match &node.name {
                        Some(name) if *name != text => {
                            return Err(format!(
                                "a file in the package is named {name} and {text}"
                            ));
                        }
                        _ => node.name = Some(text),
                    },
                    (Some("type"), Some("file")) => node.kind = text,
                    (Some("offset"), Some("data")) => {
                        node.data.get_or_insert_with(Data::default).offset = number()?;
                    }
                    (Some("length"), Some("data")) => {
                        node.data.get_or_insert_with(Data::default).length = number()?;
                    }
                    _ => {}
                }
            }
            Event::End(element) => {
                if element.name().as_ref() == "file" {
                    open.pop();
                }
                elements.pop();
            }
            Event::Eof => break,
            _ => {}
        }
    }

    let paths = nodes
        .iter()
        .map(|node| path(&nodes, node))
        .collect::<Result<Vec<_>, _>>()?;

    let mut entries = BTreeMap::new();
    for (path, node) in paths.into_iter().zip(nodes) {
        let entry = Entry {
            is_directory: node.kind == "directory",
            data: node.data,
        };

        if entries.insert(path.clone(), entry).is_some() {
            return Err(format!("the package holds {path} twice"));
        }
    }

    Ok(entries)
}

fn path(nodes: &[Node], node: &Node) -> Result<String, String> {
    let mut names = Vec::new();
    let mut current = Some(node);
    while let Some(node) = current {
        let name = node
            .name
            .as_deref()
            .ok_or("a file in the package has no name")?;
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            return Err(format!("a file in the package is named {name:?}"));
        }

        names.push(name);
        current = node.parent.and_then(|parent| nodes.get(parent));
        if names.len() > nodes.len() {
            return Err("the package's table of contents loops".into());
        }
    }

    names.reverse();
    Ok(names.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_written_twice_or_after_children_still_make_paths() {
        let toc = r#"<?xml version="1.0"?>
<xar><toc>
 <checksum style="sha1"><offset>0</offset><size>20</size></checksum>
 <file id="1">
  <file id="2">
   <data><length>5</length><encoding style="application/octet-stream"/><offset>20</offset><size>5</size></data>
   <type>file</type><name>Payload</name><name>Payload</name>
  </file>
  <type>directory</type><name>Tool.pkg</name>
 </file>
</toc></xar>"#;
        let found = entries(toc).unwrap();
        let payload = found.get("Tool.pkg/Payload").unwrap();
        assert_eq!(payload.data.as_ref().unwrap().offset, 20);
        assert!(found.get("Tool.pkg").unwrap().is_directory);

        let renamed = toc.replace(
            "<name>Payload</name><name>Payload</name>",
            "<name>A</name><name>B</name>",
        );
        assert!(entries(&renamed).is_err());

        let climbing = toc.replace("<name>Tool.pkg</name>", "<name>..</name>");
        assert!(entries(&climbing).is_err());
    }
}
