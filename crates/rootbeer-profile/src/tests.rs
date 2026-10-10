use crate::{Error, Generation, Member, Profile, SCHEMA, bin};
use rootbeer_drv::Key;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

struct Fixture {
    _temporary: tempfile::TempDir,
    profiles: PathBuf,
    store: PathBuf,
}

fn fixture() -> Fixture {
    let temporary = tempfile::tempdir().unwrap();
    let profiles = temporary.path().join("profiles");
    let store = temporary.path().join("store");
    fs::create_dir_all(&profiles).unwrap();

    Fixture {
        profiles,
        store,
        _temporary: temporary,
    }
}

fn key(digit: char) -> Key {
    digit.to_string().repeat(32).parse().unwrap()
}

fn output(store: &Path, digit: char, bins: &[&str]) -> Member {
    let key = key(digit);
    let bin = store.join(key.as_str()).join("bin");
    fs::create_dir_all(&bin).unwrap();
    for name in bins {
        fs::write(bin.join(name), "").unwrap();
    }

    Member {
        version: "1.0".into(),
        is_pinned: false,
        key,
        manifest: format!("sha256:{}", "0".repeat(64)),
        bins: bins.iter().map(|name| name.to_string()).collect(),
        apps: BTreeMap::new(),
        extra: BTreeMap::new(),
    }
}

fn with_app(store: &Path, mut member: Member, app: &str) -> Member {
    let path = format!("Contents/{app}");
    let bundle = store.join(member.key.as_str()).join(&path);
    fs::create_dir_all(bundle.join("Contents/MacOS")).unwrap();
    member.apps.insert(app.into(), path);
    member
}

fn holding(packages: &BTreeMap<String, Member>) -> Generation {
    Generation {
        packages: packages.clone(),
        ..Generation::default()
    }
}

fn locate(store: &Path) -> impl Fn(&Key) -> Result<PathBuf, String> {
    let store = store.to_path_buf();
    move |key| Ok(store.join(key.as_str()))
}

#[test]
fn a_generation_links_its_bins_and_becomes_current() {
    let fixture = fixture();
    let profile = Profile::open(&fixture.profiles, "user").unwrap();
    assert_eq!(profile.current().unwrap(), None);

    let jq = output(&fixture.store, 'a', &["jq"]);
    let packages = BTreeMap::from([("jq".to_string(), jq)]);
    assert_eq!(
        profile.create(holding(&packages), locate(&fixture.store)),
        Ok(1)
    );

    let current = profile.current().unwrap().unwrap();
    assert_eq!((current.number, current.packages), (1, packages));

    let link = bin(&fixture.profiles, "user").join("jq");
    let target = fixture.store.join(key('a').as_str()).join("bin/jq");
    assert_eq!(fs::read_link(&link).unwrap(), target);

    let record = fs::read_to_string(fixture.profiles.join("user-1/generation.json")).unwrap();
    assert!(record.contains(&format!("\"schema\": {SCHEMA}")));
}

#[test]
fn rollback_goes_back_one_and_a_new_generation_still_numbers_after_the_newest() {
    let fixture = fixture();
    let profile = Profile::open(&fixture.profiles, "user").unwrap();
    let jq = output(&fixture.store, 'a', &["jq"]);
    let fd = output(&fixture.store, 'b', &["fd"]);

    let store = locate(&fixture.store);
    profile.create(Generation::default(), &store).unwrap();
    let one = BTreeMap::from([("jq".to_string(), jq)]);
    profile.create(holding(&one), &store).unwrap();
    let mut two = one.clone();
    two.insert("fd".into(), fd);
    profile.create(holding(&two), &store).unwrap();

    assert_eq!(profile.rollback(), Ok(2));
    assert_eq!(profile.current().unwrap().unwrap().packages, one);
    assert!(!bin(&fixture.profiles, "user").join("fd").exists());

    assert_eq!(profile.create(holding(&one), &store), Ok(4));
    assert_eq!(profile.rollback(), Ok(3));
    assert_eq!(profile.rollback(), Ok(2));
    assert_eq!(profile.rollback(), Ok(1));
    assert!(matches!(profile.rollback(), Err(Error::Refused(_))));

    assert_eq!(profile.generations().unwrap(), [1, 2, 3, 4]);
}

