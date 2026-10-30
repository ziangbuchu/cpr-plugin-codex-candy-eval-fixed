//! Exercise the compiled plugin through the official SDK framing, without a parent request.
use gateway_plugin_sdk::call::data::{
    AccountFacts, AccountFactsPage, AccountFactsQuery, ACCOUNTS_LIST,
};
use gateway_plugin_sdk::call::host::{
    ModelEventBatch, ModelExecuteRequest, ModelOperation, ModelStreamReadResult, ModelStreamResult,
    StatePutResult,
};
use gateway_plugin_sdk::call::model::{ExecutionEvent, WireEvent, WirePayload};
use gateway_plugin_sdk::client::{read_frame, write_frame};
use gateway_plugin_sdk::{
    CallContext, ErrorCode, Frame, Handshake, Message, PluginFault, Stage, PROTOCOL_VERSION,
};
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

#[derive(Clone, Copy)]
enum KeyCase {
    Enabled,
    Disabled,
    LookupFailure,
}

async fn run_contract(key_case: KeyCase) -> (Value, usize, bool) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_codex-proxy-plugin-candy-eval"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = child.stdout.take().unwrap();
    let handshake = Handshake {
        protocol_version: PROTOCOL_VERSION,
        artifact_sha256: "test-artifact".into(),
        plugin_id: "nmbb.codex-candy-eval".into(),
        instance_id: "test-instance".into(),
        generation: 1,
        incarnation: "test-incarnation".into(),
        configuration: json!({}),
        contributes: codex_proxy_plugin_candy_eval::manifest()
            .unwrap()
            .contributes,
    };
    write_frame(&mut input, &Frame::control(Message::Hello { handshake }))
        .await
        .unwrap();
    assert!(matches!(
        read_frame(&mut output).await.unwrap().message,
        Message::Ready { .. }
    ));
    let context = CallContext {
        request: None,
        call_id: 1,
        instance_id: "test-instance".into(),
        generation: 1,
        incarnation: "test-incarnation".into(),
        stage: Stage::Management,
        timeout_ms: 10_000,
        resource_stream: false,
        resource_scope_id: "test-scope".into(),
        request_id: None,
        attempt_id: None,
        account_id: None,
        credential_revision: None,
    };
    write_frame(&mut input, &Frame {
        message:Message::Call {
            id:1, method:"management.handle".into(), context,
            params:json!({"method":"POST","path":"api/run","query":"","content_type":"application/json","headers":[]}),
        },
        payload:br#"{"model":"gpt-6.1-sol","effort":"low","runs":1,"all":true}"#.to_vec(),
    }).await.unwrap();
    let mut executions = 0;
    let mut key_pages = 0;
    let mut stream_closed = false;
    let mut record = None;
    let mut states = std::collections::BTreeMap::<String, Value>::new();
    let mut returned = false;
    for _ in 0..100 {
        if record.is_some() && returned {
            break;
        }
        let frame = read_frame(&mut output).await.unwrap();
        let (id, method, params) = match frame.message {
            Message::Result { id, result } => {
                assert_eq!(id, 1);
                assert_eq!(result["status"], 200);
                let response: Value = serde_json::from_slice(&frame.payload).unwrap();
                assert_eq!(response["started"], 1);
                returned = true;
                continue;
            }
            Message::Callback {
                id, method, params, ..
            } => (id, method, params),
            _ => panic!("unexpected IPC message"),
        };
        let mut payload = Vec::new();
        // 宿主与 openai Provider 只接受 `openai` 协议名：Provider 编码阶段直接比对
        // `protocol != "openai"`，历史移植误用 `openai-response` 会让上游请求被判为
        // InvalidRequest（对普通用户表现为“invalid upstream request”）。
        if matches!(
            method.as_str(),
            "host.models.list" | "host.model.execute" | "host.model.execute_stream"
        ) {
            assert_eq!(
                params["protocol"].as_str(),
                Some("openai"),
                "{method} 必须使用 CPR 的 openai 协议标识"
            );
        }
        let result = match method.as_str() {
            "host.state.get" => {
                json!({"record": states.get(params["key"].as_str().unwrap()).map(|value| json!({"value":value,"version":1,"schema_version":1}))})
            }
            ACCOUNTS_LIST => {
                assert_eq!(params, json!({}));
                let _query: AccountFactsQuery = serde_json::from_slice(&frame.payload).unwrap();
                payload = serde_json::to_vec(&AccountFactsPage {
                    schema_version: 1,
                    accounts: vec![AccountFacts {
                        account_id: "test-account".into(),
                        provider_id: "openai".into(),
                        name: "fixture account".into(),
                        email: None,
                        group_ids: vec![],
                        enabled: true,
                        updated_at_ms: 0,
                    }],
                    next_cursor: None,
                })
                .unwrap();
                json!({})
            }
            "host.keys.list" => {
                key_pages += 1;
                assert!(frame.payload.is_empty());
                assert!(key_pages <= 8, "key pagination must terminate");
                match key_case {
                    KeyCase::LookupFailure => {
                        write_frame(
                            &mut input,
                            &Frame::control(Message::Error {
                                id,
                                error: PluginFault::new(
                                    ErrorCode::PermissionDenied,
                                    "fixture lookup denied",
                                ),
                            }),
                        )
                        .await
                        .unwrap();
                        continue;
                    }
                    KeyCase::Disabled => json!({
                        "keys":[{"id":"test-disabled-key","name":"fixture","enabled":false}],"next_cursor":null
                    }),
                    KeyCase::Enabled if params["cursor"].is_null() => {
                        assert_eq!(params["cursor"], Value::Null);
                        json!({"keys":[{"id":"test-disabled-key","name":"fixture","enabled":false}],"next_cursor":"test-next-page"})
                    }
                    KeyCase::Enabled => {
                        assert_eq!(params["cursor"], "test-next-page");
                        json!({"keys":[{"id":"test-enabled-key","name":"fixture","enabled":true}],"next_cursor":null})
                    }
                }
            }
            "host.model.execute_stream" => {
                executions += 1;
                let request: ModelExecuteRequest = serde_json::from_value(params).unwrap();
                assert_eq!(request.client_key_id.as_deref(), Some("test-enabled-key"));
                assert_eq!(request.account_id.as_deref(), Some("test-account"));
                assert_eq!(request.provider.as_deref(), Some("openai"));
                assert_eq!(request.protocol, "openai");
                assert_eq!(request.operation, ModelOperation::Generate);
                assert_eq!(request.model, "gpt-6.1-sol");
                let body: Value = serde_json::from_slice(&frame.payload).unwrap();
                assert_eq!(body["model"], "gpt-6.1-sol");
                assert_eq!(body["reasoning"]["effort"], "low");
                assert_eq!(body["stream"], true);
                serde_json::to_value(ModelStreamResult {
                    request_id: "test-request".into(),
                    stream: "test-model-stream".into(),
                })
                .unwrap()
            }
            "host.model.stream_read" => {
                assert_eq!(params["stream"], "test-model-stream");
                let response = json!({"type":"response.completed","response":{
                    "id":"test-response","status":"completed","model":"gpt-6.1-sol",
                    "output":[{"type":"message","content":[{"type":"output_text","text":"最少需要 21 颗糖果。"}]}],
                    "usage":{"input_tokens":42,"output_tokens":12,"output_tokens_details":{"reasoning_tokens":3}}
                }});
                let raw = format!("event: response.completed\ndata: {}\n\n", response);
                payload = ModelEventBatch {
                    events: vec![ExecutionEvent::wire(WireEvent {
                        protocol: "openai".to_owned(),
                        payload: WirePayload::RawSse {
                            frame: raw.into_bytes(),
                        },
                    })],
                }
                .encode()
                .unwrap();
                serde_json::to_value(ModelStreamReadResult {
                    events: 1,
                    end: true,
                })
                .unwrap()
            }
            "host.model.stream_close" => {
                assert_eq!(params["stream"], "test-model-stream");
                stream_closed = true;
                json!({})
            }
            "host.state.put" => {
                assert!(frame.payload.is_empty());
                let key = params["key"].as_str().unwrap();
                states.insert(key.to_owned(), params["value"].clone());
                if key == "snapshot-v2" {
                    let bank = params["value"]["bank"].as_u64().unwrap();
                    let chunks = params["value"]["chunks"].as_u64().unwrap();
                    let encoded = (0..chunks)
                        .map(|n| {
                            states[&format!("snapshot-{bank}-{n:02}")]["data"]
                                .as_str()
                                .unwrap()
                        })
                        .collect::<String>();
                    let snapshot: Value = serde_json::from_str(&encoded).unwrap();
                    record = snapshot["results"]["test-account"]
                        .as_array()
                        .unwrap()
                        .last()
                        .cloned();
                }
                serde_json::to_value(StatePutResult { version: 1 }).unwrap()
            }
            "host.model.list" => json!({"models":["gpt-6.1-sol"]}),
            _ => panic!("unexpected IPC callback method: {method}"),
        };
        write_frame(
            &mut input,
            &Frame {
                message: Message::Result { id, result },
                payload,
            },
        )
        .await
        .unwrap();
    }
    assert!(returned, "management reply must complete");
    (
        record.expect("test outcome must be persisted"),
        executions,
        stream_closed,
    )
}

