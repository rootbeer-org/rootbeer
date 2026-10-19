use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;

/// Installs a package from the signed index for rb's own use and returns its
/// commands' absolute paths, keeping it from GC while rb runs.
pub type Install = fn(&str, &str, bool) -> io::Result<BTreeMap<String, PathBuf>>;

#[derive(Debug, Default)]
pub(crate) struct ToolRuntime {
    install: Option<Install>,
    is_offline: bool,
    packages: Mutex<BTreeMap<(String, String), BTreeMap<String, PathBuf>>>,
}

impl ToolRuntime {
    pub(crate) fn new(install: Option<Install>, is_offline: bool) -> Self {
        Self {
            install,
            is_offline,
            ..Self::default()
        }
    }

    pub(crate) fn command(&self, name: &str, version: &str, bin: &str) -> io::Result<Command> {
        self.command_with(name, version, bin, |name, version, is_offline| {
            let install = self
                .install
                .ok_or_else(|| io::Error::other("only rb can install it"))?;
            install(name, version, is_offline)
        })
    }

    fn command_with(
        &self,
        name: &str,
        version: &str,
        bin: &str,
        install: impl FnOnce(&str, &str, bool) -> io::Result<BTreeMap<String, PathBuf>>,
    ) -> io::Result<Command> {
        let key = (name.to_string(), version.to_string());
        let mut packages = self
            .packages
            .lock()
            .map_err(|_| io::Error::other("tool runtime lock poisoned"))?;
        let bins = match packages.entry(key) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(
                install(name, version, self.is_offline)
                    .map_err(|error| io::Error::other(format!("{name}@{version}: {error}")))?,
            ),
        };

        let path = bins
            .get(bin)
            .ok_or_else(|| io::Error::other(format!("{name}@{version} has no command {bin}")))?;
        if !path.is_absolute() {
            return Err(io::Error::other(format!(
                "{name}@{version}'s {bin} isn't an absolute path"
            )));
        }
        Ok(Command::new(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    #[test]
    fn shares_package_exports_and_separates_versions() {
        let tools = ToolRuntime::new(None, true);
        let count = Cell::new(0);
        let install = |_: &str, _: &str, is_offline: bool| {
            assert!(is_offline);
            count.set(count.get() + 1);
            Ok(BTreeMap::from([
                ("first".into(), PathBuf::from("/store/first")),
                ("second".into(), PathBuf::from("/store/second")),
            ]))
        };
        for bin in ["first", "second", "first"] {
            let command = tools.command_with("tool", "1", bin, install).unwrap();
            assert_eq!(
                command.get_program(),
                PathBuf::from(format!("/store/{bin}"))
            );
        }
        assert_eq!(count.get(), 1);
        tools.command_with("tool", "2", "first", install).unwrap();
        assert_eq!(count.get(), 2);
        assert!(tools.command_with("tool", "1", "missing", install).is_err());
        assert_eq!(count.get(), 2);
    }

    #[test]
    fn retries_failed_installs_and_rejects_relative_executables() {
        let tools = ToolRuntime::default();
        let error = tools
            .command_with("example", "1", "tool", |_, _, _| {
                Err(io::Error::other("offline"))
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "example@1: offline");
        assert!(tools
            .command_with("example", "1", "tool", |_, _, _| {
                Ok(BTreeMap::from([("tool".into(), PathBuf::from("relative"))]))
            })
            .is_err());
        assert!(tools.command("example", "1", "tool").is_err());
    }

    #[test]
    fn providers_share_managed_tool_across_lua_and_apply() {
        let root = tempfile::tempdir().unwrap();
        crate::age::tests::fixture(root.path(), b"decrypted", false);
        let executable = root.path().join("provider-tool");
        fs::write(&executable, format!(
            "#!/bin/sh\nif [ \"$3\" = op://vault/item/identity ]; then /bin/cat '{}'; else printf '%s' managed-value; fi\n",
            root.path().join("key.txt").display()
        )).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let tools = Arc::new(ToolRuntime::new(None, true));
        tools
            .command_with("op", "2.39.0", "op", |_, _, _| {
                Ok(BTreeMap::from([("op".into(), executable)]))
            })
            .unwrap();
        let vm = crate::lua::Vm::new(crate::Runtime {
            tools: tools.clone(),
            script_dir: root.path().into(),
            script_name: "test.lua".into(),
            lua_dir: crate::lua_dir(),
            profile: None,
        })
        .unwrap();
        vm.exec(r#"
            local rb = require("rootbeer")
            rb.file("read", rb.secret.op("op://vault/item/value"))
            rb.secret.op_document("document", "document")
            rb.secret.age_file("secret.age", "decrypted", { identity_op = "op://vault/item/identity" })
        "#, "test").unwrap();
        let ops = vm.drain_ops();
        struct Handler;
        impl crate::ExecutionHandler for Handler {
            fn on_start(&mut self, _: &crate::Op) {}
            fn on_output(&mut self, _: &str) {}
            fn on_result(&mut self, _: &crate::OpResult) {}
        }
        crate::executor::apply_with_tools(&tools, &ops, false, &mut Handler).unwrap();
        assert_eq!(
            fs::read(root.path().join("read")).unwrap(),
            b"managed-value"
        );
        assert_eq!(
            fs::read(root.path().join("document")).unwrap(),
            b"managed-value"
        );
        assert_eq!(
            fs::read(root.path().join("decrypted")).unwrap(),
            b"decrypted"
        );
        assert_eq!(tools.packages.lock().unwrap().len(), 1);
    }
}
