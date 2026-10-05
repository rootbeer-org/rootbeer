use super::{store_error, HELPER};
use rootbeer_cache::Cache;
use rootbeer_drv::{output_path, Derivation, Key, STORE_ROOT};
use rootbeer_store::{Store, ROOT};
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(clap::Args, Debug)]
pub(super) struct Registry {
    #[arg(long)]
    registry: String,
    #[arg(long, default_value = "rootbeer-org/store")]
    namespace: String,
    #[arg(long = "allow-http")]
    is_http_allowed: bool,
}

impl Registry {
    pub(super) fn cache(&self) -> Cache {
        self.cache_at(&self.namespace)
    }

    pub(super) fn cache_at(&self, namespace: &str) -> Cache {
        open(&self.registry, namespace, self.is_http_allowed)
    }
}

#[derive(clap::Args, Debug)]
pub(super) struct Caches {
    #[arg(long, requires = "namespaces")]
    registry: Option<String>,
    #[arg(long = "from", requires = "registry")]
    namespaces: Vec<String>,
    #[arg(long = "allow-http", requires = "registry")]
    is_http_allowed: bool,
}

impl Caches {
    pub(super) fn open(&self) -> Vec<Cache> {
        let Some(registry) = &self.registry else {
            return Vec::new();
        };

        self.namespaces
            .iter()
            .map(|namespace| open(registry, namespace, self.is_http_allowed))
            .collect()
    }
}

pub(super) fn open(registry: &str, namespace: &str, is_http_allowed: bool) -> Cache {
    let mut cache = Cache::new(registry, namespace);
    if is_http_allowed {
        cache = cache.allow_http();
    }

    let user = std::env::var("RB_REGISTRY_USER");
    let token = std::env::var("RB_REGISTRY_TOKEN");
    if let (Ok(user), Ok(token)) = (user, token) {
        cache = cache.with_credentials(&user, &token);
    }

    cache
}

pub(super) fn find<'a>(
    caches: &'a [Cache],
    name: &str,
    key: &Key,
) -> Result<Option<&'a Cache>, String> {
    for cache in caches {
        if cache.exists(name, key).map_err(|error| error.to_string())? {
            return Ok(Some(cache));
        }
    }

    Ok(None)
}

pub(super) fn install_into(
    cache: &Cache,
    store: &mut Store,
    is_root: bool,
    name: &str,
    key: &Key,
) -> Result<(), String> {
    let mut installer = Installer {
        cache,
        store,
        is_root,
        installing: Vec::new(),
    };

    installer.visit(name, key)
}

pub(super) fn references_first(
    store: &Store,
    key: &Key,
    seen: &mut BTreeSet<Key>,
    order: &mut Vec<(Key, BTreeSet<Key>)>,
) -> Result<(), String> {
    if !seen.insert(key.clone()) {
        return Ok(());
    }

    let references = store.references(key).map_err(|error| error.to_string())?;
    for reference in &references {
        references_first(store, reference, seen, order)?;
    }

    order.push((key.clone(), references));
    Ok(())
}

pub(super) fn install(name: &str, key: &Key, registry: &Registry) -> Result<(), String> {
    let root = Path::new(ROOT);
    let is_root = rustix::process::geteuid().is_root();
    if is_root {
        fs::create_dir_all(STORE_ROOT).map_err(|error| format!("{STORE_ROOT}: {error}"))?;
    }

    let mut store = match is_root {
        true => Store::open(root),
        false => Store::open_read_only(root),
    }
    .map_err(store_error)?;

    install_into(&registry.cache(), &mut store, is_root, name, key)?;
    let path = store
        .path(key)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("{key} isn't present after installing"))?;

    println!("{}", path.display());
    Ok(())
}

struct Installer<'a> {
    cache: &'a Cache,
    store: &'a mut Store,
    is_root: bool,
    installing: Vec<Key>,
}

