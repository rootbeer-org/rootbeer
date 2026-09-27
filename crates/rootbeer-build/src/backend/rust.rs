use super::{Context, Phase};
use rootbeer_package::RustBuild;

pub(super) fn plan(options: &RustBuild, context: &Context<'_>) -> Result<Vec<Phase>, String> {
    for tool in ["cargo", "rustc"] {
        if !context.host_tools.join(tool).is_file() {
            return Err(format!(
                "Rust builds require a pinned toolchain providing {tool}"
            ));
        }
    }
    if context.bins.is_empty() {
        return Err("Rust builds require exported binaries".into());
    }
    let cargo = context
        .host_tools
        .join("cargo")
        .to_string_lossy()
        .into_owned();
    let vendor = context.workspace.join("vendor");
    let config = context.workspace.join("vendor.toml");
    let mut selection = Vec::new();
    for package in &options.packages {
        selection.extend(["--package".into(), package.clone()]);
    }
    if !options.features.is_empty() {
        selection.extend(["--features".into(), options.features.join(",")]);
    }
    if options.no_default_features {
        selection.push("--no-default-features".into());
    }
    let command = |action: &str| {
        let mut command = vec![
            cargo.clone(),
            action.into(),
            "--frozen".into(),
            "--release".into(),
            "--jobs".into(),
            context.jobs.to_string(),
            "--config".into(),
            config.to_string_lossy().into_owned(),
        ];
        command.extend(selection.clone());
        command
    };
    let mut build = command("build");
    for bin in context.bins {
        build.extend(["--bin".into(), bin.clone()]);
    }
    let bin_directory = context.prefix.join("bin");
    let mut install = vec![vec![
        "mkdir".into(),
        "-p".into(),
        bin_directory.to_string_lossy().into_owned(),
    ]];
    for bin in context.bins {
        install.push(vec![
            "cp".into(),
            context
                .workspace
                .join("cargo-target/release")
                .join(bin)
                .to_string_lossy()
                .into_owned(),
            bin_directory.join(bin).to_string_lossy().into_owned(),
        ]);
    }
    Ok(vec![
        Phase {
            name: "fetch",
            requires_network: true,
            commands: vec![vec![
                "/usr/bin/env".into(),
                format!("CARGO_HOME={}", context.downloads.join("cargo").display()),
                "sh".into(), "-ec".into(),
                "test -f Cargo.lock || { echo 'Rust builds require Cargo.lock' >&2; exit 1; }; \"$1\" vendor --locked --versioned-dirs \"$2\" > \"$3\"".into(),
                "cargo-vendor".into(), cargo, vendor.to_string_lossy().into_owned(), config.to_string_lossy().into_owned(),
            ]],
        },
        Phase { name: "build", requires_network: false, commands: vec![build] },
        Phase { name: "install", requires_network: false, commands: install },
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::BTreeMap, fs, process::Command, time::Duration};

    #[test]
    fn builds_and_installs_a_locked_workspace_without_host_cargo_state() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("workspace with spaces");
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"backend-probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(
            root.join("Cargo.lock"),
            "version = 4\n[[package]]\nname = \"backend-probe\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        fs::write(
            root.join("src/main.rs"),
            "fn main() { println!(\"{}\", env!(\"PROBE_VALUE\")); }\n",
        )
        .unwrap();
        let environment = crate::environment::Environment::resolve(None)
            .unwrap()
            .with_rust()
            .unwrap();
        let tools = root.join("tools");
        environment.stage(&tools).unwrap();
        let prefix = root.join("prefix");
        let downloads = root.join("downloads");
        let mut variables = environment.variables(&tools, &tools, &root);
        variables.insert(
            "CARGO_HOME",
            root.join("cargo-home").to_string_lossy().into_owned(),
        );
        variables.insert(
            "CARGO_TARGET_DIR",
            root.join("cargo-target").to_string_lossy().into_owned(),
        );
        variables.insert(
            "RUSTC",
            environment.lock.tools["rustc"]
                .path
                .to_string_lossy()
                .into_owned(),
        );
        variables.insert("PROBE_VALUE", "configured".into());
        let bins = vec!["backend-probe".into()];
        let options = RustBuild {
            packages: vec!["backend-probe".into()],
            ..Default::default()
        };
        let runtime = BTreeMap::new();
        let context = Context {
            prefix: &prefix,
            downloads: &downloads,
            host_tools: &tools,
            bins: &bins,
            dependencies: &root,
            tools: &tools,
            workspace: &root,
            jobs: 2,
            runtime: &runtime,
        };
        let phases = plan(&options, &context).unwrap();
        assert!(phases[0].requires_network);
        assert!(phases[1..].iter().all(|phase| !phase.requires_network));
        for phase in phases {
            for command in phase.commands {
                crate::run(
                    &command,
                    &root,
                    &variables,
                    &root.join("build.log"),
                    Duration::from_secs(60),
                )
                .unwrap();
            }
        }
        let output = Command::new(prefix.join("bin/backend-probe"))
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"configured\n");
        fs::remove_file(root.join("Cargo.lock")).unwrap();
        let fetch = plan(&options, &context).unwrap().remove(0);
        assert!(crate::run(
            &fetch.commands[0],
            &root,
            &variables,
            &root.join("missing-lock.log"),
            Duration::from_secs(60)
        )
        .is_err());
    }
}
