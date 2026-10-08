//! rootbeer-cache bridges outputs in a local store with an OCi registry. It's
//! the primary mechanism to share/distribute built derivations.

mod manifest;

#[cfg(test)]
mod tests;

use data_encoding::{BASE64, HEXLOWER};
use manifest::{Descriptor, Manifest};
use rootbeer_drv::{Derivation, Key};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;
use std::time::Duration;
use ureq::config::RedirectAuthHeaders;
use ureq::http::Response;
use ureq::{Agent, Body, RequestBuilder};

const DOCUMENT_LIMIT: u64 = 16 << 20;
const TIMEOUT: Duration = Duration::from_secs(30);
const SLOWEST: u64 = 64 << 10;

pub struct Cache {
    agent: Agent,
    registry: String,
    namespace: String,
    credentials: Option<String>,
    is_http_allowed: bool,
}

pub struct Pulled<'a> {
    pub derivation: Derivation,
    pub references: BTreeMap<Key, String>,
    pub digest: String,
    session: Session<'a>,
    layer: Descriptor,
}

#[derive(Debug)]
pub enum Error {
    Status { url: String, status: u16 },
    Transport { url: String, reason: String },
    Invalid(String),
    Io { path: String, source: io::Error },
}

impl Cache {
    pub fn new(registry: &str, namespace: &str) -> Cache {
        let agent = Agent::config_builder()
            .http_status_as_error(false)
            .redirect_auth_headers(RedirectAuthHeaders::Never)
            .timeout_per_call(Some(TIMEOUT))
            .build()
            .into();

        Cache {
            agent,
            registry: registry.trim_end_matches('/').to_string(),
            namespace: namespace.trim_matches('/').to_string(),
            credentials: None,
            is_http_allowed: false,
        }
    }

    pub fn allow_http(self) -> Cache {
        Cache {
            is_http_allowed: true,
            ..self
        }
    }

    pub fn with_credentials(self, user: &str, secret: &str) -> Cache {
        let encoded = BASE64.encode(format!("{user}:{secret}").as_bytes());
        Cache {
            credentials: Some(encoded),
            ..self
        }
    }

    pub fn exists(&self, name: &str, key: &Key) -> Result<bool, Error> {
        let mut session = self.session(name, "pull")?;
        let url = session.url(&format!("manifests/{key}"));
        let response = session.request(&url, |agent| {
            agent.head(&url).header("Accept", manifest::MANIFEST).call()
        });

        let response = match response {
            Err(Error::Status { status: 403, .. }) if self.credentials.is_none() => {
                return Ok(false);
            }
            response => response?,
        };

        match response.status().as_u16() {
            200 => Ok(true),
            404 => Ok(false),
            status => Err(Error::Status { url, status }),
        }
    }

    pub fn digests(&self, name: &str, keys: &[&Key]) -> Result<Vec<Option<String>>, Error> {
        let mut session = self.session(name, "pull")?;
        let mut digests = Vec::new();
        for key in keys {
            let url = session.url(&format!("manifests/{key}"));
            let response = session.request(&url, |agent| {
                agent.get(&url).header("Accept", manifest::MANIFEST).call()
            });

            // This means that it was never published to the registry
            let response = match response {
                Err(Error::Status { status: 403, .. }) if self.credentials.is_none() => {
                    return Ok(keys.iter().map(|_| None).collect());
                }
                response => response?,
            };

            if response.status() == 404 {
                digests.push(None);
                continue;
            }

            let bytes = read_limited(expect(response, &url, 200)?, &url)?;
            let manifest: Manifest = serde_json::from_slice(&bytes).map_err(invalid)?;
            let is_key = manifest.annotations.get(manifest::KEY) == Some(&key.to_string());
            if manifest.artifact_type != manifest::ARTIFACT || !is_key {
                return Err(Error::Invalid(format!("{url} isn't the output of {key}")));
            }

            digests.push(Some(encode(&Sha256::digest(&bytes))));
        }

        Ok(digests)
    }

