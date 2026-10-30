//! Provision only instance-owned resources, then release the SDK scope before execution.
use crate::{
    execution::{self, HttpExecutor},
    model::Credential,
};
use gateway_plugin_sdk::call::{
    middleware::http::Version,
    resources::{GroupEnsureRequest, GroupMembersChange, KeyEnsureRequest},
};
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
struct Member {
    id: String,
}
#[derive(Deserialize)]
struct Members {
    items: Vec<Member>,
    page: MemberPage,
}
#[derive(Deserialize)]
struct MemberPage {
    total: u64,
}
#[derive(Deserialize)]
struct GroupRef {
    id: String,
    enabled: bool,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct KeyInfo {
    id: String,
    enabled: bool,
    groups: Vec<GroupRef>,
    max_concurrency: u64,
    requests_per_minute: u64,
    daily_limit_usd: String,
    weekly_limit_usd: String,
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
fn verify_members(members: &Members, account: &str) -> Result<(), PluginFault> {
    if members.page.total != 1 || members.items.len() != 1 || members.items[0].id != account {
        return Err(invalid(
            "专用测试分组必须且只能包含目标账号；已拒绝执行，未改动其他成员",
        ));
    }
    Ok(())
}
fn verify_key(key: &KeyInfo, group: &str) -> Result<(), PluginFault> {
    let positive = |s: &str| s.parse::<f64>().is_ok_and(|n| n.is_finite() && n > 0.0);
    if !key.enabled
        || key.groups.len() != 1
        || key.groups[0].id != group
        || !key.groups[0].enabled
        || !(1..=16).contains(&key.max_concurrency)
        || !(1..=120).contains(&key.requests_per_minute)
        || !positive(&key.daily_limit_usd)
        || !positive(&key.weekly_limit_usd)
    {
        return Err(invalid(
            "专用测试 Key 的分组、启用状态或预算限制已变更；已拒绝借用业务 Key",
        ));
    }
    Ok(())
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
    let resource_key = format!("background-{}", credential.id);
    let group = host
        .ensure_group(GroupEnsureRequest {
            resource_key: resource_key.clone(),
            name: format!("糖果自检·{resource_key}"),
            color: "#D97706FF".into(),
            description: Some("插件自有单账号测试组；不替换账号原有分组。".into()),
        })
        .await?;
    if !group.enabled {
        return Err(invalid("专用测试分组已停用"));
    }
    host.change_group_members(GroupMembersChange {
        resource_key: resource_key.clone(),
        add: vec![credential.id.clone()],
        remove: vec![],
    })
    .await?;
    let members: Members = admin(
        host,
        query(
            "/api/admin/accounts",
            &[("groupId", &group.id), ("pageSize", "2")],
        ),
    )
    .await?;
    verify_members(&members, &credential.id)?;
    let key = host
        .ensure_key(KeyEnsureRequest {
            resource_key: resource_key.clone(),
            name: format!("糖果自检·{resource_key}"),
            group_resource_keys: vec![resource_key],
            max_concurrency: 16,
            requests_per_minute: 120,
            daily_limit_usd: "5".into(),
            weekly_limit_usd: "20".into(),
        })
        .await?;
    if !key.enabled {
        return Err(invalid("专用测试 Key 已停用"));
    }
    let mut cursor = String::new();
    loop {
        let mut params = vec![("search", key.name.as_str()), ("limit", "100")];
        if !cursor.is_empty() {
            params.push(("cursor", cursor.as_str()));
        }
        let page: Keys = admin(host, query("/api/admin/client-keys", &params)).await?;
        if let Some(info) = page.items.iter().find(|v| v.id == key.id) {
            verify_key(info, &group.id)?;
            break;
        }
        let Some(next) = page.next_cursor.filter(|next| next != &cursor) else {
            return Err(invalid("无法核验专用测试 Key 的权限"));
        };
        cursor = next;
    }
    let secret: Revealed = admin(
        host,
        query("/api/admin/client-keys/reveal", &[("id", &key.id)]),
    )
    .await?;
    if secret.id != key.id {
        return Err(invalid("专用测试 Key 身份不一致"));
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
    fn reject_wrong_or_multiple_accounts_including_truncated_page() {
        for data in [
            json!({"items":[],"page":{"total":0}}),
            json!({"items":[{"id":"other"}],"page":{"total":1}}),
            json!({"items":[{"id":"fixture"}],"page":{"total":2}}),
        ] {
            assert!(verify_members(&serde_json::from_value(data).unwrap(), "fixture").is_err());
        }
        assert!(verify_members(
            &Members {
                items: vec![Member {
                    id: "fixture".into()
                }],
                page: MemberPage { total: 1 }
            },
            "fixture"
        )
        .is_ok());
    }
    #[test]
    fn account_page_requires_official_nested_pagination() {
        // CPR 3.28 AccountPageData uses PageMeta: data.page.total, not data.total.
        let official = json!({"data":{"items":[{"id":"fixture"}],"page":{"page":1,"pageSize":2,"total":1,"totalPages":1}}});
        let page: Envelope<Members> = serde_json::from_value(official).unwrap();
        assert!(verify_members(&page.data, "fixture").is_ok());
        for malformed in [
            json!({"items":[{"id":"fixture"}],"total":1}),
            json!({"items":[{"id":"fixture"}],"page":{}}),
        ] {
            assert!(serde_json::from_value::<Members>(malformed).is_err());
        }
    }
    #[test]
    fn reject_unscoped_disabled_and_unlimited_keys() {
        let baseline = json!({"id":"fixture", "enabled":true,"groups":[{"id":"group","enabled":true}],"maxConcurrency":16,"requestsPerMinute":120,"dailyLimitUsd":"5","weeklyLimitUsd":"20"});
        assert!(verify_key(&serde_json::from_value(baseline.clone()).unwrap(), "group").is_ok());
        for (field, value) in [
            ("groups", json!([])),
            ("groups", json!([{"id":"other","enabled":true}])),
            ("enabled", json!(false)),
            ("dailyLimitUsd", json!("0")),
            ("weeklyLimitUsd", json!("NaN")),
            ("maxConcurrency", json!(0)),
            ("requestsPerMinute", json!(0)),
        ] {
            let mut data = baseline.clone();
            data[field] = value;
            assert!(verify_key(&serde_json::from_value(data).unwrap(), "group").is_err());
        }
    }
}
