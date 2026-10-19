use super::realize;
use data_encoding::HEXLOWER;
use rootbeer_drv::{
    fetch_path, is_version, Derivation, Fetch, Format, Key, Platform, Sha256, Unpack,
};
use rootbeer_profile::{is_repository, Libc, Release};
use rootbeer_store::Store;
use serde::Deserialize;
use sha2::Digest;
use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::time::Duration;

const API: &str = "https://api.github.com";
const STALL: Duration = Duration::from_secs(30);
const QUERY: Duration = Duration::from_secs(60);
const DOWNLOAD: Duration = Duration::from_secs(30 * 60);

/// A `github:owner/repo[@tag]` request, with the asset and bins it may name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Request {
    pub(crate) repository: String,
    pub(crate) tag: Option<String>,
    pub(crate) asset: Option<String>,
    pub(crate) paths: BTreeMap<String, String>,
}

/// A release asset resolved for this machine and unpacked into the store.
pub(crate) struct Resolved {
    pub(crate) version: String,
    pub(crate) key: Key,
    pub(crate) release: Release,
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    digest: Option<String>,
}

impl Request {
    pub(crate) fn new(repository: &str, tag: Option<String>) -> Result<Request, String> {
        if !is_repository(repository) {
            return Err(format!(
                "github:{repository} isn't in the form github:owner/repo"
            ));
        }

        Ok(Request {
            repository: repository.to_string(),
            tag,
            asset: None,
            paths: BTreeMap::new(),
        })
    }

    /// What `release` was asked for with, without its tag.
    pub(crate) fn again(release: &Release) -> Request {
        Request {
            repository: release.repository.clone(),
            tag: None,
            asset: release.should_keep_asset.then(|| release.asset.clone()),
            paths: match release.should_keep_paths {
                true => release.paths.clone(),
                false => BTreeMap::new(),
            },
        }
    }

    /// Whether `release`, locked at `version`, still answers this request on
    /// this machine. A gnu build on a machine without glibc never does, unless
    /// it's the asset asked for.
    pub(crate) fn is_answered_by(&self, release: &Release, version: &str) -> bool {
        let is_asked = release.repository.eq_ignore_ascii_case(&self.repository)
            && self.tag.as_ref().is_none_or(|tag| tag == version)
            && (self.paths.is_empty() || self.paths == release.paths);

        let is_runnable = host_libc() != Some(Libc::Musl) || release.libc != Some(Libc::Gnu);
        match &self.asset {
            Some(asset) => is_asked && *asset == release.asset,
            None => is_asked && is_runnable,
        }
    }
}

/// Asks GitHub which asset answers `request` on `platform`, then fetches and
/// unpacks it under `name`. The same asset as `previous` keeps its bins, so
/// an unchanged release needs no download.
pub(crate) fn resolve(
    store: &mut Store,
    is_root: bool,
    name: &str,
    request: &Request,
    platform: Platform,
    previous: Option<&Release>,
) -> Result<Resolved, String> {
    let published = published(request)?;
    let version = published.tag_name;
    if !is_version(&version) {
        return Err(format!(
            "github:{}'s tag {version} isn't a valid version",
            request.repository
        ));
    }

    let repo = request.repository.split_once('/').unwrap_or_default().1;
    let asset = select(
        &published.assets,
        request.asset.as_deref(),
        repo,
        platform,
        host_libc(),
    )?;
    let format = format(&asset.name)
        .ok_or_else(|| format!("{} isn't an archive or binary rb can unpack", asset.name))?;

    let digest = asset
        .digest
        .as_deref()
        .and_then(|digest| digest.strip_prefix("sha256:"));
    let sha256 = match digest {
        Some(digest) => Sha256::try_from(digest.to_string()).map_err(|error| error.to_string())?,
        None => {
            eprintln!(
                "GitHub publishes no sha256 for {}. Trusting the sha256 of this download",
                asset.name
            );
            download_digest(&asset.browser_download_url)?
        }
    };

    let unchanged = previous.filter(|previous| previous.sha256 == sha256);
    let paths = match (request.paths.is_empty(), format, unchanged) {
        (false, ..) => request.paths.clone(),
        (true, _, Some(previous)) => previous.paths.clone(),
        (true, Format::Binary, None) => {
            let repo = request.repository.split_once('/').unwrap_or_default().1;
            BTreeMap::from([(repo.to_string(), repo.to_string())])
        }
        (true, _, None) => {
            let fetch = Fetch {
                sha256: sha256.clone(),
                urls: vec![asset.browser_download_url.clone()],
            };
            let source = present(store, is_root, &BTreeMap::new(), Derivation::Fetch(fetch))?;
            let found = rootbeer_sandbox::executables(&fetch_path(&source), format)
                .map_err(|error| format!("{}: {error}", asset.name))?;
            bins(&asset.name, found)?
        }
    };

    let release = Release {
        repository: request.repository.clone(),
        asset: asset.name.clone(),
        url: asset.browser_download_url.clone(),
        sha256,
        format,
        paths,
        libc: libc(&asset.name),
        should_keep_asset: request.asset.is_some(),
        should_keep_paths: !request.paths.is_empty(),
    };

    let key = install(store, is_root, name, &version, &release, platform, false)?;
    Ok(Resolved {
        version,
        key,
        release,
    })
}

