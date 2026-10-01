use rootbeer_drv::Platform;
use rootbeer_eval::{Catalog, Host};
use std::collections::BTreeMap;
use std::fs;

#[test]
#[ignore = "needs RB_CATALOG"]
fn every_declared_target_evaluates() {
    let directory = std::env::var("RB_CATALOG").unwrap();
    let mut files = fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "lua"))
        .map(|path| {
            (
                path.display().to_string(),
                fs::read_to_string(&path).unwrap(),
            )
        })
        .collect::<Vec<_>>();

    files.sort();
    let catalog = Catalog::parse(
        files
            .iter()
            .map(|(file, source)| (file.as_str(), source.as_str())),
    )
    .unwrap();

    let host = Host {
        system: "builder".to_string(),
        rust: "1".to_string(),
    };

    let hosts = [
        Platform::Aarch64Macos,
        Platform::Aarch64Linux,
        Platform::X86_64Linux,
    ]
    .map(|platform| (platform, host.clone()))
    .into_iter()
    .collect::<BTreeMap<_, _>>();

    let failures = catalog
        .targets()
        .into_iter()
        .filter_map(|target| catalog.evaluate(&hosts, &[target]).err())
        .map(|error| error.to_string())
        .collect::<Vec<_>>();

    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
