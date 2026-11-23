use super::{github, load, parse_platform, trees, version, Sources};
use rootbeer_drv::{is_version, Platform, Sha256};
use rootbeer_eval::{Lines, Target};
use serde::Serialize;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::ops::Range;

#[derive(clap::Args, Debug)]
pub(super) struct Args {
    /// The package to add a version to
    name: String,
    /// The version to add, which becomes the default. A recipe that follows
    /// release lines only moves a default forward, so an older line's version
    /// is added beside it
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
    /// defaults to or another recipe depends on always stays. A recipe that
    /// follows release lines keeps the newest version of each line instead
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
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    trees: BTreeMap<Platform, Sha256>,
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
    let declared_block = recipe
        .versions
        .iter()
        .find(|(declared, _)| declared == version)
        .map(|(_, span)| &original[span.clone()]);

    let recorded = declared_block.and_then(|block| recipe.commit_of(block));
    if let Some((recorded, commit)) = recorded.zip(commit).filter(|(a, b)| a != b) {
        return Err(format!(
            "{name}@{version} is declared at commit {recorded}, not {commit}"
        ));
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

    // A declared version gains only the platforms it lacks, such as one whose
    // download appeared after the version was added.
    let missing: Vec<Platform> = platforms
        .iter()
        .copied()
        .filter(|platform| declared_block.is_none_or(|block| !recipe.has_digest(block, *platform)))
        .collect();

    // Following release lines, an older line's version never becomes the
    // default.
    let lines = catalog
        .upstreams(name)
        .into_iter()
        .find_map(|(_, upstream)| upstream.lines.as_ref());
    let defaulted: Vec<Platform> = platforms
        .iter()
        .copied()
        .filter(|&platform| {
            let default = catalog.default_version(name, platform).unwrap_or_default();
            lines.is_none() || version::compare(version, default) != Ordering::Less
        })
        .collect();

    let is_default =
        |platform: &Platform| catalog.default_version(name, *platform) == Some(version);
    if declared_block.is_some() && missing.is_empty() && defaulted.iter().all(is_default) {
        return print(&Bumped {
            name,
            version,
            added: false,
            digests: BTreeMap::new(),
            trees: BTreeMap::new(),
            dropped: Vec::new(),
        });
    }

    // A tree needs the download itself, so a platform that records one
    // downloads it even when GitHub publishes its digest.
    let mut digests = BTreeMap::new();
    let mut trees = BTreeMap::new();
    let mut by_url = BTreeMap::<String, (Sha256, Option<Sha256>)>::new();
    for &platform in &missing {
        let at = |error: String| format!("{name}@{version} on {platform}: {error}");
        let download = catalog
            .download(name, version, commit, args.serial, platform)
            .map_err(at)?;

        let format = download.extract;
        let published = || match &download.release {
            Some(release) => {
                github::asset_digest(&release.repository, &release.tag, &release.asset).map(Some)
            }
            None => Ok(None),
        };

        let (digest, tree) = match (by_url.get(&download.url).cloned(), format) {
            (Some((digest, _)), None) => (digest, None),
            (Some((digest, Some(tree))), Some(_)) => (digest, Some(tree)),
            (Some((digest, None)), Some(format)) => {
                let (_, tree) =
                    trees::download(&download.url, Some(&digest), format).map_err(at)?;
                (digest, Some(tree))
            }
            (None, Some(format)) => {
                let published = published().map_err(at)?;
                let (digest, tree) =
                    trees::download(&download.url, published.as_ref(), format).map_err(at)?;
                (digest, Some(tree))
            }
            (None, None) => match published().map_err(at)? {
                Some(digest) => (digest, None),
                None => (github::download_digest(&download.url).map_err(at)?, None),
            },
        };

        by_url.insert(download.url, (digest.clone(), tree.clone()));
        digests.insert(platform, digest);
        if let Some(tree) = tree {
            trees.insert(platform, tree);
        }
    }

    let entry = Entry {
        version,
        digests: &digests,
        trees: &trees,
        commit,
        serial: args.serial,
    };
    // An always-newest download no longer serves an older version, so only
    // the new one can still build.
    let is_latest = catalog
        .upstreams(name)
        .into_iter()
        .any(|(_, upstream)| upstream.latest);
    let depended = catalog.depended_on(name);
    let keep = match (is_latest, lines) {
        (true, _) => Some(Keep::Newest(1)),
        (false, Some(lines)) => args.keep.map(|_| Keep::Lines(lines)),
        (false, None) => args.keep.map(Keep::Newest),
    };
    let (text, dropped) = recipe.with(&entry, &defaulted, keep, &depended)?;
    if dropped.iter().any(|dropped| dropped == version) {
        return Err(format!(
            "{name}@{version} isn't the newest of a release line {name} keeps"
        ));
    }

    fs::write(&path, &text).map_err(|error| format!("{}: {error}", path.display()))?;

    // The edit must leave a recipe that still evaluates, or it's undone.
    if let Err(error) = evaluates(&args.sources, name, version, &platforms, &defaulted) {
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
        trees,
        dropped,
    })
}

