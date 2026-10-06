use super::{store_error, HELPER};
use rootbeer_cache::Cache;
use rootbeer_drv::{output_path, Derivation, Key, STORE_ROOT};
use rootbeer_store::{Store, ROOT};
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
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
    #[arg(long)]
    registry: Option<String>,
    #[arg(long = "from", requires = "registry")]
    namespaces: Vec<String>,
    /// Namespace whose outputs are used only once `--verify` accepts them,
    /// such as `rootbeer-org/staging`. It's searched after every `--from`.
    #[arg(
        long = "from-verified",
        value_name = "NAMESPACE",
        requires_all = ["registry", "verify"]
    )]
    verified: Vec<String>,
    /// Program run on each manifest from a `--from-verified` namespace, given
    /// the manifest's path. Any exit but success rejects that output.
    #[arg(long, value_name = "PROGRAM", requires = "verified")]
    verify: Option<PathBuf>,
    #[arg(long = "allow-http", requires = "registry")]
    is_http_allowed: bool,
}

/// A namespace to substitute from, and the program its outputs must pass.
pub(super) struct Source {
    pub(super) cache: Cache,
    pub(super) verify: Option<PathBuf>,
}

impl Caches {
    pub(super) fn open(&self) -> Vec<Source> {
        let Some(registry) = &self.registry else {
            return Vec::new();
        };

        let source = |namespace: &String, verify: Option<PathBuf>| Source {
            cache: open(registry, namespace, self.is_http_allowed),
            verify,
        };

        let trusted = self
            .namespaces
            .iter()
            .map(|namespace| source(namespace, None));
        let verified = self
            .verified
            .iter()
            .map(|namespace| source(namespace, self.verify.clone()));

        trusted.chain(verified).collect()
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

/// Installs an output from the first source that has it. A source anyone can
/// write to may hold a bad output, so that one is built instead.
pub(super) fn substitute(
    sources: &[Source],
    store: &mut Store,
    is_root: bool,
    name: &str,
    key: &Key,
) -> Result<bool, String> {
    for Source { cache, verify } in sources {
        if !cache.exists(name, key).map_err(|error| error.to_string())? {
            continue;
        }

        let result = install_into(cache, verify.as_deref(), store, is_root, name, key);
        match (result, verify) {
            (Ok(()), _) => return Ok(true),
            (Err(error), Some(_)) => eprintln!("not substituting {name} {key}: {error}"),
            (Err(error), None) => return Err(error),
        }
    }

    Ok(false)
}

pub(super) fn install_into(
    cache: &Cache,
    verify: Option<&Path>,
    store: &mut Store,
    is_root: bool,
    name: &str,
    key: &Key,
) -> Result<(), String> {
    let mut installer = Installer {
        cache,
        verify,
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

    install_into(&registry.cache(), None, &mut store, is_root, name, key)?;
    let path = store
        .path(key)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("{key} isn't present after installing"))?;

    println!("{}", path.display());
    Ok(())
}

struct Installer<'a> {
    cache: &'a Cache,
    verify: Option<&'a Path>,
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

        if let Some(program) = self.verify {
            run_verifier(program, pulled.manifest())
                .map_err(|error| format!("{name} {key} wasn't verified: {error}"))?;
        }

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

/// Copies an output and its references between namespaces. With a verifier,
/// each manifest must pass it, or nothing more is copied.
pub(super) fn promote(
    source: &Cache,
    target: &Cache,
    verify: Option<&Path>,
    name: &str,
    key: &Key,
) -> Result<(), String> {
    let mut promoting = Vec::new();
    promote_into(source, target, verify, name, key, &mut promoting)
}

fn promote_into(
    source: &Cache,
    target: &Cache,
    verify: Option<&Path>,
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
    if let Some(program) = verify {
        run_verifier(program, pulled.manifest())
            .map_err(|error| format!("{name} {key} wasn't verified: {error}"))?;
    }

    promoting.push(key.clone());
    for (reference, name) in &pulled.references {
        promote_into(source, target, verify, name, reference, promoting)?;
    }

    promoting.pop();
    eprintln!("promoting {key} {name}");
    target.promote(pulled).map_err(|error| error.to_string())
}

/// Runs a verifier on a manifest's exact bytes, which the registry addresses
/// by their digest. Its output goes to stderr, and any exit but success
/// rejects the manifest.
fn run_verifier(program: &Path, manifest: &[u8]) -> Result<(), String> {
    let mut file = tempfile::NamedTempFile::new().map_err(|error| error.to_string())?;
    file.write_all(manifest)
        .map_err(|error| error.to_string())?;

    let status = Command::new(program)
        .arg(file.path())
        .stdin(Stdio::null())
        .stdout(io::stderr())
        .status()
        .map_err(|error| format!("{}: {error}", program.display()))?;

    if !status.success() {
        return Err(format!("{} {status}", program.display()));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn verifiers_see_the_exact_manifest_and_reject_by_exit_status() {
        let directory = tempfile::tempdir().unwrap();
        let program = directory.path().join("verify");
        fs::write(&program, "#!/bin/sh\n[ \"$(cat \"$1\")\" = signed ]\n").unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();

        run_verifier(&program, b"signed").unwrap();
        let error = run_verifier(&program, b"forged").unwrap_err();
        assert!(error.ends_with("exit status: 1"), "{error}");

        let missing = directory.path().join("missing");
        assert!(run_verifier(&missing, b"signed").is_err());
    }
}
