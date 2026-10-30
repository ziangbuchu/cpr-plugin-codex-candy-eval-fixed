//! 宿主回调封装：账号事实、模型目录、状态持久化与按账号执行的模型流式调用。

use std::time::{Duration, Instant};

use gateway_plugin_sdk::{
    call::{
        data::{AccountFacts, AccountFactsQuery},
        host::{
            ClientKey, KeyListRequest, KeyListResult, ModelEventBatch, ModelExecuteRequest,
            ModelListRequest, ModelListResult, ModelOperation, ModelStreamReadResult,
            ModelStreamResult, StateGetRequest, StateGetResult, StatePutRequest, StatePutResult,
            StreamClose,
        },
        model::{CanonicalEvent, ExecutionEvent, WirePayload},
    },
    client::{HostClient, SessionError},
    ErrorCode, PluginFault,
};
use serde::{de::DeserializeOwned, Serialize};

/// 单次探测的读取上限，防止异常上游把整段流堆进内存。
const MAXIMUM_STREAM_BYTES: usize = 8 * 1024 * 1024;
/// 单次读取分块上限，宿主自身也有更小的硬上限。
const STREAM_CHUNK_BYTES: u32 = 64 * 1024;
/// CPR 的 OpenAI Responses 协议标识；宿主与 Provider 均以 `openai` 为准。
const OPENAI_PROTOCOL: &str = "openai";

pub(crate) async fn metadata<I, O>(
    host: &HostClient,
    method: &str,
    input: &I,
) -> Result<O, PluginFault>
where
    I: Serialize,
    O: DeserializeOwned,
{
    let reply = host
        .call(
            method,
            serde_json::to_value(input).map_err(|_| invalid_callback())?,
            Vec::new(),
        )
        .await
        .map_err(SessionError::into_plugin_fault)?;
    serde_json::from_value(reply.result).map_err(|_| invalid_callback())
}

fn invalid_callback() -> PluginFault {
    PluginFault::new(ErrorCode::Fault, "宿主回调返回了无效响应")
}

// ------------------------------------------------------------------ 账号与目录

/// 分页读取全部账号事实；`provider_id` 为空时读取所有 Provider。
pub async fn list_accounts(
    host: &HostClient,
    provider_id: Option<&str>,
) -> Result<Vec<AccountFacts>, PluginFault> {
    let mut accounts = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..32 {
        let page = host
            .account_facts(AccountFactsQuery {
                provider_id: provider_id.map(str::to_owned),
                cursor: cursor.clone(),
                limit: 200,
            })
            .await?;
        accounts.extend(page.accounts);
        match page.next_cursor {
            Some(next) if !next.is_empty() && cursor.as_ref() != Some(&next) => {
                cursor = Some(next);
            }
            Some(next) if !next.is_empty() => return Err(invalid_callback()),
            _ => return Ok(accounts),
        }
    }
    // 不把达到分页上限的部分清单伪装成完整账号列表。
    Err(invalid_callback())
}

/// 按本次选择的 Key 查询可见模型；不读取或返回 Key 明文。
pub async fn list_models(
    host: &HostClient,
    client_key_id: &str,
) -> Result<Vec<String>, PluginFault> {
    let result: ModelListResult = metadata(
        host,
        "host.models.list",
        &ModelListRequest {
            client_key_id: client_key_id.to_owned(),
            protocol: OPENAI_PROTOCOL.to_owned(),
            client_version: format!("nmbb.codex-candy-eval/{}", env!("CARGO_PKG_VERSION")),
        },
    )
    .await?;
    Ok(result.models)
}

/// 分页读取 Client Key 的非秘密信息，用于取得探测身份。
pub async fn list_client_keys(host: &HostClient) -> Result<Vec<ClientKey>, PluginFault> {
    let mut keys = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..32 {
        let page: KeyListResult = metadata(
            host,
            "host.keys.list",
            &KeyListRequest {
                cursor: cursor.clone(),
                limit: 200,
            },
        )
        .await?;
        keys.extend(page.keys);
        match page.next_cursor {
            Some(next) if !next.is_empty() => cursor = Some(next),
            _ => break,
        }
    }
    Ok(keys)
}

