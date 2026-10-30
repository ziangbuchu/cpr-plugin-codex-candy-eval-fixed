//! CPR 原生 KV 上的双缓冲快照；每帧小于 SDK 的 64 KiB 元数据上限。
//! 保留旧 global 键不改动；完整写好非活动分块后才 CAS 提交小索引。
use std::future::Future;

use gateway_plugin_sdk::client::HostClient;
use gateway_plugin_sdk::{ErrorCode, PluginFault};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{
    host_calls,
    state::{STATE_KEY, STATE_NAMESPACE},
};

pub const MAXIMUM_SNAPSHOT_BYTES: usize = 240 * 1024;
const CHUNK_BYTES: usize = 16 * 1024;
const MAX_CHUNKS: usize = 16;
const INDEX_KEY: &str = "snapshot-v2";
const FORMAT: &str = "candy-eval-chunks-v2";

#[derive(Clone)]
pub struct Record {
    pub value: Value,
    pub version: u64,
}

pub struct Snapshot {
    pub value: Value,
    // None means the index has not been created, including legacy-only storage.
    pub version: Option<u64>,
}

pub trait Store: Sync {
    fn get(&self, key: &str) -> impl Future<Output = Result<Option<Record>, PluginFault>> + Send;
    fn put(
        &self,
        key: &str,
        value: Value,
        expected: Option<u64>,
    ) -> impl Future<Output = Result<u64, PluginFault>> + Send;
}

