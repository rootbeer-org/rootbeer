use super::cache::{self, Signed};
use super::github;
use super::{store_error, this_platform};
use data_encoding::HEXLOWER;
use rootbeer_cache::Cache;
use rootbeer_drv::{is_package_name, AppSettings, Key, Platform, STORE_ROOT};
use rootbeer_profile::{release_name, Locked, Member, Release};
use rootbeer_store::{Store, ROOT};
use rootbeer_trust::{Index, Output, Package};
use sha2::{Digest, Sha256};
use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const INDEX: &str = "https://index.rbpkg.com/";
const TRUSTED_ROOT: &[u8] = include_bytes!("root.json");

#[derive(clap::Args, Debug)]
pub(super) struct Args {
    /// `name` or `name@version`, defaulting to the platform's default
    /// version. With `--key`, the name the output was pushed under
    package: String,
    /// Install this output by key, trusting the registry instead of the
    /// index
    #[arg(long, requires = "is_unverified", conflicts_with_all = ["index", "root"])]
    key: Option<Key>,
    /// Required with `--key`, since only the registry vouches for the output
    #[arg(long = "unverified", requires = "key")]
    is_unverified: bool,
    #[command(flatten)]
    source: Source,
}

#[derive(clap::Args, Debug)]
pub(crate) struct Source {
    /// A TUF repository to install from instead of rootbeer's own
    #[arg(long, requires = "root")]
    index: Option<String>,
    /// The trusted root metadata of `--index`
    #[arg(long, requires = "index")]
    root: Option<PathBuf>,
    #[arg(long, default_value = "https://ghcr.io")]
    registry: String,
    #[arg(long, default_value = "rootbeer-org/store")]
    namespace: String,
    #[arg(long = "allow-http")]
    is_http_allowed: bool,
}

impl Default for Source {
    fn default() -> Source {
        Source {
            index: None,
            root: None,
            registry: "https://ghcr.io".into(),
            namespace: "rootbeer-org/store".into(),
            is_http_allowed: false,
        }
    }
}

impl Source {
    pub(super) fn cache(&self) -> Cache {
        cache::open(&self.registry, &self.namespace, self.is_http_allowed)
    }
}

pub(super) fn install(args: &Args) -> Result<(), String> {
    let (mut store, is_root) = open_store()?;
    let _held = store.hold().map_err(|error| error.to_string())?;
    let cache = args.source.cache();
    let key = match &args.key {
        Some(key) => {
            if !is_package_name(&args.package) {
                return Err(format!(
                    "{:?} isn't a package name, and `--key` takes the name the output was \
                     pushed under",
                    args.package
                ));
            }

            cache::install_into(&cache, &mut store, is_root, &args.package, key, None)?;
            key.clone()
        }
        None => {
            let index = open_index(&args.source, is_root)?;
            let (asked, version) = request(&args.package);
            let name = index.canonical(asked).unwrap_or_else(|| asked.to_string());
            let mut signed =
                Signed::new(|name| index.package(name).map_err(|error| error.to_string()));
            let (_, output) = select(signed.package(&name)?, version, this_platform()?)?;
            let key = output.key.clone();
            cache::install_into(&cache, &mut store, is_root, &name, &key, Some(&mut signed))?;
            key
        }
    };

    let path = store
        .path(&key)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("{key} isn't present after installing"))?;

    println!("{}", path.display());
    Ok(())
}

pub(super) fn open_store() -> Result<(Store, bool), String> {
    super::setup::ensure()?;
    let is_root = rustix::process::geteuid().is_root();
    if is_root {
        let logs = Path::new(ROOT).join("var/log");
        for directory in [Path::new(STORE_ROOT), &logs] {
            fs::create_dir_all(directory)
                .map_err(|error| format!("{}: {error}", directory.display()))?;
        }
    }

    let store = match is_root {
        true => Store::open(Path::new(ROOT)),
        false => Store::open_read_only(Path::new(ROOT)),
    }
    .map_err(store_error)?;

    Ok((store, is_root))
}

pub(super) fn request(package: &str) -> (&str, Option<&str>) {
    match package.split_once('@') {
        Some((name, version)) => (name, Some(version)),
        None => (package, None),
    }
}

/// What a configuration or `rb use` asks for under one package name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Wanted {
    /// The index's default version on this platform, or the version pinned
    Index(Option<String>),
    Release(github::Request),
    /// A checkout's recipe at the key it evaluates to
    Local {
        version: Option<String>,
        key: Key,
        bins: BTreeSet<String>,
        apps: BTreeMap<String, String>,
        app_settings: BTreeMap<String, AppSettings>,
    },
}

