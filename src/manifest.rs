use gateway_plugin_sdk::{Manifest, ManifestError};

pub const PLUGIN_ID: &str = "nmbb.codex-candy-eval";

pub fn manifest() -> Result<Manifest, ManifestError> {
    Manifest::from_author_slice(include_bytes!("../plugin.json"))
}
