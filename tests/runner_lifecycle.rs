//! Real plugin-process lifecycle checks. All accounts, keys and model output are synthetic;
//! the background fixture uses loopback only and never reads production configuration.
#[path = "support/local_model.rs"]
mod local_model;
use gateway_plugin_sdk::client::{read_frame, write_frame};
use gateway_plugin_sdk::{CallContext, Frame, Handshake, Message, Stage};
use local_model::LocalModel;
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    process::Stdio,
    time::Duration,
};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

struct Fixture {
    _child: Child,
    input: ChildStdin,
    output: ChildStdout,
    states: BTreeMap<String, (Value, u64)>,
    replies: BTreeMap<u64, (u16, Value)>,
    held_reads: Vec<u64>,
    executions: usize,
    closed_calls: BTreeSet<u64>,
    http_bodies: BTreeMap<String, Vec<u8>>,
}

impl Fixture {
    async fn new() -> Self {
        Self::configured(json!({})).await
    }

    async fn configured(configuration: Value) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_codex-proxy-plugin-candy-eval"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let mut fixture = Self {
            _child: child,
            input,
            output,
            states: BTreeMap::new(),
            replies: BTreeMap::new(),
            held_reads: Vec::new(),
            executions: 0,
            closed_calls: BTreeSet::new(),
            http_bodies: BTreeMap::new(),
        };
        fixture
            .send(
                Message::Hello {
                    handshake: Handshake {
                        protocol_version: gateway_plugin_sdk::PROTOCOL_VERSION,
                        artifact_sha256: "fixture-artifact".into(),
                        plugin_id: "nmbb.codex-candy-eval".into(),
                        instance_id: "fixture-instance".into(),
                        generation: 1,
                        incarnation: "fixture-incarnation".into(),
                        configuration,
                        contributes: codex_proxy_plugin_candy_eval::manifest()
                            .unwrap()
                            .contributes,
                    },
                },
                Vec::new(),
            )
            .await;
        let frame = fixture.read().await;
        assert!(matches!(frame.message, Message::Ready { .. }));
        fixture
    }

    async fn send(&mut self, message: Message, payload: Vec<u8>) {
        write_frame(&mut self.input, &Frame { message, payload })
            .await
            .unwrap();
    }

    async fn read(&mut self) -> Frame {
        tokio::time::timeout(Duration::from_secs(8), read_frame(&mut self.output))
            .await
            .expect("fixture deadline")
            .expect("plugin must remain alive")
    }

    async fn call(
        &mut self,
        id: u64,
        path: &str,
        body: Option<Value>,
        background: bool,
        timeout_ms: u64,
    ) {
        self.request(id, path, body, background, background, timeout_ms)
            .await;
    }

    async fn request(
        &mut self,
        id: u64,
        path: &str,
        body: Option<Value>,
        background: bool,
        confirmed: bool,
        timeout_ms: u64,
    ) {
        let (path, query) = path.split_once('?').unwrap_or((path, ""));
        let context = CallContext {
            request: None,
            call_id: id,
            instance_id: "fixture-instance".into(),
            generation: 1,
            incarnation: "fixture-incarnation".into(),
            stage: Stage::Management,
            timeout_ms,
            resource_stream: false,
            resource_scope_id: format!("fixture-scope-{id}"),
            request_id: None,
            attempt_id: None,
            account_id: None,
            credential_revision: None,
        };
        let _ = confirmed; // No patched-host confirmation header is sent.
        let headers = json!([]);
        self.send(
            Message::Call {
                id,
                method: "management.handle".into(),
                context,
                params: json!({
                    "method": if body.is_some() { "POST" } else { "GET" }, "path":path,
                    "query": if background { "background=1" } else { query },
                    "content_type": if body.is_some() { Some("application/json") } else { None },
                    "headers":headers,
                })
                .try_into()
                .unwrap(),
            },
            body.map(|value| serde_json::to_vec(&value).unwrap())
                .unwrap_or_default(),
        )
        .await;
    }

    async fn pump(&mut self) {
        let frame = self.read().await;
        let (id, method, params) = match frame.message {
            Message::Callback {
                id,
                parent_id,
                method,
                params,
            } => {
                assert!(
                    !self.closed_calls.contains(&parent_id),
                    "callback used a finished SDK scope"
                );
                (id, method, params)
            }
            Message::Result { id, result } => {
                self.closed_calls.insert(id);
                let body = serde_json::from_slice(&frame.payload).unwrap_or(Value::Null);
                self.replies
                    .insert(id, (result["status"].as_u64().unwrap_or(200) as u16, body));
                return;
            }
            Message::Error { id, error } => {
                self.replies
                    .insert(id, (500, json!({"fixture_fault":error.message})));
                return;
            }
            Message::Cancelled { .. } => return,
            other => panic!("unexpected fixture frame: {other:?}"),
        };
        let mut payload = Vec::new();
        let result = match method.as_str() {
            "host.groups.ensure" => {
                // Official native-resource callbacks use a payload, not metadata params.
                assert_eq!(params, json!({}));
                let request: gateway_plugin_sdk::call::resources::GroupEnsureRequest =
                    serde_json::from_slice(&frame.payload).unwrap();
                assert!(
                    request.color.len() == 9
                        && request.color.starts_with('#')
                        && request.color[1..].bytes().all(|b| b.is_ascii_hexdigit()),
                    "CPR requires #RRGGBBAA group colors"
                );
                payload = serde_json::to_vec(
                    &json!({"id":"fixture-group","name":"fixture group","enabled":true}),
                )
                .unwrap();
                json!({})
            }
            "host.groups.change_members" => {
                assert_eq!(params, json!({}));
                let request: gateway_plugin_sdk::call::resources::GroupMembersChange =
                    serde_json::from_slice(&frame.payload).unwrap();
                assert_eq!(request.add, ["fixture-account"]);
                assert!(request.remove.is_empty());
                payload = serde_json::to_vec(&json!({"added":1,"removed":0})).unwrap();
                json!({})
            }
            "host.keys.ensure" => {
                assert_eq!(params, json!({}));
                let request: gateway_plugin_sdk::call::resources::KeyEnsureRequest =
                    serde_json::from_slice(&frame.payload).unwrap();
                assert_eq!(request.group_resource_keys, ["background-fixture-account"]);
                assert_eq!(request.max_concurrency, 16);
                assert_eq!(request.requests_per_minute, 120);
                assert_eq!(request.daily_limit_usd, "5");
                assert_eq!(request.weekly_limit_usd, "20");
                payload = serde_json::to_vec(&json!({"id":"fixture-background-key","name":"fixture background key","enabled":true})).unwrap();
                json!({})
            }
            "host.http.dispatch" => {
                assert!(params["settings"].is_null());
                let uri = params["uri"].as_str().unwrap();
                let data = if uri.starts_with("/api/admin/accounts?") {
                    assert!(uri.contains("groupId=fixture-group"));
                    json!({"items":[{"id":"fixture-account"}],"page":{"page":1,"pageSize":2,"total":1,"totalPages":1}})
                } else if uri.starts_with("/api/admin/client-keys/reveal?") {
                    assert!(uri.contains("id=fixture-background-key"));
                    json!({"id":"fixture-background-key","plaintextKey":"fixture-only-not-a-real-key"})
                } else if uri.starts_with("/api/admin/client-keys?") {
                    json!({"items":[{"id":"fixture-background-key","enabled":true,"groups":[{"id":"fixture-group","enabled":true}],"maxConcurrency":16,"requestsPerMinute":120,"dailyLimitUsd":"5","weeklyLimitUsd":"20"}],"nextCursor":null})
                } else {
                    panic!("unexpected synthetic admin route")
                };
                let handle = format!("fixture-body-{id}");
                self.http_bodies.insert(
                    handle.clone(),
                    serde_json::to_vec(&json!({"data":data})).unwrap(),
                );
                json!({"status":200,"version":"HTTP/1.1","headers":[],"body":{"kind":"handle","handle":handle},"response":null,"session":false})
            }
            "host.http.body_read" => {
                if let Some(bytes) = self.http_bodies.remove(params["handle"].as_str().unwrap()) {
                    payload = bytes;
                    json!({"eof":false,"trailers":null})
                } else {
                    json!({"eof":true,"trailers":null})
                }
            }
            "host.http.body_close" => json!({}),
            "host.state.get" => {
                let record = self.states.get(params["key"].as_str().unwrap()).map(
                    |(value, version)| json!({"value":value,"version":version,"schema_version":1}),
                );
                json!({"record":record})
            }
            "host.state.put" => {
                let key = params["key"].as_str().unwrap().to_owned();
                let version = self.states.get(&key).map_or(1, |(_, version)| version + 1);
                self.states.insert(key, (params["value"].clone(), version));
                json!({"version":version})
            }
            "host.data.accounts.list" => {
                payload = serde_json::to_vec(&json!({"schema_version":1,"accounts":[{
                    "account_id":"fixture-account","provider_id":"openai","name":"fixture account",
                    "email":null,"group_ids":[],"enabled":true,"updated_at_ms":0,
                }],"next_cursor":null}))
                .unwrap();
                json!({})
            }
            "host.model.list" | "host.models.list" => json!({"models":["gpt-6.1-sol"]}),
            "host.keys.list" => {
                json!({"keys":[{"id":"fixture-key-id","name":"fixture","enabled":true}],"next_cursor":null})
            }
            "host.model.execute_stream" => {
                self.executions += 1;
                json!({"request_id":format!("fixture-request-{}", self.executions),"stream":format!("fixture-stream-{}", self.executions)})
            }
            "host.model.stream_read" => {
                self.held_reads.push(id);
                return;
            }
            "host.model.stream_close" => json!({}),
            other => panic!("unexpected fixture callback: {other}"),
        };
        self.send(Message::Result { id, result }, payload).await;
    }

    async fn reply(&mut self, id: u64) -> (u16, Value) {
        while !self.replies.contains_key(&id) {
            self.pump().await;
        }
        self.replies.remove(&id).unwrap()
    }

    async fn running(&mut self, id: u64) -> Value {
        self.call(id, "api/state", None, false, 5000).await;
        let (status, value) = self.reply(id).await;
        assert_eq!(status, 200);
        value["auths"][0].clone()
    }

    async fn start_trace(&mut self, timeout_ms: u64, background: bool) {
        self.call(
            1,
            "api/modeltrace/run",
            Some(json!({"model":"gpt-6.1-sol","concurrency":3,"all":true})),
            background,
            timeout_ms,
        )
        .await;
        while self.held_reads.len() < 3 {
            self.pump().await;
        }
        assert_eq!(self.executions, 3);
    }

    async fn maintenance(&mut self, id: u64) {
        self.send(
            Message::Call {
                id,
                method: "plugin.reconcile".into(),
                context: CallContext {
                    request: None,
                    call_id: id,
                    instance_id: "fixture-instance".into(),
                    generation: 1,
                    incarnation: "fixture-incarnation".into(),
                    stage: Stage::Maintenance,
                    timeout_ms: 5000,
                    resource_stream: false,
                    resource_scope_id: format!("maintenance-{id}"),
                    request_id: None,
                    attempt_id: None,
                    account_id: None,
                    credential_revision: None,
                },
                params: json!({}).try_into().unwrap(),
            },
            vec![],
        )
        .await;
        assert_eq!(self.reply(id).await.0, 200);
    }
}

