use std::fmt;

/// A derivation field that fails validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub field: String,
    pub value: String,
    pub reason: &'static str,
}

impl Error {
    pub(crate) fn invalid(field: impl Into<String>, value: &str, reason: &'static str) -> Self {
        Error {
            field: field.into(),
            value: value.to_string(),
            reason,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid {} {:?}: {}",
            self.field, self.value, self.reason
        )
    }
}

impl std::error::Error for Error {}
