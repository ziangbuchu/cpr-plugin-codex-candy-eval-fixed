//! Real SDK frames + compiled plugin. No model execution and no private fixtures.
use gateway_plugin_sdk::client::{read_frame, write_frame};
use gateway_plugin_sdk::{
    CallContext, ErrorCode, Frame, Handshake, Message, PluginFault, Stage, PROTOCOL_VERSION,
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    process::Stdio,
    time::Duration,
};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

type Records = BTreeMap<String, (Value, u64)>;
struct Host {
    child: Child,
    input: ChildStdin,
    output: ChildStdout,
    records: Records,
    next_call: u64,
    writes: usize,
    fail_put: Option<usize>,
    fail_get: bool,
}
impl Host {
    async fn start(records: Records) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_codex-proxy-plugin-candy-eval"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
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
        Self {
            child,
            input,
            output,
            records,
            next_call: 1,
            writes: 0,
            fail_put: None,
            fail_get: false,
        }
    }
    async fn send(&mut self, method: &str, path: &str, body: &Value) -> u64 {
        let id = self.next_call;
        self.next_call += 2; // Native host calls are odd; plugin callbacks are even.
        let context = CallContext {
            request: None,
            call_id: id,
            instance_id: "test-instance".into(),
            generation: 1,
            incarnation: "test-incarnation".into(),
            stage: Stage::Management,
            timeout_ms: 10_000,
            resource_stream: false,
            resource_scope_id: format!("test-scope-{id}"),
            request_id: None,
            attempt_id: None,
            account_id: None,
            credential_revision: None,
        };
        let frame = Frame {
            message: Message::Call {
                id,
                method: "management.handle".into(),
                context,
                params: json!({"method":method,"path":path,"query":"","content_type":"application/json","headers":[]}),
            },
            payload: if body.is_null() {
                vec![]
            } else {
                serde_json::to_vec(body).unwrap()
            },
        };
        write_frame(&mut self.input, &frame).await.unwrap();
        id
    }
    async fn receive(&mut self, mut pending: BTreeSet<u64>) -> BTreeMap<u64, (u64, Value)> {
        let mut replies = BTreeMap::new();
        while !pending.is_empty() {
            let frame = tokio::time::timeout(Duration::from_secs(10), read_frame(&mut self.output))
                .await
                .unwrap()
                .unwrap();
            let (id, method, params) = match frame.message {
                Message::Result { id, result } if pending.remove(&id) => {
                    replies.insert(
                        id,
                        (
                            result["status"].as_u64().unwrap(),
                            serde_json::from_slice(&frame.payload).unwrap(),
                        ),
                    );
                    continue;
                }
                Message::Callback {
                    id, method, params, ..
                } => (id, method, params),
                other => panic!("unexpected IPC: {other:?}"),
            };
            let reply = match method.as_str() {
                "host.state.get" => {
                    assert!(frame.payload.is_empty());
                    if self.fail_get { Frame::control(Message::Error { id, error: PluginFault::new(ErrorCode::Fault,"synthetic-read-failure") }) }
                    else { Frame::control(Message::Result { id, result: json!({"record":self.records.get(params["key"].as_str().unwrap()).map(|(value,version)| json!({"value":value,"version":version,"schema_version":1}))}) }) }
                }
                "host.state.put" => {
                    assert!(frame.payload.is_empty(),"host state contract rejects a nonempty payload");
                    assert_eq!(params["namespace"], "candy-eval");
                    self.writes += 1;
                    if self.fail_put == Some(self.writes) { Frame::control(Message::Error { id, error: PluginFault::new(ErrorCode::Fault,"synthetic-write-failure") }) }
                    else {
                        let name = params["key"].as_str().unwrap();
                        assert_eq!(params["expected_version"].as_u64(), self.records.get(name).map(|(_,v)| *v));
                        let version = self.records.values().map(|(_,v)| *v).max().unwrap_or(0)+1;
                        self.records.insert(name.to_owned(), (params["value"].clone(),version));
                        Frame::control(Message::Result { id, result:json!({"version":version}) })
                    }
                }
                gateway_plugin_sdk::call::data::ACCOUNTS_LIST => Frame { message: Message::Result { id, result:json!({}) }, payload:serde_json::to_vec(&json!({"schema_version":1,"accounts":[{"account_id":"test-account","provider_id":"openai","name":"test-account","group_ids":[],"enabled":true,"updated_at_ms":0}],"next_cursor":null})).unwrap() },
                "host.keys.list" => Frame::control(Message::Result { id, result: json!({"keys":[],"next_cursor":null}) }),
                _ => panic!("unexpected host call; no models may be executed: {method}"),
            };
            // write_frame enforces the SDK 64KiB metadata limit on every reply, too.
            write_frame(&mut self.input, &reply).await.unwrap();
        }
        replies
    }
    async fn call(&mut self, method: &str, path: &str, value: &Value) -> (u64, Value) {
        let id = self.send(method, path, value).await;
        self.receive(BTreeSet::from([id]))
            .await
            .remove(&id)
            .unwrap()
    }
    async fn stop(mut self) -> Records {
        self.child.kill().await.unwrap();
        self.records
    }
}

