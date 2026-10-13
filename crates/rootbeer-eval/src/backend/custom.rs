use super::Script;
use crate::recipe::Build;

pub(super) fn build(build: &Build) -> Result<Script, String> {
    build.steps.as_ref().ok_or("a custom build needs steps")?;
    Ok(Script::default())
}
