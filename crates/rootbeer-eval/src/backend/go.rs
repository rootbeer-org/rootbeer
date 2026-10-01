use super::{Script, env, lines};
use crate::recipe::{Bins, Build};
use crate::template::Values;
use std::collections::BTreeSet;

// Both steps ignore user and workspace config and never download a toolchain.
const MODULES: [(&str, &str); 4] = [
    ("GO111MODULE", "on"),
    ("GOENV", "off"),
    ("GOTOOLCHAIN", "local"),
    ("GOWORK", "off"),
];

pub(super) fn build(values: &Values, build: &Build, bins: Option<&Bins>) -> Result<Script, String> {
    let go = build.go.as_ref().ok_or("a go build needs go settings")?;
    let declared = BTreeSet::from_iter(bins.map(Bins::names).unwrap_or_default());
    if go.binaries.keys().map(String::as_str).ne(declared) {
        return Err("go binaries must match outputs.bins".into());
    }

    let mut variables = env(&MODULES);
    variables.extend(env(&[
        ("CGO_ENABLED", if go.is_cgo_enabled { "1" } else { "0" }),
        ("GOFLAGS", "-mod=vendor"),
        ("GOPROXY", "off"),
        ("GOSUMDB", "off"),
        ("GOVCS", "*:off"),
    ]));

    if !go.experiments.is_empty() {
        variables.insert("GOEXPERIMENT".into(), go.experiments.join(","));
    }

    let tags = match go.tags.is_empty() {
        true => Vec::new(),
        false => vec!["-tags".to_string(), go.tags.join(",")],
    };

    let mut flags = ["-trimpath", "-buildvcs=false", "-p", "{jobs}"]
        .map(String::from)
        .to_vec();

    flags.extend(tags.iter().cloned());
    if !go.variables.is_empty() {
        let ldflags = go
            .variables
            .iter()
            .map(|(name, value)| Ok(format!("-X '{name}={}'", values.literal(value)?)))
            .collect::<Result<Vec<_>, String>>()?;

        flags.extend(["-ldflags".into(), ldflags.join(" ")]);
    }

    let flags = values.command(&flags)?;
    let mut script = lines(&[
        "rm -rf vendor",
        r#"cp -R "${vendor}" vendor"#,
        r#"export GOPATH="${TMPDIR}/go" GOCACHE="${TMPDIR}/go-cache""#,
    ]);

    if !go.generate.is_empty() {
        // Build tags pick the files whose directives run.
        let packages = values.command(&[tags.as_slice(), &go.generate].concat())?;
        script.push(format!("go generate {packages}"));
    }

    script.push(r#"mkdir -p "${out}/bin""#.into());
    for (bin, package) in &go.binaries {
        let output = format!("{{prefix}}/bin/{bin}");
        let output = values.command(&["-o", &output, package])?;
        script.push(format!("go build {flags} {output}"));
    }

    Ok(Script {
        env: variables,
        lines: script,
    })
}

pub(super) fn vendor() -> Script {
    let mut variables = env(&MODULES);
    variables.extend(env(&[
        ("GOFLAGS", "-modcacherw"),
        ("GOPROXY", "https://proxy.golang.org"),
        ("GOSUMDB", "sum.golang.org"),
    ]));

    Script {
        env: variables,
        lines: lines(&[
            r#"export GOPATH="${TMPDIR}/go" GOMODCACHE="${TMPDIR}/go-modules""#,
            r#"cp go.mod go.sum "${TMPDIR}""#,
            "mkdir -p vendor",
            "go mod vendor",
            r#"cmp go.mod "${TMPDIR}/go.mod""#,
            r#"cmp go.sum "${TMPDIR}/go.sum""#,
            "go mod verify",
            r#"cp -R vendor "${out}""#,
        ]),
    }
}
