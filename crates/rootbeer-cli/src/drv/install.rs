use super::cache::{self, Signed};
use super::{store_error, this_platform};
use data_encoding::HEXLOWER;
use rootbeer_drv::{is_package_name, Key, Platform, STORE_ROOT};
use rootbeer_store::{Store, ROOT};
use rootbeer_trust::{Index, Package};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

const INDEX: &str = "https://rootbeer-org.github.io/index/";
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

pub(super) fn install(args: &Args) -> Result<(), String> {
    let is_root = rustix::process::geteuid().is_root();
    if is_root {
        fs::create_dir_all(STORE_ROOT).map_err(|error| format!("{STORE_ROOT}: {error}"))?;
    }

    let mut store = match is_root {
        true => Store::open(Path::new(ROOT)),
        false => Store::open_read_only(Path::new(ROOT)),
    }
    .map_err(store_error)?;

    let cache = cache::open(&args.registry, &args.namespace, args.is_http_allowed);
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
            let index = open_index(args, is_root)?;
            let mut signed =
                Signed::new(|name| index.package(name).map_err(|error| error.to_string()));
            let (name, version) = match args.package.split_once('@') {
                Some((name, version)) => (name, Some(version)),
                None => (args.package.as_str(), None),
            };

            let key = select(signed.package(name)?, version, this_platform()?)?;
            cache::install_into(&cache, &mut store, is_root, name, &key, Some(&mut signed))?;
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

fn select(package: &Package, version: Option<&str>, platform: Platform) -> Result<Key, String> {
    let name = &package.name;
    let version = match version {
        Some(version) => version,
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

    Ok(output.key.clone())
}

fn open_index(args: &Args, is_root: bool) -> Result<Index, String> {
    let root = match &args.root {
        Some(path) => fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?,
        None => TRUSTED_ROOT.to_vec(),
    };

    let url = args.index.as_deref().unwrap_or(INDEX);
    let datastore = datastore(&root, is_root)?;
    Index::refresh(&root, url, &datastore, args.is_http_allowed).map_err(|error| error.to_string())
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
    use rootbeer_trust::Output;
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
        assert_eq!(args.registry, "https://ghcr.io");
        assert!(args.key.is_none() && args.index.is_none());

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
            retired: BTreeMap::new(),
        };

        let key = |digit: char| Ok(output(digit).key);
        assert_eq!(select(&package, None, linux), key('b'));
        assert_eq!(select(&package, Some("1.5.6"), linux), key('a'));

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
}
