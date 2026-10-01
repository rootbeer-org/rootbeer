//! Pinned keys. A failure here means the encoding changed, which rekeys every
//! package ever published. The constants here should NEVER change unless we are
//! cutting a new encoding version.

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
        "izkadmkrosdwrss76rud7k52ufmp632g",
    ),
    (
        "check-zlib",
        include_str!("fixtures/check-zlib.json"),
        "znh3warqgvo5nqz5kg5joqeug6lglknv",
    ),
];

#[test]
fn fixtures_have_pinned_keys() {
    for (name, source, expected) in GOLDEN {
        let derivation: Derivation = serde_json::from_str(source).unwrap();
        assert_eq!(derivation.key().unwrap().as_str(), expected, "{name}");
    }
}
