//! 插件运行态与持久化：凭证历史、进行中的测试进度与宿主状态存储读写。
//!
//! 单个历史快照最多 240 KiB，经官方 KV 的双缓冲小分块保存，避免超过 IPC 64 KiB 元数据上限。
//! 底层索引最后 CAS 提交；保存串行化，读取或校验失败时不覆盖原记录。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Weak};

use gateway_plugin_sdk::call::host::StatePutResult;
use gateway_plugin_sdk::client::HostClient;
use gateway_plugin_sdk::{ErrorCode, PluginFault};
use tokio::sync::{Mutex, RwLock};

use crate::execution::StorageContext;
use crate::fingerprint::FingerprintResult;
use crate::model::*;
use crate::modeltrace::TraceResult;
use crate::storage::{self, HostStore};

pub const STATE_NAMESPACE: &str = "candy-eval";
pub const STATE_KEY: &str = "global";
/// 沿用原快照保留预算；底层分块，不把此预算误当 IPC 单帧上限。
const MAXIMUM_STATE_BYTES: usize = storage::MAXIMUM_SNAPSHOT_BYTES;

#[derive(Debug, Default)]
struct Inner {
    credentials: BTreeMap<String, Credential>,
    results: BTreeMap<String, Vec<CandyResult>>,
    pelicans: BTreeMap<String, Vec<PelicanResult>>,
    fingerprints: BTreeMap<String, Vec<FingerprintResult>>,
    traces: BTreeMap<String, Vec<TraceResult>>,
    running: BTreeMap<String, RunProgress>,
    pelican_running: BTreeSet<String>,
    fingerprint_running: BTreeMap<String, FingerprintProgress>,
    trace_running: BTreeMap<String, TraceProgress>,
    /// 活跃执行者的所有权标记；取得标记与忙碌判断在同一写锁内完成。
    leases: BTreeMap<String, Weak<()>>,
    /// 已请求取消的凭证；采集循环在每个样本前检查。
    cancelled: BTreeSet<String>,
    /// 最近一次巡检结果。
    inspection: serde_json::Value,
    hourly_candy_at: i64,
    hourly_dispatch: serde_json::Value,
    /// 读取到损坏状态时暂停保存，避免覆盖用户原文件。
    load_error: Option<String>,
    storage_error: String,
    loaded: bool,
    /// 小提交索引的 CAS 版本；None 表示尚无索引（可能仍有旧 global 状态）。
    version: Option<u64>,
    revision: u64,
    committed_revision: u64,
    attempted_revision: u64,
}

#[derive(Clone)]
pub struct StateManager {
    inner: Arc<RwLock<Inner>>,
    storage_lock: Arc<Mutex<()>>,
    commit_changed: Arc<tokio::sync::Notify>,
}

impl Default for StateManager {
    fn default() -> Self {
        Self::new()
    }
}

/// 执行者退出（包括宿主取消、超时和 panic）时必须归还运行状态。
/// 不持有 HostClient；清理只改内存，已经保存的历史保持不变。
pub struct RunLease {
    state: StateManager,
    id: String,
    token: Arc<()>,
    armed: bool,
}

impl RunLease {
    pub async fn finish(mut self) {
        clear_owned_run(&mut *self.state.inner.write().await, &self.id, &self.token);
        self.armed = false;
    }

    pub async fn cancelled(&self) {
        loop {
            if self.state.is_cancelled(&self.id).await {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }
}

impl Drop for RunLease {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(mut inner) = self.state.inner.try_write() {
            clear_owned_run(&mut inner, &self.id, &self.token);
            return;
        }
        // Drop 不能等待锁；锁被状态查询占用时，将同一所有权清理交给运行时。
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let (state, id, token) = (self.state.clone(), self.id.clone(), self.token.clone());
            runtime.spawn(async move {
                clear_owned_run(&mut *state.inner.write().await, &id, &token);
            });
        }
    }
}

fn clear_owned_run(inner: &mut Inner, id: &str, token: &Arc<()>) {
    if !inner
        .leases
        .get(id)
        .is_some_and(|owner| owner.ptr_eq(&Arc::downgrade(token)))
    {
        return;
    }
    inner.running.remove(id);
    inner.pelican_running.remove(id);
    inner.fingerprint_running.remove(id);
    inner.trace_running.remove(id);
    inner.cancelled.remove(id);
    inner.leases.remove(id);
}

