use super::{github, load, parse_platform, Sources};
use rootbeer_drv::{is_version, Platform, Sha256};
use rootbeer_eval::Target;
use serde::Serialize;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::ops::Range;

#[derive(clap::Args, Debug)]
pub(super) struct Args {
    /// The package to add a version to
    name: String,
    /// The version to add, which becomes the default
    version: String,
    /// The commit the version builds from, for recipes that name `{commit}`
    #[arg(long)]
    commit: Option<String>,
    /// The version's serial, for recipes that name `{serial}`
    #[arg(long)]
    serial: Option<u64>,
    /// A platform to add the version for and make it the default on. Every
    /// platform the recipe declares when none is named
    #[arg(long = "platform", value_parser = parse_platform)]
    platforms: Vec<Platform>,
    /// How many versions to keep, newest first. A version a platform still
    /// defaults to or another recipe depends on always stays
    #[arg(long)]
    keep: Option<usize>,
    #[command(flatten)]
    sources: Sources,
}

#[derive(Serialize)]
struct Bumped<'a> {
    name: &'a str,
    version: &'a str,
    added: bool,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    digests: BTreeMap<Platform, Sha256>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    dropped: Vec<String>,
}

/// Adds a version to a recipe as the default on its platforms, pinning each
/// platform's download, then drops versions beyond `--keep`.
pub(super) fn bump(args: &Args) -> Result<(), String> {
    let (name, version) = (args.name.as_str(), args.version.as_str());
    if !is_version(version) {
        return Err(format!("{version} isn't a valid version"));
    }

    let commit = args.commit.as_deref();
    if commit.is_some_and(|commit| {
        commit.len() != 40 || !commit.bytes().all(|byte| byte.is_ascii_hexdigit())
    }) {
        return Err("--commit takes a full commit SHA".into());
    }

    let (catalog, _) = load(&args.sources)?;
    if !catalog.contains(name) {
        return Err(format!("{name} isn't in the catalog"));
    }

    let path = args.sources.catalog.join(format!("{name}.lua"));
    let original =
        fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let recipe = Recipe::read(&original).map_err(|error| format!("{}: {error}", path.display()))?;
    if recipe
        .versions
        .iter()
        .any(|(declared, _)| declared == version)
    {
        return print(&Bumped {
            name,
            version,
            added: false,
            digests: BTreeMap::new(),
            dropped: Vec::new(),
        });
    }

    let declared = catalog.platforms(name);
    let platforms = match args.platforms.is_empty() {
        true => declared.clone(),
        false => args.platforms.clone(),
    };

    if let Some(platform) = platforms
        .iter()
        .find(|platform| !declared.contains(platform))
    {
        return Err(format!("{name} doesn't declare {platform}"));
    }

    let mut digests = BTreeMap::new();
    let mut by_url = BTreeMap::<String, Sha256>::new();
    for &platform in &platforms {
        let at = |error: String| format!("{name}@{version} on {platform}: {error}");
        let download = catalog
            .download(name, version, commit, args.serial, platform)
            .map_err(at)?;

        let digest = match (by_url.get(&download.url), &download.release) {
            (Some(digest), _) => digest.clone(),
            (None, Some(release)) => {
                github::asset_digest(&release.repository, &release.tag, &release.asset)
                    .map_err(at)?
            }
            (None, None) => github::download_digest(&download.url).map_err(at)?,
        };

        by_url.insert(download.url, digest.clone());
        digests.insert(platform, digest);
    }

    let entry = Entry {
        version,
        digests: &digests,
        commit,
        serial: args.serial,
    };
    let depended = catalog.depended_on(name);
    let (text, dropped) = recipe.with(&entry, &platforms, args.keep, &depended)?;
    fs::write(&path, &text).map_err(|error| format!("{}: {error}", path.display()))?;

    // The edit must leave a recipe that still evaluates, or it's undone.
    if let Err(error) = evaluates(&args.sources, name, version, &platforms) {
        fs::write(&path, &original).map_err(|restore| {
            format!(
                "{name}@{version} doesn't evaluate ({error}) and restoring {} failed: \
                 {restore}",
                path.display()
            )
        })?;
        return Err(format!(
            "{name}@{version} doesn't evaluate, so {} is unchanged: {error}",
            path.display()
        ));
    }

    print(&Bumped {
        name,
        version,
        added: true,
        digests,
        dropped,
    })
}

