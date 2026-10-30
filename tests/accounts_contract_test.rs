//! Exercise the compiled plugin over the SDK's real framed IPC, not a browser mock.
use gateway_plugin_sdk::call::data::{
    AccountFacts, AccountFactsPage, AccountFactsQuery, ACCOUNTS_LIST,
};
use gateway_plugin_sdk::client::{read_frame, write_frame};
use gateway_plugin_sdk::{
    CallContext, ErrorCode, Frame, Handshake, Message, PluginFault, Stage, PROTOCOL_VERSION,
};
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

#[derive(Clone, Copy)]
enum ResponseMode {
    Payload,
    MetadataOnly,
    Failed,
    Paginated,
    RepeatedCursor,
}

async fn accounts_roundtrip(mode: ResponseMode) -> Frame {
    let mut child = Command::new(env!("CARGO_BIN_EXE_codex-proxy-plugin-candy-eval"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn compiled plugin");
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
    write_frame(&mut input, &Frame::control(Message::Call {
        id: 1, method: "management.handle".into(), context,
        params: json!({"method":"GET", "path":"api/accounts", "query":"", "content_type":null, "headers":[]}),
    })).await.unwrap();
    let mut account_queries = 0;
    loop {
        let frame = read_frame(&mut output).await.unwrap();
        match &frame.message {
            Message::Callback {
                id, method, params, ..
            } if method == ACCOUNTS_LIST => {
                account_queries += 1;
                assert_eq!(params, &json!({}), "account query metadata must be empty");
                let query: AccountFactsQuery = serde_json::from_slice(&frame.payload)
                    .expect("native account query must use the binary payload");
                assert_eq!(query.provider_id, None);
                assert_eq!(query.limit, 200);
                assert_eq!(
                    query.cursor.as_deref(),
                    if account_queries == 1 {
                        None
                    } else {
                        Some("test-next")
                    }
                );
                let page = AccountFactsPage {
                    schema_version: 1,
                    accounts: vec![AccountFacts {
                        account_id: format!("test-account-{account_queries}"),
                        provider_id: "codex".into(),
                        name: "测试账号".into(),
                        email: None,
                        group_ids: vec![],
                        enabled: true,
                        updated_at_ms: 0,
                    }],
                    next_cursor: match mode {
                        ResponseMode::Paginated if account_queries == 1 => Some("test-next".into()),
                        ResponseMode::RepeatedCursor => Some("test-next".into()),
                        _ => None,
                    },
                };
                let reply = match mode {
                    ResponseMode::Failed => Frame::control(Message::Error {
                        id: *id,
                        error: PluginFault::new(ErrorCode::Fault, "synthetic-host-error-detail"),
                    }),
                    ResponseMode::MetadataOnly => Frame::control(Message::Result {
                        id: *id,
                        result: serde_json::to_value(page).unwrap(),
                    }),
                    _ => Frame {
                        message: Message::Result {
                            id: *id,
                            result: json!({}),
                        },
                        payload: serde_json::to_vec(&page).unwrap(),
                    },
                };
                write_frame(&mut input, &reply).await.unwrap();
            }
            Message::Callback { id, method, .. } => {
                // State loading is deliberately unavailable in this isolated host.
                // It must not prevent a fresh account query from succeeding.
                assert_eq!(method, "host.state.get");
                write_frame(
                    &mut input,
                    &Frame::control(Message::Error {
                        id: *id,
                        error: PluginFault::new(ErrorCode::Fault, "synthetic-state-unavailable"),
                    }),
                )
                .await
                .unwrap();
            }
            Message::Result { id: 1, .. } | Message::Error { id: 1, .. } => {
                assert!(account_queries > 0, "must read the host account inventory");
                child.kill().await.unwrap();
                return frame;
            }
            other => panic!("unexpected IPC message: {other:?}"),
        }
    }
}

async fn bounded(mode: ResponseMode) -> Frame {
    tokio::time::timeout(Duration::from_secs(15), accounts_roundtrip(mode))
        .await
        .expect("IPC deadline")
}

#[tokio::test]
async fn native_payload_account_is_exposed_as_a_cpr_account() {
    let response = bounded(ResponseMode::Payload).await;
    assert!(matches!(response.message, Message::Result { .. }));
    let body: Value = serde_json::from_slice(&response.payload).unwrap();
    assert_eq!(body["accounts"].as_array().unwrap().len(), 1);
    assert_eq!(body["accounts"][0]["source"], "cpr_accounts");
    assert_eq!(body["accounts"][0]["disabled"], false);
}

#[tokio::test]
async fn all_native_account_pages_are_preserved() {
    let response = bounded(ResponseMode::Paginated).await;
    let body: Value = serde_json::from_slice(&response.payload).unwrap();
    assert_eq!(body["accounts"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn malformed_or_failed_account_reads_are_not_empty_successes() {
    for mode in [
        ResponseMode::MetadataOnly,
        ResponseMode::Failed,
        ResponseMode::RepeatedCursor,
    ] {
        let response = bounded(mode).await;
        match response.message {
            Message::Error { error, .. } => {
                assert_eq!(error.message, "读取 CPR 宿主账号失败，请重试或检查宿主回调");
                assert!(!error.message.contains("synthetic-host-error-detail"));
            }
            _ => panic!("account failures must not produce a successful empty inventory"),
        }
    }
}