impl Installer<'_> {
    fn visit(&mut self, name: &str, key: &Key) -> Result<(), String> {
        if self.installing.contains(key) {
            return Err(format!("{name} {key} is in its own references"));
        }

        if self
            .store
            .path(key)
            .map_err(|error| error.to_string())?
            .is_some()
        {
            return Ok(());
        }

        let pulled = self
            .cache
            .pull(name, key)
            .map_err(|error| error.to_string())?;
        let Derivation::Build(build) = &pulled.derivation else {
            return Err(format!("{key} isn't a build output"));
        };

        self.installing.push(key.clone());
        for (reference, name) in &pulled.references {
            self.visit(name, reference)?;
        }

        self.installing.pop();
        let path = output_path(key, &build.name, &build.version, "out");
        let entry = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("{} has no entry name", path.display()))?
            .to_string();

        eprintln!("installing {key} {}@{}", build.name, build.version);
        let references = pulled.references.keys().cloned().collect::<BTreeSet<_>>();
        let digest = pulled.digest.clone();
        let archive = pulled.archive().map_err(|error| error.to_string())?;
        if self.is_root {
            self.store
                .pull(key, &entry, &digest, &references, archive)
                .map_err(|error| error.to_string())?;
            return Ok(());
        }

        helper_pull(key, &entry, &digest, &references, archive)
    }
}

pub(super) fn helper_pull(
    key: &Key,
    entry: &str,
    digest: &str,
    references: &BTreeSet<Key>,
    mut archive: Box<dyn Read>,
) -> Result<(), String> {
    let mut child = Command::new(HELPER)
        .args(["pull", key.as_str(), entry, digest])
        .args(references.iter().map(Key::as_str))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .map_err(|error| format!("{HELPER}: {error}"))?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| format!("{HELPER} has no stdin"))?;

    let copied = io::copy(&mut archive, &mut stdin);
    drop(stdin);

    let status = child.wait().map_err(|error| error.to_string())?;
    copied.map_err(|error| format!("downloading {key}: {error}"))?;
    if !status.success() {
        return Err(format!("{HELPER} pull {key} {status}"));
    }

    Ok(())
}

/// Copies an output and its references between namespaces. With a signer
/// workflow, each manifest must carry an attestation from it, or nothing more
/// is copied.
pub(super) fn promote(
    source: &Cache,
    target: &Cache,
    signer: Option<&str>,
    name: &str,
    key: &Key,
) -> Result<(), String> {
    let mut promoting = Vec::new();
    promote_into(source, target, signer, name, key, &mut promoting)
}

fn promote_into(
    source: &Cache,
    target: &Cache,
    signer: Option<&str>,
    name: &str,
    key: &Key,
    promoting: &mut Vec<Key>,
) -> Result<(), String> {
    if promoting.contains(key) {
        return Err(format!("{name} {key} is in its own references"));
    }

    if target
        .exists(name, key)
        .map_err(|error| error.to_string())?
    {
        return Ok(());
    }

    let pulled = source.pull(name, key).map_err(|error| error.to_string())?;
    if let Some(signer) = signer {
        verify_attestation(pulled.manifest(), signer)
            .map_err(|error| format!("{name} {key} isn't attested: {error}"))?;
    }

    promoting.push(key.clone());
    for (reference, name) in &pulled.references {
        promote_into(source, target, signer, name, reference, promoting)?;
    }

    promoting.pop();
    eprintln!("promoting {key} {name}");
    target.promote(pulled).map_err(|error| error.to_string())
}

fn verify_attestation(manifest: &[u8], signer: &str) -> Result<(), String> {
    let mut parts = signer.splitn(3, '/');
    let (Some(owner), Some(repository), Some(_)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(format!("{signer} isn't owner/repository/path"));
    };

    let mut file = tempfile::NamedTempFile::new().map_err(|error| error.to_string())?;
    file.write_all(manifest)
        .map_err(|error| error.to_string())?;

    let output = Command::new("gh")
        .args(["attestation", "verify"])
        .arg(file.path())
        .args([
            "--repo",
            &format!("{owner}/{repository}"),
            "--signer-workflow",
            signer,
        ])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("gh: {error}"))?;

    if !output.status.success() {
        let reason = String::from_utf8_lossy(&output.stderr);
        return Err(reason.trim().to_string());
    }

    Ok(())
}