impl StateManager {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(Inner::default())),
            storage_lock: Arc::new(Mutex::new(())),
            commit_changed: Arc::new(tokio::sync::Notify::new()),
        }
    }

    pub async fn acquire_run(&self, id: &str) -> Option<RunLease> {
        let mut inner = self.inner.write().await;
        if inner.leases.contains_key(id)
            || inner.running.contains_key(id)
            || inner.pelican_running.contains(id)
            || inner.fingerprint_running.contains_key(id)
            || inner.trace_running.contains_key(id)
        {
            return None;
        }
        let token = Arc::new(());
        inner.leases.insert(id.to_owned(), Arc::downgrade(&token));
        inner.cancelled.remove(id);
        Some(RunLease {
            state: self.clone(),
            id: id.to_owned(),
            token,
            armed: true,
        })
    }

    /// 首次访问时从宿主读取持久化状态；损坏时只记录错误，不阻塞页面。
    pub async fn ensure_loaded(&self, host: &HostClient) {
        let _save = self.storage_lock.lock().await;
        if self.inner.read().await.loaded {
            return;
        }
        let loaded = match storage::load(&HostStore(host)).await {
            Ok(value) => value,
            Err(_) => {
                let mut inner = self.inner.write().await;
                inner.load_error =
                    Some("读取测试记录失败，为保护原记录已暂停保存，请重试。".to_owned());
                inner.storage_error = inner.load_error.clone().unwrap_or_default();
                return;
            }
        };
        let version = loaded.as_ref().and_then(|record| record.version);
        let parsed = loaded.map(|record| serde_json::from_value::<PersistedState>(record.value));
        let mut inner = self.inner.write().await;
        match parsed {
            Some(Err(_)) => {
                inner.load_error = Some("测试记录格式无效，为保护原记录已暂停保存。".to_owned());
                inner.storage_error = inner.load_error.clone().unwrap_or_default();
                return;
            }
            Some(Ok(state)) => {
                inner.results = state.results;
                inner.pelicans = state.pelicans;
                inner.fingerprints = state.fingerprints;
                inner.traces = state.traces;
                inner.hourly_candy_at = state.hourly_candy_at;
                for history in inner.results.values_mut() {
                    trim(history);
                }
                for history in inner.pelicans.values_mut() {
                    trim(history);
                }
                for history in inner.fingerprints.values_mut() {
                    trim(history);
                }
                for history in inner.traces.values_mut() {
                    trim(history);
                }
            }
            None => {}
        }
        inner.loaded = true;
        inner.version = version;
        inner.load_error = None;
        inner.storage_error.clear();
    }

    /// 仅经宿主管理桥恢复远端私有备份；合并重复项，不覆盖用户新测试，也不裁剪历史。
    pub async fn recover(
        &self,
        host: &HostClient,
        incoming: PersistedState,
    ) -> Result<serde_json::Value, PluginFault> {
        let _save = self.storage_lock.lock().await;
        let mut inner = self.inner.write().await;
        if inner.load_error.is_some() || !inner.loaded {
            return Err(PluginFault::new(
                ErrorCode::Conflict,
                "已有持久化记录尚未读取，已拒绝覆盖",
            ));
        }
        if !inner.running.is_empty()
            || !inner.pelican_running.is_empty()
            || !inner.fingerprint_running.is_empty()
            || !inner.trace_running.is_empty()
        {
            return Err(PluginFault::new(
                ErrorCode::Conflict,
                "仍有测试运行，暂不恢复记录",
            ));
        }
        let mut merged = PersistedState {
            results: inner.results.clone(),
            pelicans: inner.pelicans.clone(),
            fingerprints: inner.fingerprints.clone(),
            traces: inner.traces.clone(),
            hourly_candy_at: inner.hourly_candy_at.max(incoming.hourly_candy_at),
        };
        merge_histories(&mut merged.results, incoming.results)?;
        merge_histories(&mut merged.pelicans, incoming.pelicans)?;
        merge_histories(&mut merged.fingerprints, incoming.fingerprints)?;
        merge_histories(&mut merged.traces, incoming.traces)?;
        let value = serde_json::to_value(&merged).map_err(|_| recovery_error())?;
        let bytes = encoded_len(&value);
        let version = storage::save(&HostStore(host), &value, inner.version).await?;
        let report = serde_json::json!({"saved":true,"bytes":bytes,"counts":{
            "results":merged.results.values().map(Vec::len).sum::<usize>(),
            "pelicans":merged.pelicans.values().map(Vec::len).sum::<usize>(),
            "fingerprints":merged.fingerprints.values().map(Vec::len).sum::<usize>(),
            "traces":merged.traces.values().map(Vec::len).sum::<usize>()}});
        inner.results = merged.results;
        inner.pelicans = merged.pelicans;
        inner.fingerprints = merged.fingerprints;
        inner.traces = merged.traces;
        inner.hourly_candy_at = merged.hourly_candy_at;
        inner.version = Some(version);
        inner.storage_error.clear();
        Ok(report)
    }

    /// 覆盖凭证列表；CPR 账号是唯一来源，不依赖前端同步。
    pub async fn set_credentials(&self, credentials: Vec<Credential>) {
        let mut inner = self.inner.write().await;
        inner.credentials = credentials
            .into_iter()
            .map(|credential| (credential.id.clone(), credential))
            .collect();
    }

    pub async fn credential(&self, id: &str) -> Option<Credential> {
        self.inner.read().await.credentials.get(id).cloned()
    }

    pub async fn credentials(&self) -> Vec<Credential> {
        self.inner
            .read()
            .await
            .credentials
            .values()
            .cloned()
            .collect()
    }

    /// 组装配发给前端的凭证视图，含各类历史与进行中进度。
    pub async fn views(&self) -> (Vec<CredentialView>, String) {
        let inner = self.inner.read().await;
        let views = inner
            .credentials
            .values()
            .map(|credential| {
                let id = &credential.id;
                CredentialView {
                    credential: credential.clone(),
                    running: inner.running.get(id).copied(),
                    results: inner.results.get(id).cloned().unwrap_or_default(),
                    pelican_running: inner.pelican_running.contains(id).then_some(true),
                    pelicans: inner
                        .pelicans
                        .get(id)
                        .map(|records| records.iter().map(PelicanResult::summary).collect())
                        .unwrap_or_default(),
                    fingerprint_running: inner.fingerprint_running.get(id).cloned(),
                    fingerprints: inner
                        .fingerprints
                        .get(id)
                        .map(|records| records.iter().map(FingerprintResult::summary).collect())
                        .unwrap_or_default(),
                    modeltrace_running: inner.trace_running.get(id).cloned(),
                    modeltraces: inner
                        .traces
                        .get(id)
                        .map(|records| records.iter().map(TraceResult::summary).collect())
                        .unwrap_or_default(),
                }
            })
            .collect();
        (views, inner.storage_error.clone())
    }

    pub async fn record(&self, id: &str) -> Option<PelicanResult> {
        let inner = self.inner.read().await;
        inner
            .pelicans
            .values()
            .flat_map(|records| records.iter())
            .find(|record| record.id == id)
            .cloned()
    }

    pub async fn set_running(&self, id: &str, progress: Option<RunProgress>) {
        let mut inner = self.inner.write().await;
        match progress {
            Some(progress) => {
                inner.running.insert(id.to_owned(), progress);
            }
            None => {
                inner.running.remove(id);
            }
        }
    }

    pub async fn set_pelican_running(&self, id: &str, running: bool) {
        let mut inner = self.inner.write().await;
        if running {
            inner.pelican_running.insert(id.to_owned());
        } else {
            inner.pelican_running.remove(id);
        }
    }

    pub async fn set_fingerprint_running(&self, id: &str, progress: Option<FingerprintProgress>) {
        let mut inner = self.inner.write().await;
        match progress {
            Some(progress) => {
                inner.fingerprint_running.insert(id.to_owned(), progress);
            }
            None => {
                inner.fingerprint_running.remove(id);
            }
        }
    }

    pub async fn set_trace_running(&self, id: &str, progress: Option<TraceProgress>) {
        let mut inner = self.inner.write().await;
        match progress {
            Some(progress) => {
                inner.trace_running.insert(id.to_owned(), progress);
            }
            None => {
                inner.trace_running.remove(id);
            }
        }
    }

    /// 更新进行中进度；仅当该凭证仍在测试时才写入，避免结束后的回填覆盖。
    pub async fn update_fingerprint(
        &self,
        id: &str,
        done: u32,
        valid: u32,
        errors: u32,
        phase: &str,
    ) {
        let mut inner = self.inner.write().await;
        if let Some(progress) = inner.fingerprint_running.get_mut(id) {
            progress.done = done;
            progress.valid = valid;
            progress.errors = errors;
            progress.phase = phase.to_owned();
        }
    }

    pub async fn update_trace(&self, id: &str, done: u32, phase: &str) {
        let mut inner = self.inner.write().await;
        if let Some(progress) = inner.trace_running.get_mut(id) {
            progress.done = done;
            progress.phase = phase.to_owned();
        }
    }

    pub async fn cancel(&self, id: &str) {
        let mut inner = self.inner.write().await;
        inner.cancelled.insert(id.to_owned());
        if let Some(progress) = inner.fingerprint_running.get_mut(id) {
            progress.phase = "cancelling".to_owned();
        }
        if let Some(progress) = inner.trace_running.get_mut(id) {
            progress.phase = "cancelling".to_owned();
        }
    }

    pub async fn is_cancelled(&self, id: &str) -> bool {
        self.inner.read().await.cancelled.contains(id)
    }

    pub async fn clear_cancel(&self, id: &str) {
        self.inner.write().await.cancelled.remove(id);
    }

    pub async fn fingerprint_record(&self, id: &str) -> Option<FingerprintResult> {
        let inner = self.inner.read().await;
        inner
            .fingerprints
            .values()
            .flat_map(|records| records.iter())
            .find(|record| record.id == id)
            .cloned()
    }

    pub async fn trace_record(&self, id: &str) -> Option<TraceResult> {
        let inner = self.inner.read().await;
        inner
            .traces
            .values()
            .flat_map(|records| records.iter())
            .find(|record| record.id == id)
            .cloned()
    }

    pub async fn set_inspection(&self, value: serde_json::Value) {
        self.inner.write().await.inspection = value;
    }

    pub async fn inspection(&self) -> serde_json::Value {
        self.inner.read().await.inspection.clone()
    }

    /// Claim once, before making model requests. Failed saves prohibit dispatch.
    pub async fn claim_hourly_candy(&self, now: i64) -> Option<u64> {
        let mut inner = self.inner.write().await;
        if !inner.loaded
            || inner.load_error.is_some()
            || !inner.storage_error.is_empty()
            || now <= 0
            || !crate::hourly::due(inner.hourly_candy_at, now)
        {
            return None;
        }
        inner.hourly_candy_at = now;
        inner.hourly_dispatch = serde_json::json!({"status":"starting"});
        inner.revision += 1;
        Some(inner.revision)
    }

    pub async fn revision_committed(&self, revision: u64) -> bool {
        self.inner.read().await.committed_revision >= revision
    }

    pub async fn hourly_candy(&self) -> (i64, serde_json::Value) {
        let inner = self.inner.read().await;
        (inner.hourly_candy_at, inner.hourly_dispatch.clone())
    }

    pub async fn set_hourly_dispatch(&self, value: serde_json::Value) {
        self.inner.write().await.hourly_dispatch = value;
    }

    /// 某凭证是否已有测试在进行中。
    pub async fn busy(&self, id: &str) -> bool {
        let inner = self.inner.read().await;
        inner.leases.contains_key(id)
            || inner.running.contains_key(id)
            || inner.pelican_running.contains(id)
            || inner.fingerprint_running.contains_key(id)
            || inner.trace_running.contains_key(id)
    }

    /// 追加糖果记录并立即持久化。
    pub async fn push_candy(&self, host: &impl StorageContext, id: &str, record: CandyResult) {
        {
            let mut inner = self.inner.write().await;
            append_recent(inner.results.entry(id.to_owned()).or_default(), record);
            inner.revision += 1;
        }
        if let Some(host) = host.storage_host() {
            self.persist(host).await;
        }
    }

    /// 追加鹈鹕记录并立即持久化。
    pub async fn push_pelican(&self, host: &impl StorageContext, id: &str, record: PelicanResult) {
        {
            let mut inner = self.inner.write().await;
            append_recent(inner.pelicans.entry(id.to_owned()).or_default(), record);
            inner.revision += 1;
        }
        if let Some(host) = host.storage_host() {
            self.persist(host).await;
        }
    }

    /// 追加指纹记录并立即持久化。
    pub async fn push_fingerprint(
        &self,
        host: &impl StorageContext,
        id: &str,
        record: FingerprintResult,
    ) {
        {
            let mut inner = self.inner.write().await;
            append_recent(inner.fingerprints.entry(id.to_owned()).or_default(), record);
            inner.revision += 1;
        }
        if let Some(host) = host.storage_host() {
            self.persist(host).await;
        }
    }

    /// 追加深度追踪记录并立即持久化。
    pub async fn push_trace(&self, host: &impl StorageContext, id: &str, record: TraceResult) {
        {
            let mut inner = self.inner.write().await;
            append_recent(inner.traces.entry(id.to_owned()).or_default(), record);
            inner.revision += 1;
        }
        if let Some(host) = host.storage_host() {
            self.persist(host).await;
        }
    }

    /// 清空指定范围的历史；未知范围返回错误。
    pub async fn clear(&self, host: &HostClient, scope: &str) -> Result<(), String> {
        {
            let mut inner = self.inner.write().await;
            match scope {
                "candy" => inner.results.clear(),
                "pelican" => inner.pelicans.clear(),
                "fingerprint" => inner.fingerprints.clear(),
                "modeltrace" => inner.traces.clear(),
                "all" => {
                    inner.results.clear();
                    inner.pelicans.clear();
                    inner.fingerprints.clear();
                    inner.traces.clear();
                }
                other => return Err(format!("未知测试类型：{other}")),
            }
        }
        self.persist(host).await;
        Ok(())
    }

    /// Only fresh management/maintenance scopes may flush queued background results.
    pub async fn flush_pending(&self, host: &HostClient) {
        let dirty = {
            let inner = self.inner.read().await;
            inner.revision > inner.committed_revision
        };
        if dirty {
            self.persist(host).await;
        }
    }

    /// Keep the run lease until the final snapshot has been attempted; save failures
    /// remain visible and queued for retry rather than stranding the run forever.
    pub async fn wait_for_commit(&self) {
        let wanted = self.inner.read().await.revision;
        loop {
            let changed = self.commit_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.inner.read().await.attempted_revision >= wanted {
                return;
            }
            changed.await;
        }
    }
    /// 快照并写入宿主；超限时裁剪最旧记录后重试，版本冲突时重读版本重试一次。
    async fn persist(&self, host: &HostClient) {
        let _save = self.storage_lock.lock().await;
        let (value, expected_version, revision) = {
            let mut inner = self.inner.write().await;
            if let Some(error) = inner.load_error.clone() {
                inner.storage_error = error;
                inner.attempted_revision = inner.revision;
                self.commit_changed.notify_waiters();
                return;
            }
            let mut state = PersistedState {
                results: inner.results.clone(),
                pelicans: inner.pelicans.clone(),
                fingerprints: inner.fingerprints.clone(),
                traces: inner.traces.clone(),
                hourly_candy_at: inner.hourly_candy_at,
            };
            let mut value = serde_json::to_value(&state).unwrap_or_default();
            // 超限时反复丢弃最长的历史中最早的一条记录，直到落在配额内。
            while encoded_len(&value) > MAXIMUM_STATE_BYTES && prune_oldest(&mut state) {
                value = serde_json::to_value(&state).unwrap_or_default();
            }
            inner.results = state.results;
            inner.pelicans = state.pelicans;
            inner.fingerprints = state.fingerprints;
            inner.traces = state.traces;
            (value, inner.version, inner.revision)
        };

        // 同一实例的写入串行化；CAS 冲突必须报告，不能盲目重读版本后覆盖其他更新。
        let outcome = put(host, value, expected_version).await;

        let mut inner = self.inner.write().await;
        inner.attempted_revision = inner.attempted_revision.max(revision);
        inner.storage_error = match outcome {
            Ok(result) => {
                inner.version = Some(result.version);
                inner.committed_revision = inner.committed_revision.max(revision);
                String::new()
            }
            Err(error) => format!("测试记录保存失败：{}", error.message),
        };
        self.commit_changed.notify_waiters();
    }
}