async fn bounded(key_case: KeyCase) -> (Value, usize, bool) {
    tokio::time::timeout(Duration::from_secs(8), run_contract(key_case))
        .await
        .expect("IPC contract timeout")
}

#[tokio::test]
async fn management_model_request_has_explicit_identity_and_decodes_official_stream() {
    let (record, executions, closed) = bounded(KeyCase::Enabled).await;
    assert_eq!(executions, 1);
    assert!(closed);
    assert_eq!(record["ok"], true);
    assert_eq!(record["error"].as_str().unwrap_or(""), "");
    assert!(record["answer"].as_str().unwrap().contains("21"));
    assert_eq!(record["input_tokens"], 42);
    assert_eq!(record["output_tokens"], 12);
}

#[tokio::test]
async fn no_enabled_key_is_an_explicit_failure_not_an_implicit_child_request() {
    let (record, executions, closed) = bounded(KeyCase::Disabled).await;
    assert_eq!(executions, 0);
    assert!(!closed);
    assert_eq!(record["ok"], false);
    assert!(record["error"]
        .as_str()
        .unwrap()
        .contains("没有启用的 API Key"));
}

#[tokio::test]
async fn key_lookup_failure_never_falls_back_to_a_missing_parent_request() {
    let (record, executions, closed) = bounded(KeyCase::LookupFailure).await;
    assert_eq!(executions, 0);
    assert!(!closed);
    assert_eq!(record["ok"], false);
    assert!(record["error"]
        .as_str()
        .unwrap()
        .contains("读取 CPR 执行 Key 目录失败"));
}