/// Fetches and unpacks a resolved release under `name`, unless the store
/// already has it. Returns its key.
pub(crate) fn install(
    store: &mut Store,
    is_root: bool,
    name: &str,
    version: &str,
    release: &Release,
    platform: Platform,
    is_offline: bool,
) -> Result<Key, String> {
    if release.libc == Some(Libc::Gnu) && host_libc() == Some(Libc::Musl) {
        return Err(format!(
            "{name}'s {} needs glibc, which this machine doesn't have, so ask for a musl \
             build with `asset`",
            release.asset
        ));
    }

    let fetch = Derivation::Fetch(Fetch {
        sha256: release.sha256.clone(),
        urls: vec![release.url.clone()],
    });
    let source = fetch.key().map_err(|error| error.to_string())?;
    let unpack = Derivation::Unpack(Unpack {
        name: name.to_string(),
        version: version.to_string(),
        platform,
        source: source.clone(),
        format: release.format,
        bins: release.paths.clone(),
    });

    // GC keeps an output but not the fetch it came from.
    let key = unpack
        .key()
        .map_err(|error| format!("{name}@{version}: {error}"))?;
    if store
        .path(&key)
        .map_err(|error| error.to_string())?
        .is_some()
    {
        return Ok(key);
    }

    let is_fetched = store
        .path(&source)
        .map_err(|error| error.to_string())?
        .is_some();
    if is_offline && !is_fetched {
        return Err(format!(
            "{name} needs to download {}. Run without --offline",
            release.url
        ));
    }

    present(store, is_root, &BTreeMap::new(), fetch.clone())?;
    present(store, is_root, &BTreeMap::from([(source, fetch)]), unpack)
}

/// Realizes a derivation whose inputs are in `graph` and present, unless the
/// store has it.
fn present(
    store: &mut Store,
    is_root: bool,
    graph: &BTreeMap<Key, Derivation>,
    derivation: Derivation,
) -> Result<Key, String> {
    let key = derivation.key().map_err(|error| error.to_string())?;

    if store
        .path(&key)
        .map_err(|error| error.to_string())?
        .is_some()
    {
        return Ok(key);
    }

    let mut graph = graph.clone();
    graph.insert(key.clone(), derivation);
    realize(store, is_root, &graph, &BTreeMap::new(), &key, false)?;
    Ok(key)
}

fn published(request: &Request) -> Result<ApiRelease, String> {
    let (endpoint, missing) = match &request.tag {
        Some(tag) => (format!("tags/{}", encode(tag)), format!("no release {tag}")),
        None => ("latest".to_string(), "no published release".to_string()),
    };

    let url = format!("{API}/repos/{}/releases/{endpoint}", request.repository);
    let mut call = agent(QUERY)
        .get(&url)
        .header("Accept", "application/vnd.github+json");
    let token = std::env::var("GITHUB_TOKEN").unwrap_or_default();
    if !token.is_empty() {
        call = call.header("Authorization", format!("Bearer {token}"));
    }

    let response = call.call().map_err(|error| match error {
        ureq::Error::StatusCode(404) => format!("github:{} has {missing}", request.repository),
        ureq::Error::StatusCode(403 | 429) if token.is_empty() => format!(
            "GitHub refused {url}, likely its rate limit for anonymous requests, so set \
             GITHUB_TOKEN"
        ),
        error => format!("{url}: {error}"),
    })?;

    let body = response
        .into_body()
        .read_to_string()
        .map_err(|error| format!("{url}: {error}"))?;
    let release: ApiRelease =
        serde_json::from_str(&body).map_err(|error| format!("{url}: {error}"))?;
    if release.draft || release.tag_name.is_empty() {
        return Err(format!("github:{} has {missing}", request.repository));
    }

    Ok(release)
}

