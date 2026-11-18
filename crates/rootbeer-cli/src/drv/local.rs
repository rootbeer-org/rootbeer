use super::cache::{self, Signed};
use super::install::Wanted;
use super::{label, load_from, present_references, realize, target};
use rootbeer_cache::Cache;
use rootbeer_drv::{Derivation, Key, Platform};
use rootbeer_eval::{Graph, Package, Target};
use rootbeer_store::Store;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// What each name a checkout serves evaluated to on this platform.
pub(crate) struct Evaluated {
    graph: Graph,
    targets: BTreeMap<String, Target>,
}

/// Hands each request for a name the checkout at `root` defines to that
/// recipe instead of the index. The checkout holds `packages/` and `host.lua`
/// like the index's, though this machine's toolchain replaces the one
/// `host.lua` names when it can be identified. None when it defines none of
/// the names.
pub(crate) fn shadow(
    requests: &mut BTreeMap<String, Wanted>,
    root: &Path,
    platform: Platform,
) -> Result<Option<Evaluated>, String> {
    let (catalog, mut hosts) = load_from(&root.join("packages"), &root.join("host.lua"))?;

    // Builds here use this machine's toolchain, so their keys must name it.
    // Prebuilts don't key on it, so they still match the index's.
    if let (Ok(system), Some(host)) = (rootbeer_sandbox::system(), hosts.get_mut(&platform)) {
        host.system = system;
    }
    let mut targets = BTreeMap::new();
    for (name, wanted) in requests.iter() {
        let Wanted::Index(version) = wanted else {
            continue;
        };

        if !catalog.contains(name) {
            continue;
        }

        let package = match version {
            Some(version) => format!("{name}@{version}"),
            None => name.clone(),
        };
        targets.insert(name.clone(), target(&catalog, &package, platform)?);
    }

    if targets.is_empty() {
        return Ok(None);
    }

    let wanted = targets.values().cloned().collect::<Vec<_>>();
    let graph = catalog
        .evaluate(&hosts, &wanted)
        .map_err(|error| error.to_string())?;
    let evaluated = Evaluated { graph, targets };

    for (name, wanted) in requests.iter_mut() {
        let Ok((_, package)) = evaluated.package(name) else {
            continue;
        };

        *wanted = Wanted::Local {
            version: wanted.version().map(str::to_string),
            key: package.build.clone(),
            bins: package.metadata.bins.clone(),
            apps: package.metadata.apps.clone(),
        };
    }

    Ok(Some(evaluated))
}

impl Evaluated {
    pub(crate) fn package(&self, name: &str) -> Result<(&Target, &Package), String> {
        let target = self
            .targets
            .get(name)
            .ok_or_else(|| format!("{name} isn't in the catalog"))?;
        let package = self
            .graph
            .packages
            .get(target)
            .ok_or_else(|| format!("{target} did not evaluate"))?;
        Ok((target, package))
    }

    /// Puts `name`'s build into the store with everything it needs. A build
    /// the signed index publishes under the same key is pulled, since it's
    /// the same build. With `should_check`, the recipe's check runs unless
    /// the index publishes the build, which means it was checked there.
    /// Offline, only what the store has will do.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn install(
        &self,
        name: &str,
        store: &mut Store,
        is_root: bool,
        cache: &Cache,
        signed: &mut Signed,
        is_offline: bool,
        should_check: bool,
    ) -> Result<(), String> {
        let (_, package) = self.package(name)?;
        let is_published = match is_offline {
            true => false,
            false => signed.published(name, &package.build)?.is_some(),
        };

        let mut planner = Planner {
            graph: &self.graph,
            store,
            is_root,
            cache,
            signed,
            is_offline,
            order: Vec::new(),
            seen: BTreeSet::new(),
        };
        planner.plan(&package.build)?;
        if let (true, false, Some(check)) = (should_check, is_published, &package.check) {
            planner.plan(check)?;
        }

        let Planner { order, seen, .. } = planner;
        let mut references = BTreeMap::new();
        for key in &seen {
            present_references(store, key, &mut references)?;
        }

        for key in &order {
            if references.contains_key(key) {
                continue;
            }

            let found = realize(
                store,
                is_root,
                &self.graph.derivations,
                &references,
                key,
                false,
            )?;
            references.insert(key.clone(), found);
        }

        Ok(())
    }
}