#[tokio::test]
async fn cancelled_management_call_does_not_leave_an_orphan_run() {
    let mut fixture = Fixture::new().await;
    fixture.start_trace(5000, false).await;
    assert!(fixture.running(3).await["modeltrace_running"].is_object());
    fixture.send(Message::Cancel { id: 1 }, Vec::new()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(fixture.running(5).await["modeltrace_running"].is_null());
    fixture
        .call(
            7,
            "api/modeltrace/cancel",
            Some(json!({"id":"fixture-account"})),
            false,
            5000,
        )
        .await;
    assert_eq!(fixture.reply(7).await.0, 200);
    assert!(fixture.running(9).await["modeltrace_running"].is_null());
}

#[tokio::test]
async fn management_deadline_releases_the_running_state() {
    let mut fixture = Fixture::new().await;
    fixture.start_trace(500, false).await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(fixture.running(3).await["modeltrace_running"].is_null());
}

#[tokio::test]
async fn native_background_job_outlives_finished_and_cancelled_page_call() {
    let server = LocalModel::new().await;
    let mut fixture = Fixture::configured(json!({"api_base_url":server.base})).await;
    fixture
        .call(
            1,
            "api/modeltrace/run",
            Some(json!({"model":"gpt-6.1-sol","concurrency":3,"all":true})),
            true,
            5000,
        )
        .await;
    let (status, started) = fixture.reply(1).await;
    assert_eq!(status, 202, "{}", started);
    assert_eq!(started["queued"], true);
    assert_eq!(started["started"], 1);
    assert_eq!(
        fixture.executions, 0,
        "background must not use scoped SDK model callbacks"
    );
    fixture.send(Message::Cancel { id: 1 }, vec![]).await;
    server.wait_started(3).await;
    assert!(fixture.running(3).await["modeltrace_running"].is_object());
    server.release.add_permits(3);
    tokio::time::sleep(Duration::from_millis(150)).await;
    // No page polling: a fresh official maintenance call saves the finished result.
    fixture.maintenance(5).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let saved_states = fixture.states.clone();
    assert!(!saved_states.is_empty());
    let state = fixture.running(7).await;
    assert!(state["modeltrace_running"].is_null());
    assert_eq!(state["modeltraces"][0]["status"], "completed");
    let path = format!(
        "api/modeltrace/record?id={}",
        state["modeltraces"][0]["id"].as_str().unwrap()
    );
    fixture.call(9, &path, None, false, 5000).await;
    let (status, detail) = fixture.reply(9).await;
    assert_eq!(status, 200);
    assert_eq!(detail["samples"].as_array().unwrap().len(), 3);
    assert_eq!(fixture.executions, 0);
    drop(fixture);
    let mut reloaded = Fixture::new().await;
    reloaded.states = saved_states;
    reloaded.call(1, &path, None, false, 5000).await;
    assert_eq!(reloaded.reply(1).await, (200, detail));
}

#[tokio::test]
async fn native_background_stop_releases_busy_state_and_allows_restart() {
    let server = LocalModel::new().await;
    let mut fixture = Fixture::configured(json!({"api_base_url":server.base})).await;
    fixture
        .call(
            1,
            "api/modeltrace/run",
            Some(json!({"model":"gpt-6.1-sol","concurrency":3,"all":true})),
            true,
            5000,
        )
        .await;
    assert_eq!(fixture.reply(1).await.0, 202);
    server.wait_started(3).await;
    fixture
        .call(
            3,
            "api/modeltrace/cancel",
            Some(json!({"id":"fixture-account"})),
            false,
            5000,
        )
        .await;
    assert_eq!(fixture.reply(3).await.0, 200);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let state = fixture.running(5).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let state = if state["modeltrace_running"].is_null() {
        state
    } else {
        fixture.running(7).await
    };
    assert!(state["modeltrace_running"].is_null());
    assert_eq!(state["modeltraces"][0]["status"], "cancelled");
    fixture
        .call(
            9,
            "api/modeltrace/run",
            Some(json!({"model":"gpt-6.1-sol","concurrency":3,"all":true})),
            true,
            5000,
        )
        .await;
    assert_eq!(fixture.reply(9).await.0, 202);
    server.wait_started(6).await;
    assert_eq!(fixture.executions, 0);
}

#[tokio::test]
async fn invalid_background_endpoint_fails_before_provisioning_or_execution() {
    let mut fixture = Fixture::configured(json!({"api_base_url":"https://example.invalid"})).await;
    fixture
        .call(
            1,
            "api/modeltrace/run",
            Some(json!({"model":"gpt-6.1-sol","all":true})),
            true,
            5000,
        )
        .await;
    assert_ne!(fixture.reply(1).await.0, 202);
    assert_eq!(fixture.executions, 0);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(fixture.running(3).await["modeltrace_running"].is_null());
}

#[tokio::test]
async fn all_suite_background_jobs_can_be_stopped_and_restarted_without_sdk_models() {
    for (path, cancel) in [
        ("api/run", "api/cancel"),
        ("api/pelican/run", "api/pelican/cancel"),
        ("api/fingerprint/run", "api/fingerprint/cancel"),
    ] {
        let server = LocalModel::new().await;
        let mut fixture = Fixture::configured(json!({"api_base_url":server.base})).await;
        let body = json!({"model":"gpt-6.1-sol","all":true,"mode":"quick","runs":1});
        fixture.call(1, path, Some(body.clone()), true, 5000).await;
        let accepted = fixture.reply(1).await;
        assert_eq!(accepted.0, 202, "suite={path} response={}", accepted.1);
        assert_eq!(accepted.1["started"], 1);
        server.wait_started(1).await;
        fixture
            .call(
                3,
                cancel,
                Some(json!({"id":"fixture-account"})),
                false,
                5000,
            )
            .await;
        assert_eq!(fixture.reply(3).await.0, 200);
        tokio::time::sleep(Duration::from_millis(200)).await;
        fixture.maintenance(5).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        fixture.call(7, path, Some(body), true, 5000).await;
        let restarted = fixture.reply(7).await;
        assert_eq!(restarted.0, 202, "suite={path}");
        assert_eq!(restarted.1["started"], 1, "suite={path}");
        assert_eq!(fixture.executions, 0);
    }
}

#[tokio::test]
async fn hourly_candy_runs_without_a_page_and_does_not_replay_after_restart() {
    let server = LocalModel::new().await;
    let config = json!({"api_base_url":server.base,"hourly_candy_enabled":true});
    let mut fixture = Fixture::configured(config.clone()).await;
    fixture.maintenance(1).await;
    server.wait_started(1).await;
    fixture.call(3, "api/state", None, false, 5000).await;
    let state = fixture.reply(3).await.1;
    assert_eq!(state["hourly_candy"]["enabled"], true);
    assert_eq!(state["hourly_candy"]["interval_seconds"], 3600);
    assert_eq!(state["hourly_candy"]["last_dispatch"]["started"], 1);
    let last = state["hourly_candy"]["last_started_at"].clone();
    let next = state["hourly_candy"]["next_run_at"].clone();
    assert_ne!(last, next);
    fixture.maintenance(5).await;
    assert_eq!(server.request_count(), 1);
    server.release.add_permits(1);
    tokio::time::sleep(Duration::from_millis(150)).await;
    fixture.maintenance(7).await;
    let saved = fixture.states.clone();
    assert_eq!(
        fixture.running(9).await["results"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(fixture.executions, 0);
    drop(fixture);
    let mut reloaded = Fixture::configured(config).await;
    reloaded.states = saved;
    reloaded.maintenance(1).await;
    reloaded.call(3, "api/state", None, false, 5000).await;
    let state = reloaded.reply(3).await.1;
    assert_eq!(state["hourly_candy"]["last_started_at"], last);
    assert_eq!(state["hourly_candy"]["next_run_at"], next);
    assert!(state["auths"][0]["running"].is_null());
    assert_eq!(state["auths"][0]["results"].as_array().unwrap().len(), 1);
    assert_eq!(server.request_count(), 1);
}

#[tokio::test]
async fn hourly_candy_skips_an_account_owned_by_a_manual_run() {
    let server = LocalModel::new().await;
    let mut fixture =
        Fixture::configured(json!({"api_base_url":server.base,"hourly_candy_enabled":true})).await;
    fixture
        .call(
            1,
            "api/run",
            Some(json!({"model":"gpt-6.1-sol","all":true,"runs":1})),
            true,
            5000,
        )
        .await;
    assert_eq!(fixture.reply(1).await.0, 202);
    server.wait_started(1).await;
    fixture.maintenance(3).await;
    fixture.call(5, "api/state", None, false, 5000).await;
    let state = fixture.reply(5).await.1;
    assert_eq!(state["hourly_candy"]["last_dispatch"]["started"], 0);
    assert_eq!(state["hourly_candy"]["last_dispatch"]["busy"], 1);
    assert_eq!(server.request_count(), 1);
    server.release.add_permits(1);
}

#[tokio::test]
async fn maintenance_with_hourly_disabled_never_submits_a_model_request() {
    let server = LocalModel::new().await;
    let mut fixture = Fixture::configured(json!({"api_base_url":server.base})).await;
    fixture.maintenance(1).await;
    fixture.call(3, "api/state", None, false, 5000).await;
    let state = fixture.reply(3).await.1;
    assert_eq!(state["hourly_candy"]["enabled"], false);
    assert!(state["hourly_candy"]["last_started_at"].is_null());
    assert_eq!(server.request_count(), 0);
}
