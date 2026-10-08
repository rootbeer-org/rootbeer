use super::HELPER;
use rootbeer_cache::Cache;
use rootbeer_drv::{output_path, Derivation, Key};
use rootbeer_store::Store;
use rootbeer_trust::Package;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read};
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
        open(&self.registry, &self.namespace, self.is_http_allowed)
    }
}

type Fetch<'a> = Box<dyn FnMut(&str) -> Result<Option<Package>, String> + 'a>;
pub(super) struct Signed<'a> {
    fetch: Fetch<'a>,
    packages: BTreeMap<String, Option<Package>>,
}

impl<'a> Signed<'a> {
    pub(super) fn new(fetch: impl FnMut(&str) -> Result<Option<Package>, String> + 'a) -> Self {
        Signed {
            fetch: Box::new(fetch),
            packages: BTreeMap::new(),
        }
    }

    pub(super) fn package(&mut self, name: &str) -> Result<&Package, String> {
        if !self.packages.contains_key(name) {
            let package = (self.fetch)(name)?;
            self.packages.insert(name.to_string(), package);
        }

        self.packages
            .get(name)
            .and_then(Option::as_ref)
            .ok_or_else(|| format!("{name} isn't in the index"))
    }

    fn manifest(&mut self, name: &str, key: &Key) -> Result<String, String> {
        let package = self.package(name)?;
        let mut outputs = package.versions.values().flat_map(BTreeMap::values);
        outputs
            .find(|output| output.key == *key)
            .map(|output| &output.manifest)
            .or_else(|| package.retired.get(key))
            .cloned()
            .ok_or_else(|| format!("{name} {key} isn't in the index"))
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
    signed: Option<&mut Signed>,
) -> Result<(), String> {
    let mut installer = Installer {
        cache,
        store,
        is_root,
        signed,
        installing: Vec::new(),
    };

    installer.visit(name, key, None)
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

struct Installer<'a, 'b> {
    cache: &'a Cache,
    store: &'a mut Store,
    is_root: bool,
    signed: Option<&'a mut Signed<'b>>,
    installing: Vec<Key>,
}

impl Installer<'_, '_> {
    fn visit(&mut self, name: &str, key: &Key, referrer: Option<&str>) -> Result<(), String> {
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

        let digest = match &mut self.signed {
            Some(signed) => Some(signed.manifest(name, key).map_err(|error| match referrer {
                Some(referrer) => format!("{error}, though {referrer} references it"),
                None => error,
            })?),
            None => None,
        };

        let pulled = self
            .cache
            .pull(name, key, digest.as_deref())
            .map_err(|error| error.to_string())?;
        let Derivation::Build(build) = &pulled.derivation else {
            return Err(format!("{key} isn't a build output"));
        };

        self.installing.push(key.clone());
        for (reference, reference_name) in &pulled.references {
            self.visit(reference_name, reference, Some(name))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use data_encoding::HEXLOWER;
    use rootbeer_drv::{Build, Platform};
    use rootbeer_trust::Output;
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::collections::HashMap;
    use std::fs;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    struct Published {
        name: String,
        key: Key,
        manifest: Vec<u8>,
        blobs: Vec<Vec<u8>>,
    }

    fn sha256(bytes: &[u8]) -> String {
        format!("sha256:{}", HEXLOWER.encode(&Sha256::digest(bytes)))
    }

    fn descriptor(media_type: &str, bytes: &[u8]) -> serde_json::Value {
        json!({ "mediaType": media_type, "digest": sha256(bytes), "size": bytes.len() })
    }

    fn publish(name: &str, file: &str, references: &[&Published]) -> Published {
        let build = Build {
            name: name.into(),
            version: "1".into(),
            platform: Platform::Aarch64Linux,
            sandbox: "linux-v1".into(),
            allow: BTreeSet::new(),
            inputs: BTreeMap::new(),
            dependencies: Vec::new(),
            env: BTreeMap::new(),
            script: format!("touch $out/{name}"),
            outputs: BTreeSet::from(["out".into()]),
        };

        let derivation = Derivation::Build(build);
        let key = derivation.key().unwrap();
        let config = serde_json::to_vec(&derivation).unwrap();

        let tree = tempfile::tempdir().unwrap();
        fs::write(tree.path().join(file), name).unwrap();
        let mut layer = Vec::new();
        rootbeer_store::pack(tree.path(), &mut layer).unwrap();

        let references = references
            .iter()
            .map(|reference| format!("{}:{}", reference.name, reference.key))
            .collect::<Vec<_>>();
        let manifest = json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "artifactType": "application/vnd.rootbeer.output.v1",
            "config": descriptor("application/vnd.rootbeer.derivation.v1+json", &config),
            "layers": [descriptor("application/vnd.rootbeer.output.v1.tar+zstd", &layer)],
            "annotations": {
                "com.rbpkg.key": key.to_string(),
                "com.rbpkg.references": references.join(","),
            },
        });

        Published {
            name: name.into(),
            key,
            manifest: serde_json::to_vec(&manifest).unwrap(),
            blobs: vec![config, layer],
        }
    }

    impl Published {
        fn entry(&self, manifest: &str) -> Package {
            let output = Output {
                key: self.key.clone(),
                manifest: manifest.into(),
                bins: BTreeSet::new(),
                apps: BTreeMap::new(),
            };

            Package {
                name: self.name.clone(),
                description: String::new(),
                license: String::new(),
                default: BTreeMap::from([(Platform::Aarch64Linux, "1".into())]),
                versions: BTreeMap::from([(
                    "1".into(),
                    BTreeMap::from([(Platform::Aarch64Linux, output)]),
                )]),
                retired: BTreeMap::new(),
            }
        }
    }

    fn serve(outputs: &[&Published]) -> String {
        let mut paths = HashMap::new();
        for output in outputs {
            paths.insert(
                format!("{}/manifests/", output.name),
                output.manifest.clone(),
            );
            for blob in &output.blobs {
                paths.insert(format!("/blobs/{}", sha256(blob)), blob.clone());
            }
        }

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut line = String::new();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                reader.read_line(&mut line).unwrap();
                while reader.read_line(&mut String::new()).unwrap() > 2 {}

                let target = line.split(' ').nth(1).unwrap_or_default();
                let body = paths
                    .iter()
                    .find(|(path, _)| target.contains(path.as_str()))
                    .map(|(_, body)| body.clone());

                let (status, body) = match body {
                    Some(body) => (200, body),
                    None => (404, Vec::new()),
                };

                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(head.as_bytes()).unwrap();
                stream.write_all(&body).unwrap();
            }
        });

