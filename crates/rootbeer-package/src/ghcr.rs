use std::io::{self, Read};
use std::time::Duration;

use serde::Deserialize;

/// A public GHCR blob addressed by its archive digest, not a mutable tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhcrBlob {
    pub repository: String,
    pub sha256: String,
}

impl GhcrBlob {
    pub fn parse(url: &str) -> Result<Self, String> {
        let (repository, sha256) = url
            .strip_prefix("ghcr://")
            .and_then(|reference| reference.split_once("@sha256:"))
            .ok_or("GHCR sources require ghcr://owner/repository@sha256:<archive digest>")?;
        validate_repository(repository)?;
        if !rootbeer_catalog::is_sha256(sha256) {
            return Err("GHCR sources require a lowercase SHA-256 digest".into());
        }
        Ok(Self {
            repository: repository.into(),
            sha256: sha256.into(),
        })
    }

    pub fn reader(&self) -> io::Result<Box<dyn Read>> {
        self.reader_with_execution(&crate::Execution::default())
            .map(|(reader, _)| reader)
    }

    /// Returns the blob body and its length when the registry reports one.
    pub(crate) fn reader_with_execution(
        &self,
        execution: &crate::Execution,
    ) -> io::Result<(Box<dyn Read>, Option<u64>)> {
        let agent = agent(execution.remaining()?.unwrap_or(Duration::from_secs(300)));
        let token = pull_token(&agent, &self.repository)?;
        let url = format!(
            "https://ghcr.io/v2/{}/blobs/sha256:{}",
            self.repository, self.sha256
        );
        execution.check()?;
        let (_, body) = agent
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .config()
            .timeout_global(Some(
                execution.remaining()?.unwrap_or(Duration::from_secs(300)),
            ))
            .build()
            .call()
            .map_err(|e| io::Error::other(format!("cannot fetch public GHCR blob: {e}")))?
            .into_parts();
        let length = body.content_length();
        Ok((Box::new(body.into_reader()), length))
    }
}

/// What a public repository's tag names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tagged {
    Manifest(Vec<u8>),
    Missing,
    /// GHCR answers a repository nobody has pushed to exactly as it answers a private one.
    Denied,
}

/// Reads the OCI manifest `tag` names in a public GHCR repository.
pub fn tagged_manifest(repository: &str, tag: &str) -> io::Result<Tagged> {
    validate_repository(repository).map_err(io::Error::other)?;
    if tag.is_empty()
        || tag.len() > 128
        || !tag
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(io::Error::other("invalid GHCR tag"));
    }
    let agent = agent(Duration::from_secs(120));
    let token = match pull_token(&agent, repository) {
        Ok(token) => token,
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return Ok(Tagged::Denied),
        Err(error) => return Err(error),
    };
    let response = agent
        .get(format!("https://ghcr.io/v2/{repository}/manifests/{tag}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("Accept", "application/vnd.oci.image.manifest.v1+json")
        .call();
    let mut response = match response {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(404)) => return Ok(Tagged::Missing),
        Err(ureq::Error::StatusCode(401 | 403)) => return Ok(Tagged::Denied),
        Err(error) => {
            return Err(io::Error::other(format!(
                "cannot read GHCR manifest {repository}:{tag}: {error}"
            )))
        }
    };
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(1 << 20)
        .read_to_end(&mut bytes)?;
    Ok(Tagged::Manifest(bytes))
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .https_only(true)
        .timeout_resolve(Some(Duration::from_secs(30)))
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_send_request(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .timeout_recv_body(Some(Duration::from_secs(30)))
        .redirect_auth_headers(ureq::config::RedirectAuthHeaders::Never)
        .timeout_global(Some(timeout))
        .build()
        .into()
}

fn pull_token(agent: &ureq::Agent, repository: &str) -> io::Result<String> {
    let mut response = agent
        .get("https://ghcr.io/token")
        .query("service", "ghcr.io")
        .query("scope", format!("repository:{repository}:pull"))
        .call()
        .map_err(|error| match error {
            ureq::Error::StatusCode(401 | 403) => {
                io::Error::new(io::ErrorKind::PermissionDenied, "GHCR denied a pull token")
            }
            error => io::Error::other(format!("cannot obtain public GHCR pull token: {error}")),
        })?;
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(65537)
        .read_to_end(&mut bytes)?;
    parse_token(&bytes)
}

pub fn validate_repository(repository: &str) -> Result<(), String> {
    if repository.len() > 255
        || repository.split('/').count() < 2
        || repository.split('/').any(|part| {
            part.is_empty()
                || !part
                    .bytes()
                    .next()
                    .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
                || !part
                    .bytes()
                    .last()
                    .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
                || !part.bytes().all(|byte| {
                    byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'-' | b'_' | b'.')
                })
        })
    {
        return Err("GHCR repository must be a lowercase owner/repository path".into());
    }
    Ok(())
}

