use super::{Package, Phases, SOURCE, Unpacker, extract, link, rb_unpack, seven_zip, unpacker};
use crate::recipe::{ArchiveFormat, Bins, Install, Prebuilt};
use crate::template::{Values, escape, path, quote};
use crate::{Download, ReleaseAsset};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use rootbeer_drv::{Build, Dependency, Fetch, Format, Key, Platform};
use std::collections::BTreeMap;

// RFC 3986 unreserved characters stay original in URL encoding
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

// This is quite possibly the worst hack I've ever written. It finds a single
// executable by name in an unpacked prebuild archive and links it to `$out/bin`
const DISCOVER: &str = r#"  [ -e "${out}/bin/$name" ] && continue
  set -- $(cd "${out}" && find . -type f -name "$name" -perm -u+x)
  [ $# -eq 1 ] || { echo "expected one executable named $name, found $#" >&2; exit 1; }
  ln -s "../${1#./}" "${out}/bin/$name"
done"#;

/// Where a prebuilt downloads from, as `values` render its templates.
pub(crate) fn locate(prebuilt: &Prebuilt, values: &Values) -> Result<Download, String> {
    let Some(repository) = &prebuilt.github else {
        let Some(url) = &prebuilt.url else {
            return Err("a prebuilt downloads from exactly one of github or url".into());
        };

        let url = values.literal(url)?;
        let install = installed(prebuilt, &url, None);
        return Ok(Download {
            url,
            release: None,
            extract: install.as_ref().map(format),
        });
    };

    if prebuilt.url.is_some() {
        return Err("a prebuilt downloads from exactly one of github or url".into());
    }

    let tag = match &prebuilt.tag {
        Some(tag) => values.literal(tag)?,
        None => values.tag.clone(),
    };

    let asset = prebuilt
        .asset
        .as_deref()
        .ok_or("a github prebuilt needs an asset")?;

    let asset = Values {
        tag: tag.clone(),
        ..values.clone()
    }
    .literal(asset)?;

    let url = format!(
        "https://github.com/{repository}/releases/download/{}/{}",
        utf8_percent_encode(&tag, SEGMENT),
        utf8_percent_encode(&asset, SEGMENT),
    );

    let install = installed(prebuilt, &url, Some(&asset));
    Ok(Download {
        url,
        release: Some(ReleaseAsset {
            repository: repository.clone(),
            tag,
            asset,
        }),
        extract: install.as_ref().map(format),
    })
}

/// How a prebuilt installs, inferred from the file it downloads unless the
/// recipe says.
fn installed(prebuilt: &Prebuilt, url: &str, asset: Option<&str>) -> Option<Install> {
    let file = asset.unwrap_or_else(|| url.rsplit('/').next().unwrap_or_default());
    prebuilt.install.clone().or_else(|| infer_install(file))
}

impl Package<'_> {
    pub(crate) fn download(&self, prebuilt: &Prebuilt) -> Result<(Fetch, Option<Install>), String> {
        let download = locate(prebuilt, &self.resolved.values)?;
        let asset = download
            .release
            .as_ref()
            .map(|release| release.asset.as_str());
        let install = installed(prebuilt, &download.url, asset);
        Ok((self.fetch(vec![download.url]), install))
    }

    pub(crate) fn unpack(
        &self,
        install: Option<Install>,
        prebuilt: &Prebuilt,
        source: Key,
        dependencies: Vec<Dependency>,
    ) -> Result<Build, String> {
        let values = &self.resolved.values;
        let outputs = &self.resolved.spec.outputs;
        let steps = &prebuilt.steps;
        let is_discovered = install.is_some();
        let has_apps = outputs.apps.as_ref().is_some_and(|apps| !apps.is_empty());

        let field = "prebuilt.dependencies";
        let unpacker = match &install {
            Some(Install::Dmg) => unpacker(&dependencies, "DMG prebuilt", field)?,
            Some(Install::Pkg) => match unpacker(&dependencies, "pkg prebuilt", field)? {
                Unpacker::SevenZip => {
                    return Err(format!(
                        "a pkg prebuilt extracts with rb-unpack, so declare it in {field}"
                    ));
                }
                unpacker => unpacker,
            },
            Some(Install::Archive {
                format: ArchiveFormat::Zip,
                ..
            }) => unpacker(&dependencies, "zip prebuilt", field)?,
            Some(Install::Archive {
                format: ArchiveFormat::TarBz2,
                ..
            }) => unpacker(&dependencies, "tar.bz2 prebuilt", field)?,
            _ => Unpacker::Tar,
        };

        let mut phases = Phases::default();
        match install {
            Some(Install::Archive {
                format,
                strip_prefix: None,
            }) if !steps.has_steps_before_install() => {
                phases.install.push(r#"mkdir -p "${out}""#.into());
                phases
                    .install
                    .push(unarchive(format, r#""${out}""#, has_apps, unpacker));
            }
            // Steps before install run in the tree that becomes $out.
            Some(Install::Archive {
                format,
                strip_prefix,
            }) => {
                let unpacked = match strip_prefix {
                    Some(prefix) => format!("unpack/{}", values.literal(&prefix)?),
                    None => "unpack".into(),
                };

                let unpacked = path("TMPDIR", &unpacked);
                phases.prepare.push(r#"mkdir "${TMPDIR}/unpack""#.into());
                phases.prepare.push(unarchive(
                    format,
                    r#""${TMPDIR}/unpack""#,
                    has_apps,
                    unpacker,
                ));
                if steps.has_steps_before_install() {
                    phases.prepare.push(format!("cd {unpacked}"));
                    phases.install.push(
                        r#"[ ! -e "${out}" ] || { echo "a step before install created ${out}, where the unpacked tree moves" >&2; exit 1; }"#
                            .into(),
                    );
                }

                phases.install.push(format!(r#"mv {unpacked} "${{out}}""#));
            }
            Some(Install::Dmg) => {
                let apps = outputs.apps.as_ref().filter(|apps| !apps.is_empty());
                let apps = apps.ok_or("a DMG needs outputs.apps")?;

                phases.prepare.push(match unpacker {
                    Unpacker::SevenZip => seven_zip(SOURCE, r#""${TMPDIR}/dmg""#),
                    _ => rb_unpack("dmg", r#""${TMPDIR}/dmg""#),
                });
                phases.install.push(r#"mkdir -p "${out}""#.into());
                for app in apps.values() {
                    phases
                        .install
                        .extend(copy_app(app.path(), "${TMPDIR}/dmg", "DMG"));
                }
            }
            Some(Install::Pkg) => {
                let apps = outputs.apps.as_ref().filter(|apps| !apps.is_empty());
                let apps = apps.ok_or("a pkg needs outputs.apps")?;

                phases.prepare.push(rb_unpack("pkg", r#""${TMPDIR}/pkg""#));
                phases.install.push(r#"mkdir -p "${out}""#.into());
                for app in apps.values() {
                    phases
                        .install
                        .extend(copy_app(app.path(), "${TMPDIR}/pkg", "package"));
                }
            }
            None => {
                let names = outputs.bins.as_ref().map(Bins::names).unwrap_or_default();
                let [name] = names.as_slice() else {
                    return Err("a raw binary provides exactly one command in outputs.bins".into());
                };

                let installed = path("out", &format!("bin/{name}"));
                phases.install.push(r#"mkdir -p "${out}/bin""#.into());
                phases
                    .install
                    .push(format!(r#"cp "${{source}}" {installed}"#));
                phases.install.push(format!("chmod 755 {installed}"));
            }
        }

        // Install steps run in $out, since the extracted tree has moved there.
        if !steps.install.is_empty() {
            phases.install.push(r#"cd "${out}""#.into());
        }

        let mut lines = phases.run(values, steps)?;
        lines.extend(link(outputs.bins.as_ref()));
        if is_discovered {
            lines.extend(discover(outputs.bins.as_ref()));
        }

        let unpacked = self.build(
            self.name.to_string(),
            BTreeMap::from([("source".into(), source)]),
            dependencies,
            BTreeMap::new(),
            lines,
        );

        Ok(Build {
            host: self.host_references()?,
            ..unpacked
        })
    }

    /// A build that copies apps and bins out of `unpacked`, an extract whose
    /// tree the recipe records, so it runs no unpacker of its own.
    pub(crate) fn copy(
        &self,
        install: &Install,
        prebuilt: &Prebuilt,
        unpacked: Key,
        dependencies: Vec<Dependency>,
    ) -> Result<Build, String> {
        let values = &self.resolved.values;
        let outputs = &self.resolved.spec.outputs;
        let steps = &prebuilt.steps;
        let platform = values.platform;

        let mut phases = Phases::default();
        match install {
            Install::Archive { strip_prefix, .. } => {
                let tree = match strip_prefix {
                    Some(prefix) => path("unpacked", &values.literal(prefix)?),
                    None => r#""${unpacked}""#.into(),
                };

                // Steps before install run in a writable copy that becomes $out.
                if steps.has_steps_before_install() {
                    phases
                        .prepare
                        .extend(copy_tree(platform, &tree, r#""${TMPDIR}/unpack""#));
                    phases.prepare.push(r#"cd "${TMPDIR}/unpack""#.into());
                    phases.install.push(
                        r#"[ ! -e "${out}" ] || { echo "a step before install created ${out}, where the unpacked tree moves" >&2; exit 1; }"#
                            .into(),
                    );
                    phases
                        .install
                        .push(r#"mv "${TMPDIR}/unpack" "${out}""#.into());
                } else {
                    phases
                        .install
                        .extend(copy_tree(platform, &tree, r#""${out}""#));
                }
            }
            Install::Dmg | Install::Pkg => {
                let archive = match install {
                    Install::Dmg => "DMG",
                    _ => "package",
                };

                let apps = outputs.apps.as_ref().filter(|apps| !apps.is_empty());
                let apps = apps.ok_or_else(|| format!("a {archive} needs outputs.apps"))?;
                phases.install.push(r#"mkdir -p "${out}""#.into());
                for app in apps.values() {
                    phases
                        .install
                        .extend(copy_app(app.path(), "${unpacked}", archive));
                }

                phases.install.push(r#"chmod -R u+w "${out}""#.into());
            }
        }

        // Install steps run in $out, since the copied tree is there.
        if !steps.install.is_empty() {
            phases.install.push(r#"cd "${out}""#.into());
        }

        let mut lines = phases.run(values, steps)?;
        lines.extend(link(outputs.bins.as_ref()));
        lines.extend(discover(outputs.bins.as_ref()));

        let copied = self.build(
            self.name.to_string(),
            BTreeMap::from([("unpacked".into(), unpacked)]),
            dependencies,
            BTreeMap::new(),
            lines,
        );

        Ok(Build {
            host: self.host_references()?,
            ..copied
        })
    }
}

/// How an extract reads the archive a prebuilt installs from.
pub(crate) fn format(install: &Install) -> Format {
    match install {
        Install::Dmg => Format::Dmg,
        Install::Pkg => Format::Pkg,
        Install::Archive { format, .. } => format.format(),
    }
}

/// Copies an extracted tree to a writable `directory` that doesn't exist yet.
/// macOS keeps the code signature attributes the tree holds, and both clone
/// where the filesystem can.
fn copy_tree(platform: Platform, tree: &str, directory: &str) -> Vec<String> {
    let copy = match platform {
        Platform::Aarch64Macos => format!("ditto {tree} {directory}"),
        Platform::Aarch64Linux | Platform::X86_64Linux => {
            format!("cp -R --reflink=auto {tree} {directory}")
        }
    };

    vec![copy, format!("chmod -R u+w {directory}")]
}

/// A zip made on macOS can store each file's metadata in an AppleDouble file
/// beside it. ditto folds those back into attributes, but 7-Zip writes them
/// out, and inside an app bundle they break the code seal. rb-unpack folds
/// them back into code signature attributes, as ditto does.
fn unarchive(format: ArchiveFormat, directory: &str, has_apps: bool, unpacker: Unpacker) -> String {
    match (format, unpacker) {
        (ArchiveFormat::Zip, Unpacker::SevenZip) if has_apps => {
            format!("{} '-xr!._*' '-xr!__MACOSX'", seven_zip(SOURCE, directory))
        }
        _ => extract(format, directory, unpacker),
    }
}

fn infer_install(file: &str) -> Option<Install> {
    let file = file.to_ascii_lowercase();
    if file.ends_with(".dmg") {
        return Some(Install::Dmg);
    }

    if file.ends_with(".pkg") {
        return Some(Install::Pkg);
    }

    let formats = [
        (".tar.gz", ArchiveFormat::TarGz),
        (".tgz", ArchiveFormat::TarGz),
        (".tar.xz", ArchiveFormat::TarXz),
        (".txz", ArchiveFormat::TarXz),
        (".tar.bz2", ArchiveFormat::TarBz2),
        (".tbz2", ArchiveFormat::TarBz2),
        (".zip", ArchiveFormat::Zip),
    ];

    let (_, format) = formats
        .into_iter()
        .find(|(extension, _)| file.ends_with(extension))?;

    Some(Install::Archive {
        format,
        strip_prefix: None,
    })
}

// An HFS+ image unpacks into a volume directory, whose name can carry a
// version, and a package's apps land in the directory it installs into.
// `directory` is expanded inside double quotes.
fn copy_app(app: &str, directory: &str, archive: &str) -> Vec<String> {
    let name = escape(app);
    vec![
        "set --".into(),
        format!(r#"for app in "{directory}/{name}" "{directory}"/*/"{name}"; do"#),
        r#"  if [ -e "$app" ] || [ -L "$app" ]; then set -- "$@" "$app"; fi"#.into(),
        "done".into(),
        format!(
            r#"[ $# -eq 1 ] || {{ echo "expected one {name} in the {archive}, found $#" >&2; exit 1; }}"#
        ),
        format!(
            r#"[ ! -L "$1" ] || {{ echo "{name} in the {archive} is a symlink, not an app bundle" >&2; exit 1; }}"#
        ),
        format!(r#"ditto "$1" {}"#, path("out", app)),
    ]
}

/// Finds each command declared by name alone in an unpacked tree.
fn discover(bins: Option<&Bins>) -> Vec<String> {
    let Some(Bins::Names(names)) = bins else {
        return Vec::new();
    };

    if names.is_empty() {
        return Vec::new();
    }

    let names = names.iter().map(|name| quote(name)).collect::<Vec<_>>();
    vec![
        r#"mkdir -p "${out}/bin""#.into(),
        format!("for name in {}; do", names.join(" ")),
        DISCOVER.into(),
    ]
}
