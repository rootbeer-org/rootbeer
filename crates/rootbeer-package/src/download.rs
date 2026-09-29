use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::progress::{report, Event};
use crate::store::hash_file;
use crate::{state_dir, Execution};

const USER_AGENT: &str = concat!("rootbeer/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone)]
pub struct DownloadCache {
    root: PathBuf,
    offline: bool,
    execution: Execution,
}

#[derive(Deserialize, Serialize)]
struct UrlCacheEntry {
    sha256: String,
    etag: Option<String>,
    last_modified: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadedFile {
    pub path: PathBuf,
    pub sha256: String,
}

impl DownloadCache {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            offline: false,
            execution: Execution::default(),
        }
    }

    pub fn offline(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            offline: true,
            execution: Execution::default(),
        }
    }

    /// Shares the caller’s cancellation and download deadline.
    pub fn with_execution(mut self, execution: Execution) -> Self {
        self.execution = execution;
        self
    }

    pub fn materialize(
        &self,
        url: &str,
        expected_sha256: Option<&str>,
    ) -> io::Result<DownloadedFile> {
        self.execution.check()?;
        let blob = url
            .starts_with("ghcr://")
            .then(|| super::ghcr::GhcrBlob::parse(url))
            .transpose()
            .map_err(io::Error::other)?;
        if let Some(blob) = &blob {
            if expected_sha256.is_some_and(|expected| expected != blob.sha256) {
                return Err(io::Error::other(
                    "GHCR reference digest does not match locked source hash",
                ));
            }
        }
        let expected_sha256 =
            expected_sha256.or_else(|| blob.as_ref().map(|blob| blob.sha256.as_str()));
        fs::create_dir_all(&self.root)?;

        if let Some(sha256) = expected_sha256 {
            if let Some(downloaded) = self.valid_cached(sha256)? {
                return Ok(downloaded);
            }
        }

        if self.offline {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                match expected_sha256 {
                    Some(sha256) => format!(
                        "source {url} with sha256 {sha256} is not in the download cache and offline mode is enabled"
                    ),
                    None => format!("source {url} cannot be fetched in offline mode"),
                },
            ));
        }

        if expected_sha256.is_none() && (url.starts_with("https://") || url.starts_with("http://"))
        {
            return with_execution_retries(&self.execution, || self.revalidate_url(url));
        }

        let (tmp, actual_sha256) = self.download_to_temp(url)?;
        let result = self.finish_download(&tmp, actual_sha256, expected_sha256, url);
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }

        result
    }

    pub fn materialize_verified(&self, url: &str, sha256: &str) -> io::Result<PathBuf> {
        self.materialize(url, Some(sha256))
            .map(|downloaded| downloaded.path)
    }

    fn revalidate_url(&self, url: &str) -> io::Result<DownloadedFile> {
        let metadata_path = self.root.join(format!(
            "url-{}.json",
            crate::store::hash_bytes(url.as_bytes())
        ));
        let metadata = match fs::read(&metadata_path) {
            Ok(bytes) => serde_json::from_slice::<UrlCacheEntry>(&bytes).ok(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let cached = match metadata
            .as_ref()
            .filter(|entry| rootbeer_catalog::is_sha256(&entry.sha256))
        {
            Some(entry) => self.valid_cached(&entry.sha256)?,
            None => None,
        };
        let validators = metadata
            .as_ref()
            .filter(|_| cached.is_some())
            .filter(|entry| entry.etag.is_some() || entry.last_modified.is_some());
        let response = http_response(url, validators, &self.execution)?;
        if response.status().as_u16() == 304 {
            return cached.filter(|_| validators.is_some()).ok_or_else(|| {
                io::Error::other("received HTTP 304 without a verified cached download")
            });
        }
        let mut metadata = UrlCacheEntry {
            sha256: String::new(),
            etag: response
                .headers()
                .get("etag")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned),
            last_modified: response
                .headers()
                .get("last-modified")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned),
        };
        let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
        let total = response.body().content_length();
        let mut reader = response.into_body().into_reader();
        let sha256 = copy_reader_to_writer(url, &mut reader, total, &mut file, &self.execution)?;
        file.as_file().sync_all()?;
        let downloaded = self.finish_download(file.path(), sha256, None, url)?;
        metadata.sha256 = downloaded.sha256.clone();
        let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
        serde_json::to_writer(&mut file, &metadata)?;
        file.persist(&metadata_path).map_err(|error| error.error)?;
        Ok(downloaded)
    }

    fn valid_cached(&self, sha256: &str) -> io::Result<Option<DownloadedFile>> {
        let path = self.cached_path(sha256);
        if !path.exists() {
            return Ok(None);
        }

        if hash_file(&path)? == sha256 {
            return Ok(Some(DownloadedFile {
                path,
                sha256: sha256.to_string(),
            }));
        }

        fs::remove_file(path)?;
        Ok(None)
    }

    fn download_to_temp(&self, url: &str) -> io::Result<(PathBuf, String)> {
        with_execution_retries(&self.execution, || self.download_once(url))
    }

    fn download_once(&self, url: &str) -> io::Result<(PathBuf, String)> {
        for attempt in 0..16 {
            let tmp = self.temp_path(attempt);
            let mut file = match OpenOptions::new().write(true).create_new(true).open(&tmp) {
                Ok(file) => file,
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(err),
            };

            match copy_url_to_writer(url, &mut file, &self.execution) {
                Ok(sha256) => {
                    if let Err(error) = file.sync_all() {
                        let _ = fs::remove_file(&tmp);
                        return Err(error);
                    }
                    return Ok((tmp, sha256));
                }

                Err(err) => {
                    let _ = fs::remove_file(&tmp);
                    return Err(err);
                }
            }
        }

        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "failed to allocate a temporary download path in {}",
                self.root.display()
            ),
        ))
    }

    fn finish_download(
        &self,
        tmp: &Path,
        actual_sha256: String,
        expected_sha256: Option<&str>,
        url: &str,
    ) -> io::Result<DownloadedFile> {
        if let Some(expected) = expected_sha256 {
            if actual_sha256 != expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("source {url} hash mismatch: expected {expected}, got {actual_sha256}"),
                ));
            }
        }

        let cached = self.cached_path(&actual_sha256);
        if cached.exists() {
            if hash_file(&cached)? == actual_sha256 {
                fs::remove_file(tmp)?;
                return Ok(DownloadedFile {
                    path: cached,
                    sha256: actual_sha256,
                });
            }

            fs::remove_file(&cached)?;
        }

        match fs::rename(tmp, &cached) {
            Ok(()) => Ok(DownloadedFile {
                path: cached,
                sha256: actual_sha256,
            }),

            Err(err) if cached.exists() => {
                if hash_file(&cached)? == actual_sha256 {
                    let _ = fs::remove_file(tmp);
                    Ok(DownloadedFile {
                        path: cached,
                        sha256: actual_sha256,
                    })
                } else {
                    Err(err)
                }
            }

            Err(err) => Err(err),
        }
    }

    fn cached_path(&self, sha256: &str) -> PathBuf {
        self.root.join(format!("sha256-{sha256}"))
    }

    fn temp_path(&self, attempt: u32) -> PathBuf {
        let pid = std::process::id();
        let epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();

        self.root
            .join(format!(".tmp-download-{pid}-{epoch}-{attempt}"))
    }
}