    pub fn push(
        &self,
        key: &Key,
        derivation: &Derivation,
        references: &BTreeMap<Key, String>,
        archive: &Path,
    ) -> Result<(), Error> {
        let Derivation::Build(build) = derivation else {
            return Err(Error::Invalid("only build outputs are cached".into()));
        };

        if let Some(name) = references.values().find(|name| !is_name(name)) {
            return Err(Error::Invalid(format!(
                "reference name {name:?} is invalid"
            )));
        }

        verify_key(key, derivation)?;
        let json = serde_json::to_vec(derivation).map_err(invalid)?;
        let mut hasher = Sha256::new();
        let size = File::open(archive)
            .and_then(|mut file| io::copy(&mut file, &mut hasher))
            .map_err(io_at(archive))?;

        let mut session = self.session(&build.name, "pull,push")?;
        let config = Descriptor {
            media_type: manifest::CONFIG.into(),
            digest: encode(&Sha256::digest(&json)),
            size: json.len() as u64,
        };

        let config = session.upload(config, || Ok(json.as_slice()))?;
        let layer = Descriptor {
            media_type: manifest::LAYER.into(),
            digest: encode(&hasher.finalize()),
            size,
        };

        let layer = session.upload(layer, || File::open(archive))?;
        let manifest = Manifest::output(key, build, references, config, layer);
        let body = serde_json::to_vec(&manifest).map_err(invalid)?;
        let url = session.url(&format!("manifests/{key}"));
        let response = session.request(&url, |agent| {
            agent
                .put(&url)
                .header("Content-Type", manifest::MANIFEST)
                .send(body.as_slice())
        })?;

        expect(response, &url, 201).map(drop)
    }

    pub fn pull(&self, name: &str, key: &Key, digest: Option<&str>) -> Result<Pulled<'_>, Error> {
        let mut session = self.session(name, "pull")?;
        let wanted = digest.unwrap_or(key.as_str());
        let url = session.url(&format!("manifests/{wanted}"));
        let response = session.request(&url, |agent| {
            agent.get(&url).header("Accept", manifest::MANIFEST).call()
        })?;

        let bytes = read_limited(expect(response, &url, 200)?, &url)?;
        let actual = encode(&Sha256::digest(&bytes));
        if digest.is_some_and(|expected| expected != actual) {
            return Err(Error::Invalid(format!("{url} is {actual}, not {wanted}")));
        }

        let manifest: Manifest = serde_json::from_slice(&bytes).map_err(invalid)?;
        let [layer] = manifest.layers.as_slice() else {
            return Err(Error::Invalid(format!("{url} has more than one layer")));
        };

        let is_ours = manifest.artifact_type == manifest::ARTIFACT
            && manifest.config.media_type == manifest::CONFIG
            && layer.media_type == manifest::LAYER;

        if !is_ours {
            return Err(Error::Invalid(format!("{url} isn't a rootbeer output")));
        }

        let config_url = session.url(&format!("blobs/{}", manifest.config.digest));
        let response = session.request(&config_url, |agent| agent.get(&config_url).call())?;
        let config = read_limited(expect(response, &config_url, 200)?, &config_url)?;

        if encode(&Sha256::digest(&config)) != manifest.config.digest {
            return Err(Error::Invalid(format!(
                "{config_url} doesn't match its digest"
            )));
        }

        let derivation: Derivation = serde_json::from_slice(&config).map_err(invalid)?;
        verify_key(key, &derivation)?;

        // Promotion puts the output under its build's name
        if !matches!(&derivation, Derivation::Build(build) if build.name == name) {
            return Err(Error::Invalid(format!("{url} isn't a build of {name}")));
        }

        let references = manifest
            .annotations
            .get(manifest::REFERENCES)
            .map(String::as_str)
            .unwrap_or_default()
            .split(',')
            .filter(|reference| !reference.is_empty())
            .map(reference)
            .collect::<Result<BTreeMap<Key, String>, Error>>()?;

