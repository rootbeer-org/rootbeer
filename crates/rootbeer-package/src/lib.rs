//! Package definitions, resolution, and verified installation.

pub use rootbeer_store_legacy as store;
pub use rootbeer_store_legacy::{deterministic, state_dir};
mod artifact;
mod build_spec;
pub mod catalog;
pub mod definition;
pub mod distribution;
mod dmg;
pub mod download;
mod execution;
pub use execution::Execution;
pub mod ghcr;
pub mod github;
pub mod graph;
mod inputs;
mod intent;
pub mod pdr;
pub mod progress;
pub mod realize;
pub mod repository;
mod resolve;
pub mod self_update;
mod source;
mod spec;
pub use source::{GitSource, SourceBuildProof, SourceSelection};
pub mod staging;
pub mod upstream;

pub use artifact::PublishedArtifact;
pub use build_spec::{
    BuildArtifact, BuildBackend, BuildDependency, BuildEnvironmentInput, BuildEnvironmentLock,
    BuildSteps, DependencyKind, GoBuild, RustBuild, SourceBuild,
};
pub use catalog::{
    Bins, CatalogPackage, CatalogProof, CatalogRecipe, CatalogVersion, ExtraFields, PackageCatalog,
};
pub use definition::{PackageDefinition, PackageUpstream, UpstreamProvider};
pub use github::GitHubResolver;
pub use inputs::{GitHubRepositoryPin, PackageResolverInputs, ResolverInput};
pub use intent::{PackageIntent, PackageLockInput};
pub use realize::{PackageRealizer, RealizedPackage};
pub use repository::{
    PackageRecordProof, Repository, RepositoryPin, RepositoryResolver, Selection,
};
pub use resolve::{
    ArtifactProof, DependencyProof, ExternalManagerProof, GitReleaseProof, MetadataClosureProof,
    MetadataDocumentProof, PackageRequest, PackageRequestResolver, PackageResolution,
    PackageResolutionInput, PackageResolver, ResolutionProof, ResolveAttempt, ResolveContext,
    ResolveError, ResolverStack, SnapshotProof, SnapshotSource,
};
pub use rootbeer_catalog::is_sha256;
pub use spec::{
    ArchiveFormat, LockedInstall, LockedPackage, LockedSource, PackageRealizationInput, Provides,
};

pub fn default_resolver_stack() -> ResolverStack {
    resolver_stack_for_inputs(&PackageResolverInputs::default())
}

pub fn resolver_stack_for_inputs(inputs: &PackageResolverInputs) -> ResolverStack {
    let mut stack = backend_stack().with_implicit_resolver("rootbeer");
    if let Some(catalog) = inputs.local_catalog() {
        stack.push(catalog::CatalogResolver::new(
            catalog,
            inputs,
            backend_stack(),
        ));
    } else if let Some(pin) = inputs.repository() {
        stack.push(RepositoryResolver::new(pin));
    }
    stack
}

pub fn backend_stack() -> ResolverStack {
    let mut stack = ResolverStack::new();
    stack.push(GitHubResolver::new());
    stack
}

pub mod lockfile;

pub mod runtime;

#[cfg(test)]
extern crate self as rootbeer_package;

#[cfg(test)]
#[path = "../tests/support/catalog.rs"]
mod test_catalog;
