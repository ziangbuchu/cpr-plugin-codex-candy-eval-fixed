//! Use one administrator-selected CPR Client Key, then release the SDK scope before execution.
use crate::{
    execution::{self, HttpExecutor},
    model::Credential,
};
use gateway_plugin_sdk::call::middleware::http::Version;
use gateway_plugin_sdk::{
    client::{HostClient, HttpBody, HttpFrame, HttpRequest},
    ErrorCode, PluginFault,
};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::Value;

fn invalid(message: &str) -> PluginFault {
    PluginFault::new(ErrorCode::Conflict, message)
}
#[derive(Deserialize)]
struct Envelope<T> {
    data: T,
}
#[derive(Deserialize)]
struct GroupRef {
    enabled: bool,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct KeyInfo {
    id: String,
    #[serde(default)]
    name: String,
    enabled: bool,
    #[serde(default)]
    groups: Vec<GroupRef>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Keys {
    items: Vec<KeyInfo>,
    next_cursor: Option<String>,
}
// Never implement Debug/Serialize on the reveal response.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Revealed {
    id: String,
    plaintext_key: String,
}

async fn admin<T: DeserializeOwned>(host: &HostClient, path: String) -> Result<T, PluginFault> {
    let mut response = host
        .dispatch_http(HttpRequest {
            settings: Value::Null,
            method: "GET".into(),
            uri: path,
            version: Version::Http11,
            headers: vec![],
            timeout_ms: Some(10_000),
            body: HttpBody::empty(),
        })
        .await
        .map_err(|_| invalid("CPR 原生管理接口调用失败"))?;
    if response.status != 200 {
        return Err(invalid("CPR 原生管理接口拒绝请求"));
    }
    let mut bytes = Vec::new();
    while let Some(frame) = response.body.read().await? {
        if let HttpFrame::Data(data) = frame {
            if bytes.len().saturating_add(data.len()) > 512 * 1024 {
                return Err(invalid("CPR 管理响应超过大小上限"));
            }
            bytes.extend_from_slice(&data);
        }
    }
    serde_json::from_slice::<Envelope<T>>(&bytes)
        .map(|v| v.data)
        .map_err(|_| invalid("CPR 原生管理响应格式不兼容"))
}
fn query(path: &str, params: &[(&str, &str)]) -> String {
    let parameters =
        reqwest::Url::parse_with_params("http://127.0.0.1/", params).expect("fixed URL");
    format!("{path}?{}", parameters.query().unwrap_or_default())
}
fn configured_selector(config: &Value) -> Result<(&str, &str), PluginFault> {
    let id = config
        .get("test_key_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or_default();
    let name = config
        .get("test_key_name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or_default();
    if id.is_empty() && name.is_empty() {
        return Err(invalid(
            "请在插件配置中填写已有测试 Key ID 或名称；插件不会自动创建分组和 Key",
        ));
    }
    if !id.is_empty() && !name.is_empty() {
        return Err(invalid("测试 Key ID 和名称只能填写一个"));
    }
    Ok((id, name))
}

async fn find_key(host: &HostClient, config: &Value) -> Result<KeyInfo, PluginFault> {
    let (wanted_id, wanted_name) = configured_selector(config)?;
    let mut cursor = String::new();
    let mut matches = Vec::new();
    loop {
        let mut params = vec![("limit", "100")];
        if !cursor.is_empty() {
            params.push(("cursor", cursor.as_str()));
        }
        let page: Keys = admin(host, query("/api/admin/client-keys", &params)).await?;
        matches.extend(page.items.into_iter().filter(|key| {
            (!wanted_id.is_empty() && key.id == wanted_id)
                || (!wanted_name.is_empty() && key.name == wanted_name)
        }));
        let Some(next) = page.next_cursor.filter(|next| next != &cursor) else {
            break;
        };
        cursor = next;
    }
    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => Err(invalid("找不到配置的测试 Key，请检查 Key ID 或名称")),
        _ => Err(invalid("测试 Key 名称不唯一，请改用 Key ID")),
    }
}

pub async fn prepare(
    host: &HostClient,
    credential: &Credential,
    config: &Value,
) -> Result<HttpExecutor, PluginFault> {
    let endpoint = execution::endpoint(
        config
            .get("api_base_url")
            .and_then(Value::as_str)
            .unwrap_or("http://127.0.0.1:8080"),
    )?;
    let key = find_key(host, config).await?;
    if !key.enabled {
        return Err(invalid("配置的测试 Key 已停用"));
    }
    if key.groups.is_empty() || key.groups.iter().any(|group| !group.enabled) {
        return Err(invalid(
            "配置的测试 Key 未绑定有效分组，请检查其账号访问范围",
        ));
    }
    let secret: Revealed = admin(
        host,
        query("/api/admin/client-keys/reveal", &[("id", &key.id)]),
    )
    .await?;
    if secret.id != key.id {
        return Err(invalid("配置的测试 Key 身份不一致"));
    }
    HttpExecutor::new(
        endpoint,
        credential.id.clone(),
        credential.provider.clone(),
        secret.plaintext_key,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn requires_one_existing_key_selector() {
        assert_eq!(
            configured_selector(&json!({"test_key_id":" key_1 "})).unwrap(),
            ("key_1", "")
        );
        assert_eq!(
            configured_selector(&json!({"test_key_name":" shared test "})).unwrap(),
            ("", "shared test")
        );
        assert!(configured_selector(&json!({})).is_err());
        assert!(
            configured_selector(&json!({"test_key_id":"key_1", "test_key_name":"shared"})).is_err()
        );
    }
    #[test]
    fn key_info_accepts_admin_list_shape() {
        let key: KeyInfo = serde_json::from_value(json!({
            "id":"key_1", "name":"shared test", "enabled":true,
            "groups":[{"id":"all", "enabled":true}]
        }))
        .unwrap();
        assert_eq!(key.id, "key_1");
        assert_eq!(key.name, "shared test");
        assert!(key.enabled);
    }
}
