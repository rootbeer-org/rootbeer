use rootbeer_tree::{Kind, Reader};
use std::fs::File;
use std::process::{Command, Stdio};

fn rb_as(tool: &str, input: File, scratch: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_rb"))
        .env_clear()
        .env("RB_TOOL", tool)
        .env("TMPDIR", scratch)
        .stdin(input)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap()
}

// The sandbox runs rb itself on untrusted input, so rb started as a tool
// serves it before reading any command.
#[test]
fn rb_started_as_a_tool_writes_the_tree_an_archive_holds() {
    let directory = tempfile::tempdir().unwrap();
    let archive = directory.path().join("tool.tar.gz");
    let encoder =
        flate2::write::GzEncoder::new(File::create(&archive).unwrap(), flate2::Compression::fast());
    let mut builder = tar::Builder::new(encoder);
    let mut header = tar::Header::new_gnu();
    header.set_size(2);
    header.set_mode(0o755);
    builder
        .append_data(&mut header, "tool/run", &b"#!"[..])
        .unwrap();
    builder.into_inner().unwrap().finish().unwrap();

    let output = rb_as(
        "unpack tar.gz",
        File::open(&archive).unwrap(),
        directory.path(),
    );
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{said}");

    let mut reader = Reader::new(output.stdout.as_slice()).unwrap();
    let mut entries = Vec::new();
    while let Some(entry) = reader.entry().unwrap() {
        entries.push((String::from_utf8(entry.path).unwrap(), entry.kind));
    }
    reader.finish().unwrap();

    let run = Kind::File {
        is_executable: true,
        attributes: Vec::new(),
        size: 2,
    };
    assert_eq!(
        entries,
        [
            ("tool".to_string(), Kind::Directory),
            ("tool/run".to_string(), run),
        ]
    );

    let output = rb_as("build", File::open(&archive).unwrap(), directory.path());
    assert!(!output.status.success());
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(said.contains("names no tool"), "{said}");
}
