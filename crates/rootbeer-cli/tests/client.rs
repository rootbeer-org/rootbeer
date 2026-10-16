use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn rb(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rb"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("ROOTBEER_ROOT", home.join("legacy"))
        .args(args)
        .output()
        .unwrap()
}

fn uid() -> String {
    rustix::process::getuid().as_raw().to_string()
}

#[test]
fn env_keeps_the_old_profiles_behind_the_new_ones_until_migrated() {
    let home = tempfile::tempdir().unwrap();
    let profiles = format!("/opt/rb/profiles/{}", uid());
    let env = |home: &Path| {
        let output = rb(home, &["env"]);
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap()
    };

    let current = env(home.path());
    assert_eq!(current.lines().count(), 2);
    assert!(current.contains(&format!("'{profiles}/default/bin'")));
    assert!(current
        .lines()
        .last()
        .unwrap()
        .contains(&format!("'{profiles}/user/bin'")));

    let roots = home.path().join("legacy/var/roots").join(uid());
    fs::create_dir_all(&roots).unwrap();
    fs::write(roots.join("user"), "").unwrap();
    let held = env(home.path());
    let lines: Vec<_> = held.lines().collect();
    assert_eq!(lines.len(), 4);
    assert!(lines[0].contains("/state/rootbeer/profiles/default/current/bin'"));
    assert!(lines[1].contains("/state/rootbeer/profiles/user/current/bin'"));
    assert!(lines[3].contains(&format!("'{profiles}/user/bin'")));

    // Clients from before roots were recorded leave only their profiles.
    fs::remove_file(roots.join("user")).unwrap();
    let state = home.path().join("state/rootbeer");
    fs::create_dir_all(state.join("profiles/default/current")).unwrap();
    assert_eq!(env(home.path()), held);

    fs::write(state.join("migrated"), "").unwrap();
    assert_eq!(env(home.path()), current);
}

#[test]
fn a_newer_lock_stops_apply_before_the_configuration_runs() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config/rootbeer");
    fs::create_dir_all(&config).unwrap();
    fs::write(config.join("init.lua"), r#"error("evaluated")"#).unwrap();
    fs::write(
        config.join("rootbeer.lock"),
        r#"{"schema":99,"packages":{}}"#,
    )
    .unwrap();

    for args in [&["apply"][..], &["apply", "--dry-run"]] {
        let output = rb(home.path(), args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert!(
            stderr.contains("was written by a newer rb (schema 99)"),
            "{stderr}"
        );
        assert!(!stderr.contains("evaluated"), "{stderr}");
    }
}