/// Whether `defaulted` now default to `version`, and it and every platform's
/// default evaluate.
fn evaluates(
    sources: &Sources,
    name: &str,
    version: &str,
    platforms: &[Platform],
    defaulted: &[Platform],
) -> Result<(), String> {
    let (catalog, hosts) = load(sources)?;
    if let Some(platform) = defaulted
        .iter()
        .find(|&&platform| catalog.default_version(name, platform) != Some(version))
    {
        return Err(format!("{platform} doesn't default to it"));
    }

    let target = |version: &str, platform| Target {
        name: name.to_string(),
        version: version.to_string(),
        platform,
    };
    let mut targets: Vec<Target> = catalog
        .platforms(name)
        .into_iter()
        .filter_map(|platform| Some(target(catalog.default_version(name, platform)?, platform)))
        .collect();

    let added = platforms
        .iter()
        .filter(|platform| !defaulted.contains(platform))
        .map(|&platform| target(version, platform));
    targets.extend(added);

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

/// Which versions a bump keeps.
#[derive(Clone, Copy)]
enum Keep<'a> {
    /// The newest this many.
    Newest(usize),
    /// The newest of each release line.
    Lines(&'a Lines),
}

/// A version to add, as the recipe will record it.
struct Entry<'a> {
    version: &'a str,
    digests: &'a BTreeMap<Platform, Sha256>,
    trees: &'a BTreeMap<Platform, Sha256>,
    commit: Option<&'a str>,
    serial: Option<u64>,
}

/// A recipe file in the layout pdr writes, with the spans `bump` edits. Text
/// edits keep every comment and line `bump` doesn't touch.
pub(super) struct Recipe<'a> {
    text: &'a str,
    unit: &'a str,
    versions: Entries,
    platforms: Entries,
}

/// Each entry's key and span.
type Entries = Vec<(String, Range<usize>)>;