/// Whether `platforms` now default to `version` and every platform's default
/// evaluates.
fn evaluates(
    sources: &Sources,
    name: &str,
    version: &str,
    platforms: &[Platform],
) -> Result<(), String> {
    let (catalog, hosts) = load(sources)?;
    if let Some(platform) = platforms
        .iter()
        .find(|&&platform| catalog.default_version(name, platform) != Some(version))
    {
        return Err(format!("{platform} doesn't default to it"));
    }

    let targets: Vec<Target> = catalog
        .platforms(name)
        .into_iter()
        .filter_map(|platform| {
            Some(Target {
                name: name.to_string(),
                version: catalog.default_version(name, platform)?.to_string(),
                platform,
            })
        })
        .collect();

    catalog
        .evaluate(&hosts, &targets)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn print(bumped: &Bumped) -> Result<(), String> {
    let json = serde_json::to_string(bumped).map_err(|error| error.to_string())?;
    println!("{json}");
    Ok(())
}

/// A version to add, as the recipe will record it.
struct Entry<'a> {
    version: &'a str,
    digests: &'a BTreeMap<Platform, Sha256>,
    commit: Option<&'a str>,
    serial: Option<u64>,
}

/// A recipe file in the layout pdr writes, with the spans `bump` edits. Text
/// edits keep every comment and line `bump` doesn't touch.
struct Recipe<'a> {
    text: &'a str,
    unit: &'a str,
    versions: Entries,
    platforms: Entries,
}

/// Each entry's key and span.
type Entries = Vec<(String, Range<usize>)>;