impl Wanted {
    /// `name[@version]` or `github:owner/repo[@tag]`, with the name it
    /// installs under.
    pub(crate) fn parse(package: &str) -> Result<(String, Wanted), String> {
        let (name, version) = request(package);
        let version = version.map(str::to_string);
        let Some(repository) = name.strip_prefix("github:") else {
            return Ok((name.to_string(), Wanted::Index(version)));
        };

        let request = github::Request::new(repository, version)?;
        Ok((named(repository)?, Wanted::Release(request)))
    }

    pub(crate) fn version(&self) -> Option<&str> {
        match self {
            Wanted::Index(version) | Wanted::Local { version, .. } => version.as_deref(),
            Wanted::Release(request) => request.tag.as_deref(),
        }
    }

    pub(crate) fn pin(&mut self, version: String) {
        match self {
            Wanted::Index(pinned)
            | Wanted::Local {
                version: pinned, ..
            } => {
                *pinned = Some(version);
            }
            Wanted::Release(request) => request.tag = Some(version),
        }
    }

    /// Whether what's installed already answers this.
    pub(crate) fn is_answered_by(&self, installed: Installed) -> bool {
        let Installed {
            version,
            pin,
            key,
            release,
            is_local,
            bins,
            apps,
            app_settings,
        } = installed;
        match (self, release, is_local) {
            // A line asked for is answered only by what was resolved from
            // it, never by a version that happens to be in it.
            (Wanted::Index(asked), None, false) => asked
                .as_deref()
                .is_none_or(|asked| asked == pin.unwrap_or(version)),
            (Wanted::Release(request), Some(release), false) => {
                request.is_answered_by(release, version)
            }
            (
                Wanted::Local {
                    key: wanted,
                    bins: names,
                    apps: bundles,
                    app_settings: settings,
                    ..
                },
                None,
                true,
            ) => wanted == key && names == bins && bundles == apps && settings == app_settings,
            _ => false,
        }
    }
}

/// A lock entry or generation member, as far as a request cares.
pub(crate) struct Installed<'a> {
    version: &'a str,
    pin: Option<&'a str>,
    key: &'a Key,
    release: Option<&'a Release>,
    is_local: bool,
    bins: &'a BTreeSet<String>,
    apps: &'a BTreeMap<String, String>,
    app_settings: &'a BTreeMap<String, AppSettings>,
}

impl<'a> From<&'a Locked> for Installed<'a> {
    fn from(locked: &'a Locked) -> Self {
        Installed {
            version: &locked.version,
            pin: locked.pin.as_deref(),
            key: &locked.key,
            release: locked.release.as_ref(),
            is_local: locked.is_local,
            bins: &locked.bins,
            apps: &locked.apps,
            app_settings: &locked.app_settings,
        }
    }
}

impl<'a> From<&'a Member> for Installed<'a> {
    fn from(member: &'a Member) -> Self {
        Installed {
            version: &member.version,
            pin: member.pin.as_deref(),
            key: &member.key,
            release: member.release.as_ref(),
            is_local: member.is_local,
            bins: &member.bins,
            apps: &member.apps,
            app_settings: &member.app_settings,
        }
    }
}

/// The package name a repository installs under.
pub(crate) fn named(repository: &str) -> Result<String, String> {
    release_name(repository)
        .ok_or_else(|| format!("github:{repository}'s repo name isn't a valid package name"))
}

/// The release line or range a request names, when it isn't the version it
/// resolved to.
pub(super) fn line(asked: Option<&str>, resolved: &str) -> Option<String> {
    asked.filter(|asked| *asked != resolved).map(str::to_string)
}

/// A package's output at the version asked for, which may name a release
/// line or a range, or at its default.
pub(super) fn select<'a>(
    package: &'a Package,
    version: Option<&str>,
    platform: Platform,
) -> Result<(String, &'a Output), String> {
    let name = &package.name;
    let version = match version {
        Some(asked) => {
            let built = package
                .versions
                .iter()
                .filter(|(_, outputs)| outputs.contains_key(&platform))
                .map(|(version, _)| version.as_str());
            super::version::resolve(built, asked).unwrap_or(asked)
        }
        None => package
            .default
            .get(&platform)
            .ok_or_else(|| format!("{name} has no default version on {platform}"))?,
    };

    let outputs = package
        .versions
        .get(version)
        .ok_or_else(|| format!("{name} has no version {version:?}"))?;

    let output = outputs
        .get(&platform)
        .ok_or_else(|| format!("{name}@{version} isn't built for {platform}"))?;

    Ok((version.to_string(), output))
}

