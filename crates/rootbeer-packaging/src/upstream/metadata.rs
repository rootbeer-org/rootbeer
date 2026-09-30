use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use rootbeer_package::download::{http_post, http_request};
use rootbeer_store::hash_bytes;

const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_ATTEMPTS: usize = 3;

#[derive(Debug)]
enum RequestError {
    Retry(String),
    Permanent(String),
}

impl From<ureq::Error> for RequestError {
    fn from(error: ureq::Error) -> Self {
        match error {
            ureq::Error::Io(_)
            | ureq::Error::Timeout(_)
            | ureq::Error::HostNotFound
            | ureq::Error::ConnectionFailed
            | ureq::Error::Protocol(_)
            | ureq::Error::Decompress(_, _) => Self::Retry(error.to_string()),
            _ => Self::Permanent(error.to_string()),
        }
    }
}

#[derive(Deserialize, Serialize)]
struct Entry {
    url: String,
    etag: Option<String>,
    sha256: String,
    body: String,
}

#[derive(Default, Serialize)]
pub(super) struct Statistics {
    pub fetched: usize,
    pub not_modified: usize,
}

pub(super) struct MetadataCache {
    directory: PathBuf,
    pub statistics: Statistics,
}

impl MetadataCache {
    pub fn new(directory: &Path) -> Result<Self, String> {
        fs::create_dir_all(directory).map_err(|e| e.to_string())?;
        Ok(Self {
            directory: directory.into(),
            statistics: Statistics::default(),
        })
    }

    /// GitHub API metadata, or None when the resource does not exist.
    pub fn fetch(&mut self, url: &str) -> Result<Option<Value>, String> {
        self.fetch_with(url, |etag| get(url, etag))
    }

    /// A document such as an appcast, or None when it does not exist.
    pub fn fetch_text(&mut self, url: &str) -> Result<Option<String>, String> {
        self.fetch_body_with(url, |_| Ok(()), |etag| get(url, etag))
    }

    /// A repository's tags over git protocol v2, as an unparsed `ls-refs` response.
    ///
    /// Not cached: smart-HTTP has no validators, and one request lists every tag.
    pub fn ls_refs(&mut self, url: &str) -> Result<Vec<u8>, String> {
        let endpoint = format!("{}/git-upload-pack", url.trim_end_matches('/'));
        let (status, body) = with_retries(url, || {
            let mut response = http_post(&endpoint)
                .header("Git-Protocol", "version=2")
                .header("Content-Type", "application/x-git-upload-pack-request")
                .header("Accept", "application/x-git-upload-pack-result")
                .config()
                .timeout_global(Some(Duration::from_secs(60)))
                .http_status_as_error(false)
                .build()
                .send(super::git::LS_REFS)
                .map_err(RequestError::from)?;
            let status = response.status().as_u16();
            if status != 200 {
                return Ok((status, Vec::new()));
            }
            Ok((status, read_body(&mut response)?))
        })?;
        if status != 200 {
            return Err(format!("unexpected git HTTP status {status}: {url}"));
        }
        self.statistics.fetched += 1;
        Ok(body)
    }

    fn fetch_with(
        &mut self,
        url: &str,
        request: impl FnMut(Option<&str>) -> Result<(u16, Option<String>, String), RequestError>,
    ) -> Result<Option<Value>, String> {
        let parse = |body: &str| {
            serde_json::from_str::<Value>(body)
                .map_err(|e| format!("invalid metadata JSON from {url}: {e}"))
        };
        let body = self.fetch_body_with(url, |body| parse(body).map(|_| ()), request)?;
        body.as_deref().map(parse).transpose()
    }

