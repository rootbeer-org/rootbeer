fn main() {
    println!("cargo:rerun-if-env-changed=RB_SOURCE_REVISION");
    println!("cargo:rerun-if-env-changed=RB_SERIAL");

    // Builds that name no source commit say so rather than embedding the clock,
    // which would make every build of the same source differ.
    let revision = std::env::var("RB_SOURCE_REVISION")
        .map(|revision| revision.chars().take(12).collect::<String>())
        .unwrap_or_else(|_| "dev".into());
    println!("cargo:rustc-env=RB_SOURCE_REVISION={revision}");

    // Release CI checks the serial in the version before signing a release.
    let package = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let version = match std::env::var("RB_SERIAL") {
        Ok(serial) => format!("{package} ({revision}, serial {serial})"),
        Err(_) => format!("{package} ({revision})"),
    };
    println!("cargo:rustc-env=RB_VERSION={version}");
}
