use futures_util::stream;
use std::time::Duration;
use tough::{
    Bytes, FilesystemTransport, Transport, TransportError, TransportErrorKind, TransportStream,
    async_trait,
};
use url::Url;

const LIMIT: u64 = 16 << 20;
const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub(crate) struct Ureq {
    agent: ureq::Agent,
    is_http_allowed: bool,
}

impl Ureq {
    pub(crate) fn new(is_http_allowed: bool) -> Ureq {
        let agent = ureq::Agent::config_builder()
            .https_only(!is_http_allowed)
            .http_status_as_error(false)
            .timeout_global(Some(TIMEOUT))
            .build()
            .into();

        Ureq {
            agent,
            is_http_allowed,
        }
    }
}

#[async_trait]
impl Transport for Ureq {
    async fn fetch(&self, url: Url) -> Result<TransportStream, TransportError> {
        match url.scheme() {
            "file" => return FilesystemTransport.fetch(url).await,
            "https" => {}
            "http" if self.is_http_allowed => {}
            _ => {
                return Err(TransportError::new(
                    TransportErrorKind::UnsupportedUrlScheme,
                    url,
                ));
            }
        }

        let failed = |error: ureq::Error| {
            TransportError::new_with_cause(TransportErrorKind::Other, &url, error)
        };

        let response = self.agent.get(url.as_str()).call().map_err(failed)?;
        match response.status().as_u16() {
            200 => {}
            404 => return Err(TransportError::new(TransportErrorKind::FileNotFound, url)),
            status => {
                let reason = format!("{url} returned {status}");
                return Err(TransportError::new_with_cause(
                    TransportErrorKind::Other,
                    &url,
                    reason,
                ));
            }
        }

        let body = response
            .into_body()
            .with_config()
            .limit(LIMIT)
            .read_to_vec()
            .map_err(failed)?;

        Ok(Box::pin(stream::iter([Ok(Bytes::from(body))])))
    }
}