    /// Revalidates a cached body, caching a new one only once `validate` accepts it.
    fn fetch_body_with(
        &mut self,
        url: &str,
        validate: impl Fn(&str) -> Result<(), String>,
        mut request: impl FnMut(Option<&str>) -> Result<(u16, Option<String>, String), RequestError>,
    ) -> Result<Option<String>, String> {
        let path = self
            .directory
            .join(format!("{}.json", hash_bytes(url.as_bytes())));
        let cached: Option<Entry> = match fs::read(&path) {
            Ok(bytes) => {
                let entry: Entry = serde_json::from_slice(&bytes)
                    .map_err(|e| format!("invalid metadata cache {}: {e}", path.display()))?;
                if entry.url != url || hash_bytes(entry.body.as_bytes()) != entry.sha256 {
                    return Err(format!(
                        "metadata cache hash or URL mismatch: {}",
                        path.display()
                    ));
                }
                Some(entry)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.to_string()),
        };
        let etag = cached.as_ref().and_then(|entry| entry.etag.as_deref());
        let (status, (new_etag, body)) = with_retries(url, || {
            request(etag).map(|(status, etag, body)| (status, (etag, body)))
        })?;
        if status == 404 {
            return Ok(None);
        }
        if status == 304 {
            let entry = cached
                .filter(|entry| entry.etag.is_some())
                .ok_or("received 304 without a cached validator")?;
            self.statistics.not_modified += 1;
            return Ok(Some(entry.body));
        }
        if status != 200 {
            return Err(format!("unexpected metadata HTTP status {status}: {url}"));
        }
        validate(&body)?;
        let entry = Entry {
            url: url.into(),
            etag: new_etag,
            sha256: hash_bytes(body.as_bytes()),
            body,
        };
        let mut temporary =
            tempfile::NamedTempFile::new_in(&self.directory).map_err(|e| e.to_string())?;
        temporary
            .write_all(&serde_json::to_vec(&entry).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        temporary.as_file().sync_all().map_err(|e| e.to_string())?;
        temporary.persist(path).map_err(|e| e.to_string())?;
        self.statistics.fetched += 1;
        Ok(Some(entry.body))
    }
}

/// One conditional GET, authenticated and versioned only for the GitHub API.
fn get(url: &str, etag: Option<&str>) -> Result<(u16, Option<String>, String), RequestError> {
    let token = std::env::var("GITHUB_TOKEN").ok();
    let mut request = http_request(url, token.as_deref());
    if url.starts_with("https://api.github.com/") {
        request = request.header("Accept", "application/vnd.github+json");
    }
    if let Some(etag) = etag {
        request = request.header("If-None-Match", etag);
    }
    let mut response = request
        .config()
        .timeout_global(Some(Duration::from_secs(60)))
        .http_status_as_error(false)
        .build()
        .call()
        .map_err(RequestError::from)?;
    let status = response.status().as_u16();
    if status != 200 {
        return Ok((status, None, String::new()));
    }
    let etag = response
        .headers()
        .get("etag")
        .and_then(|value| value.to_str().ok())
        .map(String::from);
    let body = String::from_utf8(read_body(&mut response)?)
        .map_err(|e| RequestError::Permanent(e.to_string()))?;
    Ok((status, etag, body))
}

fn read_body(response: &mut ureq::http::Response<ureq::Body>) -> Result<Vec<u8>, RequestError> {
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| RequestError::Retry(e.to_string()))?;
    if bytes.len() > MAX_BYTES {
        return Err(RequestError::Permanent(format!(
            "metadata exceeds {MAX_BYTES} bytes"
        )));
    }
    Ok(bytes)
}