impl<'a> Recipe<'a> {
    pub(super) fn read(text: &'a str) -> Result<Recipe<'a>, String> {
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
        keep: Option<Keep>,
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
        let block = match blocks.get(entry.version) {
            Some(block) => {
                let block = self.merge(block, "digests", entry.digests)?;
                self.merge(&block, "trees", entry.trees)?
            }
            None => self.render(entry),
        };
        blocks.insert(entry.version.to_string(), block);

        let dropped = match keep {
            Some(Keep::Newest(keep)) => newest_beyond(&blocks, keep, &kept, self.unit),
            Some(Keep::Lines(lines)) => beyond_lines(&blocks, lines, &kept),
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

    /// The recipe with `trees` added to the versions they name, beside each
    /// one's digests. A tree a version already records stays.
    pub(super) fn with_trees(
        &self,
        trees: &BTreeMap<String, BTreeMap<Platform, Sha256>>,
    ) -> Result<String, String> {
        let mut text = self.text.to_string();
        for (version, span) in self.versions.iter().rev() {
            let Some(trees) = trees.get(version) else {
                continue;
            };

            let block = self.merge(&self.text[span.clone()], "trees", trees)?;
            text.replace_range(span.clone(), &block);
        }

        Ok(text)
    }

    /// A version's block with `entries` added to its `table` for the
    /// platforms it lacks. A missing trees table is made after digests.
    fn merge(
        &self,
        block: &str,
        table: &str,
        entries: &BTreeMap<Platform, Sha256>,
    ) -> Result<String, String> {
        let [three, four] = [3, 4].map(|depth| self.unit.repeat(depth));
        let close = format!("{three}}},\n");
        let span = match self.span(block, table)? {
            Some(span) => span,
            None if entries.is_empty() => return Ok(block.to_string()),
            None => {
                let digests = self
                    .span(block, "digests")?
                    .ok_or("its version has no digests table")?;
                let at = digests.end + close.len();
                let opened = format!(
                    "{}{three}{table} = {{\n{close}{}",
                    &block[..at],
                    &block[at..]
                );
                return self.merge(&opened, table, entries);
            }
        };

        let mut lines = BTreeMap::new();
        for line in block[span.clone()].lines() {
            let platform = line
                .strip_prefix(&format!("{four}[\""))
                .and_then(|rest| rest.split_once("\"] = \""))
                .map(|(platform, _)| platform.to_string())
                .ok_or_else(|| format!("rb can't edit the line `{line}`"))?;
            lines.insert(platform, format!("{line}\n"));
        }

        for (platform, digest) in entries {
            let line = format!("{four}[\"{platform}\"] = \"{}\",\n", digest.as_str());
            lines.entry(platform.to_string()).or_insert(line);
        }

        let lines = lines.into_values().collect::<String>();
        Ok(format!(
            "{}{lines}{}",
            &block[..span.start],
            &block[span.end..]
        ))
    }

    /// The span of the lines inside a version's `table`, when it has one.
    fn span(&self, block: &str, table: &str) -> Result<Option<Range<usize>>, String> {
        let three = self.unit.repeat(3);
        let open = format!("\n{three}{table} = {{\n");
        let close = format!("\n{three}}},\n");
        let Some(start) = block.find(&open).map(|at| at + open.len()) else {
            return Ok(None);
        };

        let end = block[start - 1..]
            .find(&close)
            .map(|at| start + at)
            .ok_or_else(|| format!("its version's {table} table never closes"))?;
        Ok(Some(start..end))
    }

    fn has_digest(&self, block: &str, platform: Platform) -> bool {
        block.contains(&format!("\n{}[\"{platform}\"] = \"", self.unit.repeat(4)))
    }

    fn commit_of<'b>(&self, block: &'b str) -> Option<&'b str> {
        let line = format!("\n{}commit = \"", self.unit.repeat(3));
        let start = block.find(&line)? + line.len();
        block[start..].split_once('"').map(|(commit, _)| commit)
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
        if !entry.trees.is_empty() {
            block += &format!("{three}trees = {{\n");
            let trees: BTreeMap<String, &Sha256> = entry
                .trees
                .iter()
                .map(|(platform, tree)| (platform.to_string(), tree))
                .collect();
            for (platform, tree) in trees {
                block += &format!("{four}[\"{platform}\"] = \"{}\",\n", tree.as_str());
            }

            block += &format!("{three}}},\n");
        }

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
        false => versions.sort_by(|(a, _), (b, _)| version::compare(b, a)),
    }

    versions
        .into_iter()
        .skip(keep)
        .map(|(version, _)| version)
        .filter(|version| !kept.contains(*version))
        .cloned()
        .collect()
}

/// The versions that aren't the newest of one of the newest lines. Versions
/// in `kept` or in no line are never dropped.
fn beyond_lines(
    blocks: &BTreeMap<String, String>,
    lines: &Lines,
    kept: &BTreeSet<String>,
) -> Vec<String> {
    let mut newest: BTreeMap<Vec<u64>, &String> = BTreeMap::new();
    for version in blocks.keys() {
        let Some(line) = lines.line(version) else {
            continue;
        };

        let newer = newest.entry(line).or_insert(version);
        if version::compare(version, newer) == Ordering::Greater {
            *newer = version;
        }
    }

    let followed: BTreeSet<&String> = newest.into_values().rev().take(lines.keep).collect();
    blocks
        .keys()
        .filter(|version| lines.line(version).is_some())
        .filter(|version| !followed.contains(version) && !kept.contains(*version))
        .cloned()
        .collect()
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
            trees: &BTreeMap::new(),
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
            trees: &BTreeMap::new(),
            commit: None,
            serial: None,
        };