struct Planner<'a, 'b> {
    graph: &'a Graph,
    store: &'a mut Store,
    is_root: bool,
    cache: &'a Cache,
    signed: &'a mut Signed<'b>,
    is_offline: bool,
    order: Vec<Key>,
    seen: BTreeSet<Key>,
}

impl Planner<'_, '_> {
    fn plan(&mut self, key: &Key) -> Result<(), String> {
        if !self.seen.insert(key.clone()) {
            return Ok(());
        }

        if self
            .store
            .path(key)
            .map_err(|error| error.to_string())?
            .is_some()
        {
            return Ok(());
        }

        let graph = self.graph;
        let derivation = graph.derivations.get(key);
        if self.is_offline {
            return Err(format!(
                "{key} {} isn't in the store. Run without --offline to fetch it",
                label(derivation)
            ));
        }

        let needs: Vec<&Key> = match derivation {
            Some(Derivation::Build(build)) => {
                if self.signed.published(&build.name, key)?.is_some() {
                    return cache::install_into(
                        self.cache,
                        self.store,
                        self.is_root,
                        &build.name,
                        key,
                        Some(self.signed),
                    );
                }

                build
                    .inputs
                    .values()
                    .chain(build.dependencies.iter().map(|dependency| &dependency.key))
                    .collect()
            }
            Some(Derivation::Unpack(unpack)) => vec![&unpack.source],
            Some(Derivation::Check(check)) => std::iter::once(&check.target)
                .chain(check.dependencies.iter().map(|dependency| &dependency.key))
                .collect(),
            Some(Derivation::Fetch(_)) => Vec::new(),
            None => return Err(format!("{key} isn't in the evaluated catalog")),
        };

        for need in needs {
            self.plan(need)?;
        }

        self.order.push(key.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn checkout() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("packages")).unwrap();
        let digest = "0".repeat(64);
        let recipe = format!(
            r#"return {{
                name = "mac", description = "", homepage = "", default_license = "MIT",
                source = {{ url = "https://example.com/mac.tar.gz" }},
                build = {{ backend = "custom", steps = {{}} }},
                outputs = {{ bins = {{ "mac" }} }},
                platforms = {{
                    ["aarch64-macos"] = {{ default_version = "1" }},
                    ["aarch64-linux"] = {{ default_version = "1" }},
                    ["x86_64-linux"] = {{ default_version = "1" }},
                }},
                versions = {{ ["1"] = {{ digests = {{
                    ["aarch64-macos"] = "{digest}",
                    ["aarch64-linux"] = "{digest}",
                    ["x86_64-linux"] = "{digest}",
                }} }} }},
            }}"#
        );
        fs::write(root.path().join("packages/mac.lua"), recipe).unwrap();
        let host = r#"return {
            ["aarch64-macos"] = { system = "test" },
            ["aarch64-linux"] = { system = "test" },
            ["x86_64-linux"] = { system = "test" },
        }"#;
        fs::write(root.path().join("host.lua"), host).unwrap();
        root
    }

    #[test]
    fn a_checkout_takes_only_the_index_names_it_defines() {
        let root = checkout();
        let platform = Platform::X86_64Linux;
        let release = Wanted::parse("github:owner/mac").unwrap().1;
        let mut requests = BTreeMap::from([
            ("mac".to_string(), Wanted::Index(None)),
            ("jq".to_string(), Wanted::Index(None)),
        ]);

        let evaluated = shadow(&mut requests, root.path(), platform)
            .unwrap()
            .unwrap();
        let (target, package) = evaluated.package("mac").unwrap();
        assert_eq!(target.version, "1");
        assert_eq!(
            requests["mac"],
            Wanted::Local {
                version: None,
                key: package.build.clone(),
                bins: ["mac".to_string()].into(),
                apps: BTreeMap::new(),
            }
        );
        assert_eq!(requests["jq"], Wanted::Index(None));

        let mut releases = BTreeMap::from([("mac".to_string(), release.clone())]);
        assert!(shadow(&mut releases, root.path(), platform)
            .unwrap()
            .is_none());
        assert_eq!(releases["mac"], release);

        let mut missing = BTreeMap::from([("mac".to_string(), Wanted::Index(Some("2".into())))]);
        assert!(shadow(&mut missing, root.path(), platform).is_err());
    }
}