#[test]
fn a_broken_generation_can_still_be_rolled_back_from_and_listed() {
    let fixture = fixture();
    let profile = Profile::open(&fixture.profiles, "user").unwrap();
    let store = locate(&fixture.store);
    profile.create(Generation::default(), &store).unwrap();
    profile.create(Generation::default(), &store).unwrap();
    fs::remove_file(fixture.profiles.join("user-2/generation.json")).unwrap();

    assert!(matches!(profile.current(), Err(Error::Io { .. })));
    assert_eq!(profile.generations().unwrap(), [1, 2]);
    assert!(profile.read(1).is_ok());
    assert!(matches!(profile.read(2), Err(Error::Io { .. })));

    assert_eq!(profile.rollback(), Ok(1));
    assert_eq!(profile.current().unwrap().unwrap().number, 1);
}

#[test]
fn a_bin_two_packages_provide_or_one_missing_from_the_output_is_refused() {
    let fixture = fixture();
    let profile = Profile::open(&fixture.profiles, "user").unwrap();
    let busybox = output(&fixture.store, 'a', &["ls", "cat"]);
    let coreutils = output(&fixture.store, 'b', &["ls"]);

    let packages = BTreeMap::from([
        ("busybox".to_string(), busybox.clone()),
        ("coreutils".to_string(), coreutils),
    ]);
    let store = locate(&fixture.store);
    let error = profile.create(holding(&packages), &store).unwrap_err();
    assert_eq!(error.to_string(), "busybox and coreutils both provide ls");

    let mut lying = busybox;
    lying.bins.insert("sh".into());
    let packages = BTreeMap::from([("busybox".to_string(), lying)]);
    assert!(matches!(
        profile.create(holding(&packages), &store),
        Err(Error::Refused(reason)) if reason.starts_with("busybox has no ")
    ));

    for bin in ["..", "../../etc", "a\nb"] {
        let mut escaping = output(&fixture.store, 'c', &[]);
        escaping.bins.insert(bin.into());
        let packages = BTreeMap::from([("evil".to_string(), escaping)]);
        let error = profile.create(holding(&packages), &store).unwrap_err();
        let reason = format!("evil has a bin {bin:?} that isn't a plain name");
        assert_eq!(error, Error::Refused(reason));
    }

    assert_eq!(profile.current().unwrap(), None);
    assert!(profile.generations().unwrap().is_empty());
}