impl Default for DownloadCache {
    fn default() -> Self {
        Self::new(state_dir().join("downloads"))
    }
}

pub fn read_url(url: &str) -> io::Result<Vec<u8>> {
    with_retries(|| {
        let (mut reader, _) = url_reader(url, &Execution::default())?;
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .map_err(|error| body_error(url, error))?;
        Ok(bytes)
    })
}

pub fn read_json_url<T>(url: &str) -> io::Result<T>
where
    T: DeserializeOwned,
{
    let bytes = read_url(url)?;
    serde_json::from_slice(&bytes).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("failed to parse JSON from {url}: {e}"),
        )
    })
}

fn copy_url_to_writer(
    url: &str,
    writer: &mut impl Write,
    execution: &Execution,
) -> io::Result<String> {
    let (mut reader, total) = url_reader(url, execution)?;
    copy_reader_to_writer(url, &mut reader, total, writer, execution)
}

fn copy_reader_to_writer(
    url: &str,
    reader: &mut impl Read,
    total: Option<u64>,
    writer: &mut impl Write,
    execution: &Execution,
) -> io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    let mut received = 0;

    let result = loop {
        if let Err(error) = execution.check() {
            break Err(error);
        }
        let n = match reader.read(&mut buf) {
            Ok(0) => break Ok(()),
            Ok(n) => n,
            Err(error) => break Err(body_error(url, error)),
        };

        hasher.update(&buf[..n]);
        if let Err(error) = writer.write_all(&buf[..n]) {
            break Err(error);
        }
        received += n as u64;
        report(&Event::Download {
            url,
            received,
            total,
        });
    };

    report(&Event::Done);
    result.map(|()| hex(hasher.finalize().as_slice()))
}

