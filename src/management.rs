//! 管理端路由：CPR 账号/模型适配层、状态查询与四套自检的启动接口。
//!
//! 前端沿用 cpa-plugin-codex-candy-eval 的 Web 控制台，因此这里同时提供两类路由：
//! * `v0/management/*`、`v1/models`：原控制台调用的 CPA 管理接口形状，由本插件用 CPR 宿主数据重建。
//! * `api/*`：插件自有路由，承载状态、测试启动与结果清理。

use std::collections::BTreeMap;
use std::sync::Arc;

use gateway_plugin_sdk::{
    call::management::{
        ManagementRegistration, ManagementResource, ManagementResponse, ManagementRoute,
    },
    client::{HostClient, TypedCall, TypedReply},
    PluginFault,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    candy, fingerprint, host_calls, modeltrace, pelican, state::StateManager,
    UNSUPPORTED_MODEL_MESSAGE,
};

pub const API_VERSION: &str = "1.0.0";

pub fn registration() -> ManagementRegistration {
    fn route(method: &str, path: &str, body: bool) -> ManagementRoute {
        ManagementRoute {
            method: method.to_owned(),
            path: path.to_owned(),
            request_content_types: if body {
                vec!["application/json".to_owned()]
            } else {
                Vec::new()
            },
            response_content_types: vec!["application/json".to_owned()],
        }
    }
    let json = |method: &str, path: &str| route(method, path, false);
    let json_body = |method: &str, path: &str| route(method, path, true);

    ManagementRegistration {
        routes: vec![
            // 原控制台使用的 CPA 管理接口形状。
            json("GET", "api/accounts"),
            json("GET", "api/models"),
            json("GET", "api/accounts/models"),
            json("GET", "v0/management/config"),
            json("GET", "v8/management/config/api-keys"),
            json("GET", "v1/models"),
            json("GET", "v0/management/auth-files/models"),
            json_body("PATCH", "v0/management/auth-files/status"),
            // 插件自有路由。
            json_body("POST", "api/credentials/sync"),
            json("GET", "api/state"),
            json_body("POST", "api/storage/recover"),
            json_body("POST", "api/run"),
            json_body("POST", "api/cancel"),
            json("DELETE", "api/results"),
            json_body("POST", "api/fingerprint/run"),
            json_body("POST", "api/fingerprint/cancel"),
            json("GET", "api/fingerprint/record"),
            json("DELETE", "api/fingerprint/results"),
            json_body("POST", "api/modeltrace/run"),
            json_body("POST", "api/modeltrace/cancel"),
            json("GET", "api/modeltrace/record"),
            json("DELETE", "api/modeltrace/results"),
            json_body("POST", "api/pelican/run"),
            json_body("POST", "api/pelican/cancel"),
            json("GET", "api/pelican/record"),
            json("DELETE", "api/pelican/results"),
            json("DELETE", "api/all/results"),
            json_body("POST", "api/inspection"),
            json("GET", "api/inspection"),
        ],
        resources: crate::RESOURCES
            .iter()
            .map(|path| ManagementResource {
                path: (*path).to_owned(),
                public: path.ends_with(".css") || path.ends_with(".js"),
            })
            .collect(),
        pages: vec![gateway_plugin_sdk::call::management::ManagementPage {
            id: "codex-candy-eval".to_owned(),
            title: "Codex 降智与指纹测试".to_owned(),
            description: Some("Codex 账号降智检测、模型指纹对比与深度追踪套件".to_owned()),
            entry: crate::PAGE_ENTRY.to_owned(),
            icon: None,
        }],
        callbacks: Vec::new(),
    }
}