fn download_digest(url: &str) -> Result<Sha256, String> {
    let response = agent(DOWNLOAD)
        .get(url)
        .call()
        .map_err(|error| format!("{url}: {error}"))?;
    let mut hasher = sha2::Sha256::new();
    io::copy(&mut response.into_body().into_reader(), &mut hasher)
        .map_err(|error| format!("{url}: {error}"))?;

    Sha256::try_from(HEXLOWER.encode(&hasher.finalize())).map_err(|error| error.to_string())
}

fn agent(limit: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(STALL))
        .timeout_recv_body(Some(STALL))
        .timeout_global(Some(limit))
        .build()
        .into()
}

fn encode(segment: &str) -> String {
    segment
        .bytes()
        .map(
            |byte| match byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                true => char::from(byte).to_string(),
                false => format!("%{byte:02X}"),
            },
        )
        .collect()
}

/// The one asset for `platform`, by its OS and architecture in the name. On
/// Linux a gnu build wins over an unmarked one and then a musl one, and a musl
/// machine never takes a gnu build. Among the rest, one named for the repo
/// wins over companions like a runtime or a library.
fn select<'a>(
    assets: &'a [Asset],
    asked: Option<&str>,
    repo: &str,
    platform: Platform,
    host: Option<Libc>,
) -> Result<&'a Asset, String> {
    if let Some(name) = asked {
        let asset = assets
            .iter()
            .find(|asset| asset.name == name)
            .ok_or_else(|| format!("the release has no asset {name}"))?;
        if format(name).is_none() {
            return Err(format!("{name} isn't an archive or binary rb can unpack"));
        }

        return Ok(asset);
    }

    let (systems, architectures): (&[&str], &[&str]) = match platform {
        Platform::Aarch64Macos => (
            &["darwin", "macos", "osx"],
            &["aarch64", "arm64", "universal"],
        ),
        Platform::Aarch64Linux => (&["linux"], &["aarch64", "arm64"]),
        Platform::X86_64Linux => (&["linux"], &["x86_64", "amd64", "x64"]),
    };

    let mut candidates = assets
        .iter()
        .filter(|asset| {
            let name = asset.name.to_ascii_lowercase();
            is_installable(&name)
                && !["android", "androideabi"]
                    .iter()
                    .any(|token| has_token(&name, token))
                && systems.iter().any(|system| has_token(&name, system))
                && architectures.iter().any(|arch| has_token(&name, arch))
        })
        .collect::<Vec<_>>();

    let rank = |asset: &&Asset| match (host, libc(&asset.name)) {
        (Some(Libc::Musl), Some(Libc::Gnu)) => None,
        (Some(Libc::Musl), Some(Libc::Musl)) | (_, Some(Libc::Gnu)) => Some(0),
        (_, None) => Some(1),
        (_, Some(Libc::Musl)) => Some(2),
    };
    let best = candidates.iter().filter_map(rank).min();
    candidates.retain(|asset| best.is_some() && rank(asset) == best);
    candidates.sort_by(|a, b| a.name.cmp(&b.name));

    let repo = repo.to_ascii_lowercase();
    let is_named = |asset: &&Asset| {
        let name = asset.name.to_ascii_lowercase();
        name.strip_prefix(&repo)
            .is_some_and(|rest| rest.starts_with(['-', '_', '.']))
    };
    if candidates.len() > 1 && candidates.iter().any(is_named) {
        candidates.retain(is_named);
    }

    match candidates.as_slice() {
        [asset] => Ok(asset),
        [] => Err(format!(
            "no release asset matches {platform}. Name one with `asset`"
        )),
        _ => Err(format!(
            "release assets {} all match {platform}. Name one with `asset`",
            candidates
                .iter()
                .map(|asset| asset.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn format(name: &str) -> Option<Format> {
    let name = name.to_ascii_lowercase();
    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        return Some(Format::TarGz);
    }

    if name.ends_with(".tar.xz") || name.ends_with(".txz") {
        return Some(Format::TarXz);
    }

    if name.ends_with(".zip") {
        return Some(Format::Zip);
    }

    // A binary's name has no extension, though its version may hold dots.
    let extension = name.rsplit_once('.').map_or("", |(_, extension)| extension);
    let is_extension = extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
        && extension.bytes().any(|byte| byte.is_ascii_alphabetic());
    (!is_extension).then_some(Format::Binary)
}

fn is_installable(name: &str) -> bool {
    let is_extra = [
        "source",
        "debug",
        "symbols",
        "checksum",
        "sha256sum",
        "sha512sum",
        "attestation",
    ]
    .iter()
    .any(|word| name.contains(word));
    !is_extra && format(name).is_some()
}

fn libc(name: &str) -> Option<Libc> {
    let name = name.to_ascii_lowercase();
    if has_token(&name, "musl") {
        return Some(Libc::Musl);
    }

    ["gnu", "glibc"]
        .iter()
        .any(|token| has_token(&name, token))
        .then_some(Libc::Gnu)
}

fn has_token(name: &str, token: &str) -> bool {
    let is_boundary = |byte: Option<&u8>| byte.is_none_or(|byte| !byte.is_ascii_alphanumeric());
    name.match_indices(token).any(|(start, _)| {
        let before = start.checked_sub(1).and_then(|at| name.as_bytes().get(at));
        let after = name.as_bytes().get(start + token.len());
        is_boundary(before) && is_boundary(after)
    })
}

/// Every executable an archive holds, by file name. Two with one name need
/// `bins` to say which.
fn bins(asset: &str, paths: Vec<String>) -> Result<BTreeMap<String, String>, String> {
    let mut bins = BTreeMap::new();
    for path in paths {
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        if let Some(other) = bins.insert(name.clone(), path.clone()) {
            return Err(format!(
                "{asset} has two executables named {name}: {other} and {path}. Name the bins \
                 with `bins`"
            ));
        }
    }

    match bins.is_empty() {
        true => Err(format!(
            "{asset} has no executable files. Name the bins with `bins`"
        )),
        false => Ok(bins),
    }
}

/// None off Linux, where every binary shares one C library. A machine
/// without glibc's loader where a gnu build looks for it, like Alpine or
/// NixOS, runs only musl builds.
fn host_libc() -> Option<Libc> {
    if !cfg!(target_os = "linux") {
        return None;
    }

    let loader = match std::env::consts::ARCH {
        "aarch64" => "/lib/ld-linux-aarch64.so.1",
        _ => "/lib64/ld-linux-x86-64.so.2",
    };

    match Path::new(loader).exists() {
        true => Some(Libc::Gnu),
        false => Some(Libc::Musl),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assets(names: &[&str]) -> Vec<Asset> {
        names
            .iter()
            .map(|name| Asset {
                name: name.to_string(),
                browser_download_url: String::new(),
                digest: None,
            })
            .collect()
    }

    fn picked(names: &[&str], platform: Platform, host: Option<Libc>) -> Result<String, String> {
        let assets = assets(names);
        select(&assets, None, "tool", platform, host).map(|asset| asset.name.clone())
    }

    #[test]
    fn the_asset_for_this_platform_wins_and_checksums_never_do() {
        let names = [
            "tool-linux-arm64.tar.gz",
            "tool-darwin-amd64.tar.gz",
            "tool-darwin-arm64.tar.gz",
            "tool-darwin-arm64.tar.gz.sha256",
            "tool-darwin-arm64.pkg",
        ];
        assert_eq!(
            picked(&names, Platform::Aarch64Macos, None).unwrap(),
            "tool-darwin-arm64.tar.gz"
        );
        assert!(!has_token("tool-arm64e", "arm64"));
        assert_eq!(
            picked(&["tool-darwin-amd64.tar.gz"], Platform::Aarch64Macos, None).unwrap_err(),
            "no release asset matches aarch64-macos. Name one with `asset`"
        );
    }

    #[test]
    fn gnu_wins_on_glibc_and_musl_machines_never_take_it() {
        let both = [
            "rg-x86_64-unknown-linux-gnu.tar.gz",
            "rg-x86_64-unknown-linux-musl.tar.gz",
        ];
        assert_eq!(
            picked(&both, Platform::X86_64Linux, Some(Libc::Gnu)).unwrap(),
            both[0]
        );
        assert_eq!(
            picked(&both, Platform::X86_64Linux, Some(Libc::Musl)).unwrap(),
            both[1]
        );

        let musl_only = ["rg-aarch64-unknown-linux-musl.tar.gz"];
        assert_eq!(
            picked(&musl_only, Platform::Aarch64Linux, Some(Libc::Gnu)).unwrap(),
            musl_only[0]
        );

        let gnu_only = ["rg-aarch64-unknown-linux-gnu.tar.gz"];
        assert!(picked(&gnu_only, Platform::Aarch64Linux, Some(Libc::Musl)).is_err());

        let android = [
            "zoxide-aarch64-linux-android.tar.gz",
            "zoxide-aarch64-unknown-linux-musl.tar.gz",
        ];
        assert_eq!(
            picked(&android, Platform::Aarch64Linux, Some(Libc::Gnu)).unwrap(),
            android[1]
        );

        let packages = ["k9s_linux_amd64.apk", "k9s_Linux_amd64.tar.gz"];
        assert_eq!(
            picked(&packages, Platform::X86_64Linux, Some(Libc::Gnu)).unwrap(),
            packages[1]
        );

        let deno = [
            "deno-aarch64-apple-darwin.zip",
            "denort-aarch64-apple-darwin.zip",
            "libdenort-aarch64-apple-darwin.zip",
        ];
        let assets = assets(&deno);
        let deno_asset = select(&assets, None, "deno", Platform::Aarch64Macos, None).unwrap();
        assert_eq!(deno_asset.name, deno[0]);

        let unmarked = ["tool-linux-amd64.tar.gz", "tool-linux-amd64.zip"];
        let error = picked(&unmarked, Platform::X86_64Linux, Some(Libc::Gnu)).unwrap_err();
        assert!(error.contains("Name one with `asset`"), "{error}");
    }

    #[test]
    fn an_asset_asked_for_by_name_must_be_one_rb_can_unpack() {
        let assets = assets(&["tool.dmg", "tool-linux-musl"]);
        let platform = Platform::X86_64Linux;
        assert!(select(&assets, Some("tool.dmg"), "tool", platform, None).is_err());
        assert!(select(&assets, Some("missing"), "tool", platform, None).is_err());
        let musl = Some("tool-linux-musl");
        let asset = select(&assets, musl, "tool", platform, Some(Libc::Gnu)).unwrap();
        assert_eq!(format(&asset.name), Some(Format::Binary));
    }

    #[test]
    fn bins_are_executables_by_unique_file_name() {
        let found = bins("a.tar.gz", vec!["rg-1/rg".into(), "rg-1/complete/x".into()]).unwrap();
        assert_eq!(found["rg"], "rg-1/rg");

        let error = bins("a.tar.gz", vec!["a/rg".into(), "b/rg".into()]).unwrap_err();
        assert_eq!(
            error,
            "a.tar.gz has two executables named rg: a/rg and b/rg. Name the bins with `bins`"
        );
        assert_eq!(
            bins("a.tar.gz", Vec::new()).unwrap_err(),
            "a.tar.gz has no executable files. Name the bins with `bins`"
        );
        assert_eq!(format("shfmt_v3.8.0_linux_amd64"), Some(Format::Binary));
        assert_eq!(format("cosign-linux-amd64.bundle"), None);
    }

    #[test]
    fn a_locked_release_answers_until_the_request_or_machine_says_otherwise() {
        let release = Release {
            repository: "BurntSushi/ripgrep".into(),
            asset: "rg-linux-gnu.tar.gz".into(),
            url: String::new(),
            sha256: "0".repeat(64).try_into().unwrap(),
            format: Format::TarGz,
            paths: BTreeMap::from([("rg".into(), "rg-1/rg".into())]),
            libc: None,
            should_keep_asset: false,
            should_keep_paths: false,
        };

        let mut request = Request::new("burntsushi/ripgrep", None).unwrap();
        assert!(request.is_answered_by(&release, "14.1.1"));

        request.tag = Some("14.1.0".into());
        assert!(!request.is_answered_by(&release, "14.1.1"));

        request.tag = None;
        request.asset = Some("rg-linux-musl.tar.gz".into());
        assert!(!request.is_answered_by(&release, "14.1.1"));

        request.asset = None;
        request.paths = BTreeMap::from([("rg".into(), "rg".into())]);
        assert!(!request.is_answered_by(&release, "14.1.1"));

        assert!(Request::new("ripgrep", None).is_err());
        assert!(Request::new("owner/..", None).is_err());
    }
}