/// Opens a source and returns its length when known.
fn url_reader(url: &str, execution: &Execution) -> io::Result<(Box<dyn Read>, Option<u64>)> {
    execution.check()?;
    if url.starts_with("ghcr://") {
        return super::ghcr::GhcrBlob::parse(url)
            .map_err(io::Error::other)?
            .reader_with_execution(execution);
    }
    if let Some(path) = url.strip_prefix("file://") {
        let file = fs::File::open(path)
            .map_err(|e| io::Error::new(e.kind(), format!("failed to read {url}: {e}")))?;
        let length = file.metadata().ok().map(|metadata| metadata.len());
        return Ok((Box::new(file), length));
    }

    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsupported source URL `{url}`"),
        ));
    }

    let response = http_response(url, None, execution)?;
    if response.status().as_u16() == 304 {
        return Err(io::Error::other(
            "unexpected HTTP 304 without cache validators",
        ));
    }
    let length = response.body().content_length();
    Ok((Box::new(response.into_body().into_reader()), length))
}

fn http_response(
    url: &str,
    cached: Option<&UrlCacheEntry>,
    execution: &Execution,
) -> io::Result<ureq::http::Response<ureq::Body>> {
    let token = std::env::var("GITHUB_TOKEN").ok();
    let mut request = http_request(url, token.as_deref());
    if let Some(cached) = cached {
        if let Some(etag) = &cached.etag {
            request = request.header("If-None-Match", etag);
        } else if let Some(modified) = &cached.last_modified {
            request = request.header("If-Modified-Since", modified);
        }
    }
    let response = request
        .config()
        .timeout_global(execution.remaining()?)
        .http_status_as_error(false)
        .build()
        .call()
        .map_err(|error| {
            let is_transient = matches!(
                error,
                ureq::Error::StatusCode(408 | 429 | 500 | 502 | 503 | 504)
                    | ureq::Error::Io(_)
                    | ureq::Error::Timeout(_)
                    | ureq::Error::HostNotFound
                    | ureq::Error::ConnectionFailed
                    | ureq::Error::Protocol(_)
                    | ureq::Error::Decompress(_, _)
            );
            let message = format!("failed to fetch {url}: {error}");
            if is_transient {
                return io::Error::other(TransientDownload {
                    message,
                    retry_after: None,
                });
            }
            io::Error::other(message)
        })?;
    let status = response.status().as_u16();
    if status >= 400 {
        let message = format!("failed to fetch {url}: http status: {status}");
        if !matches!(status, 408 | 429 | 500 | 502 | 503 | 504) {
            return Err(io::Error::other(message));
        }
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| retry_after(value, SystemTime::now()));
        return Err(io::Error::other(TransientDownload {
            message,
            retry_after,
        }));
    }
    Ok(response)
}

#[derive(Debug)]
struct TransientDownload {
    message: String,
    retry_after: Option<Duration>,
}

impl std::fmt::Display for TransientDownload {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for TransientDownload {}

fn body_error(url: &str, error: io::Error) -> io::Error {
    let message = format!("failed to read {url}: {error}");
    if url.starts_with("http://") || url.starts_with("https://") {
        return io::Error::other(TransientDownload {
            message,
            retry_after: None,
        });
    }
    io::Error::new(error.kind(), message)
}

fn retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    value
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
        .or_else(|| {
            httpdate::parse_http_date(value)
                .ok()
                .map(|date| date.duration_since(now).unwrap_or_default())
        })
}