// ------------------------------------------------------------ 请求体

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RunRequest {
    #[serde(default)]
    auth_ids: Vec<String>,
    #[serde(default)]
    all: bool,
    #[serde(default)]
    pub(crate) model: String,
    #[serde(default)]
    pub(crate) effort: String,
    #[serde(default)]
    pub(crate) runs: u32,
    #[serde(default)]
    pub(crate) mode: String,
    #[serde(default)]
    pub(crate) concurrency: u32,
    #[serde(default)]
    model_catalog: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
struct RecordQuery {
    #[serde(default)]
    id: String,
}

// ------------------------------------------------------------ 入口

pub async fn handle(
    state: &Arc<StateManager>,
    call: TypedCall<gateway_plugin_sdk::call::management::ManagementRequest>,
) -> Result<TypedReply<ManagementResponse>, PluginFault> {
    handle_with_config(state, call, &json!({})).await
}

pub async fn handle_with_config(
    state: &Arc<StateManager>,
    call: TypedCall<gateway_plugin_sdk::call::management::ManagementRequest>,
    config: &serde_json::Value,
) -> Result<TypedReply<ManagementResponse>, PluginFault> {
    let request = call.request;
    let host = &call.host;
    let method = request.method.to_uppercase();
    let path = request.path.trim_matches('/').to_owned();
    let query = parse_query(&request.query);
    state.ensure_loaded(host).await;
    state.flush_pending(host).await;
    // Explicit opt-in preserves the synchronous contract of existing API/CLI callers.
    if method == "POST"
        && matches!(
            path.as_str(),
            "api/run" | "api/modeltrace/run" | "api/pelican/run" | "api/fingerprint/run"
        )
        && (query.get("background").is_some_and(|value| value == "1")
            || query.contains_key("__cpr_background"))
    {
        return crate::background::submit(state, host, config, &path, &call.payload).await;
    }

    match (method.as_str(), path.as_str()) {
        // ---------------------------------------------- CPA 管理接口适配
        ("GET", "api/accounts" | "v0/management/config") => {
            json_reply(200, &management_config(state, host).await?)
        }
        ("GET", "v8/management/config/api-keys") => json_reply(200, &json!({"api-keys": []})),
        ("GET", "api/models" | "v1/models") => match catalog(host).await {
            Ok(models) => json_reply(
                200,
                &json!({"data": models.iter().map(|id| json!({"id": id})).collect::<Vec<_>>()}),
            ),
            Err(error) => json_reply(502, &json!({"error": {"message": error.message}})),
        },
        ("GET", "api/accounts/models" | "v0/management/auth-files/models") => {
            let name = query
                .get("id")
                .or_else(|| query.get("name"))
                .cloned()
                .unwrap_or_default();
            let models = account_models(host, &name).await;
            json_reply(
                200,
                &json!({"models": models.iter().map(|id| json!({"id": id})).collect::<Vec<_>>()}),
            )
        }
        ("PATCH", "v0/management/auth-files/status") => {
            // 纯监控插件不改变账号启用状态，也不能假装变更成功。
            json_reply(
                409,
                &json!({"error": {"message": "请在 CPR 的账号管理中调整账号启用状态"}}),
            )
        }

        // ---------------------------------------------- 状态与同步
        ("POST", "api/credentials/sync") => {
            let count = refresh_credentials(state, host).await?;
            json_reply(
                200,
                &json!({"synced": count, "message": "已读取 CPR 宿主账号，无需导入凭证"}),
            )
        }
        ("GET", "api/state") => state_reply(state, host, config).await,
        ("POST", "api/storage/recover") => {
            if call.payload.len() > crate::storage::MAXIMUM_SNAPSHOT_BYTES {
                return json_reply(413, &json!({"error":"恢复快照超过容量，原历史未被覆盖"}));
            }
            match serde_json::from_slice::<crate::model::PersistedState>(&call.payload) {
                Ok(snapshot) => match state.recover(host, snapshot).await {
                    Ok(report) => json_reply(200, &report),
                    Err(error) => json_reply(409, &json!({"error":error.message})),
                },
                Err(_) => json_reply(400, &json!({"error":"恢复快照格式无效，原历史未被覆盖"})),
            }
        }

        // ---------------------------------------------- 糖果测试
        ("POST", "api/run") => run_candy(state, host, &call.payload).await,
        ("POST", "api/cancel" | "api/pelican/cancel") => {
            state.cancel(&body_id(&call.payload)).await;
            json_reply(200, &json!({"cancelled":true}))
        }
        ("DELETE", "api/results") => clear(state, host, "candy").await,

        // ---------------------------------------------- 鹈鹕测试
        ("POST", "api/pelican/run") => run_pelican(state, host, &call.payload).await,
        ("GET", "api/pelican/record") => {
            let id = query.get("id").cloned().unwrap_or_default();
            match state.record(&id).await {
                Some(record) => json_reply(200, &record),
                None => json_reply(404, &json!({"error": "没有找到该记录"})),
            }
        }
        ("DELETE", "api/pelican/results") => clear(state, host, "pelican").await,

        // ---------------------------------------------- 指纹测试
        ("POST", "api/fingerprint/run") => run_fingerprint(state, host, &call.payload).await,
        ("POST", "api/fingerprint/cancel") => {
            state.cancel(&body_id(&call.payload)).await;
            json_reply(200, &json!({"status": "cancelling"}))
        }
        ("GET", "api/fingerprint/record") => {
            let id = query.get("id").cloned().unwrap_or_default();
            match state.fingerprint_record(&id).await {
                Some(record) => json_reply(200, &record),
                None => json_reply(404, &json!({"error": "没有找到该记录"})),
            }
        }
        ("DELETE", "api/fingerprint/results") => clear(state, host, "fingerprint").await,

        // ---------------------------------------------- 深度追踪
        ("POST", "api/modeltrace/run") => run_modeltrace(state, host, &call.payload).await,
        ("POST", "api/modeltrace/cancel") => {
            state.cancel(&body_id(&call.payload)).await;
            json_reply(200, &json!({"status": "cancelling"}))
        }
        ("GET", "api/modeltrace/record") => {
            let id = query.get("id").cloned().unwrap_or_default();
            match state.trace_record(&id).await {
                Some(record) => json_reply(200, &record),
                None => json_reply(404, &json!({"error": "没有找到该记录"})),
            }
        }
        ("DELETE", "api/modeltrace/results") => clear(state, host, "modeltrace").await,

        // ---------------------------------------------- 汇总与巡检
        ("DELETE", "api/all/results") => clear(state, host, "all").await,
        ("POST", "api/inspection") => inspection(state, host).await,
        ("GET", "api/inspection") => json_reply(200, &state.inspection().await),

        _ => json_reply(404, &json!({"error": {"message": "Not Found"}})),
    }
}

// ------------------------------------------------------------ 凭证与目录

/// 把 CPR 账号事实投影成控制台认识的凭证形状。
fn to_credential(
    account: &gateway_plugin_sdk::call::data::AccountFacts,
) -> crate::model::Credential {
    let provider = if account.provider_id.is_empty() {
        "codex".to_owned()
    } else {
        account.provider_id.clone()
    };
    let name = if account.name.is_empty() {
        account
            .email
            .clone()
            .unwrap_or_else(|| account.account_id.clone())
    } else {
        account.name.clone()
    };
    crate::model::Credential {
        id: account.account_id.clone(),
        provider,
        source: "cpr_accounts".to_owned(),
        name,
        base_url: String::new(),
        provider_name: account.provider_id.clone(),
        email: account.email.clone().unwrap_or_default(),
        plan_type: String::new(),
        disabled: !account.enabled,
        unavailable: false,
        status_message: String::new(),
    }
}

pub(crate) async fn refresh_credentials(
    state: &StateManager,
    host: &HostClient,
) -> Result<usize, PluginFault> {
    let accounts = host_calls::list_accounts(host, None).await.map_err(|_| {
        // 宿主异常可能带账号标识；只暴露固定错误类别，不回显原始信息。
        PluginFault::new(
            gateway_plugin_sdk::ErrorCode::Fault,
            "读取 CPR 宿主账号失败，请重试或检查宿主回调",
        )
    })?;
    let count = accounts.len();
    state
        .set_credentials(accounts.iter().map(to_credential).collect())
        .await;
    Ok(count)
}

/// 取得一个可用的探测身份（Client Key）；没有可用 Key 时返回 `None`。
async fn probe_key(host: &HostClient) -> Option<String> {
    host_calls::list_client_keys(host)
        .await
        .ok()
        .and_then(|keys| keys.into_iter().find(|key| key.enabled).map(|key| key.id))
}

/// 控制台使用的全局模型目录。
async fn catalog(host: &HostClient) -> Result<Vec<String>, PluginFault> {
    let Some(key) = probe_key(host).await else {
        return Ok(Vec::new());
    };
    host_calls::list_models(host, &key).await
}

/// 单账号可见模型：CPR 不提供账号级目录，回退到全局目录。
async fn account_models(host: &HostClient, _account_id: &str) -> Vec<String> {
    catalog(host).await.unwrap_or_default()
}

/// 控制台读取的配置形状；账号来自 CPR 宿主，因此这里不带 CPA 凭证。
async fn management_config(state: &StateManager, host: &HostClient) -> Result<Value, PluginFault> {
    refresh_credentials(state, host).await?;
    let accounts = state.credentials().await;
    let revision = crate::revision(&accounts);
    Ok(json!({
        "accounts": accounts,
        "revision": revision,
        // 仅为旧版前端保留形状，所有数据仍来自 CPR 宿主；不返回任何密钥。
        "api-keys": [],
        "codex-api-key": [],
        "cpr-accounts": accounts,
        "cli-proxy-auth": {"state": {"managementKey": ""}},
    }))
}

// ------------------------------------------------------------ 状态

async fn state_reply(
    state: &StateManager,
    host: &HostClient,
    config: &Value,
) -> Result<TypedReply<ManagementResponse>, PluginFault> {
    refresh_credentials(state, host).await?;
    let (credentials, storage_error) = state.views().await;
    let models = catalog(host).await.unwrap_or_default();
    json_reply(
        200,
        &json!({
            "auths": credentials,
            "models": models,
            "storage_error": storage_error,
            "hourly_candy": crate::hourly::status(state, config).await,
            "version": API_VERSION,
        }),
    )
}

async fn clear(
    state: &StateManager,
    host: &HostClient,
    scope: &str,
) -> Result<TypedReply<ManagementResponse>, PluginFault> {
    match state.clear(host, scope).await {
        Ok(()) => json_reply(200, &json!({"status": "ok"})),
        Err(message) => json_reply(400, &json!({"error": message})),
    }
}

// ------------------------------------------------------------ 目标解析

/// 解析本次要测试的凭证；返回 (已启动, 跳过, 未勾选)。
pub(crate) fn select(
    state_credentials: &[crate::model::Credential],
    request: &RunRequest,
) -> (Vec<crate::model::Credential>, Vec<String>, Vec<String>) {
    let by_id: BTreeMap<&str, &crate::model::Credential> = state_credentials
        .iter()
        .map(|credential| (credential.id.as_str(), credential))
        .collect();

    let mut started = Vec::new();
    let mut skipped = Vec::new();
    let mut unchecked = Vec::new();

    let ids: Vec<String> = if request.all {
        state_credentials
            .iter()
            .map(|credential| credential.id.clone())
            .collect()
    } else {
        request.auth_ids.clone()
    };

    for id in ids {
        let Some(credential) = by_id.get(id.as_str()) else {
            skipped.push(id);
            continue;
        };
        if credential.disabled {
            skipped.push(id);
            continue;
        }
        if !request.model_catalog.is_empty() {
            match request.model_catalog.get(&id) {
                Some(models) => {
                    if !models.is_empty() && !models.contains(&request.model) {
                        unchecked.push(id);
                        continue;
                    }
                }
                None => {
                    unchecked.push(id);
                    continue;
                }
            }
        }
        started.push((*credential).clone());
    }
    (started, skipped, unchecked)
}

// ------------------------------------------------------------ 糖果

async fn run_candy(
    state: &Arc<StateManager>,
    host: &HostClient,
    payload: &[u8],
) -> Result<TypedReply<ManagementResponse>, PluginFault> {
    let request: RunRequest = serde_json::from_slice(payload).unwrap_or_default();
    let model = request.model.trim().to_owned();
    if !host_calls::valid_model_name(&model) {
        return json_reply(400, &json!({"error": "请选择不含推理强度后缀的模型"}));
    }
    refresh_credentials(state, host).await?;
    let credentials = state.credentials().await;
    let (targets, skipped, unchecked) = select(&credentials, &request);
    let runs = candy::clamp_runs(request.runs);
    let effort = request.effort.trim().to_owned();
    let effort = if effort.is_empty() {
        "low".to_owned()
    } else {
        effort
    };

    let mut started = 0usize;
    let mut busy = Vec::new();
    for credential in targets {
        let Some(lease) = state.acquire_run(&credential.id).await else {
            busy.push(credential.id.clone());
            continue;
        };
        started += 1;
        state
            .set_running(
                &credential.id,
                Some(crate::model::RunProgress {
                    done: 0,
                    total: runs,
                }),
            )
            .await;
        for index in 0..runs {
            let record = tokio::select! {
                biased;
                _ = lease.cancelled() => break,
                record = candy::evaluate(host, &credential, &model, &effort) => record,
            };
            state.push_candy(host, &credential.id, record).await;
            state
                .set_running(
                    &credential.id,
                    Some(crate::model::RunProgress {
                        done: index + 1,
                        total: runs,
                    }),
                )
                .await;
        }
        lease.finish().await;
    }

    json_reply(
        200,
        &json!({
            "started": started,
            "busy": busy,
            "skipped": skipped,
            "unchecked": unchecked,
        }),
    )
}

// ------------------------------------------------------------ 鹈鹕

async fn run_pelican(
    state: &Arc<StateManager>,
    host: &HostClient,
    payload: &[u8],
) -> Result<TypedReply<ManagementResponse>, PluginFault> {
    let request: RunRequest = serde_json::from_slice(payload).unwrap_or_default();
    let model = request.model.trim().to_owned();
    if !host_calls::valid_model_name(&model) {
        return json_reply(400, &json!({"error": "请选择不含推理强度后缀的模型"}));
    }
    refresh_credentials(state, host).await?;
    let credentials = state.credentials().await;
    let (targets, skipped, unchecked) = select(&credentials, &request);
    let effort = request.effort.trim().to_owned();
    let effort = if effort.is_empty() {
        "low".to_owned()
    } else {
        effort
    };

    let mut started = 0usize;
    let mut busy = Vec::new();
    for credential in targets {
        let Some(lease) = state.acquire_run(&credential.id).await else {
            busy.push(credential.id.clone());
            continue;
        };
        started += 1;
        state.set_pelican_running(&credential.id, true).await;
        let record = tokio::select! {
            biased;
            _ = lease.cancelled() => None,
            record = pelican::evaluate(host, &credential, &model, &effort) => Some(record),
        };
        if let Some(record) = record {
            state.push_pelican(host, &credential.id, record).await;
        }
        lease.finish().await;
    }

    json_reply(
        200,
        &json!({
            "started": started,
            "busy": busy,
            "skipped": skipped,
            "unchecked": unchecked,
        }),
    )
}

// ------------------------------------------------------------ 指纹

async fn run_fingerprint(
    state: &Arc<StateManager>,
    host: &HostClient,
    payload: &[u8],
) -> Result<TypedReply<ManagementResponse>, PluginFault> {
    let request: RunRequest = serde_json::from_slice(payload).unwrap_or_default();
    let model = request.model.trim().to_owned();
    if !host_calls::valid_model_name(&model) {
        return json_reply(400, &json!({"error": "请选择不含推理强度后缀的模型"}));
    }
    let Some(mode) = fingerprint::mode(request.mode.trim()) else {
        return json_reply(400, &json!({"error": "请选择快速、标准或严格模式"}));
    };
    let concurrency = if request.concurrency == 0 {
        fingerprint::FINGERPRINT_DEFAULT_CONCURRENCY
    } else {
        request.concurrency
    };
    if !(1..=fingerprint::FINGERPRINT_MAX_CONCURRENCY).contains(&concurrency) {
        return json_reply(
            400,
            &json!({"error": format!("每凭证并发须为 1–{}", fingerprint::FINGERPRINT_MAX_CONCURRENCY)}),
        );
    }

    refresh_credentials(state, host).await?;
    let credentials = state.credentials().await;
    let (targets, skipped, unchecked) = select(&credentials, &request);

    let mut started = 0usize;
    let mut busy = Vec::new();
    for credential in targets {
        let Some(lease) = state.acquire_run(&credential.id).await else {
            busy.push(credential.id.clone());
            continue;
        };
        started += 1;
        crate::runs::fingerprint_run(state, host, &credential, &model, &mode, concurrency).await;
        lease.finish().await;
    }

    json_reply(
        200,
        &json!({
            "started": started,
            "busy": busy,
            "skipped": skipped,
            "unchecked": unchecked,
        }),
    )
}

// ------------------------------------------------------------ 深度追踪

async fn run_modeltrace(
    state: &Arc<StateManager>,
    host: &HostClient,
    payload: &[u8],
) -> Result<TypedReply<ManagementResponse>, PluginFault> {
    let request: RunRequest = serde_json::from_slice(payload).unwrap_or_default();
    let model = request.model.trim().to_owned();
    if !host_calls::valid_model_name(&model) {
        return json_reply(400, &json!({"error": "请选择不含推理强度后缀的模型"}));
    }
    let concurrency = if request.concurrency == 0 {
        modeltrace::TRACE_MAX_CONCURRENCY
    } else {
        request.concurrency
    };
    if !(1..=modeltrace::TRACE_MAX_CONCURRENCY).contains(&concurrency) {
        return json_reply(
            400,
            &json!({"error": format!("每凭证并发须为 1–{}", modeltrace::TRACE_MAX_CONCURRENCY)}),
        );
    }

    refresh_credentials(state, host).await?;
    let credentials = state.credentials().await;
    let (targets, skipped, unchecked) = select(&credentials, &request);

    let mut started = 0usize;
    let mut busy = Vec::new();
    for credential in targets {
        let Some(lease) = state.acquire_run(&credential.id).await else {
            busy.push(credential.id.clone());
            continue;
        };
        started += 1;
        crate::runs::modeltrace_run(state, host, &credential, &model, concurrency).await;
        lease.finish().await;
    }

    json_reply(
        200,
        &json!({
            "started": started,
            "busy": busy,
            "skipped": skipped,
            "unchecked": unchecked,
        }),
    )
}

// ------------------------------------------------------------ 巡检

/// 对所有启用账号跑一次糖果测试，供 CLI 与定时器调用。
pub async fn inspection(
    state: &Arc<StateManager>,
    host: &HostClient,
) -> Result<TypedReply<ManagementResponse>, PluginFault> {
    let result = crate::runs::inspect(state, host).await;
    json_reply(200, &result)
}

// ------------------------------------------------------------ 工具

fn parse_query(query: &str) -> BTreeMap<String, String> {
    let mut parsed = BTreeMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        parsed.insert(percent_decode(key), percent_decode(value));
    }
    parsed
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn body_id(payload: &[u8]) -> String {
    serde_json::from_slice::<RecordQuery>(payload)
        .map(|query| query.id)
        .unwrap_or_default()
}

pub fn json_reply<T: serde::Serialize>(
    status: u16,
    data: &T,
) -> Result<TypedReply<ManagementResponse>, PluginFault> {
    let bytes = serde_json::to_vec(data)
        .map_err(|_| PluginFault::new(gateway_plugin_sdk::ErrorCode::Fault, "JSON 序列化失败"))?;
    Ok(TypedReply {
        result: ManagementResponse {
            status,
            content_type: "application/json".to_owned(),
            headers: Vec::new(),
        },
        payload: bytes,
        stream: None,
    })
}

/// 未支持模型时的跳过文案，与前端提示保持一致。
pub fn unsupported_message() -> &'static str {
    UNSUPPORTED_MODEL_MESSAGE
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credential(id: &str, disabled: bool) -> crate::model::Credential {
        crate::model::Credential {
            id: id.to_owned(),
            provider: "codex".to_owned(),
            source: "auth_files".to_owned(),
            name: id.to_owned(),
            base_url: String::new(),
            provider_name: "openai".to_owned(),
            email: String::new(),
            plan_type: String::new(),
            disabled,
            unavailable: false,
            status_message: String::new(),
        }
    }

    #[test]
    fn selects_enabled_credentials_only() {
        let credentials = vec![credential("a", false), credential("b", true)];
        let request = RunRequest {
            all: true,
            ..RunRequest::default()
        };
        let (started, skipped, unchecked) = select(&credentials, &request);
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].id, "a");
        assert_eq!(skipped, vec!["b".to_owned()]);
        assert!(unchecked.is_empty());
    }

    #[test]
    fn unknown_ids_are_skipped() {
        let credentials = vec![credential("a", false)];
        let request = RunRequest {
            auth_ids: vec!["a".to_owned(), "ghost".to_owned()],
            ..RunRequest::default()
        };
        let (started, skipped, _) = select(&credentials, &request);
        assert_eq!(started.len(), 1);
        assert_eq!(skipped, vec!["ghost".to_owned()]);
    }

    #[test]
    fn model_catalog_filters_unsupported_models() {
        let credentials = vec![credential("a", false), credential("b", false)];
        let mut catalog = BTreeMap::new();
        catalog.insert("a".to_owned(), vec!["gpt-6.1-sol".to_owned()]);
        catalog.insert("b".to_owned(), vec!["other".to_owned()]);
        let request = RunRequest {
            all: true,
            model: "gpt-6.1-sol".to_owned(),
            model_catalog: catalog,
            ..RunRequest::default()
        };
        let (started, skipped, unchecked) = select(&credentials, &request);
        assert_eq!(started.len(), 1);
        assert_eq!(started[0].id, "a");
        assert!(skipped.is_empty());
        assert_eq!(unchecked, vec!["b".to_owned()]);
    }

    #[test]
    fn empty_catalog_entry_means_unchecked() {
        let credentials = vec![credential("a", false)];
        let mut catalog = BTreeMap::new();
        catalog.insert("a".to_owned(), Vec::new());
        let request = RunRequest {
            all: true,
            model: "gpt-6.1-sol".to_owned(),
            model_catalog: catalog,
            ..RunRequest::default()
        };
        let (started, _, unchecked) = select(&credentials, &request);
        assert_eq!(started.len(), 1);
        assert!(unchecked.is_empty());
    }

    /// 页面按 `tests/routes.json` 校验它调用的路由；两者漂移会让前端 404。
    #[test]
    fn registered_routes_match_the_checked_in_list() {
        let routes: Vec<[String; 2]> = registration()
            .routes
            .iter()
            .map(|route| [route.method.clone(), route.path.clone()])
            .collect();
        let expected: Vec<[String; 2]> =
            serde_json::from_str(include_str!("../tests/routes.json")).expect("路由清单必须可解析");
        assert_eq!(routes, expected);
    }

    /// 页面入口必须同时是注册资源，否则宿主拒绝装配。
    #[test]
    fn page_entry_is_registered() {
        let registration = registration();
        assert!(registration
            .resources
            .iter()
            .any(|resource| resource.path == crate::PAGE_ENTRY));
        assert!(registration
            .pages
            .iter()
            .any(|page| page.entry == crate::PAGE_ENTRY));
    }

    #[test]
    fn query_strings_are_decoded() {
        let parsed = parse_query("id=abc%2Fdef&name=a+b&flag");
        assert_eq!(parsed["id"], "abc/def");
        assert_eq!(parsed["name"], "a b");
        assert_eq!(parsed["flag"], "");
    }
}