        Ok(Pulled {
            derivation,
            references,
            digest: layer.digest.clone(),
            layer: layer.clone(),
            session,
        })
    }

    fn session(&self, name: &str, actions: &str) -> Result<Session<'_>, Error> {
        if !self.registry.starts_with("https://") && !self.is_http_allowed {
            return Err(Error::Invalid(format!(
                "refusing plain HTTP to {} unless allowed",
                self.registry
            )));
        }

        let repository = format!("{}/{name}", self.namespace);
        Ok(Session {
            cache: self,
            scopes: vec![format!("repository:{repository}:{actions}")],
            repository,
            authorization: None,
        })
    }

    fn is_registry(&self, url: &str) -> bool {
        url.strip_prefix(&self.registry)
            .is_some_and(|path| path.starts_with('/'))
    }

    fn authorize(&self, header: &str, scopes: &[String]) -> Result<String, Error> {
        let challenges = http_auth::parse_challenges(header)
            .map_err(|error| Error::Invalid(format!("challenge {header:?}: {error}")))?;

        let scheme = |name: &str| {
            challenges
                .iter()
                .find(|challenge| challenge.scheme.eq_ignore_ascii_case(name))
        };

        let Some(bearer) = scheme("Bearer") else {
            return match (scheme("Basic"), &self.credentials) {
                (Some(_), Some(credentials)) => Ok(format!("Basic {credentials}")),
                (Some(_), None) => Err(Error::Invalid("the registry requires credentials".into())),
                (None, _) => Err(Error::Invalid(format!("unsupported challenge {header:?}"))),
            };
        };

        let parameter = |name: &str| {
            bearer
                .params
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.to_unescaped())
        };
        let realm = parameter("realm")
            .ok_or_else(|| Error::Invalid(format!("challenge {header:?} has no realm")))?;

        let is_trusted = realm.starts_with("https://") || self.is_registry(&realm);
        if self.credentials.is_some() && !is_trusted {
            return Err(Error::Invalid(format!(
                "refusing to send credentials to {realm}"
            )));
        }

        let mut request = self.agent.get(&realm);
        for scope in scopes {
            request = request.query("scope", scope);
        }

        if let Some(service) = parameter("service") {
            request = request.query("service", service);
        }

        if let Some(credentials) = &self.credentials {
            request = request.header("Authorization", format!("Basic {credentials}"));
        }

        let response = request.call().map_err(transport(&realm))?;
        let bytes = read_limited(expect(response, &realm, 200)?, &realm)?;
        let token: Token = serde_json::from_slice(&bytes).map_err(invalid)?;
        let token = token
            .token
            .or(token.access_token)
            .ok_or_else(|| Error::Invalid(format!("{realm} returned no token")))?;

        Ok(format!("Bearer {token}"))
    }
}

impl Pulled<'_> {
    pub fn archive(mut self) -> Result<Box<dyn Read>, Error> {
        let layer = self.layer;
        let url = self.session.url(&format!("blobs/{}", layer.digest));
        let response = self.session.request(&url, |agent| {
            agent
                .get(&url)
                .config()
                .timeout_per_call(Some(transfer(layer.size)))
                .build()
                .call()
        })?;

        let reader = expect(response, &url, 200)?.into_body().into_reader();
        Ok(Box::new(Verified {
            reader,
            hasher: Sha256::new(),
            expected: layer,
            read: 0,
        }))
    }
}

struct Session<'a> {
    cache: &'a Cache,
    repository: String,
    scopes: Vec<String>,
    authorization: Option<String>,
}

impl Session<'_> {
    fn url(&self, path: &str) -> String {
        format!("{}/v2/{}/{path}", self.cache.registry, self.repository)
    }

    fn request(
        &mut self,
        url: &str,
        build: impl Fn(Authorized<'_>) -> Result<Response<Body>, ureq::Error>,
    ) -> Result<Response<Body>, Error> {
        let is_registry = self.cache.is_registry(url);
        let send = |authorization: Option<&str>| {
            build(Authorized {
                agent: &self.cache.agent,
                authorization: authorization.filter(|_| is_registry),
            })
            .map_err(transport(url))
        };

        let response = send(self.authorization.as_deref())?;
        if response.status() != 401 || !is_registry {
            return Ok(response);
        }

        let challenge = response
            .headers()
            .get("www-authenticate")
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| Error::Invalid(format!("{url} returned 401 without a challenge")))?;

        let authorization = self.cache.authorize(challenge, &self.scopes)?;
        let response = send(Some(&authorization))?;
        self.authorization = Some(authorization);
        Ok(response)
    }

    fn upload<B: ureq::AsSendBody>(
        &mut self,
        descriptor: Descriptor,
        body: impl Fn() -> io::Result<B>,
    ) -> Result<Descriptor, Error> {
        let blob = self.url(&format!("blobs/{}", descriptor.digest));
        let response = self.request(&blob, |agent| agent.head(&blob).call())?;
        if response.status() == 200 {
            return Ok(descriptor);
        }

        let uploads = self.url("blobs/uploads/");
        let response = self.request(&uploads, |agent| agent.post(&uploads).send(&[]))?;
        let response = expect(response, &uploads, 202)?;
        let location = response
            .headers()
            .get("location")
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| Error::Invalid(format!("{uploads} returned no location")))?;

        let url = match location.starts_with('/') {
            true => format!("{}{location}", self.cache.registry),
            false => location.to_string(),
        };

        let response = self.request(&url, |agent| {
            agent
                .put(&url)
                .query("digest", &descriptor.digest)
                .header("Content-Type", "application/octet-stream")
                .config()
                .timeout_per_call(Some(transfer(descriptor.size)))
                .build()
                .send(body()?)
        })?;

        expect(response, &url, 201)?;
        Ok(descriptor)
    }
}

