use super::{evaluate, store_error, Sources, HELPER};
use rootbeer_cache::Cache;
use rootbeer_drv::{output_path, Derivation, Key, STORE_ROOT};
use rootbeer_store::{pack, Store, ROOT};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read};
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
    fn cache(&self) -> Cache {
        let mut cache = Cache::new(&self.registry, &self.namespace);
        if self.is_http_allowed {
            cache = cache.allow_http();
        }

        let user = std::env::var("RB_REGISTRY_USER");
        let token = std::env::var("RB_REGISTRY_TOKEN");
        if let (Ok(user), Ok(token)) = (user, token) {
            cache = cache.with_credentials(&user, &token);
        }

        cache
    }
}

pub(super) fn push(sources: &Sources, package: &str, registry: &Registry) -> Result<(), String> {
    let (graph, target) = evaluate(sources, package, None)?;
    let package = graph
        .packages
        .get(&target)
        .ok_or_else(|| format!("{target} did not evaluate"))?;

    let store = Store::open_read_only(Path::new(ROOT)).map_err(store_error)?;
    let mut order = Vec::new();
    references_first(&store, &package.build, &mut BTreeSet::new(), &mut order)?;

    let cache = registry.cache();
    let build_of = |key: &Key| match graph.derivations.get(key) {
        Some(Derivation::Build(build)) => Ok(build),
        _ => Err(format!(
            "{key} isn't a build in the graph, so it can't be pushed"
        )),
    };

    for (key, references) in &order {
        let build = build_of(key)?;
        let path = store
            .path(key)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("{} isn't built, so run `rb drv build` first", build.name))?;

        if cache
            .exists(&build.name, key)
            .map_err(|error| error.to_string())?
        {
            eprintln!("cached {key} {}@{}", build.name, build.version);
            continue;
        }

        let named = references
            .iter()
            .map(|reference| Ok((reference.clone(), build_of(reference)?.name.clone())))
            .collect::<Result<BTreeMap<_, _>, String>>()?;

        let archive = tempfile::NamedTempFile::new().map_err(|error| error.to_string())?;
        pack(&path, archive.as_file()).map_err(|error| error.to_string())?;

        eprintln!("pushing {key} {}@{}", build.name, build.version);
        let derivation = Derivation::Build(build.clone());
        cache
            .push(key, &derivation, &named, archive.path())
            .map_err(|error| error.to_string())?;
    }

    Ok(())
}

fn references_first(
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

    let cache = registry.cache();
    let mut installer = Installer {
        cache: &cache,
        store: &mut store,
        is_root,
        installing: Vec::new(),
    };

    installer.visit(name, key)?;
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

fn helper_pull(
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
