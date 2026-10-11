use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;

use crate::package::{standalone, PackageRequest};

#[derive(Debug, Default)]
pub(crate) struct ToolRuntime {
    is_offline: bool,
    packages: Mutex<BTreeMap<Vec<u8>, BTreeMap<String, PathBuf>>>,
}

impl ToolRuntime {
    pub(crate) fn new(is_offline: bool) -> Self {
        Self {
            is_offline,
            ..Self::default()
        }
    }

    pub(crate) fn command(&self, request: &PackageRequest, bin: &str) -> io::Result<Command> {
        self.command_with(request, bin, |request, is_offline| {
            let environment =
                standalone::prepare(std::slice::from_ref(request), false, is_offline, false)
                    .map_err(io::Error::other)?;
            let package = environment
                .packages
                .into_iter()
                .next()
                .ok_or_else(|| io::Error::other("tool package was not realized"))?;
            Ok(package.bins)
        })
    }

    fn command_with(
        &self,
        request: &PackageRequest,
        bin: &str,
        prepare: impl FnOnce(&PackageRequest, bool) -> io::Result<BTreeMap<String, PathBuf>>,
    ) -> io::Result<Command> {
        let key = serde_json::to_vec(request)?;
        let mut packages = self
            .packages
            .lock()
            .map_err(|_| io::Error::other("tool runtime lock poisoned"))?;
        if !packages.contains_key(&key) {
            let bins = prepare(request, self.is_offline)
                .map_err(|error| io::Error::other(format!("runtime tool {request}: {error}")))?;
            packages.insert(key.clone(), bins);
        }

        let path = packages[&key].get(bin).ok_or_else(|| {
            io::Error::other(format!("runtime tool {request} does not export {bin}"))
        })?;
        if !path.is_absolute() {
            return Err(io::Error::other(format!(
                "runtime tool {request} returned a relative executable path"
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
        let tools = ToolRuntime::new(true);
        let count = Cell::new(0);
        let request = PackageRequest::parse("github:example/tool@v1");
        let prepare = |_: &PackageRequest, is_offline: bool| {
            assert!(is_offline);
            count.set(count.get() + 1);
            Ok(BTreeMap::from([
                ("first".into(), PathBuf::from("/store/first")),
                ("second".into(), PathBuf::from("/store/second")),
            ]))
        };
        for bin in ["first", "second", "first"] {
            let command = tools.command_with(&request, bin, prepare).unwrap();
            assert_eq!(
                command.get_program(),
                PathBuf::from(format!("/store/{bin}"))
            );
        }
        assert_eq!(count.get(), 1);
        tools
            .command_with(&request.clone().version("v2"), "first", prepare)
            .unwrap();
        assert_eq!(count.get(), 2);
        assert!(tools.command_with(&request, "missing", prepare).is_err());
        assert_eq!(count.get(), 2);
    }

    #[test]
    fn retries_failed_preparation_and_rejects_relative_executables() {
        let tools = ToolRuntime::default();
        let request = PackageRequest::new("example");
        let error = tools
            .command_with(&request, "tool", |_, _| Err(io::Error::other("uncached")))
            .unwrap_err();
        assert!(error.to_string().contains("runtime tool example: uncached"));
        assert!(tools
            .command_with(&request, "tool", |_, _| {
                Ok(BTreeMap::from([("tool".into(), PathBuf::from("relative"))]))
            })
            .is_err());
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
        let tools = Arc::new(ToolRuntime::new(true));
        let request = PackageRequest::parse("op@2.39.0");
        tools
            .command_with(&request, "op", |_, _| {
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
        crate::executor::apply_with_options(
            &tools,
            &ops,
            false,
            &mut Handler,
            crate::executor::ApplyOptions {
                package_offline: true,
                ..Default::default()
            },
        )
        .unwrap();
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

#[cfg(test)]
mod integration_tests {
    #[test]
    #[ignore = "requires op@2.39.0 in the local standalone cache"]
    fn cached_provider_runtime_uses_packaged_op() {
        let tools = super::ToolRuntime::new(true);
        let mut command = crate::one_password::command(&tools).unwrap();
        assert!(std::path::Path::new(command.get_program()).is_absolute());
        let output = command.arg("--version").env("PATH", "").output().unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "2.39.0");
    }
}