fn parse_token(bytes: &[u8]) -> io::Result<String> {
    #[derive(Deserialize)]
    struct Token {
        token: String,
    }
    if bytes.len() > 65536 {
        return Err(io::Error::other("GHCR token response exceeds size limit"));
    }
    let response: Token = serde_json::from_slice(bytes)
        .map_err(|_| io::Error::other("invalid GHCR token response"))?;
    if response.token.is_empty() || !response.token.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(io::Error::other("invalid GHCR pull token"));
    }
    Ok(response.token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::DownloadCache;
    use crate::store::hash_bytes;

    #[test]
    fn requires_immutable_unambiguous_blob_coordinates() {
        let sha = "a".repeat(64);
        assert_eq!(
            GhcrBlob::parse(&format!("ghcr://tale/rootbeer/xz@sha256:{sha}"))
                .unwrap()
                .repository,
            "tale/rootbeer/xz"
        );
        for reference in [
            "ghcr://tale/xz:latest".into(),
            "ghcr://tale/xz@sha256:short".into(),
            format!("ghcr://Tale/xz@sha256:{sha}"),
            format!("ghcr://tale/../xz@sha256:{sha}"),
            format!("ghcr://tale/xz?scope=other@sha256:{sha}"),
            format!("ghcr://tale//xz@sha256:{sha}"),
            format!("ghcr://tale/xz@sha256:{sha}#other"),
            format!("ghcr://tale@sha256:{sha}"),
        ] {
            assert!(GhcrBlob::parse(&reference).is_err(), "{reference}");
        }
    }

    #[test]
    fn replays_cached_blobs_offline_and_rejects_conflicting_digests() {
        let root = tempfile::tempdir().unwrap();
        let bytes = b"cached archive fixture";
        let sha = hash_bytes(bytes);
        std::fs::write(root.path().join(format!("sha256-{sha}")), bytes).unwrap();
        let downloads = DownloadCache::offline(root.path());
        let url = format!("ghcr://tale/rootbeer/xz@sha256:{sha}");
        assert_eq!(downloads.materialize(&url, None).unwrap().sha256, sha);
        assert!(downloads
            .materialize(&url, Some(&"b".repeat(64)))
            .unwrap_err()
            .to_string()
            .contains("does not match"));
        std::fs::write(root.path().join(format!("sha256-{sha}")), b"tampered").unwrap();
        assert!(downloads.materialize(&url, Some(&sha)).is_err());
    }

    #[test]
    fn rejects_bad_token_responses_without_exposing_the_response() {
        assert_eq!(
            parse_token(br#"{"token":"public-token"}"#).unwrap(),
            "public-token"
        );
        for bytes in [
            br#"{"token":""}"#.as_slice(),
            br#"{"token":"secret\r\nheader"}"#,
            br#"{"error":"private"}"#,
            b"not json",
        ] {
            let message = parse_token(bytes).unwrap_err().to_string();
            assert!(!message.contains("secret"));
        }
    }

    #[test]
    #[ignore = "requires network and ROOTBEER_TEST_GHCR_BLOB pointing to a public blob"]
    fn downloads_public_blob_without_external_tools() {
        let url = std::env::var("ROOTBEER_TEST_GHCR_BLOB").unwrap();
        let blob = GhcrBlob::parse(&url).unwrap();
        let root = tempfile::tempdir().unwrap();
        let downloaded = DownloadCache::new(root.path())
            .materialize(&url, Some(&blob.sha256))
            .unwrap();
        assert_eq!(downloaded.sha256, blob.sha256);
    }
}
