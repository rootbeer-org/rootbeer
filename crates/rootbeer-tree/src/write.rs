use crate::order::{Order, check_attributes, check_target, malformed, show};
use crate::{Error, MAGIC};
use std::io::{self, Read, Write};

/// Writes a tree's one encoding. Entries must come in canonical order, and
/// anything else is refused rather than written.
pub struct Writer<W: Write> {
    inner: W,
    order: Order,
}

impl<W: Write> Writer<W> {
    pub fn new(mut inner: W) -> Result<Writer<W>, Error> {
        inner.write_all(MAGIC).map_err(Error::Stream)?;
        Ok(Writer {
            inner,
            order: Order::default(),
        })
    }

    pub fn directory(&mut self, path: &[u8]) -> Result<(), Error> {
        self.order.admit(path, true)?;
        self.put(b"d")?;
        self.bytes(path)
    }

    pub fn symlink(&mut self, path: &[u8], target: &[u8]) -> Result<(), Error> {
        check_target(path, target)?;
        self.order.admit(path, false)?;
        self.put(b"l")?;
        self.bytes(path)?;
        self.bytes(target)
    }

    /// Writes a file of exactly `size` bytes from `contents`.
    pub fn file(
        &mut self,
        path: &[u8],
        is_executable: bool,
        attributes: &[(String, Vec<u8>)],
        size: u64,
        contents: &mut impl Read,
    ) -> Result<(), Error> {
        check_attributes(path, attributes)?;
        self.order.admit(path, false)?;
        self.put(if is_executable { b"x" } else { b"f" })?;
        self.bytes(path)?;

        let count = u32::try_from(attributes.len())
            .map_err(|_| malformed(format!("{} has too many attributes", show(path))))?;
        self.put(&count.to_le_bytes())?;
        for (name, value) in attributes {
            self.bytes(name.as_bytes())?;
            self.bytes(value)?;
        }

        self.put(&size.to_le_bytes())?;
        let copied = io::copy(&mut contents.take(size), &mut self.inner).map_err(Error::Stream)?;
        if copied != size {
            return Err(malformed(format!(
                "{} held {copied} bytes, not {size}",
                show(path)
            )));
        }

        Ok(())
    }

    pub fn finish(mut self) -> Result<W, Error> {
        self.put(b"e")?;
        self.inner.flush().map_err(Error::Stream)?;
        Ok(self.inner)
    }

    fn put(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.inner.write_all(bytes).map_err(Error::Stream)
    }

    // Lengths were checked against the format's limits, which fit a u32.
    fn bytes(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let length = u32::try_from(bytes.len())
            .map_err(|_| malformed(format!("{} is too long", show(bytes))))?;
        self.put(&length.to_le_bytes())?;
        self.put(bytes)
    }
}
