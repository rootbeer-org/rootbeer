//! Package qualification and publication, independent of configuration execution.

pub use rootbeer_build::{
    audit, build_package, engine_generation, verify_environment, BuildCache, BuildEnvironment,
    BuildOptions, BuildPlan, Generation,
};
pub use rootbeer_package::*;
mod built;
pub use built::BuiltDependencies;
mod checks;
mod prepare;
pub use prepare::prepare_package;
mod package_plan;
mod published;
pub use published::PublishedDependencies;
mod publish_records;
mod receipt;
mod release;
pub mod selection;
pub use package_plan::{plan_packages, PackageTask};
pub use publish_records::publish_records;
pub use release::{push_package, release_package, Signer};

mod sign;
pub use sign::sign_package_record;

pub mod upstream;
pub mod work;
pub use upstream::{discover_updates, UpdateReport};

#[cfg(test)]
#[path = "../../rootbeer-package/tests/support/catalog.rs"]
mod test_catalog;

#[cfg(test)]
#[path = "../../../scripts/cache_inputs.rs"]
mod cache_inputs;