async fn put(
    host: &HostClient,
    value: serde_json::Value,
    expected_version: Option<u64>,
) -> Result<StatePutResult, gateway_plugin_sdk::PluginFault> {
    Ok(StatePutResult {
        version: storage::save(&HostStore(host), &value, expected_version).await?,
    })
}

fn recovery_error() -> PluginFault {
    PluginFault::new(
        ErrorCode::InvalidInput,
        "恢复记录无效或超过保留上限，原历史未被覆盖",
    )
}

fn merge_histories<T: serde::Serialize + Clone>(
    target: &mut BTreeMap<String, Vec<T>>,
    incoming: BTreeMap<String, Vec<T>>,
) -> Result<(), PluginFault> {
    for (account, records) in incoming {
        let history = target.entry(account).or_default();
        let mut joined = Vec::<T>::new();
        let mut identifiers = BTreeMap::<String, usize>::new();
        // 带稳定 ID 的记录不可因浮点重新编码而复制；先核对内容再合并。
        for record in history.iter().cloned() {
            let value = serde_json::to_value(&record).map_err(|_| recovery_error())?;
            if let Some(id) = record_identity(&value) {
                if let Some(&position) = identifiers.get(id) {
                    let previous =
                        serde_json::to_value(&joined[position]).map_err(|_| recovery_error())?;
                    if !equivalent_record(&previous, &value) {
                        return Err(recovery_conflict());
                    }
                    continue;
                }
                identifiers.insert(id.to_owned(), joined.len());
            }
            joined.push(record);
        }
        let mut present = BTreeMap::<Vec<u8>, usize>::new();
        for record in &joined {
            let mut value = serde_json::to_value(record).map_err(|_| recovery_error())?;
            if record_identity(&value).is_none() {
                value.sort_all_objects();
                *present
                    .entry(serde_json::to_vec(&value).map_err(|_| recovery_error())?)
                    .or_default() += 1;
            }
        }
        let mut seen = BTreeMap::<Vec<u8>, usize>::new();
        for record in records {
            let mut value = serde_json::to_value(&record).map_err(|_| recovery_error())?;
            if let Some(id) = record_identity(&value) {
                if let Some(&position) = identifiers.get(id) {
                    let previous =
                        serde_json::to_value(&joined[position]).map_err(|_| recovery_error())?;
                    if !equivalent_record(&previous, &value) {
                        return Err(recovery_conflict());
                    }
                    joined[position] = record; // 使用已保全快照的精确数值。
                    continue;
                }
                identifiers.insert(id.to_owned(), joined.len());
            } else {
                // Candy 没有稳定 ID，完整记录的重复次数必须保留。
                value.sort_all_objects();
                let encoded = serde_json::to_vec(&value).map_err(|_| recovery_error())?;
                let occurrence = seen.entry(encoded.clone()).or_default();
                *occurrence += 1;
                if *occurrence <= *present.get(&encoded).unwrap_or(&0) {
                    continue;
                }
            }
            joined.push(record);
        }
        if joined.len() > HISTORY_LIMIT {
            return Err(recovery_error());
        }
        let mut ordered = joined
            .into_iter()
            .map(|record| {
                let value = serde_json::to_value(&record).map_err(|_| recovery_error())?;
                let time = value["time"]
                    .as_str()
                    .ok_or_else(recovery_error)?
                    .to_owned();
                Ok((time, record))
            })
            .collect::<Result<Vec<_>, PluginFault>>()?;
        ordered.sort_by(|a, b| a.0.cmp(&b.0));
        *history = ordered.into_iter().map(|(_, record)| record).collect();
    }
    Ok(())
}

