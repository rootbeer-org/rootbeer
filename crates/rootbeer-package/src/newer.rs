//! Process-wide notice that a catalog or saved state needs a newer rb, so the
//! CLI can run one in its place instead of failing.

use std::sync::OnceLock;

static OBSERVER: OnceLock<fn(&str)> = OnceLock::new();

/// Registers the process-wide receiver for [`reached`]. It may replace the
/// process with a newer rb, so a caller reports as soon as it finds the
/// condition.
pub fn observe(observer: fn(&str)) {
    let _ = OBSERVER.set(observer);
}

pub(crate) fn reached(reason: &str) {
    if let Some(observer) = OBSERVER.get() {
        observer(reason);
    }
}