struct Authorized<'a> {
    agent: &'a Agent,
    authorization: Option<&'a str>,
}

impl Authorized<'_> {
    fn head(&self, url: &str) -> RequestBuilder<ureq::typestate::WithoutBody> {
        self.with(self.agent.head(url))
    }

    fn get(&self, url: &str) -> RequestBuilder<ureq::typestate::WithoutBody> {
        self.with(self.agent.get(url))
    }

    fn post(&self, url: &str) -> RequestBuilder<ureq::typestate::WithBody> {
        self.with(self.agent.post(url))
    }

    fn put(&self, url: &str) -> RequestBuilder<ureq::typestate::WithBody> {
        self.with(self.agent.put(url))
    }

    fn with<B>(&self, request: RequestBuilder<B>) -> RequestBuilder<B> {
        match self.authorization {
            Some(authorization) => request.header("Authorization", authorization),
            None => request,
        }
    }
}

#[derive(serde::Deserialize)]
struct Token {
    token: Option<String>,
    access_token: Option<String>,
}

struct Verified<R> {
    reader: R,
    hasher: Sha256,
    expected: Descriptor,
    read: u64,
}

impl<R: Read> Read for Verified<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let remaining = self.expected.size.saturating_sub(self.read);
        let limit = usize::try_from(remaining)
            .unwrap_or(usize::MAX)
            .min(buffer.len());

        let buffer = buffer.get_mut(..limit).unwrap_or_default();
        let count = self.reader.read(buffer)?;
        self.hasher.update(buffer.get(..count).unwrap_or_default());
        self.read = self.read.saturating_add(count as u64);

        let is_short = self.read < self.expected.size;
        if is_short && count == 0 && limit > 0 {
            return Err(io::Error::other(
                "the archive is smaller than its manifest says",
            ));
        }

        if is_short {
            return Ok(count);
        }

        let actual = encode(&self.hasher.clone().finalize());
        if actual != self.expected.digest {
            return Err(io::Error::other(format!(
                "the archive is {actual}, not {}",
                self.expected.digest
            )));
        }

        Ok(count)
    }
}

fn encode(hash: &[u8]) -> String {
    format!("sha256:{}", HEXLOWER.encode(hash))
}

fn transfer(size: u64) -> Duration {
    TIMEOUT.saturating_add(Duration::from_secs(size / SLOWEST))
}

fn reference(text: &str) -> Result<(Key, String), Error> {
    let (name, key) = text
        .split_once(':')
        .ok_or_else(|| Error::Invalid(format!("reference {text:?} has no name")))?;

    if !is_name(name) {
        return Err(Error::Invalid(format!(
            "reference {text:?} has an invalid name"
        )));
    }

    Ok((key.parse().map_err(invalid)?, name.to_string()))
}

fn is_name(name: &str) -> bool {
    name.bytes()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

fn verify_key(key: &Key, derivation: &Derivation) -> Result<(), Error> {
    let actual = derivation.key().map_err(invalid)?;
    if actual != *key {
        return Err(Error::Invalid(format!(
            "the derivation is {actual}, not {key}"
        )));
    }

    Ok(())
}

fn expect(response: Response<Body>, url: &str, status: u16) -> Result<Response<Body>, Error> {
    if response.status().as_u16() != status {
        return Err(Error::Status {
            url: url.to_string(),
            status: response.status().as_u16(),
        });
    }

    Ok(response)
}

fn read_limited(response: Response<Body>, url: &str) -> Result<Vec<u8>, Error> {
    response
        .into_body()
        .with_config()
        .limit(DOCUMENT_LIMIT)
        .read_to_vec()
        .map_err(transport(url))
}

fn transport(url: &str) -> impl Fn(ureq::Error) -> Error {
    let url = url.to_string();
    move |error| Error::Transport {
        url: url.clone(),
        reason: error.to_string(),
    }
}

fn invalid(error: impl fmt::Display) -> Error {
    Error::Invalid(error.to_string())
}

fn io_at(path: &Path) -> impl Fn(io::Error) -> Error {
    let path = path.display().to_string();
    move |source| Error::Io {
        path: path.clone(),
        source,
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Status { url, status } => write!(f, "{url} returned {status}"),
            Error::Transport { url, reason } => write!(f, "{url}: {reason}"),
            Error::Invalid(reason) => f.write_str(reason),
            Error::Io { path, source } => write!(f, "{path}: {source}"),
        }
    }
}

impl std::error::Error for Error {}