pub(super) fn open_index(source: &Source, is_root: bool) -> Result<Index, String> {
    let root = match &source.root {
        Some(path) => fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?,
        None => TRUSTED_ROOT.to_vec(),
    };

    let url = source.index.as_deref().unwrap_or(INDEX);
    let datastore = datastore(&root, is_root)?;
    Index::refresh(&root, url, &datastore, source.is_http_allowed)
        .map_err(|error| error.to_string())
}

/// The signed index, opened the first time something needs it.
pub(super) struct LazyIndex<'a> {
    source: &'a Source,
    index: OnceCell<Index>,
}

impl<'a> LazyIndex<'a> {
    pub(super) fn new(source: &'a Source) -> Self {
        LazyIndex {
            source,
            index: OnceCell::new(),
        }
    }

    pub(super) fn source(&self) -> &'a Source {
        self.source
    }

    pub(super) fn get(&self) -> Result<&Index, String> {
        if let Some(index) = self.index.get() {
            return Ok(index);
        }

        let index = open_index(self.source, rustix::process::geteuid().is_root())?;
        Ok(self.index.get_or_init(|| index))
    }
}

/// Renames each index request that names an alias to the package it names,
/// so a profile records one name per package. Only a request `is_known`
/// doesn't recognize opens the index.
pub(super) fn resolve_aliases(
    requests: BTreeMap<String, Wanted>,
    is_known: impl Fn(&str) -> bool,
    index: &LazyIndex,
) -> Result<BTreeMap<String, Wanted>, String> {
    let mut resolved = BTreeMap::new();
    for (name, wanted) in requests {
        let canonical = match matches!(wanted, Wanted::Index(_)) && !is_known(&name) {
            true => index.get()?.canonical(&name),
            false => None,
        };

        let canonical = canonical.unwrap_or_else(|| name.clone());
        if canonical != name {
            eprintln!("{name} is an alias of {canonical}");
        }

        let asked = resolved.insert(canonical.clone(), wanted.clone());
        if asked.is_some_and(|asked| asked != wanted) {
            return Err(format!("conflicting requests for {canonical}"));
        }
    }

    Ok(resolved)
}

