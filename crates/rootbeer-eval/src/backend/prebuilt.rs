use super::{Package, extract, link};
use crate::recipe::{ArchiveFormat, Bins, Install, Prebuilt, Removal};
use crate::template::{Values, path, quote};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use rootbeer_drv::{Allow, Build, Fetch, Key};
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

impl Package<'_> {
    pub(crate) fn download(&self, prebuilt: &Prebuilt) -> Result<(Fetch, Option<Install>), String> {
        let values = &self.resolved.values;
        let (url, file) = match (&prebuilt.github, &prebuilt.url) {
            (Some(repository), None) => {
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

                (url, asset)
            }
            (None, Some(url)) => {
                let url = values.literal(url)?;
                let file = url.rsplit('/').next().unwrap_or_default().to_string();

                (url, file)
            }
            _ => return Err("a prebuilt downloads from exactly one of github or url".into()),
        };

        let install = prebuilt.install.clone().or_else(|| infer_install(&file));
        Ok((self.fetch(url), install))
    }

    pub(crate) fn unpack(
        &self,
        install: Option<Install>,
        prebuilt: &Prebuilt,
        source: Key,
    ) -> Result<Build, String> {
        let values = &self.resolved.values;
        let outputs = &self.resolved.spec.outputs;
        let is_discovered = install.is_some();
        let is_archive = matches!(install, Some(Install::Archive { .. }));
        let is_dmg = matches!(install, Some(Install::Dmg));
        let has_disk_images = prebuilt.allow.contains(&Allow::DiskImages);

        if prebuilt
            .allow
            .iter()
            .any(|allow| *allow != Allow::DiskImages)
        {
            return Err("prebuilt.allow accepts only `disk-images`".into());
        }

        if has_disk_images && !is_dmg {
            return Err("prebuilt.allow `disk-images` needs a DMG install".into());
        }

        if is_dmg && !has_disk_images {
            return Err(
                r#"a DMG install needs prebuilt.allow = { "disk-images" } to attach it"#.into(),
            );
        }

        if !prebuilt.remove.is_empty() && !is_archive {
            return Err("prebuilt.remove applies only to an archive install".into());
        }

        let mut lines = Vec::new();
        match install {
            Some(Install::Archive {
                format,
                strip_prefix: Some(prefix),
            }) => {
                let unpacked = path("TMPDIR", &format!("unpack/{}", values.literal(&prefix)?));
                lines.push(r#"mkdir "${TMPDIR}/unpack""#.into());
                lines.push(extract(format, r#""${TMPDIR}/unpack""#));
                lines.push(format!(r#"mv {unpacked} "${{out}}""#));
            }
            Some(Install::Archive {
                format,
                strip_prefix: None,
            }) => {
                lines.push(r#"mkdir -p "${out}""#.into());
                lines.push(extract(format, r#""${out}""#));
            }
            Some(Install::Dmg) => {
                let apps = outputs.apps.as_ref().filter(|apps| !apps.is_empty());
                let apps = apps.ok_or("a DMG needs outputs.apps")?;

                lines.push(r#"mkdir -p "${out}""#.into());
                lines.push(
                    r#"trap 'hdiutil detach -force "${TMPDIR}/dmg" >/dev/null 2>&1 || :' EXIT"#
                        .into(),
                );
                lines.push(r#"hdiutil attach -nobrowse -readonly -noautoopen -mountpoint "${TMPDIR}/dmg" "${source}""#.into());
                lines.extend(apps.values().map(|app| {
                    format!(
                        "ditto {} {}",
                        path("TMPDIR", &format!("dmg/{app}")),
                        path("out", app)
                    )
                }));

                lines.push(r#"hdiutil detach "${TMPDIR}/dmg""#.into());
                lines.push("trap - EXIT".into());
            }
            None => {
                let names = outputs.bins.as_ref().map(Bins::names).unwrap_or_default();
                let [name] = names.as_slice() else {
                    return Err("a raw binary provides exactly one command in outputs.bins".into());
                };

                let installed = path("out", &format!("bin/{name}"));
                lines.push(r#"mkdir -p "${out}/bin""#.into());
                lines.push(format!(r#"cp "${{source}}" {installed}"#));
                lines.push(format!("chmod 755 {installed}"));
            }
        }

        lines.extend(
            prebuilt
                .remove
                .iter()
                .map(|Removal(pattern)| format!(r#"rm -rf "${{out}}"/{pattern}"#)),
        );

        lines.extend(link(outputs.bins.as_ref()));
        if is_discovered {
            lines.extend(discover(outputs.bins.as_ref()));
        }

        let unpacked = self.build(
            self.name.to_string(),
            BTreeMap::from([("source".into(), source)]),
            Vec::new(),
            BTreeMap::new(),
            lines,
        );

        Ok(Build {
            allow: prebuilt.allow.clone(),
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
