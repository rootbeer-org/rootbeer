use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::download::{read_url, DownloadCache};
use super::realize::{extract_archive, validate_relative_path};
use super::{
    ArchiveFormat, GitReleaseProof, LockedInstall, LockedPackage, LockedSource,
    MetadataDocumentProof, PackageRequest, PackageResolution, PackageResolver, Provides,
    ResolutionProof, ResolveContext,
};
use crate::store::hash_bytes;

#[derive(Debug, Clone)]
pub struct GitHubResolver {
    api_url: String,
    downloads: DownloadCache,
}

impl GitHubResolver {
    pub fn new() -> Self {
        Self {
            api_url: "https://api.github.com".to_string(),
            downloads: DownloadCache::default(),
        }
    }

    fn resolve_inner(
        &self,
        request: &PackageRequest,
        context: &ResolveContext,
    ) -> Result<PackageResolution, String> {
        let (owner, repo) = repository(&request.name)?;
        let endpoint = match request.version.as_deref() {
            None | Some("latest") => "latest".to_string(),
            Some(tag) => format!("tags/{}", encode_segment(tag)),
        };
        let url = format!("{}/repos/{owner}/{repo}/releases/{endpoint}", self.api_url);
        let bytes = read_url(&url).map_err(|e| e.to_string())?;
        let release: Release = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        if release.draft || release.tag_name.is_empty() {
            return Err("GitHub release must have a tag and be published".to_string());
        }
        let asset = select_asset(&release.assets, request.asset.as_deref(), context)?;
        let expected = match asset.digest.as_deref() {
            Some(digest) if digest.starts_with("sha256:") => Some(&digest[7..]),
            _ => None,
        };
        let source = self
            .downloads
            .materialize(&asset.browser_download_url, expected)
            .map_err(|e| e.to_string())?;
        let (install, bins) = match archive_format(&asset.name) {
            Some(format) => {
                let bins = if request.bins.is_empty() {
                    let extracted = tempfile::tempdir().map_err(|e| e.to_string())?;
                    extract_archive(&source.path, format, extracted.path())
                        .map_err(|e| e.to_string())?;
                    discover_bins(extracted.path())?
                } else {
                    request.bins.clone()
                };
                (
                    LockedInstall::Archive {
                        format,
                        strip_prefix: None,
                    },
                    bins,
                )
            }
            None if asset.name.to_ascii_lowercase().ends_with(".dmg") => {
                (LockedInstall::Dmg, request.bins.clone())
            }
            None => {
                let bins = if request.bins.is_empty() {
                    BTreeMap::from([(repo.to_string(), PathBuf::from(repo))])
                } else {
                    request.bins.clone()
                };
                if bins.len() != 1 {
                    return Err("raw GitHub assets must provide exactly one binary".to_string());
                }
                let path = bins.values().next().unwrap().clone();
                validate_relative_path("binary path", &path).map_err(|e| e.to_string())?;
                (LockedInstall::Binary { path }, bins)
            }
        };
        let package = LockedPackage {
            name: request.name.clone(),
            version: release.tag_name.clone(),
            source: LockedSource::Url {
                url: asset.browser_download_url.clone(),
                sha256: source.sha256,
            },
            install,
            provides: Provides {
                apps: Default::default(),
                bins,
            },
            runtime_dependencies: Default::default(),
            output_sha256: None,
        };
        let proof = ResolutionProof::GitRelease(GitReleaseProof {
            host: "github.com".to_string(),
            owner: owner.to_string(),
            repo: repo.to_string(),
            tag: Some(release.tag_name),
            target_commit: None,
            release_id: Some(release.id),
            documents: vec![MetadataDocumentProof {
                url,
                sha256: hash_bytes(&bytes),
            }],
        });
        Ok(PackageResolution::new(package, proof))
    }
}

impl Default for GitHubResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl PackageResolver for GitHubResolver {
    fn name(&self) -> &str {
        "github"
    }

