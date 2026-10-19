use super::{Package, Phases, extract, link, seven_zip, unpacker};
use crate::recipe::{ArchiveFormat, Bins, Install, Prebuilt};
use crate::template::{Values, escape, path, quote};
use crate::{Download, ReleaseAsset};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use rootbeer_drv::{Build, Dependency, Fetch, Key};
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

        return Ok(Download {
            url: values.literal(url)?,
            release: None,
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

    Ok(Download {
        url,
        release: Some(ReleaseAsset {
            repository: repository.clone(),
            tag,
            asset,
        }),
    })
}

impl Package<'_> {
    pub(crate) fn download(&self, prebuilt: &Prebuilt) -> Result<(Fetch, Option<Install>), String> {
        let download = locate(prebuilt, &self.resolved.values)?;
        let file = match &download.release {
            Some(release) => release.asset.clone(),
            None => download
                .url
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .to_string(),
        };

        let install = prebuilt.install.clone().or_else(|| infer_install(&file));
        Ok((self.fetch(download.url), install))
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

        match install {
            Some(Install::Dmg) => unpacker(&dependencies, "DMG prebuilt", "prebuilt.dependencies")?,
            Some(Install::Archive {
                format: ArchiveFormat::Zip,
                ..
            }) => unpacker(&dependencies, "zip prebuilt", "prebuilt.dependencies")?,
            _ => {}
        }

        let mut phases = Phases::default();
        match install {
            Some(Install::Archive {
                format,
                strip_prefix: None,
            }) if !steps.has_steps_before_install() => {
                phases.install.push(r#"mkdir -p "${out}""#.into());
                phases.install.push(extract(format, r#""${out}""#));
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
                phases
                    .prepare
                    .push(extract(format, r#""${TMPDIR}/unpack""#));
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

                phases.prepare.push(seven_zip(r#""${TMPDIR}/dmg""#));
                phases.install.push(r#"mkdir -p "${out}""#.into());
                for app in apps.values() {
                    phases.install.extend(copy_app(app));
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
            host: self.host_references(),
            ..unpacked
        })
    }
}

fn infer_install(file: &str) -> Option<Install> {
    let file = file.to_ascii_lowercase();
    if file.ends_with(".dmg") {
        return Some(Install::Dmg);
    }

    let formats = [
        (".tar.gz", ArchiveFormat::TarGz),
        (".tgz", ArchiveFormat::TarGz),
        (".tar.xz", ArchiveFormat::TarXz),
        (".txz", ArchiveFormat::TarXz),
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

// An HFS+ image extracts into a volume directory, whose name can carry a
// version, while an APFS image extracts without one.
fn copy_app(app: &str) -> Vec<String> {
    let name = escape(app);
    vec![
        "set --".into(),
        format!(r#"for app in "${{TMPDIR}}/dmg/{name}" "${{TMPDIR}}/dmg"/*/"{name}"; do"#),
        r#"  if [ -e "$app" ] || [ -L "$app" ]; then set -- "$@" "$app"; fi"#.into(),
        "done".into(),
        format!(
            r#"[ $# -eq 1 ] || {{ echo "expected one {name} in the DMG, found $#" >&2; exit 1; }}"#
        ),
        format!(
            r#"[ ! -L "$1" ] || {{ echo "{name} in the DMG is a symlink, not an app bundle" >&2; exit 1; }}"#
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
