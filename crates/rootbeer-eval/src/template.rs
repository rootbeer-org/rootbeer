use rootbeer_drv::Platform;
use std::collections::BTreeMap;

// These are inherently machine dependent, so we use variables for them
const VARIABLES: [(&str, &str); 2] = [("{prefix}", "${out}"), ("{jobs}", "${jobs}")];

/// What a recipe's templates may name for one version on one platform.
#[derive(Clone)]
pub(crate) struct Values<'a> {
    pub version: &'a str,
    pub tag: String,
    pub target: Option<&'a str>,
    pub commit: Option<&'a str>,
    pub platform: Platform,
    pub dependencies: BTreeMap<String, String>,
}

impl Values<'_> {
    /// Fills a URL, asset, or path, where no brace may remain.
    pub(crate) fn literal(&self, text: &str) -> Result<String, String> {
        let value = self.fill(text)?;
        if value.contains(['{', '}']) {
            return Err(format!("unsupported placeholder in `{text}`"));
        }

        Ok(value)
    }

    pub(crate) fn command(&self, argv: &[impl AsRef<str>]) -> Result<String, String> {
        let words = argv
            .iter()
            .map(|word| Ok(quote(&self.fill(word.as_ref())?)))
            .collect::<Result<Vec<_>, String>>()?;

        Ok(words.join(" "))
    }

    pub(crate) fn commands(&self, commands: &[Vec<String>]) -> Result<Vec<String>, String> {
        commands.iter().map(|argv| self.command(argv)).collect()
    }

    fn fill(&self, text: &str) -> Result<String, String> {
        let shared_extension = match self.platform {
            Platform::Aarch64Macos => "dylib",
            Platform::Aarch64Linux | Platform::X86_64Linux => "so",
        };

        let mut value = text
            .replace("{version}", self.version)
            .replace("{tag}", &self.tag)
            .replace("{shared_extension}", shared_extension);

        if let Some(target) = self.target {
            value = value.replace("{target}", target);
        }

        if value.contains("{dependencies}") {
            return Err("`{dependencies}` must name one: `{dependencies.<name>}`".into());
        }

        value = self.dependencies.iter().fold(value, |value, (name, path)| {
            value.replace(&format!("{{dependencies.{name}}}"), path)
        });

        if value.contains("{dependencies.") {
            return Err(format!(
                "`{text}` names an undeclared build or linked dependency"
            ));
        }

        if value.contains("{runtime:") {
            return Err("`{runtime:<package>}` is not supported".into());
        }

        if value.contains("{commit}") {
            let commit = self
                .commit
                .ok_or("`{commit}` needs the version to record its commit")?;
            value = value.replace("{commit}", commit);
        }

        let release = self.version.split(['-', '+']).next().unwrap_or_default();
        let parts = release.split('.').collect::<Vec<_>>();
        for (index, name) in ["{major}", "{minor}", "{patch}"].into_iter().enumerate() {
            if !value.contains(name) {
                continue;
            }

            let part = parts.get(index).ok_or_else(|| {
                format!("`{name}` needs more version parts than `{}`", self.version)
            })?;
            value = value.replace(name, part);
        }

        Ok(value)
    }
}

/// I won't even pretend to say this is good quoting, but it's probably enough
/// for now and things will fail loudly if it isn't.
pub(crate) fn quote(word: &str) -> String {
    let is_bare = !word.is_empty()
        && word
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&c));
    if is_bare {
        return word.to_string();
    }

    let expanded = VARIABLES
        .iter()
        .fold(escape(word), |word, (placeholder, variable)| {
            word.replace(placeholder, variable)
        });

    format!("\"{expanded}\"")
}

/// A path under a sandbox variable, such as `${out}/bin/fd`.
pub(crate) fn path(variable: &str, relative: &str) -> String {
    format!("\"${{{variable}}}/{}\"", escape(relative))
}

fn escape(text: &str) -> String {
    text.chars()
        .fold(String::with_capacity(text.len()), |mut escaped, c| {
            if matches!(c, '\\' | '"' | '$' | '`') {
                escaped.push('\\');
            }

            escaped.push(c);
            escaped
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn rendered_words_reach_the_shell_unchanged() {
        let words = [
            "plain",
            "",
            "two words",
            "it's",
            "\"quoted\"",
            "$HOME",
            "`id`",
            "back\\slash",
            "new\nline",
            "{}",
            "*",
            "~",
            "a;b",
            "-j{jobs}",
            "DESTDIR={prefix}",
            "{prefix}{jobs}",
        ];

        let rendered = words
            .iter()
            .map(|word| quote(word))
            .collect::<Vec<_>>()
            .join(" ");

        let script = format!("printf '%s\\0' {rendered} {}", path("out", "a b/$c"));
        let output = Command::new("/bin/sh")
            .args(["-c", &script])
            .env_clear()
            .envs([("out", "/o"), ("jobs", "4"), ("HOME", "/home")])
            .output()
            .unwrap();

        let expected = words
            .iter()
            .map(|word| word.replace("{prefix}", "/o").replace("{jobs}", "4"))
            .chain(["/o/a b/$c".to_string()])
            .map(|word| word + "\0")
            .collect::<String>();

        assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
    }

    #[test]
    fn dependency_placeholders_name_a_declared_dependency() {
        let values = Values {
            version: "1",
            tag: "1".to_string(),
            target: None,
            commit: None,
            platform: Platform::X86_64Linux,
            dependencies: BTreeMap::from([("zlib".to_string(), "/z".to_string())]),
        };

        assert_eq!(values.literal("{dependencies.zlib}/lib").unwrap(), "/z/lib");
        assert_eq!(
            values.literal("{dependencies}").unwrap_err(),
            "`{dependencies}` must name one: `{dependencies.<name>}`"
        );
        assert_eq!(
            values.command(&["{dependencies.xz}"]).unwrap_err(),
            "`{dependencies.xz}` names an undeclared build or linked dependency"
        );
    }
}
