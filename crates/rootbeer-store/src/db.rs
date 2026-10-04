use crate::{Error, Origin};
use rootbeer_drv::Key;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use std::collections::BTreeSet;
use std::path::Path;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS outputs (
    key TEXT PRIMARY KEY,
    entry TEXT NOT NULL,
    origin TEXT NOT NULL,
    digest TEXT
);
CREATE TABLE IF NOT EXISTS refs (
    referrer TEXT NOT NULL REFERENCES outputs (key),
    reference TEXT NOT NULL,
    PRIMARY KEY (referrer, reference)
);
";

pub(crate) fn open(path: &Path) -> Result<Connection, Error> {
    let connection = Connection::open(path)?;
    connection.pragma_update(None, "foreign_keys", true)?;
    connection.execute_batch(SCHEMA)?;
    Ok(connection)
}

pub(crate) fn open_read_only(path: &Path) -> Result<Connection, Error> {
    Ok(Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?)
}

pub(crate) fn register(
    connection: &mut Connection,
    key: &Key,
    entry: &str,
    origin: Origin,
    digest: Option<&str>,
    references: &BTreeSet<Key>,
) -> Result<(), Error> {
    let transaction = connection.transaction()?;
    transaction.execute(
        "INSERT INTO outputs (key, entry, origin, digest) VALUES (?1, ?2, ?3, ?4)",
        params![key.as_str(), entry, origin.as_str(), digest],
    )?;

    for reference in references {
        transaction.execute(
            "INSERT INTO refs (referrer, reference) VALUES (?1, ?2)",
            params![key.as_str(), reference.as_str()],
        )?;
    }

    transaction.commit()?;
    Ok(())
}

pub(crate) fn entry(connection: &Connection, key: &Key) -> Result<Option<String>, Error> {
    let entry = connection
        .query_row(
            "SELECT entry FROM outputs WHERE key = ?1",
            [key.as_str()],
            |row| row.get(0),
        )
        .optional()?;

    Ok(entry)
}

pub(crate) fn references(connection: &Connection, key: &Key) -> Result<BTreeSet<Key>, Error> {
    let mut statement = connection.prepare("SELECT reference FROM refs WHERE referrer = ?1")?;
    let rows = statement.query_map([key.as_str()], |row| row.get::<_, String>(0))?;

    rows.map(|row| {
        let text = row?;
        text.parse()
            .map_err(|error: rootbeer_drv::Error| Error::Database(error.to_string()))
    })
    .collect()
}
