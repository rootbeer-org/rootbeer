use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

/// A package the configuration asks rb to install into its `default`
/// profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageRequest {
    /// The signed index's package, at its default version unless pinned
    Index {
        name: String,
        version: Option<String>,
    },
    /// A GitHub release's asset, at the latest release unless a tag is pinned
    Release {
        repository: String,
        tag: Option<String>,
        asset: Option<String>,
        bins: BTreeMap<String, PathBuf>,
    },
}

impl PackageRequest {
    /// Parses `name[@version]` or `github:owner/repo[@tag]`, as `rb use`
    /// takes them.
    pub fn parse(request: &str) -> Result<Self, String> {
        let (name, version) = match request.split_once('@') {
            Some((name, version)) => (name, Some(version.to_string())),
            None => (request, None),
        };

        if name.is_empty() || version.as_deref().is_some_and(str::is_empty) {
            return Err(format!("{request} isn't name[@version]"));
        }

        if let Some(repository) = name.strip_prefix("github:") {
            return Ok(PackageRequest::Release {
                repository: repository.to_string(),
                tag: version,
                asset: None,
                bins: BTreeMap::new(),
            });
        }

        if name.contains(':') {
            return Err(format!(
                "{request} isn't in the signed index or a GitHub release, the only places \
                 rb installs from"
            ));
        }

        Ok(PackageRequest::Index {
            name: name.to_string(),
            version,
        })
    }
}

impl fmt::Display for PackageRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (name, version) = match self {
            PackageRequest::Index { name, version } => (name.clone(), version),
            PackageRequest::Release {
                repository, tag, ..
            } => (format!("github:{repository}"), tag),
        };

        match version {
            Some(version) => write!(f, "{name}@{version}"),
            None => write!(f, "{name}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_name_the_index_or_a_github_release() {
        let index = PackageRequest::parse("jq@1.8.1").unwrap();
        assert_eq!(
            index,
            PackageRequest::Index {
                name: "jq".into(),
                version: Some("1.8.1".into()),
            }
        );
        assert_eq!(index.to_string(), "jq@1.8.1");

        let release = PackageRequest::parse("github:owner/repo").unwrap();
        assert!(matches!(
            &release,
            PackageRequest::Release { repository, tag: None, .. } if repository == "owner/repo"
        ));
        assert_eq!(release.to_string(), "github:owner/repo");

        for request in ["", "jq@", "@1", "brew:jq", "cargo:ripgrep@14"] {
            assert!(PackageRequest::parse(request).is_err(), "{request}");
        }
    }
}