/// Management / CLI 调用没有父请求，必须使用显式的非秘密 Client Key ID。
/// 只读取宿主 Key 目录，不获取 Key 明文，也不修改账号或 Key 权限。
pub async fn execution_key(host: &HostClient) -> Result<String, PluginFault> {
    let keys = list_client_keys(host)
        .await
        .map_err(|error| PluginFault::new(error.code, "读取 CPR 执行 Key 目录失败"))?;
    keys.into_iter()
        .find(|key| key.enabled)
        .map(|key| key.id)
        .ok_or_else(|| {
            PluginFault::new(
                gateway_plugin_sdk::ErrorCode::InvalidInput,
                "CPR 中没有启用的 API Key，无法建立模型测试执行身份",
            )
        })
}

// ------------------------------------------------------------------ 状态

pub async fn get_state(
    host: &HostClient,
    namespace: &str,
    key: &str,
) -> Result<StateGetResult, PluginFault> {
    metadata(
        host,
        "host.state.get",
        &StateGetRequest {
            namespace: namespace.to_owned(),
            key: key.to_owned(),
        },
    )
    .await
}

pub async fn put_state(
    host: &HostClient,
    request: &StatePutRequest,
) -> Result<StatePutResult, PluginFault> {
    metadata(host, "host.state.put", request).await
}

// ------------------------------------------------------------------ 模型执行

/// 一次模型回答的可观测结果。
#[derive(Debug, Clone, Default)]
pub struct ModelAnswer {
    pub text: String,
    pub reasoning: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_tokens: i64,
    pub elapsed: Duration,
}

/// 模型响应的终止状态，用于区分"回答为空"与"上游未完成"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelFailure {
    /// 上游返回了明确的失败终态。
    Incomplete,
    /// 模型请求了工具而没有直接回答。
    ToolCall,
    /// 流在完成前中断。
    Truncated,
    /// 调用本身失败（网络、额度、权限等）。
    Host,
}

#[derive(Debug)]
pub struct ModelError {
    pub kind: ModelFailure,
    pub message: String,
}

impl std::fmt::Display for ModelError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl ModelError {
    fn host(error: PluginFault) -> Self {
        Self {
            kind: ModelFailure::Host,
            message: format!("模型请求失败：{}", error.message),
        }
    }

