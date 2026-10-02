use super::Script;
use crate::recipe::Build;
use crate::template::Values;
use std::collections::BTreeMap;

pub(super) fn build(values: &Values, build: &Build) -> Result<Script, String> {
    let configure = ["sh", "./configure", "--prefix={prefix}"].map(String::from);

    Ok(Script {
        env: BTreeMap::new(),
        lines: vec![
            values.command(&[configure.as_slice(), &build.configure].concat())?,
            values.command(&["make", "-j{jobs}"])?,
            values.command(&["make", "check"])?,
            values.command(&["make", "install"])?,
        ],
    })
}