        // Linux still defaults to 1.9.0, so 1.10.0 goes instead.
        let (text, dropped) = recipe
            .with(&entry, &macos, Some(Keep::Newest(1)), &BTreeSet::new())
            .unwrap();
        assert_eq!(dropped, ["1.10.0"]);
        assert!(text.contains(r#"["1.9.0"] = {"#));
        assert!(text.contains(r#"default_version = "1.9.0", -- until 1.10 builds"#));

        let depended = BTreeSet::from(["1.10.0"]);
        let (_, dropped) = recipe
            .with(&entry, &macos, Some(Keep::Newest(1)), &depended)
            .unwrap();
        assert!(dropped.is_empty());
    }

    #[test]
    fn a_declared_version_gains_a_platform_that_arrived_later() {
        let recipe = Recipe::read(RECIPE).unwrap();
        let (_, span) = &recipe.versions[0];
        let block = &RECIPE[span.clone()];
        assert!(recipe.has_digest(block, Platform::Aarch64Macos));
        assert!(!recipe.has_digest(block, Platform::X86_64Linux));

        let linux = [Platform::X86_64Linux];
        let digests = digests(&linux);
        let entry = Entry {
            version: "1.10.0",
            digests: &digests,
            trees: &BTreeMap::new(),
            commit: None,
            serial: None,
        };

        let (text, dropped) = recipe.with(&entry, &linux, None, &BTreeSet::new()).unwrap();
        assert!(dropped.is_empty());
        assert!(text.contains(&format!(
            "        [\"1.10.0\"] = {{\n            digests = {{\n                [\"aarch64-macos\"] = \"aa\",\n                [\"x86_64-linux\"] = \"{}\",\n            }},\n        }},\n",
            "c".repeat(64)
        )));
        assert!(text.contains(r#"default_version = "1.10.0","#));
        assert!(text.contains(r#"default_version = "1.9.0","#));
        assert_eq!(recipe.commit_of(block), None);
    }

    #[test]
    fn trees_sit_beside_digests_and_never_replace_one() {
        let recipe = Recipe::read(RECIPE).unwrap();
        let both = [Platform::Aarch64Macos, Platform::X86_64Linux];
        let digests = digests(&both);
        let trees = BTreeMap::from([(Platform::X86_64Linux, "e".repeat(64).try_into().unwrap())]);
        let entry = Entry {
            version: "1.11.0",
            digests: &digests,
            trees: &trees,
            commit: Some("abc"),
            serial: None,
        };

        let (text, _) = recipe.with(&entry, &both, None, &BTreeSet::new()).unwrap();
        assert!(text.contains(&format!(
            "            }},\n            trees = {{\n                [\"x86_64-linux\"] = \"{}\",\n            }},\n            commit = \"abc\",\n",
            "e".repeat(64)
        )));
        assert!(Recipe::read(&text).is_ok());

        // A declared version gains a trees table after its digests, then
        // only the trees it lacks.
        let tree = |c: &str| -> Sha256 { c.repeat(64).try_into().unwrap() };
        let recorded = BTreeMap::from([(
            "1.9.0".to_string(),
            BTreeMap::from([(Platform::X86_64Linux, tree("e"))]),
        )]);
        let text = recipe.with_trees(&recorded).unwrap();
        let expected = format!(
            "                [\"x86_64-linux\"] = \"bb\",\n            }},\n            trees = {{\n                [\"x86_64-linux\"] = \"{}\",\n            }},\n            revision = 2,\n",
            "e".repeat(64)
        );
        assert!(text.contains(&expected), "{text}");

        let again = Recipe::read(&text).unwrap();
        let recorded = BTreeMap::from([(
            "1.9.0".to_string(),
            BTreeMap::from([
                (Platform::X86_64Linux, tree("f")),
                (Platform::Aarch64Macos, tree("a")),
            ]),
        )]);
        let text = again.with_trees(&recorded).unwrap();
        assert!(text.contains(&format!(
            "            trees = {{\n                [\"aarch64-macos\"] = \"{}\",\n                [\"x86_64-linux\"] = \"{}\",\n            }},\n",
            "a".repeat(64),
            "e".repeat(64)
        )), "{text}");
        assert!(text.contains("[\"1.10.0\"] = {\n            digests = {\n                [\"aarch64-macos\"] = \"aa\",\n            },\n        },\n"));
    }

    #[test]
    fn serials_order_versions_when_every_version_has_one() {
        let blocks = BTreeMap::from([
            ("a".to_string(), "{\n            serial = 3,\n".to_string()),
            ("b".to_string(), "{\n            serial = 1,\n".to_string()),
            ("c".to_string(), "{\n            serial = 2,\n".to_string()),
        ]);

        assert_eq!(newest_beyond(&blocks, 2, &BTreeSet::new(), "    "), ["b"]);
    }

    #[test]
    fn release_lines_keep_the_newest_version_of_each() {
        let lines = Lines {
            by: rootbeer_eval::LineBy::Major,
            keep: 2,
        };
        let blocks: BTreeMap<String, String> = ["26.2.0", "26.10.0", "24.3.0", "24.1.0", "22.9.0"]
            .into_iter()
            .map(|version| (version.to_string(), String::new()))
            .collect();

        assert_eq!(
            beyond_lines(&blocks, &lines, &BTreeSet::new()),
            ["22.9.0", "24.1.0", "26.2.0"]
        );

        let kept = BTreeSet::from(["22.9.0".to_string()]);
        assert_eq!(beyond_lines(&blocks, &lines, &kept), ["24.1.0", "26.2.0"]);
    }

    #[test]
    fn an_older_line_is_added_beside_the_default() {
        let recipe = Recipe::read(RECIPE).unwrap();
        let both = [Platform::Aarch64Macos, Platform::X86_64Linux];
        let digests = digests(&both);
        let entry = Entry {
            version: "1.8.5",
            digests: &digests,
            trees: &BTreeMap::new(),
            commit: None,
            serial: None,
        };
        let lines = Lines {
            by: rootbeer_eval::LineBy::Minor,
            keep: 2,
        };

        // No platform moves, so 1.10 and 1.9 stay the two lines kept.
        let (text, dropped) = recipe
            .with(&entry, &[], Some(Keep::Lines(&lines)), &BTreeSet::new())
            .unwrap();
        assert_eq!(dropped, ["1.8.5"]);
        assert_eq!(text, RECIPE);

        let entry = Entry {
            version: "1.9.4",
            ..entry
        };
        let (text, dropped) = recipe
            .with(&entry, &[], Some(Keep::Lines(&lines)), &BTreeSet::new())
            .unwrap();
        assert!(dropped.is_empty());
        assert!(text.contains(r#"["1.9.4"] = {"#));
        assert_eq!(text.matches(r#"default_version = "1.9.0","#).count(), 2);
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
