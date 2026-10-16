use rootbeer_store::{Error, Layout, LAYOUT, ROOT};
use rustix::process::{getgid, getgroups, getuid};
use std::fs;
use std::io::{self, IsTerminal};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Sets /opt/rb up before a command uses it, and again when this rb's helper
/// is newer. A store from a newer rb hands the command to that rb. Root uses
/// the store directly, so it never needs the helper.
pub(crate) fn ensure() -> Result<(), String> {
    if is_current()? || getuid().is_root() {
        return Ok(());
    }

    let helper = helper()?;
    let group = trusted_group()?;
    let command = format!("sudo {} setup {group}", helper.display());
    if !io::stdin().is_terminal() {
        return Err(format!("{ROOT} needs setting up, so run `{command}`"));
    }

    eprintln!("rb needs sudo to set up {ROOT} for the {group} group:\n  {command}");
    let status = Command::new("sudo")
        .arg(&helper)
        .args(["setup", &group])
        .status()
        .map_err(|error| format!("sudo: {error}"))?;

    if !status.success() {
        return Err(format!("`{command}` {status}"));
    }

    match is_current()? {
        true => Ok(()),
        false => Err(format!("`{command}` didn't set up {ROOT} for this rb")),
    }
}

/// A layout this rb can't read is a newer rb's, so that rb runs instead.
fn is_current() -> Result<bool, String> {
    let found = match Layout::read(Path::new(ROOT)) {
        Ok(found) => found,
        Err(Error::Refused(reason)) => return Err(newer(reason)),
        Err(error) => return Err(error.to_string()),
    };

    if let Some(found) = found.filter(|found| found.layout > LAYOUT) {
        return Err(newer(format!(
            "{ROOT} has layout {}, newer than this rb understands ({LAYOUT})",
            found.layout
        )));
    }

    Ok(Layout::is_current(found))
}

fn newer(reason: String) -> String {
    crate::newer::relaunch(&reason);
    reason
}

/// The helper shipped beside this rb, which is the one its store layout needs.
/// An rb installed without one can still hand the command to a newer rb.
fn helper() -> Result<PathBuf, String> {
    let rb = std::env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|error| format!("finding rb: {error}"))?;

    let helper = rb.with_file_name("rb-helper");
    match helper.is_file() {
        true => Ok(helper),
        false => Err(newer(format!(
            "{} is missing, so this rb can't set up {ROOT}",
            helper.display()
        ))),
    }
}

/// Who may add to the store. Setting it up again keeps the group it has, and
/// otherwise it's the admin group on macOS, and elsewhere whichever of sudo or
/// wheel the caller is in.
fn trusted_group() -> Result<String, String> {
    let file = fs::read_to_string("/etc/group").map_err(|error| format!("/etc/group: {error}"))?;
    let existing = fs::metadata(Path::new(ROOT).join("store"))
        .ok()
        .map(|store| store.gid())
        .filter(|gid| *gid != 0)
        .and_then(|gid| group_name(&file, gid));
    if let Some(group) = existing {
        return Ok(group.to_string());
    }

    if cfg!(target_os = "macos") {
        return Ok("admin".into());
    }

    let mut groups = getgroups().map_err(|error| format!("reading your groups: {error}"))?;
    groups.push(getgid());
    let member = ["sudo", "wheel"].into_iter().find(|name| {
        group_id(&file, name).is_some_and(|gid| groups.iter().any(|g| g.as_raw() == gid))
    });

    member.map(str::to_string).ok_or_else(|| {
        let helper = helper().map_or("rb-helper".into(), |helper| helper.display().to_string());
        format!(
            "you aren't in the sudo or wheel group, so set the store up with \
             `sudo {helper} setup <group>` for a group you trust"
        )
    })
}

fn groups(file: &str) -> impl Iterator<Item = (&str, u32)> {
    let lines = file.lines().filter(|line| !line.starts_with('#'));
    lines.filter_map(|line| {
        let mut fields = line.split(':');
        let name = fields.next()?;
        let gid = fields.nth(1)?.parse().ok()?;
        Some((name, gid))
    })
}

fn group_id(file: &str, name: &str) -> Option<u32> {
    groups(file)
        .find(|(named, _)| *named == name)
        .map(|(_, gid)| gid)
}

fn group_name(file: &str, gid: u32) -> Option<&str> {
    groups(file)
        .find(|(_, id)| *id == gid)
        .map(|(name, _)| name)
}

#[cfg(test)]
mod tests {
    #[test]
    fn group_ids_come_from_the_named_line() {
        let file = "#sudo:x:1:\nwheel:x:10:tale\nsudo:x:27:tale\n";
        assert_eq!(super::group_id(file, "sudo"), Some(27));
        assert_eq!(super::group_id(file, "wheel"), Some(10));
        assert_eq!(super::group_id(file, "admin"), None);
        assert_eq!(super::group_name(file, 27), Some("sudo"));
        assert_eq!(super::group_name(file, 1), None);
    }
}