fn record_identity(value: &serde_json::Value) -> Option<&str> {
    value
        .get("id")
        .and_then(|id| id.as_str())
        .filter(|id| !id.is_empty())
}

fn recovery_conflict() -> PluginFault {
    PluginFault::new(ErrorCode::Conflict, "同一记录标识的内容不一致，未覆盖历史")
}

fn equivalent_record(left: &serde_json::Value, right: &serde_json::Value) -> bool {
    use serde_json::Value;
    if left == right {
        return true;
    }
    match (left, right) {
        (Value::Number(a), Value::Number(b)) if a.is_f64() && b.is_f64() => {
            let (a, b) = (a.as_f64().unwrap(), b.as_f64().unwrap());
            (a - b).abs() <= 32.0 * f64::EPSILON * a.abs().max(b.abs()).max(f64::MIN_POSITIVE)
        }
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| equivalent_record(a, b))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, a)| b.get(key).is_some_and(|b| equivalent_record(a, b)))
        }
        _ => false,
    }
}

fn trim<T>(history: &mut Vec<T>) {
    if history.len() > HISTORY_LIMIT {
        let excess = history.len() - HISTORY_LIMIT;
        history.drain(..excess);
    }
}

fn encoded_len(value: &serde_json::Value) -> usize {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX)
}

