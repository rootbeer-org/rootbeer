use super::{Context, Phase};
use rootbeer_package::GoBuild;
use std::collections::BTreeSet;
use std::path::PathBuf;

/// Prefers the Go distribution an exact catalog dependency provides over the pinned host toolchain.
fn toolchain(context: &Context<'_>) -> Result<(PathBuf, Option<PathBuf>), String> {
    let catalog = context.tools.join("go");
    if catalog.is_file() {
        let go = catalog.canonicalize().map_err(|error| error.to_string())?;
        let root = go
            .parent()
            .and_then(|bin| bin.parent())
            .filter(|root| root.join("pkg/tool").is_dir())
            .ok_or("catalog go must sit in the bin directory of a Go distribution")?
            .to_path_buf();
        return Ok((go, Some(root)));
    }

    let go = context.host_tools.join("go");
    if !go.is_file() {
        return Err("Go builds require a pinned toolchain providing go".into());
    }
    Ok((go, None))
}

pub(super) fn plan(options: &GoBuild, context: &Context<'_>) -> Result<Vec<Phase>, String> {
    let (go, root) = toolchain(context)?;
    let goroot = root.map(|root| format!("GOROOT={}", root.display()));
    if options.binaries.keys().collect::<BTreeSet<_>>()
        != context.bins.iter().collect::<BTreeSet<_>>()
    {
        return Err("Go entry points must match exported binaries".into());
    }
    let go = go.to_string_lossy().into_owned();
    let command = |action: &str| {
        let mut command: Vec<String> = vec!["/usr/bin/env".into()];
        command.extend(goroot.clone());
        command.extend([
            "GOPROXY=off".into(),
            "GOSUMDB=off".into(),
            go.clone(),
            action.into(),
            "-mod=vendor".into(),
            "-trimpath".into(),
            "-buildvcs=false".into(),
            "-p".into(),
            context.jobs.to_string(),
        ]);
        if !options.tags.is_empty() {
            command.extend(["-tags".into(), options.tags.join(",")]);
        }
        if !options.variables.is_empty() {
            let flags = options
                .variables
                .iter()
                .map(|(name, value)| format!("-X '{name}={value}'"))
                .collect::<Vec<_>>()
                .join(" ");
            command.extend(["-ldflags".into(), flags]);
        }
        command
    };
    let bin_directory = context.prefix.join("bin");
    let mut build = vec![vec![
        "mkdir".into(),
        "-p".into(),
        context
            .workspace
            .join("go-bin")
            .to_string_lossy()
            .into_owned(),
    ]];
    if !options.generate.is_empty() {
        let mut generate = command("generate");
        generate.insert(1, "GOFLAGS=-mod=vendor".into());
        generate.extend(options.generate.clone());
        build.push(generate);
    }
    let mut install = vec![vec![
        "mkdir".into(),
        "-p".into(),
        bin_directory.to_string_lossy().into_owned(),
    ]];
    for (bin, package) in &options.binaries {
        let output = context
            .workspace
            .join("go-bin")
            .join(bin)
            .to_string_lossy()
            .into_owned();
        let mut arguments = command("build");
        arguments.extend(["-o".into(), output.clone(), package.clone()]);
        build.push(arguments);
        install.push(vec![
            "cp".into(),
            output,
            bin_directory.join(bin).to_string_lossy().into_owned(),
        ]);
    }
    let mut fetch: Vec<String> = vec!["/usr/bin/env".into()];
    fetch.extend(goroot);
    fetch.extend([
        format!("GOMODCACHE={}", context.workspace.join("go-modules").display()), "GOFLAGS=-modcacherw".into(), "GOPROXY=https://proxy.golang.org".into(),
        "GOSUMDB=sum.golang.org".into(), "sh".into(), "-ec".into(),
        "test -f go.mod && test -f go.sum || { echo 'Go builds require go.mod and go.sum' >&2; exit 1; }; mkdir -p \"$GOMODCACHE/cache\" \"$3\"; ln -s \"$3\" \"$GOMODCACHE/cache/download\"; cp go.mod \"$2/go.mod\"; cp go.sum \"$2/go.sum\"; \"$1\" mod vendor; cmp go.mod \"$2/go.mod\"; cmp go.sum \"$2/go.sum\"; \"$1\" mod verify".into(),
        "go-vendor".into(), go, context.workspace.to_string_lossy().into_owned(), context.downloads.join("go/cache/download").to_string_lossy().into_owned(),
    ]);
    Ok(vec![
        Phase {
            name: "fetch",
            requires_network: true,
            commands: vec![fetch],
        },
        Phase {
            name: "build",
            requires_network: false,
            commands: build,
        },
        Phase {
            name: "install",
            requires_network: false,
            commands: install,
        },
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::BTreeMap, fs, process::Command, time::Duration};

    #[test]
    fn builds_a_module_with_pinned_tools_and_rejects_missing_sums() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace with spaces");
        let source = workspace.join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(
            source.join("go.mod"),
            "module example.com/probe\n\ngo 1.23.0\n",
        )
        .unwrap();
        fs::write(source.join("go.sum"), "").unwrap();
        fs::write(source.join("main.go"), "package main\nimport \"fmt\"\nvar version string\nfunc main() { fmt.Println(version) }\n").unwrap();
        fs::create_dir(source.join("gen")).unwrap();
        fs::write(
            source.join("generate.go"),
            "package main\n//go:generate go run ./gen\n",
        )
        .unwrap();
        fs::write(source.join("gen/main.go"), r#"package main
import "os"
func main() { if err := os.WriteFile("generated.go", []byte("package main\nconst generated = true\n"), 0600); err != nil { panic(err) } }
"#).unwrap();
        let environment = crate::environment::Environment::resolve(None)
            .unwrap()
            .with_go()
            .unwrap();
        let tools = workspace.join("tools");
        environment.stage(&tools).unwrap();
        let prefix = workspace.join("prefix");
        let downloads = workspace.join("downloads");
        let mut variables = environment.variables(&tools, &tools, &workspace);
        variables.extend([
            (
                "GOROOT",
                environment.lock.inputs["go-toolchain"]
                    .path
                    .to_string_lossy()
                    .into_owned(),
            ),
            ("GOTOOLCHAIN", "local".into()),
            ("GOENV", "off".into()),
            ("GOWORK", "off".into()),
            ("CGO_ENABLED", "0".into()),
            (
                "GOCACHE",
                workspace.join("cache").to_string_lossy().into_owned(),
            ),
        ]);
        let bins = vec!["probe".into()];
        let runtime = BTreeMap::new();
        let context = Context {
            prefix: &prefix,
            downloads: &downloads,
            host_tools: &tools,
            bins: &bins,
            dependencies: &workspace,
            tools: &tools,
            workspace: &workspace,
            jobs: 2,
            runtime: &runtime,
        };
        let options = GoBuild {
            binaries: BTreeMap::from([("probe".into(), ".".into())]),
            generate: vec![".".into()],
            variables: BTreeMap::from([("main.version".into(), "pinned version".into())]),
            ..Default::default()
        };
        let phases = plan(&options, &context).unwrap();
        assert!(phases[0].requires_network);
        assert!(phases[1..].iter().all(|phase| !phase.requires_network));
        for phase in phases {
            for command in phase.commands {
                crate::run(
                    &command,
                    &source,
                    &variables,
                    &workspace.join("build.log"),
                    Duration::from_secs(120),
                )
                .unwrap();
            }
        }
        assert!(source.join("generated.go").is_file());
        let output = Command::new(prefix.join("bin/probe")).output().unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"pinned version\n");
        fs::remove_file(source.join("go.sum")).unwrap();
        let fetch = plan(&options, &context).unwrap().remove(0);
        assert!(crate::run(
            &fetch.commands[0],
            &source,
            &variables,
            &workspace.join("missing-sums.log"),
            Duration::from_secs(10)
        )
        .is_err());
    }

    #[test]
    fn prefers_a_catalog_distribution_over_the_host_toolchain() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().canonicalize().unwrap();
        let host = crate::environment::Environment::resolve(None)
            .unwrap()
            .with_go()
            .unwrap();
        let distribution = host.lock.inputs["go-toolchain"].path.clone();
        let tools = workspace.join("tools");
        let host_tools = workspace.join("host-tools");
        fs::create_dir_all(&tools).unwrap();
        fs::create_dir_all(&host_tools).unwrap();
        std::os::unix::fs::symlink(distribution.join("bin/go"), tools.join("go")).unwrap();
        let bins = vec!["probe".into()];
        let runtime = BTreeMap::new();
        let context = Context {
            prefix: &workspace,
            downloads: &workspace,
            host_tools: &host_tools,
            bins: &bins,
            dependencies: &workspace,
            tools: &tools,
            workspace: &workspace,
            jobs: 1,
            runtime: &runtime,
        };
        let options = GoBuild {
            binaries: BTreeMap::from([("probe".into(), ".".into())]),
            ..Default::default()
        };

        let phases = plan(&options, &context).unwrap();
        let goroot = format!("GOROOT={}", distribution.canonicalize().unwrap().display());
        let go = distribution.join("bin/go").canonicalize().unwrap();
        for command in phases[..2].iter().flat_map(|phase| &phase.commands) {
            if command[0] != "/usr/bin/env" {
                continue;
            }
            assert_eq!(command[1], goroot);
            assert!(command.contains(&go.to_string_lossy().into_owned()));
        }

        fs::remove_file(tools.join("go")).unwrap();
        fs::write(tools.join("go"), "").unwrap();
        assert!(plan(&options, &context).is_err());
    }
}
