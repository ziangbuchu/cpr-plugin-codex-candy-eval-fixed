//! Acknowledged jobs are owned by the plugin process, not by a management request.
use crate::{
    background_transport, candy, fingerprint, host_calls,
    management::{self, RunRequest},
    model::{FingerprintProgress, RunProgress, TraceProgress},
    modeltrace, pelican, runs,
    state::StateManager,
};
use gateway_plugin_sdk::{
    call::management::ManagementResponse,
    client::{HostClient, TypedReply},
    PluginFault,
};
use serde_json::{json, Value};
use std::sync::Arc;

pub async fn submit(
    state: &Arc<StateManager>,
    host: &HostClient,
    config: &Value,
    path: &str,
    payload: &[u8],
) -> Result<TypedReply<ManagementResponse>, PluginFault> {
    let request: RunRequest = match serde_json::from_slice(payload) {
        Ok(request) => request,
        Err(_) => return management::json_reply(400, &json!({"error":"测试请求格式错误"})),
    };
    let model = request.model.trim().to_owned();
    if !host_calls::valid_model_name(&model) {
        return management::json_reply(400, &json!({"error":"请选择不含推理强度后缀的模型"}));
    }
    let mode = if path == "api/fingerprint/run" {
        match fingerprint::mode(request.mode.trim()) {
            Some(mode) => Some(mode),
            None => {
                return management::json_reply(400, &json!({"error":"请选择快速、标准或严格模式"}))
            }
        }
    } else {
        None
    };
    let max_concurrency = if mode.is_some() {
        fingerprint::FINGERPRINT_MAX_CONCURRENCY
    } else {
        modeltrace::TRACE_MAX_CONCURRENCY
    };
    let concurrency = if request.concurrency == 0 {
        if mode.is_some() {
            fingerprint::FINGERPRINT_DEFAULT_CONCURRENCY
        } else {
            modeltrace::TRACE_MAX_CONCURRENCY
        }
    } else {
        request.concurrency
    };
    if !(1..=max_concurrency).contains(&concurrency) {
        return management::json_reply(
            400,
            &json!({"error":format!("每凭证并发须为 1–{max_concurrency}")}),
        );
    }
    let shared = match background_transport::prepare_shared(host, config).await {
        Ok(shared) => shared,
        Err(error) => {
            return management::json_reply(409, &json!({"error": error.message}));
        }
    };
    if let Err(error) = management::refresh_credentials(state, host).await {
        return management::json_reply(502, &json!({"error": error.message}));
    }
    let (targets, skipped, unchecked) = management::select(&state.credentials().await, &request);
    let mut busy = Vec::new();
    let mut prepared = Vec::new();
    for credential in targets {
        let Some(lease) = state.acquire_run(&credential.id).await else {
            busy.push(credential.id);
            continue;
        };
        let executor = tokio::select! {
            biased;
            _ = lease.cancelled() => continue,
            executor = async { shared.for_credential(&credential) } => executor,
        };
        let executor = match executor {
            Ok(executor) => executor,
            Err(error) => {
                lease.finish().await;
                return management::json_reply(409, &json!({"error": error.message}));
            }
        };
        prepared.push((credential, lease, executor));
    }
    // All authorization checks succeed before any model call or success acknowledgement.
    let started = prepared.len();
    let ids: Vec<_> = prepared.iter().map(|(c, _, _)| c.id.clone()).collect();
    let run_count = candy::clamp_runs(request.runs);
    let effort = if request.effort.trim().is_empty() {
        "low".to_owned()
    } else {
        request.effort.trim().to_owned()
    };
    for (credential, lease, executor) in prepared {
        match path {
            "api/run" => {
                state
                    .set_running(
                        &credential.id,
                        Some(RunProgress {
                            done: 0,
                            total: run_count,
                        }),
                    )
                    .await
            }
            "api/pelican/run" => state.set_pelican_running(&credential.id, true).await,
            "api/modeltrace/run" => {
                state
                    .set_trace_running(
                        &credential.id,
                        Some(TraceProgress {
                            model: model.clone(),
                            phase: "collecting".into(),
                            done: 0,
                            concurrency,
                        }),
                    )
                    .await
            }
            "api/fingerprint/run" => {
                let mode = mode.as_ref().expect("validated mode");
                state
                    .set_fingerprint_running(
                        &credential.id,
                        Some(FingerprintProgress {
                            model: model.clone(),
                            mode: mode.id.clone(),
                            done: 0,
                            total: (mode.cells * mode.samples_per_cell) as u32,
                            valid: 0,
                            errors: 0,
                            phase: "collecting".into(),
                            concurrency,
                        }),
                    )
                    .await;
            }
            _ => unreachable!("validated run route"),
        }
        let (state, model, effort, path, mode) = (
            Arc::clone(state),
            model.clone(),
            effort.clone(),
            path.to_owned(),
            mode.clone(),
        );
        tokio::spawn(async move {
            match path.as_str() {
                "api/run" => {
                    for index in 0..run_count {
                        let record = tokio::select! {
                            biased;
                            _ = lease.cancelled() => break,
                            record = candy::evaluate(&executor, &credential, &model, &effort) => record,
                        };
                        state.push_candy(&executor, &credential.id, record).await;
                        state
                            .set_running(
                                &credential.id,
                                Some(RunProgress {
                                    done: index + 1,
                                    total: run_count,
                                }),
                            )
                            .await;
                    }
                }
                "api/pelican/run" => {
                    let record = tokio::select! {
                        biased;
                        _ = lease.cancelled() => None,
                        record = pelican::evaluate(&executor, &credential, &model, &effort) => Some(record),
                    };
                    if let Some(record) = record {
                        state.push_pelican(&executor, &credential.id, record).await;
                    }
                }
                "api/modeltrace/run" => {
                    runs::modeltrace_run(&state, &executor, &credential, &model, concurrency).await
                }
                "api/fingerprint/run" => {
                    runs::fingerprint_run(
                        &state,
                        &executor,
                        &credential,
                        &model,
                        &mode.expect("validated mode"),
                        concurrency,
                    )
                    .await
                }
                _ => unreachable!("validated run route"),
            }
            // An active page flushes immediately. Without any page, the official
            // maintenance callback flushes within its normal 30-second interval.
            state.wait_for_commit().await;
            lease.finish().await;
        });
    }
    management::json_reply(
        202,
        &json!({"queued":true,"started":started,"auth_ids":ids,"busy":busy,"skipped":skipped,"unchecked":unchecked}),
    )
}
