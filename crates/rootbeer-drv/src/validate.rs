use crate::{Build, Check, Dependency, Derivation, Error, Fetch};
use std::collections::{BTreeMap, BTreeSet};

struct Pattern {
    reason: &'static str,
    first: fn(u8) -> bool,
    rest: fn(u8) -> bool,
}

const PACKAGE_NAME: Pattern = Pattern {
    reason: "must match [a-z0-9][a-z0-9-]*",
    first: |c| matches!(c, b'a'..=b'z' | b'0'..=b'9'),
    rest: |c| matches!(c, b'a'..=b'z' | b'0'..=b'9' | b'-'),
};

const VERSION: Pattern = Pattern {
    reason: "must match [A-Za-z0-9][A-Za-z0-9._+~-]*",
    first: |c| c.is_ascii_alphanumeric(),
    rest: |c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'+' | b'~' | b'-'),
};

const SANDBOX: Pattern = Pattern {
    reason: "must match [a-z0-9-]+",
    first: |c| matches!(c, b'a'..=b'z' | b'0'..=b'9' | b'-'),
    rest: |c| matches!(c, b'a'..=b'z' | b'0'..=b'9' | b'-'),
};

const INPUT_NAME: Pattern = Pattern {
    reason: "must match [a-z_][a-z0-9_]*",
    first: |c| matches!(c, b'a'..=b'z' | b'_'),
    rest: |c| matches!(c, b'a'..=b'z' | b'0'..=b'9' | b'_'),
};

// Lowercase names are the sandbox's ($out, $jobs, inputs), so env can never shadow one.
const ENV_NAME: Pattern = Pattern {
    reason: "must match [A-Z][A-Za-z0-9_]*",
    first: |c| c.is_ascii_uppercase(),
    rest: |c| c.is_ascii_alphanumeric() || c == b'_',
};

const OUTPUT_NAME: Pattern = Pattern {
    reason: "must match [a-z][a-z0-9]*",
    first: |c| c.is_ascii_lowercase(),
    rest: |c| c.is_ascii_lowercase() || c.is_ascii_digit(),
};

const SANDBOX_INPUTS: [&str; 2] = ["jobs", "target"];
const SANDBOX_ENV: [&str; 14] = [
    "PATH",
    "HOME",
    "TMPDIR",
    "LC_ALL",
    "TZ",
    "SOURCE_DATE_EPOCH",
    "CC",
    "CXX",
    "CONFIG_SHELL",
    "ZERO_AR_DATE",
    "CPATH",
    "LIBRARY_PATH",
    "PKG_CONFIG_PATH",
    "CMAKE_PREFIX_PATH",
];

impl Pattern {
    fn is_match(&self, value: &str) -> bool {
        value.bytes().next().is_some_and(self.first) && value.bytes().all(self.rest)
    }

    fn check(&self, field: &str, value: &str) -> Result<(), Error> {
        if !self.is_match(value) {
            return Err(Error::invalid(field, value, self.reason));
        }

        Ok(())
    }
}

/// Whether `name` is a package name a derivation accepts.
pub fn is_package_name(name: &str) -> bool {
    PACKAGE_NAME.is_match(name)
}

/// One path component, never `.` or `..`, without control characters.
pub fn is_file_name(name: &str) -> bool {
    !matches!(name, "" | "." | "..") && !name.contains('/') && !name.contains(char::is_control)
}

impl Derivation {
    pub(crate) fn validate(&self) -> Result<(), Error> {
        match self {
            Derivation::Build(build) => build.validate(),
            Derivation::Fetch(fetch) => fetch.validate(),
            Derivation::Check(check) => check.validate(),
        }
    }
}

impl Build {
    fn validate(&self) -> Result<(), Error> {
        PACKAGE_NAME.check("name", &self.name)?;
        VERSION.check("version", &self.version)?;
        SANDBOX.check("sandbox", &self.sandbox)?;

        script(&self.script)?;
        dependencies(&self.dependencies)?;
        env(&self.env)?;

        self.inputs.keys().try_for_each(|name| {
            INPUT_NAME.check("inputs", name)?;
            if SANDBOX_INPUTS.contains(&name.as_str()) || self.outputs.contains(name) {
                return Err(Error::invalid("inputs", name, "is set by the sandbox"));
            }

            Ok(())
        })?;

        if !self.outputs.contains("out") {
            return Err(Error::invalid("outputs", "", "must include \"out\""));
        }

        self.outputs
            .iter()
            .try_for_each(|output| OUTPUT_NAME.check("outputs", output))
    }
}

impl Fetch {
    fn validate(&self) -> Result<(), Error> {
        if self.urls.is_empty() {
            return Err(Error::invalid("urls", "", "must list at least one URL"));
        }

        self.urls.iter().enumerate().try_for_each(|(index, url)| {
            if url.is_empty() || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
                return Err(Error::invalid(
                    format!("urls[{index}]"),
                    url,
                    "must be a non-empty URL without whitespace",
                ));
            }

            Ok(())
        })
    }
}

impl Check {
    fn validate(&self) -> Result<(), Error> {
        PACKAGE_NAME.check("name", &self.name)?;
        SANDBOX.check("sandbox", &self.sandbox)?;

        script(&self.script)?;
        dependencies(&self.dependencies)?;
        env(&self.env)
    }
}

fn script(value: &str) -> Result<(), Error> {
    if value.trim().is_empty() {
        return Err(Error::invalid("script", value, "must not be empty"));
    }

    if value.contains('\0') {
        return Err(Error::invalid("script", value, "must not contain NUL"));
    }

    Ok(())
}

fn dependencies(dependencies: &[Dependency]) -> Result<(), Error> {
    let mut seen = BTreeSet::new();
    dependencies
        .iter()
        .enumerate()
        .try_for_each(|(index, dependency)| {
            PACKAGE_NAME.check(&format!("dependencies[{index}].name"), &dependency.name)?;
            if !seen.insert((&dependency.key, dependency.kind)) {
                return Err(Error::invalid(
                    format!("dependencies[{index}]"),
                    dependency.key.as_str(),
                    "is listed twice with the same kind",
                ));
            }

            Ok(())
        })
}

fn env(env: &BTreeMap<String, String>) -> Result<(), Error> {
    env.iter().try_for_each(|(name, value)| {
        ENV_NAME.check("env", name)?;
        if SANDBOX_ENV.contains(&name.as_str()) {
            return Err(Error::invalid("env", name, "is set by the sandbox"));
        }

        if value.contains('\0') {
            return Err(Error::invalid(
                format!("env.{name}"),
                value,
                "must not contain NUL",
            ));
        }

        Ok(())
    })
}