        url
    }

    fn install(
        served: &[&Published],
        index: Vec<Package>,
        top: &Published,
    ) -> (Result<(), String>, Vec<bool>) {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("store")).unwrap();
        let mut store = Store::open(root.path()).unwrap();
        let cache = Cache::new(&serve(served), "rootbeer-test/store").allow_http();

        let index = index
            .into_iter()
            .map(|package| (package.name.clone(), package))
            .collect::<BTreeMap<_, _>>();
        let mut signed = Signed::new(|name| Ok(index.get(name).cloned()));

        let result = install_into(
            &cache,
            &mut store,
            true,
            &top.name,
            &top.key,
            Some(&mut signed),
        );
        let present = served
            .iter()
            .map(|output| store.path(&output.key).unwrap().is_some())
            .collect();

        std::process::Command::new("chmod")
            .args(["-R", "u+w"])
            .arg(root.path())
            .status()
            .unwrap();

        (result, present)
    }

    #[test]
    fn a_closure_installs_only_through_manifests_the_index_signed() {
        let zlib = publish("zlib", "libz", &[]);
        let zstd = publish("zstd", "zstd", &[&zlib]);
        let swapped = publish("zlib", "libz-swapped", &[]);
        let signed = |output: &Published| output.entry(&sha256(&output.manifest));

        let (result, present) = install(&[&zlib, &zstd], vec![signed(&zlib), signed(&zstd)], &zstd);
        result.unwrap();
        assert_eq!(present, [true, true]);

        let mut retired = zlib.entry(&sha256(&swapped.manifest));
        retired.versions.clear();
        retired
            .retired
            .insert(zlib.key.clone(), sha256(&zlib.manifest));

        let (result, present) = install(&[&zlib, &zstd], vec![retired, signed(&zstd)], &zstd);
        result.unwrap();
        assert_eq!(present, [true, true]);

        let unsigned = zlib.entry(&sha256(&swapped.manifest));
        let (result, present) = install(&[&zlib, &zstd], vec![unsigned, signed(&zstd)], &zstd);
        assert!(result.unwrap_err().contains(", not sha256:"));
        assert_eq!(present, [false, false]);

        let (result, present) = install(&[&zlib, &zstd], vec![signed(&zstd)], &zstd);
        assert_eq!(
            result.unwrap_err(),
            "zlib isn't in the index, though zstd references it"
        );

        assert_eq!(present, [false, false]);
        let mut other = signed(&zlib);
        other.versions.values_mut().for_each(|outputs| {
            outputs
                .values_mut()
                .for_each(|output| output.key = zstd.key.clone())
        });

        let (result, _) = install(&[&zlib, &zstd], vec![other, signed(&zstd)], &zstd);
        assert_eq!(
            result.unwrap_err(),
            format!(
                "zlib {} isn't in the index, though zstd references it",
                zlib.key
            )
        );
    }
}