pub struct HostStore<'a>(pub &'a HostClient);
impl Store for HostStore<'_> {
    async fn get(&self, key: &str) -> Result<Option<Record>, PluginFault> {
        Ok(host_calls::get_state(self.0, STATE_NAMESPACE, key)
            .await?
            .record
            .map(|r| Record {
                value: r.value,
                version: r.version,
            }))
    }
    async fn put(
        &self,
        key: &str,
        value: Value,
        expected: Option<u64>,
    ) -> Result<u64, PluginFault> {
        use gateway_plugin_sdk::call::host::StatePutRequest;
        Ok(host_calls::put_state(
            self.0,
            &StatePutRequest {
                namespace: STATE_NAMESPACE.to_owned(),
                key: key.to_owned(),
                value,
                expected_version: expected,
            },
        )
        .await?
        .version)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    format: String,
    bank: u8,
    chunks: usize,
    bytes: usize,
    sha256: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Chunk {
    sha256: String,
    index: usize,
    data: String,
}

fn invalid() -> PluginFault {
    PluginFault::new(
        ErrorCode::InvalidInput,
        "持久化分块不完整或校验失败，已暂停保存以保护历史",
    )
}
fn conflict() -> PluginFault {
    PluginFault::new(ErrorCode::Conflict, "持久化版本已变化，未覆盖其他更新")
}
fn key(bank: u8, index: usize) -> String {
    format!("snapshot-{bank}-{index:02}")
}
fn index(record: &Record) -> Result<Index, PluginFault> {
    let value: Index = serde_json::from_value(record.value.clone()).map_err(|_| invalid())?;
    if value.format != FORMAT
        || value.bank > 1
        || value.chunks == 0
        || value.chunks > MAX_CHUNKS
        || value.bytes > MAXIMUM_SNAPSHOT_BYTES
        || value.sha256.len() != 64
    {
        return Err(invalid());
    }
    Ok(value)
}

/// 新索引不存在时原样读取旧单键；不把读取失败伪装为空状态。
pub async fn load(store: &impl Store) -> Result<Option<Snapshot>, PluginFault> {
    for _ in 0..3 {
        let Some(record) = store.get(INDEX_KEY).await? else {
            return Ok(store.get(STATE_KEY).await?.map(|r| Snapshot {
                value: r.value,
                version: None,
            }));
        };
        let manifest = index(&record)?;
        let value = read_chunks(store, &manifest).await;
        if store.get(INDEX_KEY).await?.map(|r| r.version) != Some(record.version) {
            continue;
        }
        return Ok(Some(Snapshot {
            value: value?,
            version: Some(record.version),
        }));
    }
    Err(conflict())
}

async fn read_chunks(store: &impl Store, manifest: &Index) -> Result<Value, PluginFault> {
    let mut body = String::with_capacity(manifest.bytes);
    for n in 0..manifest.chunks {
        let record = store
            .get(&key(manifest.bank, n))
            .await?
            .ok_or_else(invalid)?;
        let chunk: Chunk = serde_json::from_value(record.value).map_err(|_| invalid())?;
        if chunk.sha256 != manifest.sha256 || chunk.index != n || chunk.data.len() > CHUNK_BYTES {
            return Err(invalid());
        }
        body.push_str(&chunk.data);
        if body.len() > manifest.bytes {
            return Err(invalid());
        }
    }
    if body.len() != manifest.bytes
        || hex::encode(Sha256::digest(body.as_bytes())) != manifest.sha256
    {
        return Err(invalid());
    }
    serde_json::from_str(&body).map_err(|_| invalid())
}

/// Caller serializes writers for this active plugin instance. No keys are deleted.
/// Interrupted writes leave the previous index and snapshot intact; both banks reuse fixed keys.
pub async fn save(
    store: &impl Store,
    value: &Value,
    expected: Option<u64>,
) -> Result<u64, PluginFault> {
    let body = serde_json::to_string(value).map_err(|_| invalid())?;
    if body.len() > MAXIMUM_SNAPSHOT_BYTES {
        return Err(PluginFault::new(
            ErrorCode::Capacity,
            "测试记录超过快照容量，未覆盖原状态",
        ));
    }
    let current = store.get(INDEX_KEY).await?;
    if current.as_ref().map(|r| r.version) != expected {
        return Err(conflict());
    }
    let bank = match &current {
        Some(r) => 1 - index(r)?.bank,
        None => 0,
    };
    let digest = hex::encode(Sha256::digest(body.as_bytes()));
    let mut start = 0;
    let mut count = 0;
    while start < body.len() {
        if count >= MAX_CHUNKS {
            return Err(invalid());
        }
        let mut end = (start + CHUNK_BYTES).min(body.len());
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        let name = key(bank, count);
        let previous = store.get(&name).await?.map(|r| r.version);
        let chunk = json!({"sha256": digest, "index": count, "data": &body[start..end]});
        store.put(&name, chunk, previous).await?;
        start = end;
        count += 1;
    }
    let manifest = Index {
        format: FORMAT.to_owned(),
        bank,
        chunks: count,
        bytes: body.len(),
        sha256: digest,
    };
    store
        .put(
            INDEX_KEY,
            serde_json::to_value(manifest).map_err(|_| invalid())?,
            expected,
        )
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use gateway_plugin_sdk::{Frame, Message};
    use std::{collections::BTreeMap, sync::Mutex};

    #[derive(Default)]
    struct Memory {
        records: Mutex<BTreeMap<String, Record>>,
        writes: Mutex<usize>,
        fail_at: Mutex<Option<usize>>,
    }
    impl Store for Memory {
        async fn get(&self, name: &str) -> Result<Option<Record>, PluginFault> {
            Ok(self.records.lock().unwrap().get(name).cloned())
        }
        async fn put(
            &self,
            name: &str,
            value: Value,
            expected: Option<u64>,
        ) -> Result<u64, PluginFault> {
            let mut writes = self.writes.lock().unwrap();
            *writes += 1;
            if *self.fail_at.lock().unwrap() == Some(*writes) {
                return Err(invalid());
            }
            // Real SDK encoding, including nested JSON escaping and reply metadata.
            let request = Frame::control(Message::Callback {
                id: 1,
                parent_id: 1,
                method: "host.state.put".into(),
                params: json!({"namespace":STATE_NAMESPACE,"key":name,"value":value,"expected_version":expected}),
            });
            gateway_plugin_sdk::client::validate_frame(&request).expect("request fits 64 KiB");
            let reply = Frame::control(Message::Result {
                id: 1,
                result: json!({"record":{"value":value,"version":1,"schema_version":1}}),
            });
            gateway_plugin_sdk::client::validate_frame(&reply).expect("read reply fits 64 KiB");
            let mut records = self.records.lock().unwrap();
            if records.get(name).map(|r| r.version) != expected {
                return Err(conflict());
            }
            let version = *writes as u64;
            records.insert(name.to_owned(), Record { value, version });
            Ok(version)
        }
    }

    #[tokio::test]
    async fn legacy_reads_do_not_modify_the_original_key() {
        let store = Memory::default();
        let legacy = json!({"results":{"test":[]}});
        store.put(STATE_KEY, legacy.clone(), None).await.unwrap();
        let loaded = load(&store).await.unwrap().unwrap();
        assert_eq!(loaded.value, legacy);
        assert_eq!(loaded.version, None);
        save(&store, &json!({"results":{"new":[]}}), None)
            .await
            .unwrap();
        assert_eq!(store.get(STATE_KEY).await.unwrap().unwrap().value, legacy);
    }

    #[tokio::test]
    async fn escaped_unicode_near_maximum_capacity_round_trips_with_bounded_frames() {
        let store = Memory::default();
        let state = json!({"text":"绘图\\\"\n".repeat(20_000)});
        assert!(serde_json::to_vec(&state).unwrap().len() > 64 * 1024);
        save(&store, &state, None).await.unwrap();
        assert_eq!(load(&store).await.unwrap().unwrap().value, state);
    }

    #[tokio::test]
    async fn interrupted_inactive_bank_never_replaces_the_last_complete_snapshot() {
        let store = Memory::default();
        let old = json!({"text":"a".repeat(70_000)});
        let version = save(&store, &old, None).await.unwrap();
        *store.fail_at.lock().unwrap() = Some(*store.writes.lock().unwrap() + 2);
        assert!(
            save(&store, &json!({"text":"b".repeat(70_000)}), Some(version))
                .await
                .is_err()
        );
        assert_eq!(load(&store).await.unwrap().unwrap().value, old);
        *store.fail_at.lock().unwrap() = None;
        save(&store, &json!({"text":"c".repeat(70_000)}), Some(version))
            .await
            .unwrap();
        assert_eq!(
            load(&store).await.unwrap().unwrap().value["text"]
                .as_str()
                .unwrap()
                .len(),
            70_000
        );
    }

    #[tokio::test]
    async fn invalid_or_missing_chunks_are_not_empty_successes() {
        let store = Memory::default();
        save(&store, &json!({"text":"x".repeat(70_000)}), None)
            .await
            .unwrap();
        store
            .records
            .lock()
            .unwrap()
            .get_mut(&key(0, 0))
            .unwrap()
            .value["data"] = json!("tampered");
        assert!(load(&store).await.is_err());
    }

    #[tokio::test]
    async fn stale_version_is_rejected_before_writing_any_chunk() {
        let store = Memory::default();
        let value = json!({"text":"ok"});
        save(&store, &value, None).await.unwrap();
        let before = *store.writes.lock().unwrap();
        assert!(save(&store, &value, None).await.is_err());
        assert_eq!(*store.writes.lock().unwrap(), before);
    }

    #[tokio::test]
    async fn full_capacity_saves_reuse_a_bounded_number_of_keys() {
        let store = Memory::default();
        let mut version = None;
        for n in 0..6 {
            let value =
                json!({"text":char::from(b'a'+n).to_string().repeat(MAXIMUM_SNAPSHOT_BYTES-11)});
            assert_eq!(
                serde_json::to_vec(&value).unwrap().len(),
                MAXIMUM_SNAPSHOT_BYTES
            );
            version = Some(save(&store, &value, version).await.unwrap());
            assert_eq!(load(&store).await.unwrap().unwrap().value, value);
        }
        let records = store.records.lock().unwrap();
        assert!(records.len() <= 33);
        assert!(
            records
                .values()
                .map(|r| serde_json::to_vec(&r.value).unwrap().len())
                .sum::<usize>()
                < 2 * 1024 * 1024
        );
    }
}
