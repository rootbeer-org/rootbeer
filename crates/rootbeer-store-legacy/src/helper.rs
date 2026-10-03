//! Versions of the setuid `rb-store` helper built from this source. The release
//! and the stream protocols move independently.

use crate::layout::Helper;
use crate::stream;

/// Bumped whenever the helper's code changes.
pub const RELEASE: u32 = 2;

/// Raised only for fixes every machine must take; forces a sudo reinstall.
pub const MINIMUM_RELEASE: u32 = 2;

/// Stream formats this helper reads. Older ones stay so older `rb`s keep working.
pub const PROTOCOLS: &[u32] = &[stream::PROTOCOL];

pub fn current() -> Helper {
    Helper {
        release: RELEASE,
        protocols: PROTOCOLS.to_vec(),
    }
}

/// Whether an installed helper serves this build as is. A newer one does too:
/// helpers are never downgraded.
pub fn is_sufficient(installed: Option<&Helper>) -> bool {
    installed.is_some_and(|helper| {
        helper.release >= MINIMUM_RELEASE && helper.protocols.contains(&stream::PROTOCOL)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_the_minimum_release_and_this_protocol() {
        let helper = |release, protocols: &[u32]| Helper {
            release,
            protocols: protocols.to_vec(),
        };

        assert!(!is_sufficient(None));
        assert!(is_sufficient(Some(&current())));
        assert!(is_sufficient(Some(&helper(
            RELEASE + 5,
            &[stream::PROTOCOL, 99]
        ))));
        assert!(!is_sufficient(Some(&helper(
            MINIMUM_RELEASE - 1,
            PROTOCOLS
        ))));
        assert!(!is_sufficient(Some(&helper(RELEASE, &[99]))));
    }
}