impl<'a> Recipe<'a> {
    fn read(text: &'a str) -> Result<Recipe<'a>, String> {
        if text.contains('\r') {
            return Err("rb can't edit CRLF line endings".into());
        }

        let unit = text
            .lines()
            .find(|line| line.trim_start() == "versions = {")
            .map(|line| &line[..line.len() - line.trim_start().len()])
            .filter(|unit| !unit.is_empty())
            .ok_or("it has no versions table")?;

        let versions_span = table(text, unit, "versions")?;
        let platforms_span = table(text, unit, "platforms")?;
        let depth = unit.repeat(2);
        let (versions, note) = entries(text, versions_span, &depth)?;

        // Versions are rewritten whole, so a note among them would be lost.
        if let Some(note) = note {
            return Err(format!("rb can't edit the line `{note}`"));
        }

        let (platforms, _) = entries(text, platforms_span, &depth)?;
        Ok(Recipe {
            text,
            unit,
            versions,
            platforms,
        })
    }

    /// The recipe with `entry` added and made the default on `platforms`, and
    /// the versions `keep` drops. Defaults and the versions in `depended` stay.
    fn with(
        &self,
        entry: &Entry,
        platforms: &[Platform],
        keep: Option<usize>,
        depended: &BTreeSet<&str>,
    ) -> Result<(String, Vec<String>), String> {
        let mut text = self.text.to_string();
        let mut kept: BTreeSet<String> =
            depended.iter().map(|version| version.to_string()).collect();

        // Defaults first, back to front, so earlier spans stay valid.
        let line = format!("\n{}default_version = \"", self.unit.repeat(3));
        for (name, span) in self.platforms.iter().rev() {
            let start = self.text[span.clone()]
                .find(&line)
                .map(|at| span.start + at + line.len());
            let current = start
                .and_then(|start| self.text[start..].split_once('"'))
                .map(|(current, _)| current);
            let (Some(start), Some(current)) = (start, current) else {
                return Err(format!("its {name} platform has no default_version"));
            };

            let is_bumped = platforms
                .iter()
                .any(|platform| platform.to_string() == *name);
            if !is_bumped {
                kept.insert(current.to_string());
                continue;
            }

            // Only the quoted value changes, so a trailing comment stays.
            text.replace_range(start..start + current.len(), entry.version);
            kept.insert(entry.version.to_string());
        }

        let mut blocks: BTreeMap<String, String> = self
            .versions
            .iter()
            .map(|(version, span)| (version.clone(), self.text[span.clone()].to_string()))
            .collect();
        blocks.insert(entry.version.to_string(), self.render(entry));

        let dropped = match keep {
            Some(keep) => newest_beyond(&blocks, keep, &kept, self.unit),
            None => Vec::new(),
        };

        for version in &dropped {
            blocks.remove(version);
        }

        // A default's new length moves the versions table, so find it again.
        let span = table(&text, self.unit, "versions")?;
        text.replace_range(span, &blocks.into_values().collect::<String>());
        Ok((text, dropped))
    }

    fn render(&self, entry: &Entry) -> String {
        let [two, three, four] = [2, 3, 4].map(|depth| self.unit.repeat(depth));
        let mut block = format!("{two}[\"{}\"] = {{\n{three}digests = {{\n", entry.version);
        // Recipes list platforms by name.
        let digests: BTreeMap<String, &Sha256> = entry
            .digests
            .iter()
            .map(|(platform, digest)| (platform.to_string(), digest))
            .collect();

        for (platform, digest) in digests {
            block += &format!("{four}[\"{platform}\"] = \"{}\",\n", digest.as_str());
        }

        block += &format!("{three}}},\n");
        if let Some(commit) = entry.commit {
            block += &format!("{three}commit = \"{commit}\",\n");
        }

        if let Some(serial) = entry.serial {
            block += &format!("{three}serial = {serial},\n");
        }

        block + &format!("{two}}},\n")
    }
}

/// The span of a top-level table's entries, between its opening and closing
/// lines.
fn table(text: &str, unit: &str, key: &str) -> Result<Range<usize>, String> {
    let open = format!("\n{unit}{key} = {{\n");
    let start = text
        .find(&open)
        .map(|at| at + open.len())
        .ok_or_else(|| format!("it has no top-level {key} table"))?;

    let close = format!("\n{unit}}},\n");
    let end = text[start - 1..]
        .find(&close)
        .map(|at| start - 1 + at + 1)
        .ok_or_else(|| format!("its {key} table never closes"))?;

    Ok(start..end)
}

/// Each `["key"] = { ... },` entry in a span, and the first blank or comment
/// line between them. Anything else is refused.
fn entries<'a>(
    text: &'a str,
    span: Range<usize>,
    indent: &str,
) -> Result<(Entries, Option<&'a str>), String> {
    let mut entries = Vec::new();
    let mut note = None;
    let mut at = span.start;
    while at < span.end {
        let rest = &text[at..span.end];
        let line = rest.split_inclusive('\n').next().unwrap_or_default();
        if line.trim().is_empty() || line.trim_start().starts_with("--") {
            note = note.or(Some(line.trim_end()));
            at += line.len();
            continue;
        }

        let header = rest
            .strip_prefix(indent)
            .and_then(|line| line.strip_prefix("[\""))
            .and_then(|line| line.split_once("\"] = {\n"))
            .filter(|(key, _)| !key.contains(['"', '\n']));

        let Some((key, _)) = header else {
            return Err(format!("rb can't edit the line `{}`", line.trim_end()));
        };

        let close = format!("\n{indent}}},\n");
        let length = rest
            .find(&close)
            .map(|end| end + close.len())
            .ok_or_else(|| format!("the {key} entry never closes"))?;

        entries.push((key.to_string(), at..at + length));
        at += length;
    }

    Ok((entries, note))
}

/// The versions beyond the newest `keep`, by serial when every version has
/// one, and otherwise by version order. Versions in `kept` are never dropped.
fn newest_beyond(
    blocks: &BTreeMap<String, String>,
    keep: usize,
    kept: &BTreeSet<String>,
    unit: &str,
) -> Vec<String> {
    let line = format!("\n{}serial = ", unit.repeat(3));
    let serial = |block: &str| {
        let (_, rest) = block.split_once(&line)?;
        rest.split(',').next()?.trim().parse::<u64>().ok()
    };

    let mut versions: Vec<(&String, Option<u64>)> = blocks
        .iter()
        .map(|(version, block)| (version, serial(block)))
        .collect();

    match versions.iter().all(|(_, serial)| serial.is_some()) {
        true => versions.sort_by_key(|&(_, serial)| std::cmp::Reverse(serial)),
        false => versions.sort_by(|(a, _), (b, _)| compare_versions(b, a)),
    }

    versions
        .into_iter()
        .skip(keep)
        .map(|(version, _)| version)
        .filter(|version| !kept.contains(*version))
        .cloned()
        .collect()
}