/// Retries transport failures and transient statuses, returning any other status.
fn with_retries<T>(
    url: &str,
    mut request: impl FnMut() -> Result<(u16, T), RequestError>,
) -> Result<(u16, T), String> {
    let mut attempts = 0;
    loop {
        attempts += 1;
        let failure = match request() {
            Ok((status @ (408 | 429 | 500 | 502 | 503 | 504), _)) => {
                RequestError::Retry(format!("metadata HTTP status {status}"))
            }
            Ok(response) => return Ok(response),
            Err(error) => error,
        };
        match failure {
            RequestError::Retry(error) if attempts == MAX_ATTEMPTS => {
                return Err(format!(
                    "cannot fetch {url} after {attempts} attempts: {error}"
                ));
            }
            RequestError::Permanent(error) => return Err(format!("cannot fetch {url}: {error}")),
            RequestError::Retry(_) => std::thread::sleep(Duration::from_secs(attempts as u64)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revalidates_with_etags_without_using_stale_data_on_errors() {
        let root = tempfile::tempdir().unwrap();
        let mut cache = MetadataCache::new(root.path()).unwrap();
        let url = "https://api.github.com/repos/owner/tool";
        let value = cache
            .fetch_with(url, |etag| {
                assert!(etag.is_none());
                Ok((200, Some("\"one\"".into()), "{\"id\":42}".into()))
            })
            .unwrap()
            .unwrap();
        let repeated = cache
            .fetch_with(url, |etag| {
                assert_eq!(etag, Some("\"one\""));
                Ok((304, None, String::new()))
            })
            .unwrap()
            .unwrap();
        assert_eq!(value, repeated);
        assert_eq!(cache.statistics.fetched, 1);
        assert_eq!(cache.statistics.not_modified, 1);
        assert!(cache
            .fetch_with(url, |_| Err(RequestError::Retry("rate limited".into())))
            .is_err());
        let updated = cache
            .fetch_with(url, |_| {
                Ok((200, Some("\"two\"".into()), "{\"id\":43}".into()))
            })
            .unwrap()
            .unwrap();
        assert_eq!(updated["id"], 43);
        cache
            .fetch_with(url, |etag| {
                assert_eq!(etag, Some("\"two\""));
                Ok((304, None, String::new()))
            })
            .unwrap();
    }

    #[test]
    fn rejects_corrupt_entries_and_unsolicited_not_modified() {
        let root = tempfile::tempdir().unwrap();
        let mut cache = MetadataCache::new(root.path()).unwrap();
        let url = "https://api.github.com/repos/owner/tool";
        assert!(cache
            .fetch_with(url, |_| Ok((304, None, String::new())))
            .is_err());
        cache
            .fetch_with(url, |_| Ok((200, Some("one".into()), "{}".into())))
            .unwrap();
        let path = root
            .path()
            .join(format!("{}.json", hash_bytes(url.as_bytes())));
        let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value["body"] = "{\"changed\":true}".into();
        fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(cache
            .fetch_with(url, |_| panic!("corrupt data must not reach the network"))
            .unwrap_err()
            .contains("mismatch"));
    }

    #[test]
    fn retries_truncated_bodies_and_transient_statuses_with_the_same_validator() {
        let root = tempfile::tempdir().unwrap();
        let mut cache = MetadataCache::new(root.path()).unwrap();
        let url = "https://api.github.com/repos/owner/tool/releases";
        cache
            .fetch_with(url, |_| Ok((200, Some("one".into()), "[]".into())))
            .unwrap();
        let mut attempts = 0;
        let value = cache
            .fetch_with(url, |etag| {
                attempts += 1;
                assert_eq!(etag, Some("one"));
                match attempts {
                    1 => Err(ureq::Error::Decompress(
                        "gzip",
                        std::io::Error::from(std::io::ErrorKind::UnexpectedEof),
                    )
                    .into()),
                    2 => Ok((503, None, String::new())),
                    3 => Ok((200, Some("two".into()), "[42]".into())),
                    _ => panic!("too many requests"),
                }
            })
            .unwrap()
            .unwrap();
        assert_eq!(attempts, 3);
        assert_eq!(value, serde_json::json!([42]));
        assert_eq!(cache.statistics.fetched, 2);
        cache
            .fetch_with(url, |etag| {
                assert_eq!(etag, Some("two"));
                Ok((304, None, String::new()))
            })
            .unwrap();
    }

    #[test]
    fn exhausted_retries_preserve_cache_without_returning_stale_metadata() {
        let root = tempfile::tempdir().unwrap();
        let mut cache = MetadataCache::new(root.path()).unwrap();
        let url = "https://api.github.com/repos/owner/tool/releases";
        cache
            .fetch_with(url, |_| Ok((200, Some("one".into()), "[42]".into())))
            .unwrap();
        let path = root
            .path()
            .join(format!("{}.json", hash_bytes(url.as_bytes())));
        let original = fs::read(&path).unwrap();
        let mut attempts = 0;
        let error = cache
            .fetch_with(url, |etag| {
                attempts += 1;
                assert_eq!(etag, Some("one"));
                if attempts == 1 {
                    return Err(ureq::Error::ConnectionFailed.into());
                }
                Ok((429, None, String::new()))
            })
            .unwrap_err();
        assert_eq!(attempts, MAX_ATTEMPTS);
        assert!(error.contains(url));
        assert!(error.contains("after 3 attempts"));
        assert!(error.contains("429"));
        assert_eq!(fs::read(path).unwrap(), original);
        assert_eq!(cache.statistics.fetched, 1);
        assert_eq!(cache.statistics.not_modified, 0);
        let value = cache
            .fetch_with(url, |etag| {
                assert_eq!(etag, Some("one"));
                Ok((304, None, String::new()))
            })
            .unwrap()
            .unwrap();
        assert_eq!(value, serde_json::json!([42]));
    }

    #[test]
    fn permanent_errors_and_invalid_json_are_not_retried_or_cached() {
        let root = tempfile::tempdir().unwrap();
        let mut cache = MetadataCache::new(root.path()).unwrap();
        let url = "https://api.github.com/repos/owner/tool/releases";
        for response in [
            Ok((401, None, String::new())),
            Ok((501, None, String::new())),
            Ok((200, Some("invalid".into()), "not json".into())),
            Err(ureq::Error::BadUri("invalid".into()).into()),
            Err(RequestError::Permanent("metadata exceeds limit".into())),
        ] {
            let mut response = Some(response);
            let error = cache
                .fetch_with(url, |_| response.take().expect("must not retry"))
                .unwrap_err();
            assert!(error.contains(url));
            assert_eq!(cache.statistics.fetched, 0);
            assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
        }
    }

    #[test]
    fn a_missing_resource_is_none_and_not_cached() {
        let root = tempfile::tempdir().unwrap();
        let mut cache = MetadataCache::new(root.path()).unwrap();
        let url = "https://api.github.com/repos/owner/tool/releases/tags/v1";
        let value = cache
            .fetch_with(url, |_| Ok((404, None, String::new())))
            .unwrap();
        assert!(value.is_none());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    }
}