    pub(crate) fn other(kind: ModelFailure, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

/// 在指定账号上执行一次 Codex 轮次请求，读取到终态并返回回答正文。
pub async fn execute_codex_turn(
    host: &impl crate::execution::ExecutionContext,
    account_id: &str,
    provider: &str,
    model: &str,
    effort: &str,
    prompt: &str,
) -> Result<ModelAnswer, ModelError> {
    let turn = crate::codex::codex_turn(account_id, model, effort, prompt);
    host.execute(account_id, provider, model, &turn.body).await
}

/// 按给定 Responses 请求体在指定账号上执行一次模型调用。
pub async fn execute_body(
    host: &HostClient,
    account_id: &str,
    provider: &str,
    model: &str,
    body: &serde_json::Value,
) -> Result<ModelAnswer, ModelError> {
    let body = serde_json::to_vec(body)
        .map_err(|_| ModelError::other(ModelFailure::Host, "请求载荷无法序列化"))?;
    let started = Instant::now();

    let request = ModelExecuteRequest {
        client_key_id: Some(execution_key(host).await.map_err(ModelError::host)?),
        model: model.to_owned(),
        protocol: OPENAI_PROTOCOL.to_owned(),
        operation: ModelOperation::Generate,
        provider: Some(provider.to_owned()),
        account_id: Some(account_id.to_owned()),
        previous_response_id: None,
    };
    // 流式路径：取得流句柄后逐块读取到终态。
    let (stream_result, _): (ModelStreamResult, Vec<u8>) =
        call_with_payload(host, "host.model.execute_stream", &request, body)
            .await
            .map_err(ModelError::host)?;

    let answer = read_stream(host, &stream_result.stream, started).await;
    let _ = close_stream(host, &stream_result.stream).await;
    answer
}

async fn call_with_payload<I, O>(
    host: &HostClient,
    method: &str,
    input: &I,
    payload: Vec<u8>,
) -> Result<(O, Vec<u8>), PluginFault>
where
    I: Serialize,
    O: DeserializeOwned,
{
    let reply = host
        .call(
            method,
            serde_json::to_value(input).map_err(|_| invalid_callback())?,
            payload,
        )
        .await
        .map_err(SessionError::into_plugin_fault)?;
    let result = serde_json::from_value(reply.result).map_err(|_| invalid_callback())?;
    Ok((result, reply.payload))
}

async fn close_stream(host: &HostClient, stream: &str) -> Result<(), PluginFault> {
    let _: serde_json::Value = metadata(
        host,
        "host.model.stream_close",
        &StreamClose {
            stream: stream.to_owned(),
        },
    )
    .await?;
    Ok(())
}

/// 读取模型执行流到终态，拼装 SSE 帧并解析出最终响应。
async fn read_stream(
    host: &HostClient,
    stream: &str,
    started: Instant,
) -> Result<ModelAnswer, ModelError> {
    let mut sse = Vec::new();
    let mut closed = false;
    for _ in 0..4096 {
        let reply = host
            .call(
                "host.model.stream_read",
                serde_json::json!({
                    "stream": stream,
                    "maximum_bytes": STREAM_CHUNK_BYTES,
                }),
                Vec::new(),
            )
            .await
            .map_err(|error| ModelError::host(SessionError::into_plugin_fault(error)))?;
        let result: ModelStreamReadResult = serde_json::from_value(reply.result)
            .map_err(|_| ModelError::other(ModelFailure::Host, "模型响应流返回了无效响应"))?;
        if !reply.payload.is_empty() {
            let batch = ModelEventBatch::decode(&reply.payload)
                .map_err(|_| ModelError::other(ModelFailure::Host, "模型响应流事件批次无效"))?;
            for event in batch.events {
                append_event(&mut sse, &event)?;
            }
        }
        if sse.len() > MAXIMUM_STREAM_BYTES {
            return Err(ModelError::other(
                ModelFailure::Host,
                "模型响应超过读取上限，已中断",
            ));
        }
        if result.end {
            closed = true;
            break;
        }
    }
    if !closed {
        return Err(ModelError::other(
            ModelFailure::Truncated,
            "模型响应流在完成前中断",
        ));
    }
    parse_response(&sse, started)
}

/// 把事件中的原始 SSE 字节追加到缓冲；无 raw_sse 时由解析后的字段重建帧。
fn append_event(sse: &mut Vec<u8>, event: &ExecutionEvent) -> Result<(), ModelError> {
    if let Some(wire) = &event.wire {
        match &wire.payload {
            WirePayload::Json {
                raw_sse: Some(raw), ..
            } => {
                sse.extend_from_slice(raw);
                if !raw.ends_with(b"\n") {
                    sse.push(b'\n');
                }
                return Ok(());
            }
            WirePayload::Json {
                event: name,
                data,
                id,
                retry,
                raw_sse: None,
            } => {
                if let Some(name) = name {
                    sse.extend_from_slice(format!("event: {name}\n").as_bytes());
                }
                if let Some(id) = id {
                    sse.extend_from_slice(format!("id: {id}\n").as_bytes());
                }
                if let Some(retry) = retry {
                    sse.extend_from_slice(format!("retry: {retry}\n").as_bytes());
                }
                sse.extend_from_slice(format!("data: {data}\n\n").as_bytes());
                return Ok(());
            }
            WirePayload::RawSse { frame } => {
                sse.extend_from_slice(frame);
                if !frame.ends_with(b"\n") {
                    sse.push(b'\n');
                }
                return Ok(());
            }
            WirePayload::WebSocket { data, message, .. } => {
                let data = match data {
                    Some(data) => data.clone(),
                    None => serde_json::from_slice(message).map_err(|_| {
                        ModelError::other(ModelFailure::Host, "模型 WebSocket 消息不是有效 JSON")
                    })?,
                };
                sse.extend_from_slice(format!("data: {data}\n\n").as_bytes());
                return Ok(());
            }
            WirePayload::RawJson { body } | WirePayload::RawBody { body } => {
                sse.extend_from_slice(body);
                if !body.ends_with(b"\n") {
                    sse.push(b'\n');
                }
                return Ok(());
            }
        }
    }
    // 没有 wire 帧时退回规范事件，至少能取到文本与用量。
    for fact in &event.facts {
        if let CanonicalEvent::TextDelta { text, .. } = fact {
            let data = serde_json::json!({
                "type": "response.output_text.delta",
                "delta": text,
            });
            sse.extend_from_slice(format!("data: {data}\n\n").as_bytes());
        }
    }
    Ok(())
}

#[derive(Default)]
struct ResponseBody {
    status: String,
    error: Option<serde_json::Value>,
    output: Vec<serde_json::Value>,
    input_tokens: i64,
    output_tokens: i64,
    reasoning_tokens: i64,
}

/// 解析 Responses 事件流，返回最终回答；同时保留推理正文与用量。
pub(crate) fn parse_response(sse: &[u8], started: Instant) -> Result<ModelAnswer, ModelError> {
    let text = String::from_utf8_lossy(sse);
    let mut items: Vec<serde_json::Value> = Vec::new();
    let mut reasoning = String::new();
    let mut body = ResponseBody::default();
    let mut final_seen = false;

    for line in text.lines() {
        let line = line.trim();
        let Some(data) = line.strip_prefix("data:") else {
            continue;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let Ok(event) = serde_json::from_str::<serde_json::Value>(data) else {
            continue;
        };
        match event
            .get("type")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
        {
            "response.output_item.done" => {
                if let Some(item) = event.get("item") {
                    items.push(item.clone());
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(delta) = event.get("delta").and_then(|value| value.as_str()) {
                    reasoning.push_str(delta);
                }
            }
            "response.completed" | "response.incomplete" | "response.failed" => {
                if let Some(response) = event.get("response") {
                    body.status = response
                        .get("status")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default()
                        .to_owned();
                    body.error = response.get("error").cloned();
                    body.output = response
                        .get("output")
                        .and_then(|value| value.as_array())
                        .cloned()
                        .unwrap_or_default();
                    if let Some(usage) = response.get("usage") {
                        body.input_tokens = usage
                            .get("input_tokens")
                            .and_then(serde_json::Value::as_i64)
                            .unwrap_or_default();
                        body.output_tokens = usage
                            .get("output_tokens")
                            .and_then(serde_json::Value::as_i64)
                            .unwrap_or_default();
                        body.reasoning_tokens = usage
                            .get("output_tokens_details")
                            .and_then(|details| details.get("reasoning_tokens"))
                            .and_then(serde_json::Value::as_i64)
                            .unwrap_or_default();
                    }
                }
                final_seen = true;
            }
            "error" => {
                return Err(ModelError::other(
                    ModelFailure::Incomplete,
                    format!("模型返回错误：{}", truncate(data, 300)),
                ));
            }
            _ => {}
        }
    }

    if !final_seen {
        return Err(ModelError::other(
            ModelFailure::Truncated,
            "模型响应流在完成前中断",
        ));
    }

    // Codex 会流式下发 output item，但终态响应里的 output 可能为空。
    if body.output.is_empty() {
        body.output = items;
    }

    if !body.status.is_empty() && body.status != "completed" {
        let detail = body
            .error
            .as_ref()
            .filter(|value| !value.is_null())
            .map(|value| truncate(&value.to_string(), 300))
            .unwrap_or_default();
        return Err(ModelError::other(
            ModelFailure::Incomplete,
            format!("模型响应未完成（{}）：{}", body.status, detail),
        ));
    }
    if let Some(error) = body.error.as_ref().filter(|value| !value.is_null()) {
        return Err(ModelError::other(
            ModelFailure::Incomplete,
            format!("模型响应未完成：{}", truncate(&error.to_string(), 300)),
        ));
    }

    let mut answer = String::new();
    for item in &body.output {
        if item.get("type").and_then(|value| value.as_str()) != Some("message") {
            continue;
        }
        if let Some(parts) = item.get("content").and_then(|value| value.as_array()) {
            for part in parts {
                let kind = part
                    .get("type")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default();
                if kind == "output_text" || kind == "text" {
                    if let Some(value) = part.get("text").and_then(|value| value.as_str()) {
                        answer.push_str(value);
                    }
                }
            }
        }
    }

    if answer.trim().is_empty() {
        // Codex 轮次会声明工具，function_call / custom_tool_call 表示模型请求了工具。
        if body.output.iter().any(|item| {
            item.get("type")
                .and_then(|value| value.as_str())
                .is_some_and(|kind| kind.ends_with("_call"))
        }) {
            return Err(ModelError::other(
                ModelFailure::ToolCall,
                "模型调用了工具，没有直接回答",
            ));
        }
        return Err(ModelError::other(
            ModelFailure::Incomplete,
            "模型未返回文本",
        ));
    }

    Ok(ModelAnswer {
        text: answer,
        reasoning,
        input_tokens: body.input_tokens,
        output_tokens: body.output_tokens,
        reasoning_tokens: body.reasoning_tokens,
        elapsed: started.elapsed(),
    })
}

pub fn truncate(value: &str, maximum: usize) -> String {
    if value.chars().count() <= maximum {
        return value.to_owned();
    }
    let mut out: String = value.chars().take(maximum).collect();
    out.push('…');
    out
}

/// 收集类探测最多重试两次，退避 2s / 4s。
pub async fn execute_probe(
    host: &impl crate::execution::ExecutionContext,
    account_id: &str,
    provider: &str,
    model: &str,
    effort: &str,
    prompt: &str,
) -> (Result<ModelAnswer, ModelError>, u32) {
    execute_probe_body(host, account_id, provider, model, || {
        crate::codex::codex_turn(account_id, model, effort, prompt).body
    })
    .await
}

/// 同 [`execute_probe`]，但请求体由调用方提供（指纹探测使用精简体）。
pub async fn execute_probe_body<F>(
    host: &impl crate::execution::ExecutionContext,
    account_id: &str,
    provider: &str,
    model: &str,
    body: F,
) -> (Result<ModelAnswer, ModelError>, u32)
where
    F: Fn() -> serde_json::Value,
{
    let mut attempts = 0u32;
    let mut last: Option<ModelError> = None;
    for attempt in 0..3u32 {
        attempts += 1;
        let outcome = tokio::time::timeout(
            PROBE_TIMEOUT,
            host.execute(account_id, provider, model, &body()),
        )
        .await;
        let outcome = match outcome {
            Ok(result) => result,
            Err(_) => {
                last = Some(ModelError::other(
                    ModelFailure::Host,
                    "模型 3 分钟内未完成回答，已中断请求",
                ));
                if attempt == 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(2u64 << attempt)).await;
                continue;
            }
        };
        match outcome {
            Ok(answer) => return (Ok(answer), attempts),
            Err(error) => {
                // 工具调用、内容不完整属于确定性结果，重试无意义。
                let retryable = error.kind == ModelFailure::Host;
                last = Some(error);
                if !retryable || attempt == 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(2u64 << attempt)).await;
            }
        }
    }
    (
        Err(last.unwrap_or_else(|| ModelError::other(ModelFailure::Host, "模型请求失败"))),
        attempts,
    )
}

/// 探测用模型名不含推理强度后缀。
pub fn valid_model_name(model: &str) -> bool {
    !model.is_empty()
        && model.chars().count() <= 200
        && !model.chars().any(char::is_control)
        && !model.contains(['(', ')'])
}

/// 单次探测期限：模型回答最长 3 分钟。
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(180);

#[cfg(test)]
mod tests {
    use super::*;

    fn sse(events: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for event in events {
            out.extend_from_slice(format!("data: {event}\n\n").as_bytes());
        }
        out
    }

    #[test]
    fn decodes_websocket_batches_with_or_without_json_view() {
        use gateway_plugin_sdk::call::model::WireEvent;
        assert_eq!(gateway_plugin_sdk::PROTOCOL_VERSION, 6);
        let response = serde_json::json!({
            "type": "response.completed",
            "response": {"status": "completed", "output": [{"type": "message", "content": [
                {"type": "output_text", "text": "21"}
            ]}], "usage": {"input_tokens": 10, "output_tokens": 4}}
        });
        for data in [None, Some(response.clone())] {
            let encoded = ModelEventBatch {
                events: vec![ExecutionEvent::wire(WireEvent {
                    protocol: "openai".into(),
                    payload: WirePayload::WebSocket {
                        event: Some("response.completed".into()),
                        data,
                        message: serde_json::to_vec_pretty(&response).unwrap(),
                    },
                })],
            }
            .encode()
            .unwrap();
            let batch = ModelEventBatch::decode(&encoded).unwrap();
            let mut buffer = Vec::new();
            append_event(&mut buffer, &batch.events[0]).unwrap();
            let answer = parse_response(&buffer, Instant::now()).unwrap();
            assert_eq!(answer.text, "21");
            assert_eq!(answer.input_tokens, 10);
            assert_eq!(answer.output_tokens, 4);
        }
    }

    #[test]
    fn rejects_websocket_message_without_valid_json() {
        use gateway_plugin_sdk::call::model::WireEvent;
        let event = ExecutionEvent::wire(WireEvent {
            protocol: "openai".into(),
            payload: WirePayload::WebSocket {
                event: None,
                data: None,
                message: b"not-json".to_vec(),
            },
        });
        let error = append_event(&mut Vec::new(), &event).unwrap_err();
        assert_eq!(error.kind, ModelFailure::Host);
    }

    #[test]
    fn parses_answer_from_streamed_items() {
        let raw = sse(&[
            r#"{"type":"response.output_item.done","item":{"type":"message","content":[{"type":"output_text","text":"21"}]}}"#,
            r#"{"type":"response.completed","response":{"status":"completed","output":[],"usage":{"input_tokens":10,"output_tokens":4,"output_tokens_details":{"reasoning_tokens":2}}}}"#,
        ]);
        let answer = parse_response(&raw, Instant::now()).expect("应当解析成功");
        assert_eq!(answer.text, "21");
        assert_eq!(answer.input_tokens, 10);
        assert_eq!(answer.reasoning_tokens, 2);
    }

    #[test]
    fn rejects_tool_call_without_text() {
        let raw = sse(&[
            r#"{"type":"response.completed","response":{"status":"completed","output":[{"type":"function_call"}]}}"#,
        ]);
        let error = parse_response(&raw, Instant::now()).expect_err("工具调用应判定失败");
        assert_eq!(error.kind, ModelFailure::ToolCall);
    }

    #[test]
    fn rejects_incomplete_status() {
        let raw = sse(&[
            r#"{"type":"response.incomplete","response":{"status":"incomplete","error":{"message":"x"},"output":[]}}"#,
        ]);
        let error = parse_response(&raw, Instant::now()).expect_err("未完成应判定失败");
        assert_eq!(error.kind, ModelFailure::Incomplete);
    }

    #[test]
    fn rejects_stream_without_terminal_event() {
        let raw = sse(&[r#"{"type":"response.output_text.delta","delta":"a"}"#]);
        let error = parse_response(&raw, Instant::now()).expect_err("缺少终态应判定失败");
        assert_eq!(error.kind, ModelFailure::Truncated);
    }

    #[test]
    fn collects_reasoning_deltas() {
        let raw = sse(&[
            r#"{"type":"response.reasoning_summary_text.delta","delta":"想"}"#,
            r#"{"type":"response.reasoning_summary_text.delta","delta":"了想"}"#,
            r#"{"type":"response.completed","response":{"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"21"}]}]}}"#,
        ]);
        let answer = parse_response(&raw, Instant::now()).expect("应当解析成功");
        assert_eq!(answer.reasoning, "想了想");
    }
}
