//! Codex 降智与指纹测试插件（CPR 原生移植版）。
//!
//! 移植自 cpa-plugin-codex-candy-eval：糖果降智测试、模型指纹对比、深度追踪与鹈鹕绘图测试。

pub mod app;
mod background;
mod background_transport;
pub mod candy;
pub mod codex;
pub mod command;
pub mod execution;
pub mod fingerprint;
pub mod host_calls;
mod hourly;
pub mod management;
pub mod manifest;
pub mod model;
pub mod modeltrace;
pub mod pelican;
pub mod runs;
pub mod state;
pub mod storage;

pub use app::{plugin, plugin_with_config};
pub use manifest::{manifest, PLUGIN_ID};

/// 打包进插件包的前端静态资源；清单与注册表必须保持一致。
pub const RESOURCES: &[&str] = &[
    // 页面入口。
    "web/ui.html",
    // 样式：宿主主题 token、组件样式与离线公式渲染。
    "web/theme.css",
    "web/style.css",
    "web/vendor/temml.css",
    // 从样式表里抽出的内联资源；插件沙箱不接受 data: URL。
    "web/vendor/temml-fc70501355ae.woff2",
    "web/vendor/temml-d065b0c621f6.svg",
    "web/assets/bg-98826c21ff06.svg",
    // JavaScript 在构建时内联进入口 HTML，与 CPA 原版交付方式一致。
    // 不注册独立脚本资源，避免宿主预取时命中客户端的指纹跟踪过滤。
];

/// 插件页面入口，必须同时出现在 [`RESOURCES`] 中。
pub const PAGE_ENTRY: &str = "web/ui.html";

/// 模型不支持所选凭证时的跳过提示。
pub const UNSUPPORTED_MODEL_MESSAGE: &str = "该凭证不支持所选模型";

/// 凭证集合的稳定指纹，用于控制台判断是否需要重新拉取目录。
pub fn revision(credentials: &[model::Credential]) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    for credential in credentials {
        hasher.update(credential.id.as_bytes());
        hasher.update([0]);
        hasher.update(credential.provider.as_bytes());
        hasher.update([0]);
        hasher.update([u8::from(credential.disabled)]);
        hasher.update([1]);
    }
    hex::encode(&hasher.finalize()[..8])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credential(id: &str, disabled: bool) -> model::Credential {
        model::Credential {
            id: id.to_owned(),
            provider: "codex".to_owned(),
            source: "auth_files".to_owned(),
            name: id.to_owned(),
            base_url: String::new(),
            provider_name: String::new(),
            email: String::new(),
            plan_type: String::new(),
            disabled,
            unavailable: false,
            status_message: String::new(),
        }
    }

    #[test]
    fn revision_changes_with_credentials() {
        let base = revision(&[credential("a", false)]);
        assert_eq!(base, revision(&[credential("a", false)]));
        assert_ne!(base, revision(&[credential("a", true)]));
        assert_ne!(base, revision(&[credential("b", false)]));
    }

    #[test]
    fn page_entry_is_a_registered_resource() {
        assert!(RESOURCES.contains(&PAGE_ENTRY));
    }

    #[test]
    fn resources_are_unique() {
        let mut sorted = RESOURCES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), RESOURCES.len());
    }
}
