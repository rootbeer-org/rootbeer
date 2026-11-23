use crate::order::{Order, check_attributes, check_target, malformed, show};
use crate::{Error, LONGEST_NAME, LONGEST_VALUE, MAGIC, MOST_ATTRIBUTES};
use data_encoding::HEXLOWER;
use rootbeer_drv::Sha256;
use sha2::Digest;
use std::io::{self, Read};

/// One entry of a tree. A file's contents follow it in the stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: Vec<u8>,
    pub kind: Kind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Directory,
    Symlink {
        target: Vec<u8>,
    },
    File {
        is_executable: bool,
        attributes: Vec<(String, Vec<u8>)>,
        size: u64,
    },
}

/// Reads a stream, accepting only the canonical encoding of a tree and
/// hashing every byte it reads.
pub struct Reader<R: Read> {
    inner: R,
    hasher: sha2::Sha256,
    order: Order,
    /// The bytes of the last file's contents not read yet.
    pending: u64,
    is_ended: bool,
}

/// The contents of the file `Reader::entry` last returned.
pub struct Contents<'a, R: Read> {
    reader: &'a mut Reader<R>,
}

/// The digest of the tree a stream encodes, once all of it is read and found
/// canonical.
pub fn digest(stream: impl Read) -> Result<Sha256, Error> {
    let mut reader = Reader::new(stream)?;
    while reader.entry()?.is_some() {}
    reader.finish()
}

impl<R: Read> Reader<R> {
    pub fn new(inner: R) -> Result<Reader<R>, Error> {
        let mut reader = Reader {
            inner,
            hasher: sha2::Sha256::new(),
            order: Order::default(),
            pending: 0,
            is_ended: false,
        };

        let mut magic = [0; MAGIC.len()];
        reader.fill(&mut magic)?;
        if magic != MAGIC {
            return Err(malformed("the stream isn't rootbeer-tree-v1".into()));
        }

        Ok(reader)
    }

    /// The next entry, or none once the end record is read. Contents the
    /// caller left unread are skipped.
    pub fn entry(&mut self) -> Result<Option<Entry>, Error> {
        io::copy(&mut self.contents(), &mut io::sink()).map_err(|error| match error.kind() {
            io::ErrorKind::UnexpectedEof => malformed("the stream ends inside a file".into()),
            _ => Error::Stream(error),
        })?;
        if self.is_ended {
            return Ok(None);
        }

        let mut tag = [0];
        self.fill(&mut tag)?;
        let [tag] = tag;
        let kind = match tag {
            b'e' => return self.end().map(|()| None),
            b'd' => None,
            b'l' => Some(None),
            b'f' => Some(Some(false)),
            b'x' => Some(Some(true)),
            _ => return Err(malformed(format!("a record starts with {tag:#04x}"))),
        };

        let path = self.bytes(LONGEST_NAME)?;
        let kind = match kind {
            None => Kind::Directory,
            Some(None) => {
                let target = self.bytes(LONGEST_NAME)?;
                check_target(&path, &target)?;
                Kind::Symlink { target }
            }
            Some(Some(is_executable)) => {
                let count = self.u32()?;
                if count > MOST_ATTRIBUTES {
                    return Err(malformed(format!("{} has {count} attributes", show(&path))));
                }

                let mut attributes = Vec::new();
                for _ in 0..count {
                    let name = String::from_utf8(self.bytes(LONGEST_NAME)?).map_err(|_| {
                        malformed(format!(
                            "{} has an attribute name that isn't UTF-8",
                            show(&path)
                        ))
                    })?;
                    attributes.push((name, self.bytes(LONGEST_VALUE)?));
                }

                check_attributes(&path, &attributes)?;
                let size = self.u64()?;
                Kind::File {
                    is_executable,
                    attributes,
                    size,
                }
            }
        };

        self.order.admit(&path, matches!(kind, Kind::Directory))?;
        if let Kind::File { size, .. } = kind {
            self.pending = size;
        }

        Ok(Some(Entry { path, kind }))
    }

    pub fn contents(&mut self) -> Contents<'_, R> {
        Contents { reader: self }
    }

    /// The tree's digest. The whole stream must have been read.
    pub fn finish(self) -> Result<Sha256, Error> {
        if !self.is_ended {
            return Err(malformed("the stream was read only in part".into()));
        }

        let digest = HEXLOWER.encode(&self.hasher.finalize());
        Sha256::try_from(digest).map_err(|error| malformed(error.to_string()))
    }

    // Nothing may follow the end record, or one tree would have many streams.
    fn end(&mut self) -> Result<(), Error> {
        let mut trailing = [0];
        let read = loop {
            match self.inner.read(&mut trailing) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                read => break read.map_err(Error::Stream)?,
            }
        };

        if read != 0 {
            return Err(malformed("bytes follow the end record".into()));
        }

        self.is_ended = true;
        Ok(())
    }

    fn fill(&mut self, buffer: &mut [u8]) -> Result<(), Error> {
        self.inner
            .read_exact(buffer)
            .map_err(|error| match error.kind() {
                io::ErrorKind::UnexpectedEof => malformed("the stream ends early".into()),
                _ => Error::Stream(error),
            })?;
        self.hasher.update(&*buffer);
        Ok(())
    }

    fn u32(&mut self) -> Result<u32, Error> {
        let mut bytes = [0; 4];
        self.fill(&mut bytes)?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn u64(&mut self) -> Result<u64, Error> {
        let mut bytes = [0; 8];
        self.fill(&mut bytes)?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn bytes(&mut self, limit: u32) -> Result<Vec<u8>, Error> {
        let length = self.u32()?;
        if length > limit {
            return Err(malformed(format!("a record holds {length} bytes")));
        }

        let length = usize::try_from(length)
            .map_err(|_| malformed(format!("a record holds {length} bytes")))?;
        let mut bytes = vec![0; length];
        self.fill(&mut bytes)?;
        Ok(bytes)
    }
}

impl<R: Read> Read for Contents<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let reader = &mut *self.reader;
        let limit = usize::try_from(reader.pending).unwrap_or(usize::MAX);
        let length = limit.min(buffer.len());
        let buffer = buffer.get_mut(..length).unwrap_or_default();
        if buffer.is_empty() {
            return Ok(0);
        }

        let count = reader.inner.read(buffer)?;
        if count == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }

        let read = buffer.get(..count).unwrap_or_default();
        reader.hasher.update(read);
        reader.pending = reader
            .pending
            .saturating_sub(u64::try_from(count).unwrap_or(u64::MAX));
        Ok(count)
    }
}