#[test]
fn a_newer_record_is_reported_before_anything_else_is_read() {
    let fixture = fixture();
    let profile = Profile::open(&fixture.profiles, "user").unwrap();
    profile
        .create(Generation::default(), locate(&fixture.store))
        .unwrap();

    let newer = SCHEMA + 1;
    let record = format!(r#"{{"schema": {newer}, "packages": "a shape this rb can't read"}}"#);
    fs::write(fixture.profiles.join("user-1/generation.json"), record).unwrap();
    assert!(matches!(
        profile.current(),
        Err(Error::Newer { schema, .. }) if schema == newer
    ));

    let record = r#"{"schema": 0, "packages": {}}"#;
    fs::write(fixture.profiles.join("user-1/generation.json"), record).unwrap();
    assert!(matches!(profile.current(), Err(Error::Invalid { .. })));
}

#[test]
fn fields_a_newer_rb_added_survive_a_rewrite() {
    let fixture = fixture();
    let profile = Profile::open(&fixture.profiles, "user").unwrap();
    let jq = output(&fixture.store, 'a', &["jq"]);
    let record = serde_json::json!({
        "schema": SCHEMA,
        "packages": { "jq": jq },
        "added": "later",
    });

    let mut record = record.to_string();
    record = record.replace(r#""key":"#, r#""origin":"cache","key":"#);
    fs::create_dir(fixture.profiles.join("user-1")).unwrap();
    fs::write(fixture.profiles.join("user-1/generation.json"), record).unwrap();
    std::os::unix::fs::symlink("user-1", fixture.profiles.join("user")).unwrap();

    let current = profile.current().unwrap().unwrap();
    profile.create(current, locate(&fixture.store)).unwrap();

    let written = fs::read_to_string(fixture.profiles.join("user-2/generation.json")).unwrap();
    let written: serde_json::Value = serde_json::from_str(&written).unwrap();
    assert_eq!(written["added"], "later");
    assert_eq!(written["packages"]["jq"]["origin"], "cache");
}

#[test]
fn a_profile_opened_to_read_refuses_changes() {
    let fixture = fixture();
    let profile = Profile::open_shared(&fixture.profiles, "user").unwrap();
    let error = profile
        .create(Generation::default(), locate(&fixture.store))
        .unwrap_err();

    assert_eq!(error, Error::Refused("user is open only to read".into()));
    assert!(matches!(profile.rollback(), Err(Error::Refused(_))));
}

#[test]
fn leftovers_of_an_interrupted_create_are_cleaned_and_never_counted() {
    let fixture = fixture();
    let profile = Profile::open(&fixture.profiles, "user").unwrap();
    fs::create_dir_all(fixture.profiles.join(".tmp-crashed/bin")).unwrap();
    fs::write(fixture.profiles.join(".tmp-file"), "").unwrap();
    fs::create_dir_all(fixture.profiles.join("user-07")).unwrap();
    fs::create_dir_all(fixture.profiles.join("default-9")).unwrap();

    assert_eq!(
        profile.create(Generation::default(), locate(&fixture.store)),
        Ok(1)
    );
    assert!(!fixture.profiles.join(".tmp-crashed").exists());
    assert!(!fixture.profiles.join(".tmp-file").exists());
}

#[test]
fn profile_names_never_collide_with_generations() {
    let fixture = fixture();
    for name in ["", "user-1", ".lock", "a/b"] {
        assert!(Profile::open(&fixture.profiles, name).is_err(), "{name:?}");
    }
}

impl PartialEq for Error {
    fn eq(&self, other: &Self) -> bool {
        self.to_string() == other.to_string()
    }
}

#[test]
fn apps_link_through_the_profile_and_follow_every_switch() {
    let fixture = fixture();
    let applications = fixture.profiles.join("../Applications");
    let profile = Profile::open(&fixture.profiles, "user").unwrap();
    let store = locate(&fixture.store);

    fs::create_dir_all(applications.join("Notes.app")).unwrap();
    std::os::unix::fs::symlink("/elsewhere/Other.app", applications.join("Other.app")).unwrap();

    let slack = with_app(
        &fixture.store,
        output(&fixture.store, 'a', &[]),
        "Slack.app",
    );
    let packages = BTreeMap::from([("slack".to_string(), slack)]);
    profile.create(holding(&packages), &store).unwrap();
    assert!(profile.link_apps(&applications).unwrap().is_empty());

    let farm = fixture.profiles.join("user/Applications/Slack.app");
    let link = applications.join("Slack.app");
    assert_eq!(fs::read_link(&link).unwrap(), farm);
    let bundle = fixture
        .store
        .join(key('a').as_str())
        .join("Contents/Slack.app");
    assert_eq!(fs::read_link(&farm).unwrap(), bundle);

    profile.create(Generation::default(), &store).unwrap();
    assert!(profile.link_apps(&applications).unwrap().is_empty());
    assert!(fs::symlink_metadata(&link).is_err());
    assert!(applications.join("Notes.app").is_dir());
    assert!(fs::symlink_metadata(applications.join("Other.app")).is_ok());

    profile.rollback().unwrap();
    assert!(profile.link_apps(&applications).unwrap().is_empty());
    assert_eq!(fs::read_link(&link).unwrap(), farm);
    assert!(profile.link_apps(&applications).unwrap().is_empty());
    assert_eq!(fs::read_link(&link).unwrap(), farm);

    fs::remove_dir(fixture.profiles.join("user-2/Applications")).unwrap();
    std::os::unix::fs::symlink("user-2", fixture.profiles.join(".user.link")).unwrap();
    fs::rename(
        fixture.profiles.join(".user.link"),
        fixture.profiles.join("user"),
    )
    .unwrap();
    assert!(profile.link_apps(&applications).unwrap().is_empty());
    assert!(fs::symlink_metadata(&link).is_err());
}

#[test]
fn a_link_to_an_app_under_another_name_is_replaced() {
    let fixture = fixture();
    let applications = fixture.profiles.join("../Applications");
    let profile = Profile::open(&fixture.profiles, "user").unwrap();
    let slack = with_app(
        &fixture.store,
        output(&fixture.store, 'a', &[]),
        "Slack.app",
    );
    let packages = BTreeMap::from([("slack".to_string(), slack)]);
    profile
        .create(holding(&packages), locate(&fixture.store))
        .unwrap();

    let farm = fixture.profiles.join("user/Applications");
    fs::create_dir_all(&applications).unwrap();
    std::os::unix::fs::symlink(farm.join("slack.app"), applications.join("slack.app")).unwrap();
    std::os::unix::fs::symlink(farm.join("Slack.app"), applications.join("Chat.app")).unwrap();
    profile
        .check_apps(&applications, ["slack.app", "Chat.app"])
        .unwrap();

    assert!(profile.link_apps(&applications).unwrap().is_empty());
    let mut names = fs::read_dir(&applications)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(names, ["Slack.app"]);
}

#[test]
fn an_app_something_else_holds_is_reported_and_left_alone() {
    let fixture = fixture();
    let applications = fixture.profiles.join("../Applications");
    let profile = Profile::open(&fixture.profiles, "user").unwrap();

    fs::create_dir_all(applications.join("Slack.app")).unwrap();
    let other = fixture.profiles.join("default/Applications/Zed.app");
    std::os::unix::fs::symlink(&other, applications.join("Zed.app")).unwrap();

    let taken = |path: &str| {
        Error::Refused(format!(
            "{} already exists",
            applications.join(path).display()
        ))
    };
    assert_eq!(
        profile.check_apps(&applications, ["Free.app", "Slack.app"]),
        Err(taken("Slack.app"))
    );
    assert_eq!(
        profile.check_apps(&applications, ["Zed.app"]),
        Err(taken("Zed.app"))
    );
    assert_eq!(profile.check_apps(&applications, ["Free.app"]), Ok(()));

    let slack = with_app(
        &fixture.store,
        output(&fixture.store, 'a', &[]),
        "Slack.app",
    );
    let packages = BTreeMap::from([("slack".to_string(), slack)]);
    profile
        .create(holding(&packages), locate(&fixture.store))
        .unwrap();

    assert_eq!(
        profile.link_apps(&applications).unwrap(),
        [taken("Slack.app")]
    );
    assert!(applications.join("Slack.app").is_dir());
    assert_eq!(fs::read_link(applications.join("Zed.app")).unwrap(), other);
}

#[test]
fn an_app_must_be_a_bundle_inside_its_output_and_provided_once() {
    let fixture = fixture();
    let profile = Profile::open(&fixture.profiles, "user").unwrap();
    let store = locate(&fixture.store);
    let slack = with_app(
        &fixture.store,
        output(&fixture.store, 'a', &[]),
        "Slack.app",
    );

    let refuse = |member: Member, reason: &str| {
        let packages = BTreeMap::from([("slack".to_string(), member)]);
        let error = profile.create(holding(&packages), &store).unwrap_err();
        assert_eq!(error, Error::Refused(reason.into()));
    };

    let mut renamed = slack.clone();
    renamed.apps = BTreeMap::from([("Slack".into(), "Contents/Slack.app".into())]);
    refuse(
        renamed,
        r#"slack has an app "Slack" that isn't a bundle name"#,
    );

    let mut escaping = slack.clone();
    escaping.apps = BTreeMap::from([("Slack.app".into(), "../Slack.app".into())]);
    refuse(
        escaping,
        r#"slack has an app at "../Slack.app" outside its output"#,
    );

    let output_path = fixture.store.join(key('a').as_str());
    fs::write(output_path.join("File.app"), "").unwrap();
    let mut file = slack.clone();
    file.apps = BTreeMap::from([("File.app".into(), "File.app".into())]);
    refuse(
        file,
        &format!(
            "slack's {} isn't a directory",
            output_path.join("File.app").display()
        ),
    );

    let mut hidden = slack.clone();
    hidden.apps = BTreeMap::from([(".Slack.app".into(), "Contents/Slack.app".into())]);
    refuse(
        hidden,
        r#"slack has an app ".Slack.app" that isn't a bundle name"#,
    );

    let fork = with_app(
        &fixture.store,
        output(&fixture.store, 'b', &[]),
        "slack.app",
    );
    let packages = BTreeMap::from([("fork".to_string(), fork), ("slack".to_string(), slack)]);
    let error = profile.create(holding(&packages), &store).unwrap_err();
    assert_eq!(
        error,
        Error::Refused("fork and slack both provide Slack.app".into())
    );
    assert!(profile.generations().unwrap().is_empty());
}