fn snapshot(count: usize) -> Value {
    json!({"results":{"test-account":(0..count).map(|n|json!({"time":format!("2026-01-01T00:00:{n:02}Z"),"model":"test-model","effort":"low","ok":true,"answer":"绘图abc\\\"\n".repeat(350),"input_tokens":1,"output_tokens":1,"reasoning_tokens":0,"duration_ms":1})).collect::<Vec<_>>()},"pelicans":{},"fingerprints":{},"traces":{}})
}
fn legacy() -> Records {
    BTreeMap::from([("global".into(), (snapshot(2), 1))])
}

#[tokio::test]
async fn large_recovery_is_idempotent_and_survives_a_new_process() {
    let value = snapshot(18);
    assert!(serde_json::to_vec(&value).unwrap().len() > 64 * 1024);
    let old = Frame::control(Message::Callback {
        id: 1,
        parent_id: 1,
        method: "host.state.put".into(),
        params: json!({"namespace":"candy-eval","key":"global","value":value,"expected_version":1}),
    });
    // RPC v4 keeps the 64KiB metadata limit but moves business JSON into a data section.
    let mut wire = Vec::new();
    write_frame(&mut wire, &old).await.unwrap();
    let metadata_bytes = u32::from_be_bytes(wire[..4].try_into().unwrap()) as usize;
    let payload_bytes = u64::from_be_bytes(wire[4..12].try_into().unwrap());
    assert!(metadata_bytes <= 64 * 1024);
    assert!(
        payload_bytes > 64 * 1024,
        "large state must use the v4 data section"
    );
    let decoded = read_frame(&mut wire.as_slice()).await.unwrap();
    let Message::Callback { params, .. } = decoded.message else {
        panic!("expected state callback")
    };
    assert_eq!(
        params["value"], value,
        "v4 must preserve the complete large state"
    );
    let mut host = Host::start(legacy()).await;
    let (status, report) = host.call("POST", "api/storage/recover", &value).await;
    assert_eq!(status, 200, "{report}");
    assert_eq!(report["counts"]["results"], 18);
    let (status, report) = host.call("POST", "api/storage/recover", &value).await;
    assert_eq!(status, 200);
    assert_eq!(report["counts"]["results"], 18);
    assert_eq!(
        host.records["global"].0,
        snapshot(2),
        "legacy key is never overwritten"
    );
    let records = host.stop().await;
    let mut reloaded = Host::start(records).await;
    let (status, state) = reloaded.call("GET", "api/state", &Value::Null).await;
    assert_eq!(status, 200);
    assert_eq!(state["auths"][0]["results"].as_array().unwrap().len(), 18);
    assert_eq!(state["storage_error"], "");
    reloaded.stop().await;
}

#[tokio::test]
async fn failed_chunk_write_keeps_the_previous_snapshot_and_can_retry() {
    let mut host = Host::start(legacy()).await;
    assert_eq!(
        host.call("POST", "api/storage/recover", &snapshot(18))
            .await
            .0,
        200
    );
    let pointer = host.records["snapshot-v2"].clone();
    host.fail_put = Some(host.writes + 2);
    assert_eq!(
        host.call("POST", "api/storage/recover", &snapshot(19))
            .await
            .0,
        409
    );
    assert_eq!(host.records["snapshot-v2"], pointer);
    let records = host.stop().await;
    let mut reloaded = Host::start(records).await;
    let (_, state) = reloaded.call("GET", "api/state", &Value::Null).await;
    assert_eq!(state["auths"][0]["results"].as_array().unwrap().len(), 18);
    assert_eq!(
        reloaded
            .call("POST", "api/storage/recover", &snapshot(19))
            .await
            .0,
        200
    );
    reloaded.stop().await;
}

#[tokio::test]
async fn a_state_read_failure_never_becomes_an_empty_overwrite() {
    let mut host = Host::start(legacy()).await;
    host.fail_get = true;
    assert_eq!(
        host.call("POST", "api/storage/recover", &snapshot(18))
            .await
            .0,
        409
    );
    assert_eq!(host.writes, 0);
    host.stop().await;
}

#[tokio::test]
async fn concurrent_recoveries_are_merged_instead_of_overwriting_each_other() {
    let mut host = Host::start(legacy()).await;
    let first = snapshot(18);
    let mut second = snapshot(18);
    second["results"]["test-account"][17]["time"] = json!("2026-01-01T00:01:00Z");
    let a = host.send("POST", "api/storage/recover", &first).await;
    let b = host.send("POST", "api/storage/recover", &second).await;
    let replies = host.receive(BTreeSet::from([a, b])).await;
    assert!(replies.values().all(|(status, _)| *status == 200));
    let records = host.stop().await;
    let mut reloaded = Host::start(records).await;
    let (_, state) = reloaded.call("GET", "api/state", &Value::Null).await;
    assert_eq!(state["auths"][0]["results"].as_array().unwrap().len(), 19);
    reloaded.stop().await;
}