    fn resolve(
        &self,
        request: &PackageRequest,
        context: &ResolveContext,
    ) -> Result<Option<PackageResolution>, String> {
        self.resolve_inner(request, context).map(Some)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub id: u64,
    pub tag_name: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub name: String,
    browser_download_url: String,
    #[serde(default)]
    digest: Option<String>,
}

impl Asset {
    /// The digest GitHub publishes for this asset, without its algorithm prefix.
    pub fn sha256(&self) -> Option<&str> {
        self.digest.as_deref()?.strip_prefix("sha256:")
    }
}

pub fn repository(name: &str) -> Result<(&str, &str), String> {
    let Some((owner, repo)) = name.split_once('/') else {
        return Err("GitHub packages require `github:owner/repo@tag`".to_string());
    };
    if [owner, repo].iter().any(|part| {
        part.is_empty()
            || *part == "."
            || *part == ".."
            || !part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    }) {
        return Err(format!("invalid GitHub repository `{name}`"));
    }
    Ok((owner, repo))
}

fn encode_segment(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

fn archive_format(name: &str) -> Option<ArchiveFormat> {
    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        return Some(ArchiveFormat::TarGz);
    }
    if name.ends_with(".tar.xz") || name.ends_with(".txz") {
        return Some(ArchiveFormat::TarXz);
    }
    name.ends_with(".zip").then_some(ArchiveFormat::Zip)
}

fn is_installable(name: &str) -> bool {
    if name.ends_with(".dmg") {
        return true;
    }
    if archive_format(name).is_some() {
        return !name.contains("source") && !name.contains("debug") && !name.contains("symbols");
    }
    let extension = name.rsplit('.').next().unwrap_or(name);
    ![
        "gz", "xz", "bz2", "zst", "7z", "tar", "deb", "rpm", "dmg", "pkg", "exe", "msi", "sha256",
        "sha512", "sig", "asc", "minisig", "sigstore", "txt", "json", "yml", "yaml", "pem", "spdx",
        "sbom",
    ]
    .contains(&extension)
        && !name.contains("checksum")
        && !name.contains("sha256sum")
        && !name.contains("sha512sum")
        && !name.contains("attestation")
}

pub fn select_asset<'a>(
    assets: &'a [Asset],
    selected: Option<&str>,
    context: &ResolveContext,
) -> Result<&'a Asset, String> {
    if let Some(name) = selected {
        let asset = assets
            .iter()
            .find(|asset| asset.name == name)
            .ok_or_else(|| format!("GitHub release has no asset named `{name}`"))?;
        if !is_installable(&asset.name.to_ascii_lowercase()) {
            return Err(format!("unsupported GitHub asset format `{name}`"));
        }
        if name.to_ascii_lowercase().ends_with(".dmg") && !context.system.ends_with("-macos") {
            return Err("DMG assets require macOS".into());
        }
        return Ok(asset);
    }
    let (arch, os) = context
        .system
        .split_once('-')
        .ok_or_else(|| format!("invalid target system `{}`", context.system))?;
    let os_names: &[&str] = match os {
        "macos" => &["darwin", "macos", "osx"],
        "linux" => &["linux"],
        _ => return Err(format!("unsupported GitHub target OS `{os}`")),
    };
    let arch_names: &[&str] = match arch {
        "aarch64" => &["aarch64", "arm64"],
        "x86_64" => &["x86_64", "amd64", "x64"],
        _ => return Err(format!("unsupported GitHub target architecture `{arch}`")),
    };
    let mut candidates: Vec<&Asset> = assets
        .iter()
        .filter(|asset| {
            let name = asset.name.to_ascii_lowercase();
            is_installable(&name)
                && (!name.ends_with(".dmg") || os == "macos")
                && os_names.iter().any(|os| has_token(&name, os))
                && (arch_names.iter().any(|arch| has_token(&name, arch))
                    || (os == "macos" && has_token(&name, "universal")))
        })
        .collect();
    candidates.sort_by(|a, b| a.name.cmp(&b.name));
    match candidates.as_slice() {
        [asset] => Ok(*asset),
        [] => Err(format!(
            "no GitHub release asset matches {}; specify `asset` explicitly",
            context.system
        )),
        _ => Err(format!(
            "ambiguous GitHub release assets for {}: {}; specify `asset` explicitly",
            context.system,
            candidates
                .iter()
                .map(|asset| asset.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn has_token(name: &str, token: &str) -> bool {
    name.match_indices(token).any(|(start, _)| {
        let end = start + token.len();
        (start == 0 || !name.as_bytes()[start - 1].is_ascii_alphanumeric())
            && (end == name.len() || !name.as_bytes()[end].is_ascii_alphanumeric())
    })
}

pub(crate) fn discover_bins(root: &Path) -> Result<BTreeMap<String, PathBuf>, String> {
    let mut bins = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let mut entries = fs::read_dir(dir)
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let kind = entry.file_type().map_err(|e| e.to_string())?;
            if kind.is_dir() {
                pending.push(entry.path());
                continue;
            }
            if !kind.is_file()
                || entry
                    .metadata()
                    .map_err(|e| e.to_string())?
                    .permissions()
                    .mode()
                    & 0o111
                    == 0
            {
                continue;
            }
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| "non-UTF8 binary name")?;
            if bins.insert(name.clone(), relative).is_some() {
                return Err(format!(
                    "multiple binaries named `{name}`; specify `bins` explicitly"
                ));
            }
        }
    }
    if bins.is_empty() {
        return Err(
            "GitHub archive has no executable files; specify `bins` explicitly".to_string(),
        );
    }
    Ok(bins)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(name: &str) -> Asset {
        Asset {
            name: name.to_string(),
            browser_download_url: String::new(),
            digest: None,
        }
    }

    #[test]
    fn selects_native_release_and_ignores_checksums() {
        let assets = vec![
            asset("tool-linux-arm64.tar.gz"),
            asset("tool-darwin-amd64.tar.gz"),
            asset("tool-darwin-arm64.tar.gz"),
            asset("tool-darwin-arm64.tar.gz.sha256"),
        ];
        assert_eq!(
            select_asset(&assets, None, &ResolveContext::new("aarch64-macos"))
                .unwrap()
                .name,
            "tool-darwin-arm64.tar.gz"
        );
    }

    #[test]
    fn rejects_ambiguous_assets_and_allows_exact_override() {
        let assets = vec![
            asset("tool-linux-x86_64-gnu.tar.gz"),
            asset("tool-linux-x86_64-musl.tar.gz"),
        ];
        let context = ResolveContext::new("x86_64-linux");
        assert!(select_asset(&assets, None, &context)
            .unwrap_err()
            .contains("ambiguous"));
        assert!(select_asset(&assets, Some("tool-linux-x86_64-musl.tar.gz"), &context).is_ok());
    }

    #[test]
    fn rejects_wrong_architecture_and_unsupported_installers() {
        let assets = vec![
            asset("tool-darwin-arm64.pkg"),
            asset("tool-darwin-amd64.tar.gz"),
        ];
        let context = ResolveContext::new("aarch64-macos");
        assert!(select_asset(&assets, None, &context).is_err());
        assert!(select_asset(&assets, Some("tool-darwin-arm64.pkg"), &context).is_err());
        assert!(!has_token("tool-arm64e", "arm64"));
    }

    #[test]
    fn selects_dmg_only_for_macos_and_keeps_declared_command_paths() {
        let assets = vec![asset("tool-darwin-arm64.dmg")];
        let context = ResolveContext::new("aarch64-macos");
        assert_eq!(
            select_asset(&assets, None, &context).unwrap().name,
            assets[0].name
        );
        assert!(select_asset(
            &assets,
            Some(&assets[0].name),
            &ResolveContext::new("aarch64-linux")
        )
        .is_err());
        let root = tempfile::tempdir().unwrap();
        let resolver = release_fixture(root.path(), "owner/tool", &assets[0].name, b"disk image");
        let mut request = PackageRequest::new("owner/tool")
            .resolver("github")
            .version("v1");
        request.asset = Some(assets[0].name.clone());
        request.bins = BTreeMap::from([("tool".into(), "Tool.app/Contents/MacOS/tool".into())]);
        let resolution = resolver.resolve(&request, &context).unwrap().unwrap();
        assert_eq!(resolution.package.install, LockedInstall::Dmg);
        assert_eq!(resolution.package.provides.bins, request.bins);
    }

    #[test]
    fn resolves_raw_release_with_verified_digest() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("repos/owner/tool/releases/tags");
        fs::create_dir_all(&dir).unwrap();
        let binary = root.path().join("tool");
        fs::write(&binary, b"#!/bin/sh\n").unwrap();
        let digest = hash_bytes(b"#!/bin/sh\n");
        fs::write(
            dir.join("v1"),
            serde_json::to_vec(&serde_json::json!({
                "id": 42, "tag_name": "v1", "assets": [{"name": "tool-darwin-arm64",
                    "browser_download_url": format!("file://{}", binary.display()),
                    "digest": format!("sha256:{digest}")}]
            }))
            .unwrap(),
        )
        .unwrap();
        let resolver = GitHubResolver {
            api_url: format!("file://{}", root.path().display()),
            downloads: DownloadCache::new(root.path().join("downloads")),
        };
        let request = PackageRequest::parse("github:owner/tool@v1");
        let resolution = resolver
            .resolve(&request, &ResolveContext::new("aarch64-macos"))
            .unwrap()
            .unwrap();
        assert_eq!(
            resolution.package.provides.bins["tool"],
            PathBuf::from("tool")
        );
        assert!(matches!(
            resolution.proof,
            ResolutionProof::GitRelease(GitReleaseProof {
                release_id: Some(42),
                ..
            })
        ));
        fs::write(&binary, b"changed").unwrap();
        fs::remove_dir_all(root.path().join("downloads")).unwrap();
        assert!(resolver
            .resolve(&request, &ResolveContext::new("aarch64-macos"))
            .unwrap_err()
            .contains("hash mismatch"));
    }

    #[test]
    fn discovers_binary_names_and_rejects_collisions() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("release/bin")).unwrap();
        let path = root.path().join("release/bin/rg");
        fs::write(&path, b"binary").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(root.path().join("README"), b"docs").unwrap();
        assert_eq!(
            discover_bins(root.path()).unwrap()["rg"],
            PathBuf::from("release/bin/rg")
        );
        fs::copy(path, root.path().join("rg")).unwrap();
        assert!(discover_bins(root.path())
            .unwrap_err()
            .contains("multiple binaries"));
    }

    fn release_fixture(
        root: &Path,
        repository: &str,
        asset_name: &str,
        bytes: &[u8],
    ) -> GitHubResolver {
        let dir = root.join(format!("repos/{repository}/releases/tags"));
        fs::create_dir_all(&dir).unwrap();
        let binary = root.join("asset");
        fs::write(&binary, bytes).unwrap();
        fs::write(
            dir.join("v1"),
            serde_json::to_vec(&serde_json::json!({
                "id": 42, "tag_name": "v1", "assets": [{
                    "name": asset_name,
                    "browser_download_url": format!("file://{}", binary.display()),
                    "digest": format!("sha256:{}", hash_bytes(bytes))
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        GitHubResolver {
            api_url: format!("file://{}", root.display()),
            downloads: DownloadCache::new(root.join("downloads")),
        }
    }

    fn catalog_resolver(
        resolver: GitHubResolver,
        name: &str,
        repository: &str,
        asset_name: &str,
        commands: &[&str],
    ) -> super::super::catalog::CatalogResolver {
        catalog_resolver_with_contract(
            resolver,
            name,
            repository,
            asset_name,
            commands,
            BTreeMap::new(),
            BTreeMap::new(),
        )
    }

    fn catalog_resolver_with_contract(
        resolver: GitHubResolver,
        name: &str,
        repository: &str,
        asset_name: &str,
        commands: &[&str],
        bin_paths: BTreeMap<String, PathBuf>,
        checksums: BTreeMap<String, String>,
    ) -> super::super::catalog::CatalogResolver {
        let bins = if bin_paths.is_empty() {
            serde_json::json!(commands)
        } else {
            serde_json::json!(bin_paths)
        };
        let catalog: crate::PackageCatalog = serde_json::from_value(serde_json::json!({
            "packages": { name: {
                "name": name,
                "description": "Command contract test",
                "homepage": format!("https://github.com/{repository}"),
                "default_versions": { "aarch64-macos": "1" },
                "versions": { "1": {
                    "license": "MIT",
                    "revision": 1,
                    "platforms": { "aarch64-macos": {
                        "source": format!("github:{repository}@v1"),
                        "asset": asset_name,
                        "bins": bins,
                        "sha256": checksums.get("aarch64-macos"),
                        "checks": [[commands[0], "--version"]]
                    }}
                }}
            }}
        }))
        .unwrap();
        catalog.validate().unwrap();
        let mut backends = crate::ResolverStack::new();
        backends.push(resolver);
        super::super::catalog::CatalogResolver::new(
            &catalog,
            &crate::PackageResolverInputs::default(),
            backends,
        )
    }

    #[test]
    fn catalog_raw_assets_install_the_declared_command_instead_of_the_repository_name() {
        let contents = b"#!/bin/sh\nprintf 'version 1\\n'\n";
        for (repository, name, command) in [
            ("mvdan/sh", "shfmt", "shfmt"),
            ("tealdeer-rs/tealdeer", "tealdeer", "tldr"),
            ("docker/compose", "compose", "docker-compose"),
            ("argoproj/argo-cd", "argo-cd", "argocd"),
        ] {
            let root = tempfile::tempdir().unwrap();
            let asset_name = format!("{command}-darwin-arm64");
            let resolver = release_fixture(root.path(), repository, &asset_name, contents);
            let resolver = catalog_resolver(resolver, name, repository, &asset_name, &[command]);
            let resolution = resolver
                .resolve(
                    &PackageRequest::new(name),
                    &ResolveContext::new("aarch64-macos"),
                )
                .unwrap()
                .unwrap();
            assert_eq!(
                resolution.package.provides.bins,
                BTreeMap::from([(command.into(), PathBuf::from(command))])
            );
            assert_eq!(
                resolution.package.install,
                LockedInstall::Binary {
                    path: command.into()
                }
            );
            assert!(matches!(resolution.proof, ResolutionProof::Catalog(_)));
            let realized = crate::PackageRealizer::with_dirs(
                crate::store::Store::new(root.path().join("store")),
                root.path().join("downloads"),
                root.path().join("temp"),
            )
            .realize(&resolution.package)
            .unwrap();
            assert_eq!(fs::read(&realized.bins[command]).unwrap(), contents);
            assert_ne!(
                fs::metadata(&realized.bins[command])
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o111,
                0
            );
        }
    }

    #[test]
    fn catalog_raw_assets_reject_multiple_declared_commands() {
        let root = tempfile::tempdir().unwrap();
        let resolver = release_fixture(root.path(), "owner/tool", "tool-darwin-arm64", b"binary");
        let resolver = catalog_resolver(
            resolver,
            "tool",
            "owner/tool",
            "tool-darwin-arm64",
            &["one", "two"],
        );
        let error = resolver
            .resolve(
                &PackageRequest::new("tool"),
                &ResolveContext::new("aarch64-macos"),
            )
            .unwrap_err();
        assert!(error.contains("exactly one command"));
    }

    #[test]
    fn catalog_archives_preserve_discovered_paths_and_reject_missing_commands() {
        let root = tempfile::tempdir().unwrap();
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut archive = tar::Builder::new(encoder);
        let contents = b"#!/bin/sh\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        archive
            .append_data(&mut header, "release/bin/actual", &contents[..])
            .unwrap();
        let bytes = archive.into_inner().unwrap().finish().unwrap();
        let resolver = release_fixture(
            root.path(),
            "owner/tool",
            "tool-darwin-arm64.tar.gz",
            &bytes,
        );
        let context = ResolveContext::new("aarch64-macos");
        let valid = catalog_resolver(
            resolver.clone(),
            "tool",
            "owner/tool",
            "tool-darwin-arm64.tar.gz",
            &["actual"],
        );
        let resolution = valid
            .resolve(&PackageRequest::new("tool"), &context)
            .unwrap()
            .unwrap();
        assert_eq!(
            resolution.package.provides.bins["actual"],
            PathBuf::from("release/bin/actual")
        );
        assert!(matches!(
            resolution.package.install,
            LockedInstall::Archive { .. }
        ));

        let invalid = catalog_resolver(
            resolver,
            "tool",
            "owner/tool",
            "tool-darwin-arm64.tar.gz",
            &["missing"],
        );
        assert!(invalid
            .resolve(&PackageRequest::new("tool"), &context)
            .unwrap_err()
            .contains("backend did not provide declared command `missing`"));
    }

    #[test]
    fn explicit_raw_binary_overrides_keep_paths_and_reject_invalid_mappings() {
        let root = tempfile::tempdir().unwrap();
        let resolver = release_fixture(root.path(), "mvdan/sh", "shfmt-darwin-arm64", b"binary");
        let mut request = PackageRequest::parse("github:mvdan/sh@v1");
        request.bins.insert("shfmt".into(), "bin/shfmt".into());
        let context = ResolveContext::new("aarch64-macos");
        let resolution = resolver.resolve(&request, &context).unwrap().unwrap();
        assert_eq!(resolution.package.provides.bins, request.bins);
        assert_eq!(
            resolution.package.install,
            LockedInstall::Binary {
                path: "bin/shfmt".into()
            }
        );

        request.bins.insert("second".into(), "second".into());
        assert!(resolver
            .resolve(&request, &context)
            .unwrap_err()
            .contains("exactly one binary"));
        request.bins.remove("second");
        request.bins.insert("shfmt".into(), "../escape".into());
        assert!(resolver.resolve(&request, &context).is_err());
    }

    #[test]
    fn catalog_maps_nested_archive_commands_and_rejects_changed_pinned_assets() {
        let root = tempfile::tempdir().unwrap();
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut archive = tar::Builder::new(encoder);
        let contents = b"#!/bin/sh\nprintf tool\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        archive
            .append_data(&mut header, "Tool.app/Contents/MacOS/client", &contents[..])
            .unwrap();
        let bytes = archive.into_inner().unwrap().finish().unwrap();
        let resolver = release_fixture(
            root.path(),
            "owner/tool",
            "tool-darwin-arm64.tar.gz",
            &bytes,
        );
        let paths = BTreeMap::from([(
            "tool".into(),
            PathBuf::from("Tool.app/Contents/MacOS/client"),
        )]);
        let checksums = BTreeMap::from([("aarch64-macos".into(), hash_bytes(&bytes))]);
        let catalog = catalog_resolver_with_contract(
            resolver.clone(),
            "tool",
            "owner/tool",
            "tool-darwin-arm64.tar.gz",
            &["tool"],
            paths.clone(),
            checksums,
        );
        let context = ResolveContext::new("aarch64-macos");
        let resolution = catalog
            .resolve(&PackageRequest::new("tool"), &context)
            .unwrap()
            .unwrap();
        assert_eq!(resolution.package.provides.bins, paths);
        let ResolutionProof::Catalog(proof) = resolution.proof else {
            panic!("expected catalog proof")
        };
        assert_eq!(proof.source.bins, paths);
        let realized = crate::PackageRealizer::with_dirs(
            crate::store::Store::new(root.path().join("store")),
            root.path().join("downloads"),
            root.path().join("temp"),
        )
        .realize(&resolution.package)
        .unwrap();
        assert_eq!(fs::read(&realized.bins["tool"]).unwrap(), contents);

        let invalid = catalog_resolver_with_contract(
            resolver,
            "tool",
            "owner/tool",
            "tool-darwin-arm64.tar.gz",
            &["tool"],
            paths,
            BTreeMap::from([("aarch64-macos".into(), "0".repeat(64))]),
        );
        assert!(invalid
            .resolve(&PackageRequest::new("tool"), &context)
            .unwrap_err()
            .contains("checksum differs"));
    }

    #[test]
    fn catalog_preserves_explicit_raw_install_paths() {
        let root = tempfile::tempdir().unwrap();
        let resolver = release_fixture(root.path(), "mvdan/sh", "shfmt-darwin-arm64", b"binary");
        let paths = BTreeMap::from([("shfmt".into(), PathBuf::from("bin/shfmt"))]);
        let catalog = catalog_resolver_with_contract(
            resolver,
            "shfmt",
            "mvdan/sh",
            "shfmt-darwin-arm64",
            &["shfmt"],
            paths.clone(),
            BTreeMap::new(),
        );
        let resolution = catalog
            .resolve(
                &PackageRequest::new("shfmt"),
                &ResolveContext::new("aarch64-macos"),
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            resolution.package.install,
            LockedInstall::Binary {
                path: "bin/shfmt".into()
            }
        );
        assert_eq!(resolution.package.provides.bins, paths);
    }
}
