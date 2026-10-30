use codex_proxy_plugin_candy_eval::{manifest, plugin, PLUGIN_ID};
use gateway_plugin_sdk::{Capability, MANIFEST_VERSION};

#[test]
fn checked_in_manifest_is_the_complete_author_contract() {
    let manifest = manifest().unwrap();
    assert_eq!(manifest.plugin_id().unwrap(), PLUGIN_ID);
    assert_eq!(manifest.manifest_version, MANIFEST_VERSION);
    assert!(manifest.contributes.contains_key(&Capability::Management));
    assert!(manifest.contributes.contains_key(&Capability::CommandLine));
}

#[test]
fn capabilities_and_typed_handlers_do_not_drift() {
    plugin().unwrap();
}