#[derive(Clone, Copy)]
enum History {
    Candy,
    Pelican,
    Fingerprint,
    ModelTrace,
}

/// 丢弃长度最大的历史中最早的一条记录；全部为空时返回 false。
fn prune_oldest(state: &mut PersistedState) -> bool {
    let mut best: Option<(History, String, usize)> = None;
    let mut consider = |kind: History, id: &str, len: usize| {
        if len == 0 {
            return;
        }
        let better = match best.as_ref() {
            Some((_, _, current)) => len > *current,
            None => true,
        };
        if better {
            best = Some((kind, id.to_owned(), len));
        }
    };
    for (id, history) in &state.results {
        consider(History::Candy, id, history.len());
    }
    for (id, history) in &state.pelicans {
        consider(History::Pelican, id, history.len());
    }
    for (id, history) in &state.fingerprints {
        consider(History::Fingerprint, id, history.len());
    }
    for (id, history) in &state.traces {
        consider(History::ModelTrace, id, history.len());
    }

    let Some((kind, id, _)) = best else {
        return false;
    };
    match kind {
        History::Candy => drop_oldest(&mut state.results, &id),
        History::Pelican => drop_oldest(&mut state.pelicans, &id),
        History::Fingerprint => drop_oldest(&mut state.fingerprints, &id),
        History::ModelTrace => drop_oldest(&mut state.traces, &id),
    }
    true
}

