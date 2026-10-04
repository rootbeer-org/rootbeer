use rootbeer_drv::{Build, Key};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub(crate) const ARTIFACT: &str = "application/vnd.rootbeer.output.v1";
pub(crate) const CONFIG: &str = "application/vnd.rootbeer.derivation.v1+json";
pub(crate) const LAYER: &str = "application/vnd.rootbeer.output.v1.tar+zstd";

pub(crate) const KEY: &str = "com.rbpkg.key";
pub(crate) const NAME: &str = "com.rbpkg.name";
pub(crate) const VERSION: &str = "com.rbpkg.version";
pub(crate) const OUTPUT: &str = "com.rbpkg.output";
pub(crate) const REFERENCES: &str = "com.rbpkg.references";

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Manifest {
    pub schema_version: u32,
    pub media_type: String,
    pub artifact_type: String,
    pub config: Descriptor,
    pub layers: Vec<Descriptor>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Descriptor {
    pub media_type: String,
    pub digest: String,
    pub size: u64,
}

impl Manifest {
    pub(crate) fn output(
        key: &Key,
        build: &Build,
        references: &BTreeSet<Key>,
        config: Descriptor,
        layer: Descriptor,
    ) -> Manifest {
        let references = references.iter().map(Key::as_str).collect::<Vec<_>>();
        let annotations = BTreeMap::from([
            (KEY.to_string(), key.to_string()),
            (NAME.to_string(), build.name.clone()),
            (VERSION.to_string(), build.version.clone()),
            (OUTPUT.to_string(), "out".to_string()),
            (REFERENCES.to_string(), references.join(",")),
        ]);

        Manifest {
            schema_version: 2,
            media_type: MANIFEST.into(),
            artifact_type: ARTIFACT.into(),
            config,
            layers: vec![layer],
            annotations,
        }
    }
}
