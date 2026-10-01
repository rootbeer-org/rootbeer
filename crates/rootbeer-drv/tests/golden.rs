//! Pinned keys. A failure here means the encoding changed, which rekeys every
//! package ever published. Fix the code, not these constants, unless the change
//! is a deliberate new encoding version.
//!
//! The expected keys were computed independently of this crate (Python's json,
//! hashlib, and base64) to confirm the encoding matches its specification.

use rootbeer_drv::Derivation;

const GOLDEN: [(&str, &str, &str); 3] = [
    (
        "fetch-zlib",
        include_str!("fixtures/fetch-zlib.json"),
        "7gp4ws33c64skpue226mhhk3irsahogn",
    ),
    (
        "build-zlib",
        include_str!("fixtures/build-zlib.json"),
        "xxjp3tkegc5rodgmm34vggrqae7ev3tm",
    ),
    (
        "check-zlib",
        include_str!("fixtures/check-zlib.json"),
        "z4sxveypp32jz4pifs7pxxmonz7s2odi",
    ),
];

#[test]
fn fixtures_have_pinned_keys() {
    for (name, source, expected) in GOLDEN {
        let derivation: Derivation = serde_json::from_str(source).unwrap();
        assert_eq!(derivation.key().unwrap().as_str(), expected, "{name}");
    }
}
