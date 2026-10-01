//! The human view of a derivation, for diffs and terminals. Keys never read it.

use crate::{Build, Check, Dependency, DependencyKind, Derivation, Fetch, Platform};
use std::fmt::{self, Display, Formatter};

impl Display for Derivation {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Derivation::Build(build) => build.fmt(f),
            Derivation::Fetch(fetch) => fetch.fmt(f),
            Derivation::Check(check) => check.fmt(f),
        }
    }
}

impl Display for Build {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        writeln!(f, "kind: build")?;
        writeln!(f, "name: {}", self.name)?;
        writeln!(f, "version: {}", self.version)?;
        writeln!(f, "platform: {}", self.platform)?;
        writeln!(f, "sandbox: {}", self.sandbox)?;
        map(
            f,
            "inputs",
            self.inputs.iter().map(|(n, k)| (n, k.as_str())),
        )?;
        dependencies(f, &self.dependencies)?;
        map(f, "env", self.env.iter().map(|(n, v)| (n, v.as_str())))?;
        list(f, "outputs", &self.outputs)?;
        script(f, &self.script)
    }
}

impl Display for Fetch {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        writeln!(f, "kind: fetch")?;
        writeln!(f, "sha256: {}", self.sha256.as_str())?;
        list(f, "urls", &self.urls)
    }
}

impl Display for Check {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        writeln!(f, "kind: check")?;
        writeln!(f, "name: {}", self.name)?;
        writeln!(f, "target: {}", self.target)?;
        writeln!(f, "platform: {}", self.platform)?;
        writeln!(f, "sandbox: {}", self.sandbox)?;
        dependencies(f, &self.dependencies)?;
        map(f, "env", self.env.iter().map(|(n, v)| (n, v.as_str())))?;
        script(f, &self.script)
    }
}

impl Display for Platform {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Display for DependencyKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// Quotes a value that would otherwise hide whitespace or read as a different value.
struct Scalar<'a>(&'a str);

impl Display for Scalar<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let value = self.0;
        let is_plain = !value.is_empty()
            && value.trim() == value
            && !value.starts_with('"')
            && !value.contains(char::is_control);
        if is_plain {
            return f.write_str(value);
        }

        write!(f, "{value:?}")
    }
}

fn map<'a>(
    f: &mut Formatter<'_>,
    name: &str,
    entries: impl IntoIterator<Item = (&'a String, &'a str)>,
) -> fmt::Result {
    let mut entries = entries.into_iter().peekable();
    if entries.peek().is_none() {
        return Ok(());
    }

    writeln!(f, "{name}:")?;
    entries.try_for_each(|(key, value)| writeln!(f, "  {key}: {}", Scalar(value)))
}

fn list<'a>(
    f: &mut Formatter<'_>,
    name: &str,
    items: impl IntoIterator<Item = &'a String>,
) -> fmt::Result {
    let mut items = items.into_iter().peekable();
    if items.peek().is_none() {
        return Ok(());
    }

    writeln!(f, "{name}:")?;
    items.try_for_each(|item| writeln!(f, "  - {}", Scalar(item)))
}

fn dependencies(f: &mut Formatter<'_>, dependencies: &[Dependency]) -> fmt::Result {
    if dependencies.is_empty() {
        return Ok(());
    }

    writeln!(f, "dependencies:")?;
    dependencies.iter().try_for_each(|dependency| {
        writeln!(
            f,
            "  - {} ({}) {}",
            dependency.name, dependency.kind, dependency.key
        )
    })
}

// Scripts keep their real lines (YAML block chomping marks the final newline), so a
// one-line change diffs as one line. Anything a block can't show exactly is quoted.
fn script(f: &mut Formatter<'_>, script: &str) -> fmt::Result {
    let (indicator, body) = match script.strip_suffix('\n') {
        Some(body) => ("|", body),
        None => ("|-", script),
    };
    let is_hidden = |c: char| c.is_control() && c != '\n' && c != '\t';
    if body.is_empty() || body.ends_with('\n') || body.contains(is_hidden) {
        return writeln!(f, "script: {script:?}");
    }

    writeln!(f, "script: {indicator}")?;
    body.split('\n').try_for_each(|line| match line {
        "" => writeln!(f),
        line => writeln!(f, "  {line}"),
    })
}