/// Dotted versions compared part by part, numbers numerically.
fn compare_versions(a: &str, b: &str) -> Ordering {
    let parts = |version: &str| {
        version
            .split(|byte: char| !byte.is_ascii_alphanumeric())
            .map(|part| part.parse::<u64>().map_err(|_| part.to_string()))
            .collect::<Vec<_>>()
    };

    parts(a).cmp(&parts(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECIPE: &str = r#"return {
    name = "demo",
    -- A comment bump must keep
    platforms = {
        ["aarch64-macos"] = {
            target = "macos",
            default_version = "1.9.0",
        },
        -- Linux waits on a newer compiler
        ["x86_64-linux"] = {
            default_version = "1.9.0", -- until 1.10 builds
        },
    },
    versions = {
        ["1.10.0"] = {
            digests = {
                ["aarch64-macos"] = "aa",
            },
        },
        ["1.9.0"] = {
            digests = {
                ["aarch64-macos"] = "bb",
                ["x86_64-linux"] = "bb",
            },
            revision = 2,
        },
    },
}
"#;

    fn digests(platforms: &[Platform]) -> BTreeMap<Platform, Sha256> {
        platforms
            .iter()
            .map(|&platform| (platform, "c".repeat(64).try_into().unwrap()))
            .collect()
    }

    #[test]
    fn a_version_is_added_as_the_default_and_the_rest_left_alone() {
        let recipe = Recipe::read(RECIPE).unwrap();
        let both = [Platform::Aarch64Macos, Platform::X86_64Linux];
        let digests = digests(&both);
        let entry = Entry {
            version: "1.11.0",
            digests: &digests,
            commit: Some("abc"),
            serial: None,
        };

        let (text, dropped) = recipe.with(&entry, &both, None, &BTreeSet::new()).unwrap();
        assert!(dropped.is_empty());
        assert!(text.contains("-- A comment bump must keep"));
        assert_eq!(text.matches(r#"default_version = "1.11.0","#).count(), 2);
        assert!(text.contains(&format!(
            "        [\"1.11.0\"] = {{\n            digests = {{\n                [\"aarch64-macos\"] = \"{c}\",\n                [\"x86_64-linux\"] = \"{c}\",\n            }},\n            commit = \"abc\",\n        }},\n",
            c = "c".repeat(64)
        )));
        assert!(text.contains("            revision = 2,\n"));
        assert!(Recipe::read(&text).is_ok());
    }

    #[test]
    fn keep_drops_the_oldest_but_never_a_default_or_dependency() {
        let recipe = Recipe::read(RECIPE).unwrap();
        let macos = [Platform::Aarch64Macos];
        let digests = digests(&macos);
        let entry = Entry {
            version: "1.11.0",
            digests: &digests,
            commit: None,
            serial: None,
        };

        // Linux still defaults to 1.9.0, so 1.10.0 goes instead.
        let (text, dropped) = recipe
            .with(&entry, &macos, Some(1), &BTreeSet::new())
            .unwrap();
        assert_eq!(dropped, ["1.10.0"]);
        assert!(text.contains(r#"["1.9.0"] = {"#));
        assert!(text.contains(r#"default_version = "1.9.0", -- until 1.10 builds"#));

        let depended = BTreeSet::from(["1.10.0"]);
        let (_, dropped) = recipe.with(&entry, &macos, Some(1), &depended).unwrap();
        assert!(dropped.is_empty());
    }

    #[test]
    fn serials_order_versions_when_every_version_has_one() {
        let blocks = BTreeMap::from([
            ("a".to_string(), "{\n            serial = 3,\n".to_string()),
            ("b".to_string(), "{\n            serial = 1,\n".to_string()),
            ("c".to_string(), "{\n            serial = 2,\n".to_string()),
        ]);

        assert_eq!(newest_beyond(&blocks, 2, &BTreeSet::new(), "    "), ["b"]);
        assert_eq!(compare_versions("1.10.0", "1.9.0"), Ordering::Greater);
        assert_eq!(
            compare_versions("2026.2.5+262", "2026.2.5+100"),
            Ordering::Greater
        );
    }

    #[test]
    fn an_unexpected_line_among_versions_is_refused() {
        let commented = RECIPE.replace(
            "        [\"1.9.0\"] = {",
            "        -- why 1.9.0 stays\n        [\"1.9.0\"] = {",
        );

        let error = Recipe::read(&commented).err().unwrap();
        assert!(error.contains("-- why 1.9.0 stays"), "{error}");
    }
}