/// Each trusted root keeps its own metadata, so another index's newer root
/// can never stand in for this one's. Root keeps it beside the store, since
/// sudo leaves HOME pointing at the user's own directory.
fn datastore(root: &[u8], is_root: bool) -> Result<PathBuf, String> {
    let id = HEXLOWER.encode(&Sha256::digest(root));
    if is_root {
        return Ok(Path::new(ROOT).join("var/index").join(id));
    }

    let absolute = |name: &str| {
        std::env::var_os(name)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };

    let state = absolute("XDG_STATE_HOME")
        .or_else(|| absolute("HOME").map(|home| home.join(".local/state")))
        .ok_or("set XDG_STATE_HOME or HOME to an absolute path")?;

    Ok(state.join("rootbeer/index").join(id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::collections::{BTreeMap, BTreeSet};

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: Args,
    }

    #[test]
    fn the_key_path_must_be_asked_for_and_never_mixes_with_an_index() {
        let parse = |extra: &[&str]| {
            Cli::try_parse_from(["install", "zstd"].iter().chain(extra)).map(|cli| cli.args)
        };

        let args = parse(&[]).unwrap();
        assert_eq!(args.source.registry, "https://ghcr.io");
        let default = Source::default();
        assert_eq!(
            (
                &default.registry,
                &default.namespace,
                default.is_http_allowed
            ),
            (&args.source.registry, &args.source.namespace, false)
        );
        assert!(args.key.is_none() && args.source.index.is_none());

        let key = "a".repeat(32);
        assert!(parse(&["--key", &key, "--unverified"]).is_ok());
        assert!(parse(&["--index", "file:///i", "--root", "root.json"]).is_ok());

        let refused: [&[&str]; 5] = [
            &["--key", &key],
            &["--unverified"],
            &["--index", "file:///i"],
            &["--root", "root.json"],
            &[
                "--key",
                &key,
                "--unverified",
                "--index",
                "file:///i",
                "--root",
                "r",
            ],
        ];
        for extra in refused {
            assert!(parse(extra).is_err(), "{extra:?}");
        }
    }

    #[test]
    fn a_version_and_platform_pick_one_output_or_say_which_is_missing() {
        let [linux, macos] = [Platform::Aarch64Linux, Platform::Aarch64Macos];
        let output = |digit: char| Output {
            key: digit.to_string().repeat(32).parse().unwrap(),
            manifest: format!("sha256:{}", "0".repeat(64)),
            bins: BTreeSet::new(),
            apps: BTreeMap::new(),
            app_settings: BTreeMap::new(),
        };

        let package = Package {
            name: "zstd".into(),
            description: String::new(),
            license: String::new(),
            default: BTreeMap::from([(linux, "1.5.7".into())]),
            versions: BTreeMap::from([
                ("1.5.6".into(), BTreeMap::from([(linux, output('a'))])),
                ("1.5.7".into(), BTreeMap::from([(linux, output('b'))])),
            ]),
            ..Package::default()
        };

        let key = |version: Option<&str>| {
            select(&package, version, linux).map(|(version, output)| (version, output.key.clone()))
        };
        assert_eq!(key(None), Ok(("1.5.7".into(), output('b').key)));
        assert_eq!(key(Some("1.5.6")), Ok(("1.5.6".into(), output('a').key)));

        let missing = [
            (None, macos, "zstd has no default version on aarch64-macos"),
            (Some("1.4"), linux, "zstd has no version \"1.4\""),
            (Some(""), linux, "zstd has no version \"\""),
            (
                Some("1.5.6"),
                macos,
                "zstd@1.5.6 isn't built for aarch64-macos",
            ),
        ];

        for (version, platform, error) in missing {
            assert_eq!(select(&package, version, platform).unwrap_err(), error);
        }
    }

    #[test]
    fn root_keeps_index_metadata_beside_the_store() {
        let path = datastore(b"root", true).unwrap();
        assert!(path.starts_with("/opt/rb/var/index"));
        assert_ne!(path, datastore(b"other", true).unwrap());
    }

    #[test]
    fn a_version_asked_for_may_name_a_release_line() {
        let output = Output {
            key: "a".repeat(32).parse().unwrap(),
            manifest: format!("sha256:{}", "0".repeat(64)),
            bins: BTreeSet::new(),
            apps: BTreeMap::new(),
            app_settings: BTreeMap::new(),
        };
        let built = |platforms: &[Platform]| {
            platforms
                .iter()
                .map(|&platform| (platform, output.clone()))
                .collect::<BTreeMap<_, _>>()
        };
        let (macos, linux) = (Platform::Aarch64Macos, Platform::X86_64Linux);
        let versions = [
            ("24.9.0", built(&[macos, linux])),
            ("24.10.0", built(&[macos])),
            ("26.1.0", built(&[macos, linux])),
            ("2.4", built(&[macos])),
            ("2.4.1", built(&[macos])),
        ];
        let package = Package {
            name: "node".into(),
            default: BTreeMap::from([(macos, "26.1.0".into())]),
            versions: versions
                .into_iter()
                .map(|(version, outputs)| (version.to_string(), outputs))
                .collect(),
            ..Package::default()
        };

        let selected =
            |asked, platform| select(&package, asked, platform).map(|(version, _)| version);
        assert_eq!(selected(None, macos).unwrap(), "26.1.0");
        assert_eq!(selected(Some("24"), macos).unwrap(), "24.10.0");
        assert_eq!(selected(Some("24.9"), macos).unwrap(), "24.9.0");
        assert_eq!(selected(Some("24.9.0"), macos).unwrap(), "24.9.0");

        // A line holds only what the platform has, and a version wins over
        // the line it starts.
        assert_eq!(selected(Some("24"), linux).unwrap(), "24.9.0");
        assert_eq!(selected(Some("2.4"), macos).unwrap(), "2.4");
        assert_eq!(selected(Some("2"), macos).unwrap(), "2.4.1");

        assert!(selected(Some("2"), linux).is_err());
        assert!(selected(Some("25"), macos).is_err());
        assert!(selected(Some("24.1"), macos).is_err());

        assert_eq!(line(Some("24"), "24.10.0").as_deref(), Some("24"));
        assert_eq!(line(Some("24.10.0"), "24.10.0"), None);
        assert_eq!(line(None, "26.1.0"), None);
    }
}
