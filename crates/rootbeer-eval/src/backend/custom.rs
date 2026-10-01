use super::Script;
use crate::recipe::Build;
use crate::template::Values;
use std::collections::BTreeMap;

pub(super) fn build(values: &Values, build: &Build) -> Result<Script, String> {
    let steps = build.steps.as_ref().ok_or("a custom build needs steps")?;
    let phases = [&steps.configure, &steps.build, &steps.check, &steps.install];

    let lines = phases
        .into_iter()
        .map(|phase| values.commands(phase))
        .collect::<Result<Vec<_>, String>>()?;

    Ok(Script {
        env: BTreeMap::new(),
        lines: lines.concat(),
    })
}