fn drop_oldest<T>(map: &mut BTreeMap<String, Vec<T>>, id: &str) {
    if let Some(history) = map.get_mut(id) {
        history.remove(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_lease_atomically_excludes_duplicate_starts() {
        let state = StateManager::new();
        let (first, second) = tokio::join!(
            state.acquire_run("fixture-account"),
            state.acquire_run("fixture-account")
        );
        assert_ne!(first.is_some(), second.is_some());
        let lease = first.or(second).unwrap();
        assert!(state.busy("fixture-account").await);
        lease.finish().await;
        assert!(!state.busy("fixture-account").await);
        assert!(state.acquire_run("fixture-account").await.is_some());
    }

    #[tokio::test]
    async fn aborted_executor_releases_all_progress_and_cancel_state() {
        let state = StateManager::new();
        let lease = state.acquire_run("fixture-account").await.unwrap();
        state
            .set_running("fixture-account", Some(RunProgress { done: 0, total: 3 }))
            .await;
        state.set_pelican_running("fixture-account", true).await;
        state
            .set_fingerprint_running(
                "fixture-account",
                Some(FingerprintProgress {
                    model: "fixture-model".into(),
                    mode: "quick".into(),
                    done: 0,
                    total: 3,
                    valid: 0,
                    errors: 0,
                    phase: "collecting".into(),
                    concurrency: 3,
                }),
            )
            .await;
        state
            .set_trace_running(
                "fixture-account",
                Some(TraceProgress {
                    model: "fixture-model".into(),
                    phase: "collecting".into(),
                    done: 0,
                    concurrency: 3,
                }),
            )
            .await;
        state.cancel("fixture-account").await;
        let task = tokio::spawn(async move {
            let _lease = lease;
            std::future::pending::<()>().await;
        });
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(!state.busy("fixture-account").await);
        assert!(!state.is_cancelled("fixture-account").await);
        let inner = state.inner.read().await;
        assert!(inner.running.is_empty() && inner.pelican_running.is_empty());
        assert!(inner.fingerprint_running.is_empty() && inner.trace_running.is_empty());
    }

    #[tokio::test]
    async fn interrupted_executor_cleanup_waits_for_a_busy_state_lock() {
        let state = StateManager::new();
        let lease = state.acquire_run("fixture-account").await.unwrap();
        let lock = state.inner.read().await;
        drop(lease);
        assert!(lock.leases.contains_key("fixture-account"));
        drop(lock);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while state.busy("fixture-account").await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(state.acquire_run("fixture-account").await.is_some());
    }

    #[tokio::test]
    async fn old_executor_cleanup_cannot_clear_a_new_executor() {
        let state = StateManager::new();
        let old = state.acquire_run("fixture-account").await.unwrap();
        clear_owned_run(
            &mut *state.inner.write().await,
            "fixture-account",
            &old.token,
        );
        let new = state.acquire_run("fixture-account").await.unwrap();
        state
            .set_running("fixture-account", Some(RunProgress { done: 1, total: 3 }))
            .await;
        drop(old);
        assert!(state.busy("fixture-account").await);
        assert_eq!(state.inner.read().await.running["fixture-account"].done, 1);
        new.finish().await;
        assert!(!state.busy("fixture-account").await);
    }

    #[tokio::test]
    async fn an_explicit_stop_is_observed_without_waiting_for_upstream() {
        let state = StateManager::new();
        let lease = state.acquire_run("fixture-account").await.unwrap();
        state.cancel("fixture-account").await;
        tokio::time::timeout(std::time::Duration::from_millis(100), lease.cancelled())
            .await
            .unwrap();
        lease.finish().await;
        assert!(!state.is_cancelled("fixture-account").await);
    }

    fn candy(answer: &str) -> CandyResult {
        CandyResult {
            skipped: false,
            time: "2026-01-01T00:00:00.000Z".to_owned(),
            model: "gpt-6.1-sol".to_owned(),
            effort: "low".to_owned(),
            ok: true,
            answer: answer.to_owned(),
            error: String::new(),
            input_tokens: 1,
            output_tokens: 2,
            reasoning_tokens: 3,
            duration_ms: 4,
        }
    }

    #[test]
    fn prunes_the_longest_history_first() {
        let mut state = PersistedState::default();
        state
            .results
            .insert("a".to_owned(), vec![candy("1"), candy("2")]);
        state.results.insert("b".to_owned(), vec![candy("3")]);
        assert!(prune_oldest(&mut state));
        assert_eq!(state.results["a"].len(), 1);
        assert_eq!(state.results["b"].len(), 1);
        assert!(prune_oldest(&mut state));
        assert!(prune_oldest(&mut state));
        assert!(!prune_oldest(&mut state));
    }

    #[test]
    fn oversized_state_is_pruned_until_it_fits() {
        let mut state = PersistedState::default();
        // 约 300 KiB 的正文，必然超过单记录上限。
        let big = "x".repeat(30 * 1024);
        state.results.insert(
            "a".to_owned(),
            (0..HISTORY_LIMIT).map(|_| candy(&big)).collect(),
        );
        let mut value = serde_json::to_value(&state).unwrap();
        let mut guard = 0;
        while encoded_len(&value) > MAXIMUM_STATE_BYTES && prune_oldest(&mut state) {
            value = serde_json::to_value(&state).unwrap();
            guard += 1;
            assert!(guard < 100, "裁剪未能收敛");
        }
        assert!(encoded_len(&value) <= MAXIMUM_STATE_BYTES);
        assert!(state.results["a"].len() < HISTORY_LIMIT);
    }

    #[test]
    fn recovery_uses_stable_ids_and_preserves_the_backed_up_float() {
        let original = serde_json::json!({"id":"fixture-result","time":"2026-01-01T00:00:00Z","answer":"original body","attribution":{"p":1e-9}});
        let mut rounded = original.clone();
        rounded["attribution"]["p"] = serde_json::json!(f64::from_bits(1e-9_f64.to_bits() + 1));
        let mut target = BTreeMap::from([("a".to_owned(), vec![original.clone(), rounded])]);
        merge_histories(
            &mut target,
            BTreeMap::from([("a".to_owned(), vec![original.clone()])]),
        )
        .unwrap();
        assert_eq!(target["a"], vec![original]);
    }

    #[test]
    fn recovery_rejects_same_id_with_different_primary_content() {
        let original = serde_json::json!({"id":"fixture-result","time":"2026-01-01T00:00:00Z","html":"original"});
        let mut changed = original.clone();
        changed["html"] = serde_json::json!("different");
        let mut target = BTreeMap::from([("a".to_owned(), vec![original])]);
        assert!(merge_histories(
            &mut target,
            BTreeMap::from([("a".to_owned(), vec![changed])])
        )
        .is_err());
    }

    #[test]
    fn recovery_preserves_the_multiplicity_of_idless_results() {
        let record = candy("21");
        let mut target = BTreeMap::from([("a".to_owned(), vec![record.clone(), record.clone()])]);
        merge_histories(
            &mut target,
            BTreeMap::from([("a".to_owned(), vec![record.clone(), record])]),
        )
        .unwrap();
        assert_eq!(target["a"].len(), 2);
    }

    #[test]
    fn json_probabilities_round_trip_without_floating_point_drift() {
        for seed in [1.0 / 3.0_f64, 1e-9, 1e-30, 1e200] {
            for step in 0..128 {
                let n = f64::from_bits(seed.to_bits() + step);
                let value = serde_json::json!({"probability":n});
                let decoded: serde_json::Value =
                    serde_json::from_slice(&serde_json::to_vec(&value).unwrap()).unwrap();
                assert_eq!(
                    decoded["probability"].as_f64().unwrap().to_bits(),
                    n.to_bits()
                );
            }
        }
    }
}