fn with_retries<T>(operation: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    with_execution_retries(&Execution::default(), operation)
}

fn with_execution_retries<T>(
    execution: &Execution,
    mut operation: impl FnMut() -> io::Result<T>,
) -> io::Result<T> {
    retry_with_sleep(
        || {
            execution.check()?;
            operation()
        },
        |delay| {
            let _ = execution.sleep(delay);
        },
    )
}

fn retry_with_sleep<T>(
    mut operation: impl FnMut() -> io::Result<T>,
    mut sleep: impl FnMut(Duration),
) -> io::Result<T> {
    const ATTEMPTS: u32 = 5;
    for attempt in 0..ATTEMPTS {
        let error = match operation() {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        let transient = error
            .get_ref()
            .and_then(|cause| cause.downcast_ref::<TransientDownload>());
        let Some(transient) = transient.filter(|_| attempt + 1 < ATTEMPTS) else {
            return Err(error);
        };
        let backoff = Duration::from_secs(2 << attempt);
        let delay = transient.retry_after.unwrap_or_default().max(backoff);
        if delay > Duration::from_secs(60) {
            return Err(error);
        }
        let jitter = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos() as u64
            % 1000;
        let delay = (delay + Duration::from_millis(jitter)).min(Duration::from_secs(60));
        eprintln!(
            "download retry {}/{ATTEMPTS} in {:.1}s: {error}",
            attempt + 2,
            delay.as_secs_f64()
        );
        sleep(delay);
    }
    unreachable!()
}

fn http_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .redirect_auth_headers(ureq::config::RedirectAuthHeaders::Never)
        .timeout_resolve(Some(Duration::from_secs(30)))
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_send_request(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .timeout_recv_body(Some(Duration::from_secs(30)))
        .build()
        .into()
}

/// An unauthenticated POST, such as a git smart-HTTP request.
pub fn http_post(url: &str) -> ureq::RequestBuilder<ureq::typestate::WithBody> {
    http_agent().post(url).header("User-Agent", USER_AGENT)
}

pub fn http_request(
    url: &str,
    token: Option<&str>,
) -> ureq::RequestBuilder<ureq::typestate::WithoutBody> {
    let request = http_agent().get(url).header("User-Agent", USER_AGENT);
    let Some(token) = token.map(str::trim).filter(|token| !token.is_empty()) else {
        return request;
    };
    if !url.starts_with("https://api.github.com/") {
        return request;
    }

    request.header("Authorization", format!("Bearer {token}"))
}

fn hex(bytes: &[u8]) -> String {
    const CHARS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);

    for byte in bytes {
        out.push(CHARS[(byte >> 4) as usize] as char);
        out.push(CHARS[(byte & 0x0f) as usize] as char);
    }

    out
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::store::hash_bytes;

    struct HttpServer {
        url: String,
        is_done: std::sync::Arc<std::sync::atomic::AtomicBool>,
        requests: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        headers: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl HttpServer {
        fn new(responses: Vec<&'static str>) -> Self {
            use std::io::{BufRead, BufReader};
            use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
            use std::sync::Arc;

            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let is_done = Arc::new(AtomicBool::new(false));
            let requests = Arc::new(AtomicUsize::new(0));
            let headers = Arc::new(std::sync::Mutex::new(Vec::new()));
            let server_headers = headers.clone();
            let server_done = is_done.clone();
            let server_requests = requests.clone();
            let thread = std::thread::spawn(move || {
                while !server_done.load(Ordering::SeqCst) {
                    let (mut stream, _) = match listener.accept() {
                        Ok(connection) => connection,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(5));
                            continue;
                        }
                        Err(error) => panic!("{error}"),
                    };
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                        .unwrap();
                    let mut reader = BufReader::new(&stream);
                    let mut headers = String::new();
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                            break;
                        }
                        headers.push_str(&line);
                    }
                    server_headers.lock().unwrap().push(headers.to_lowercase());
                    let request = server_requests.fetch_add(1, Ordering::SeqCst);
                    let response = responses[request.min(responses.len() - 1)];
                    stream.write_all(response.as_bytes()).unwrap();
                }
            });
            Self {
                url,
                is_done,
                requests,
                headers,
                thread: Some(thread),
            }
        }

        fn request_count(&self) -> usize {
            self.requests.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl Drop for HttpServer {
        fn drop(&mut self) {
            self.is_done
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let result = self.thread.take().unwrap().join();
            if !std::thread::panicking() {
                result.unwrap();
            }
        }
    }

    const SUCCESS: &str =
        "HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\narchive";
    const UNAVAILABLE: &str =
        "HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    const PARTIAL: &str =
        "HTTP/1.1 200 OK\r\nContent-Length: 50\r\nConnection: close\r\n\r\npartial";

    const ETAG_SUCCESS: &str =
        "HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nContent-Length: 7\r\nConnection: close\r\n\r\narchive";
    const NOT_MODIFIED: &str = "HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n";

    #[test]
    fn unpinned_download_revalidates_cached_content_across_instances() {
        let root = tempfile::tempdir().unwrap();
        let server = HttpServer::new(vec![ETAG_SUCCESS, NOT_MODIFIED, SUCCESS]);
        let first = DownloadCache::new(root.path())
            .materialize(&server.url, None)
            .unwrap();
        let second = DownloadCache::new(root.path())
            .materialize(&server.url, None)
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(fs::read(&second.path).unwrap(), b"archive");
        assert!(server.headers.lock().unwrap()[1].contains("if-none-match: \"v1\""));
        fs::write(&second.path, b"corrupted").unwrap();
        DownloadCache::new(root.path())
            .materialize(&server.url, None)
            .unwrap();
        assert!(!server.headers.lock().unwrap()[2].contains("if-none-match"));
        assert_eq!(fs::read(&first.path).unwrap(), b"archive");
    }

    #[test]
    fn unpinned_download_accepts_changed_content_and_new_validators() {
        let root = tempfile::tempdir().unwrap();
        let server =
            HttpServer::new(vec![ETAG_SUCCESS,
            "HTTP/1.1 200 OK\r\nETag: \"v2\"\r\nContent-Length: 3\r\nConnection: close\r\n\r\nnew",
            NOT_MODIFIED]);
        let cache = DownloadCache::new(root.path());
        let first = cache.materialize(&server.url, None).unwrap();
        let second = cache.materialize(&server.url, None).unwrap();
        assert_ne!(first.sha256, second.sha256);
        assert_eq!(fs::read(&second.path).unwrap(), b"new");
        assert_eq!(cache.materialize(&server.url, None).unwrap(), second);
        assert!(server.headers.lock().unwrap()[2].contains("if-none-match: \"v2\""));
    }

    #[test]
    fn unpinned_download_uses_last_modified_and_rejects_unexpected_304() {
        let root = tempfile::tempdir().unwrap();
        let server = HttpServer::new(vec![
            "HTTP/1.1 200 OK\r\nLast-Modified: Wed, 16 Sep 2026 12:00:00 GMT\r\nContent-Length: 7\r\nConnection: close\r\n\r\narchive",
            NOT_MODIFIED]);
        let cache = DownloadCache::new(root.path());
        let first = cache.materialize(&server.url, None).unwrap();
        assert_eq!(cache.materialize(&server.url, None).unwrap(), first);
        assert!(server.headers.lock().unwrap()[1].contains("if-modified-since:"));
        let empty = tempfile::tempdir().unwrap();
        assert!(DownloadCache::new(empty.path())
            .materialize(&server.url, None)
            .is_err());
        assert_eq!(fs::read_dir(empty.path()).unwrap().count(), 0);
    }

    #[test]
    fn execution_deadline_bounds_stalled_http_and_removes_partial_downloads() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/archive", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nx")
                .unwrap();
            std::thread::sleep(Duration::from_millis(300));
        });
        let root = tempfile::tempdir().unwrap();
        let execution = Execution::with_timeout(Duration::from_millis(100)).unwrap();
        let started = std::time::Instant::now();
        let error = DownloadCache::new(root.path())
            .with_execution(execution)
            .materialize(&url, Some(&"a".repeat(64)))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
        server.join().unwrap();
    }

    #[test]
    fn retry_backoff_is_bounded_and_preserves_server_minimum() {
        let mut attempts = 0;
        let mut delays = Vec::new();
        let result = retry_with_sleep(
            || -> io::Result<()> {
                attempts += 1;
                Err(io::Error::other(TransientDownload {
                    message: "busy".into(),
                    retry_after: Some(Duration::from_secs(10)),
                }))
            },
            |delay| delays.push(delay),
        );
        assert!(result.is_err());
        assert_eq!(attempts, 5);
        assert_eq!(delays.len(), 4);
        for (delay, minimum) in delays.iter().zip([10, 10, 10, 16]) {
            assert!(*delay >= Duration::from_secs(minimum));
            assert!(*delay < Duration::from_secs(minimum + 1));
        }
        let result = retry_with_sleep(
            || -> io::Result<()> {
                Err(io::Error::other(TransientDownload {
                    message: "busy".into(),
                    retry_after: Some(Duration::from_secs(3600)),
                }))
            },
            |_| panic!("must not retry earlier than the server permits"),
        );
        assert!(result.is_err());
    }

    #[test]
    fn retry_after_accepts_seconds_and_http_dates() {
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(retry_after("12", now), Some(Duration::from_secs(12)));
        assert_eq!(
            retry_after(&httpdate::fmt_http_date(now + Duration::from_secs(20)), now),
            Some(Duration::from_secs(20))
        );
        assert_eq!(
            retry_after(&httpdate::fmt_http_date(now - Duration::from_secs(20)), now),
            Some(Duration::ZERO)
        );
        assert_eq!(retry_after("invalid", now), None);
    }

    #[test]
    fn retries_transient_status_and_partial_body_with_clean_output() {
        let server = HttpServer::new(vec![UNAVAILABLE, PARTIAL, SUCCESS]);
        let tmp = tempfile::tempdir().unwrap();
        let cache = DownloadCache::new(tmp.path());
        let file = cache
            .materialize(&server.url, Some(&hash_bytes(b"archive")))
            .unwrap();
        assert_eq!(fs::read(file.path).unwrap(), b"archive");
        assert_eq!(server.request_count(), 3);
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);
    }

    #[test]
    fn retries_connection_closed_before_response() {
        let server = HttpServer::new(vec!["", SUCCESS]);
        assert_eq!(read_url(&server.url).unwrap(), b"archive");
        assert_eq!(server.request_count(), 2);
    }

    #[test]
    fn exhausted_body_retries_leave_no_partial_downloads() {
        let server = HttpServer::new(vec![PARTIAL]);
        let tmp = tempfile::tempdir().unwrap();
        let error = DownloadCache::new(tmp.path())
            .materialize(&server.url, None)
            .unwrap_err();
        assert!(error.to_string().contains(&server.url));
        assert_eq!(server.request_count(), 5);
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[test]
    fn exhausted_status_retries_are_bounded() {
        let server = HttpServer::new(vec![UNAVAILABLE]);
        let error = read_url(&server.url).unwrap_err();
        assert!(error.to_string().contains("503"));
        assert_eq!(server.request_count(), 5);
    }

    #[test]
    fn server_retry_deadline_beyond_budget_is_not_retried_early() {
        let server = HttpServer::new(vec![
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 3600\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        ]);
        assert!(read_url(&server.url)
            .unwrap_err()
            .to_string()
            .contains("429"));
        assert_eq!(server.request_count(), 1);
    }

    #[test]
    fn permanent_status_and_checksum_failures_are_not_retried() {
        for response in [
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            SUCCESS,
        ] {
            let server = HttpServer::new(vec![response]);
            let tmp = tempfile::tempdir().unwrap();
            let error = DownloadCache::new(tmp.path())
                .materialize(&server.url, Some(&hash_bytes(b"other")))
                .unwrap_err();
            assert!(error.to_string().contains(if response == SUCCESS {
                "hash mismatch"
            } else {
                "404"
            }));
            assert_eq!(server.request_count(), 1);
            assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 0);
        }
    }

    #[test]
    fn invalid_json_is_not_retried() {
        let server = HttpServer::new(vec![SUCCESS]);
        assert!(read_json_url::<serde_json::Value>(&server.url).is_err());
        assert_eq!(server.request_count(), 1);
    }

    #[test]
    fn authenticates_only_https_github_api_requests() {
        let url = "https://api.github.com/repos/owner/repo/releases/latest";
        let request = http_request(url, Some("test-token"));
        assert_eq!(
            request.headers_ref().unwrap()["authorization"],
            "Bearer test-token"
        );

        for url in [
            "http://api.github.com/repos/owner/repo",
            "https://api.github.com.evil.example/repos/owner/repo",
            "https://api.github.com@evil.example/repos/owner/repo",
            "https://github.com/owner/repo/releases/download/v1/tool",
            "https://example.com/tool",
        ] {
            assert!(!http_request(url, Some("test-token"))
                .headers_ref()
                .unwrap()
                .contains_key("authorization"));
        }
        for token in [None, Some(""), Some("  ")] {
            assert!(!http_request(url, token)
                .headers_ref()
                .unwrap()
                .contains_key("authorization"));
        }
    }

    #[test]
    fn strips_authorization_on_redirects() {
        use std::io::{BufRead, BufReader};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let redirect = format!(
            "HTTP/1.1 302 Found\r\nLocation: {url}/redirected\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let server = std::thread::spawn(move || {
            for is_redirected in [false, true] {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(&stream);
                let mut headers = String::new();
                loop {
                    let mut line = String::new();
                    assert!(reader.read_line(&mut line).unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                    headers.push_str(&line.to_ascii_lowercase());
                }
                assert_eq!(
                    headers.contains("authorization: bearer test-token"),
                    !is_redirected
                );
                let response = if is_redirected {
                    "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                } else {
                    &redirect
                };
                stream.write_all(response.as_bytes()).unwrap();
            }
        });

        http_request(&url, None)
            .header("Authorization", "Bearer test-token")
            .call()
            .unwrap();
        server.join().unwrap();
    }

    #[test]
    fn materializes_file_url_into_content_addressed_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source.tar.gz");
        fs::write(&source, b"archive bytes").unwrap();
        let cache = DownloadCache::new(tmp.path().join("downloads"));

        let downloaded = cache
            .materialize(&format!("file://{}", source.display()), None)
            .unwrap();

        let sha256 = hash_bytes(b"archive bytes");
        assert_eq!(downloaded.sha256, sha256);
        assert_eq!(downloaded.path, cache.cached_path(&sha256));
        assert_eq!(fs::read(downloaded.path).unwrap(), b"archive bytes");
    }

    #[test]
    fn returns_valid_cached_file_before_opening_url() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = DownloadCache::new(tmp.path().join("downloads"));
        fs::create_dir_all(&cache.root).unwrap();
        let sha256 = hash_bytes(b"cached bytes");
        fs::write(cache.cached_path(&sha256), b"cached bytes").unwrap();

        let downloaded = cache
            .materialize("unsupported://does-not-matter", Some(&sha256))
            .unwrap();

        assert_eq!(downloaded.path, cache.cached_path(&sha256));
        assert_eq!(downloaded.sha256, sha256);
    }

    #[test]
    fn rejects_expected_hash_mismatch_without_caching() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("source.tar.gz");
        fs::write(&source, b"actual bytes").unwrap();
        let cache = DownloadCache::new(tmp.path().join("downloads"));

        let err = cache
            .materialize(
                &format!("file://{}", source.display()),
                Some(&hash_bytes(b"expected bytes")),
            )
            .unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(fs::read_dir(&cache.root).unwrap().next().is_none());
    }

    #[test]
    fn offline_cache_misses_do_not_open_url() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = DownloadCache::offline(tmp.path().join("downloads"));

        let err = cache
            .materialize("unsupported://does-not-matter", Some("missing"))
            .unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().contains("offline mode"));
    }
}
