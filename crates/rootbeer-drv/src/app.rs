use serde::{Deserialize, Serialize};

/// How an app bundle is installed. Neither setting reaches a key, since both
/// describe the installed copy rather than what the build produces.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppSettings {
    #[serde(default, skip_serializing_if = "is_default")]
    pub updates: Updates,
    #[serde(default, skip_serializing_if = "is_default")]
    pub location: Location,
}

/// Who replaces an installed app once a newer one exists.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Updates {
    /// rb replaces the copy whenever the profile selects a different bundle.
    #[default]
    Rootbeer,
    /// rb installs the app once, then the app's own updater takes over.
    #[serde(rename = "self")]
    Itself,
}

/// Where an app is installed. macOS only activates system extensions for
/// apps in `/Applications`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Location {
    /// `~/Applications`
    #[default]
    User,
    /// `/Applications`
    System,
}

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}
