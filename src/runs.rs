//! 采集编排：指纹与深度追踪的并发探测、取消响应，以及定时巡检入口。
//!
//! 采集在宿主保留的调用作用域内完成；后台模式的 HTTP 等待者可离开，
//! 模型权限、并发和调用预算仍受宿主约束。执行者的运行标记由 RunLease 统一归还。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use crate::execution::ExecutionContext;
use serde_json::json;
use tokio::sync::Semaphore;

use crate::{
    candy, fingerprint, host_calls,
    model::{Credential, FingerprintProgress, RunProgress, TraceProgress},
    modeltrace,
    state::StateManager,
    UNSUPPORTED_MODEL_MESSAGE,
};

/// 指纹采集的全局并发上限（跨凭证）。
const FINGERPRINT_GLOBAL: usize = 6;
/// 深度追踪采集的全局并发上限（跨凭证）。
const TRACE_GLOBAL: usize = 6;

fn fingerprint_slots() -> &'static Semaphore {
    static SLOTS: OnceLock<Semaphore> = OnceLock::new();
    SLOTS.get_or_init(|| Semaphore::new(FINGERPRINT_GLOBAL))
}

fn trace_slots() -> &'static Semaphore {
    static SLOTS: OnceLock<Semaphore> = OnceLock::new();
    SLOTS.get_or_init(|| Semaphore::new(TRACE_GLOBAL))
}

/// 停止按钮置位后立即结束等待。
///
/// 采集在管理调用内部同步执行，若不主动放弃在途请求，状态机会一直停在 `cancelling`：
/// 上游不返回时那个请求可以挂任意久，用户看不到任何变化。
async fn wait_cancelled(state: &Arc<StateManager>, id: &str) {
    while !state.is_cancelled(id).await {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
}

/// 一次采集的样本结果。
struct Sample {
    category: String,
    normalized: String,
    error: String,
    input_tokens: i64,
    output_tokens: i64,
    reasoning_tokens: i64,
}

// ------------------------------------------------------------ 指纹

pub async fn fingerprint_run(
    state: &Arc<StateManager>,
    host: &impl ExecutionContext,
    credential: &Credential,
    model: &str,
    mode: &fingerprint::FingerprintMode,
    concurrency: u32,
) {
    let total = (mode.cells * mode.samples_per_cell) as u32;
    state
        .set_fingerprint_running(
            &credential.id,
            Some(FingerprintProgress {
                model: model.to_owned(),
                mode: mode.id.clone(),
                done: 0,
                total,
                valid: 0,
                errors: 0,
                phase: "collecting".to_owned(),
                concurrency,
            }),
        )
        .await;

    let started = std::time::Instant::now();
    let probes = fingerprint::probes();
    let mut jobs: Vec<usize> = Vec::with_capacity(total as usize);
    for index in 0..mode.cells.min(probes.len()) {
        for _ in 0..mode.samples_per_cell {
            jobs.push(index);
        }
    }
    {
        use rand::seq::SliceRandom;
        jobs.shuffle(&mut rand::thread_rng());
    }

    let cursor = Arc::new(AtomicUsize::new(0));
    let jobs = Arc::new(jobs);
    let results = Arc::new(tokio::sync::Mutex::new(Vec::<(usize, Sample)>::new()));
    let done = Arc::new(AtomicUsize::new(0));
    let valid = Arc::new(AtomicUsize::new(0));
    let errors = Arc::new(AtomicUsize::new(0));

    let mut set = tokio::task::JoinSet::new();
    for _ in 0..concurrency.max(1) {
        let cursor = Arc::clone(&cursor);
        let results = Arc::clone(&results);
        let done = Arc::clone(&done);
        let valid = Arc::clone(&valid);
        let errors = Arc::clone(&errors);
        let state = Arc::clone(state);
        let host = host.clone();
        let credential = credential.clone();
        let jobs = Arc::clone(&jobs);
        let model = model.to_owned();
        set.spawn(async move {
            loop {
                if state.is_cancelled(&credential.id).await {
                    return;
                }
                let index = cursor.fetch_add(1, Ordering::SeqCst);
                let Some(&probe_index) = jobs.get(index) else {
                    return;
                };
                let probe = &fingerprint::probes()[probe_index];
                let sample =
                    collect_fingerprint_sample(&state, &host, &credential, &model, probe).await;
                if sample.category == "cancelled" {
                    return;
                }
                let current = done.fetch_add(1, Ordering::SeqCst) + 1;
                if sample.category == "valid" {
                    valid.fetch_add(1, Ordering::SeqCst);
                }
                if sample.category == "error" {
                    errors.fetch_add(1, Ordering::SeqCst);
                }
                results.lock().await.push((probe_index, sample));
                state
                    .update_fingerprint(
                        &credential.id,
                        current as u32,
                        valid.load(Ordering::SeqCst) as u32,
                        errors.load(Ordering::SeqCst) as u32,
                        "collecting",
                    )
                    .await;
            }
        });
    }
    while set.join_next().await.is_some() {}

    let cancelled = state.is_cancelled(&credential.id).await;
    state
        .update_fingerprint(
            &credential.id,
            done.load(Ordering::SeqCst) as u32,
            0,
            0,
            if cancelled { "cancelling" } else { "comparing" },
        )
        .await;

    let collected = results.lock().await;
    let mut cells: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let (mut input_tokens, mut output_tokens, mut reasoning_tokens) = (0i64, 0i64, 0i64);
    let mut first_error = String::new();
    for (probe_index, sample) in collected.iter() {
        if first_error.is_empty() && !sample.error.is_empty() {
            first_error = sample.error.clone();
        }
        input_tokens += sample.input_tokens;
        output_tokens += sample.output_tokens;
        reasoning_tokens += sample.reasoning_tokens;
        if sample.category == "valid" {
            cells
                .entry(fingerprint::probes()[*probe_index].id.clone())
                .or_default()
                .push(sample.normalized.clone());
        }
    }
    drop(collected);

    let attribution = fingerprint::attribute(model, &cells);
    let completed = done.load(Ordering::SeqCst) as u32;
    let status = if cancelled && completed < total {
        "cancelled"
    } else {
        "completed"
    };

    let record = fingerprint::FingerprintResult {
        id: format!("{}", started.elapsed().as_nanos()),
        time: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        model: model.to_owned(),
        mode: mode.id.clone(),
        concurrency,
        effort: "low".to_owned(),
        status: status.to_owned(),
        total,
        done: completed,
        valid: valid.load(Ordering::SeqCst) as u32,
        errors: errors.load(Ordering::SeqCst) as u32,
        duration_ms: started.elapsed().as_millis() as i64,
        input_tokens: Some(input_tokens),
        output_tokens: Some(output_tokens),
        reasoning_tokens: Some(reasoning_tokens),
        // 全部样本都失败时把首个错误暴露出来，部分失败只体现在 errors 计数。
        error: if done.load(Ordering::SeqCst) > 0
            && errors.load(Ordering::SeqCst) == done.load(Ordering::SeqCst)
        {
            first_error
        } else {
            String::new()
        },
        attribution,
    };

    if host.storage_host().is_some() {
        state.set_fingerprint_running(&credential.id, None).await;
        state.clear_cancel(&credential.id).await;
    }
    state.push_fingerprint(host, &credential.id, record).await;
}

async fn collect_fingerprint_sample(
    state: &Arc<StateManager>,
    host: &impl ExecutionContext,
    credential: &Credential,
    model: &str,
    probe: &fingerprint::FingerprintProbe,
) -> Sample {
    /// 被取消的样本不计入有效或错误，直接结束该凭证的采集。
    fn cancelled() -> Sample {
        Sample {
            category: "cancelled".to_owned(),
            normalized: String::new(),
            error: String::new(),
            input_tokens: 0,
            output_tokens: 0,
            reasoning_tokens: 0,
        }
    }

    use rand::seq::SliceRandom;

    let Some(prompt) = probe.prompts.choose(&mut rand::thread_rng()) else {
        return Sample {
            category: "error".to_owned(),
            normalized: String::new(),
            error: "探测题库缺少提示词".to_owned(),
            input_tokens: 0,
            output_tokens: 0,
            reasoning_tokens: 0,
        };
    };
    let prompt = prompt.clone();
    let instructions = probe.instructions.clone();
    let model = model.to_owned();
    let account_id = credential.id.clone();
    let provider = credential.provider.clone();

    let permit = fingerprint_slots().acquire().await;
    let outcome = tokio::select! {
        biased;
        _ = wait_cancelled(state, &account_id) => None,
        outcome = host_calls::execute_probe_body(host, &account_id, &provider, &model, || {
            crate::codex::fingerprint_body(&model, &instructions, &prompt)
        }) => Some(outcome),
    };
    drop(permit);
    let Some(outcome) = outcome else {
        return cancelled();
    };

    match outcome.0 {
        Ok(answer) => {
            let (normalized, category) = fingerprint::normalize_answer(&answer.text, probe);
            Sample {
                category,
                normalized,
                error: String::new(),
                input_tokens: answer.input_tokens,
                output_tokens: answer.output_tokens,
                reasoning_tokens: answer.reasoning_tokens,
            }
        }
        Err(error) => Sample {
            category: "error".to_owned(),
            normalized: String::new(),
            error: host_calls::truncate(&error.message, 500),
            input_tokens: 0,
            output_tokens: 0,
            reasoning_tokens: 0,
        },
    }
}

// ------------------------------------------------------------ 深度追踪

pub async fn modeltrace_run(
    state: &Arc<StateManager>,
    host: &impl ExecutionContext,
    credential: &Credential,
    model: &str,
    concurrency: u32,
) {
    let target = modeltrace::TRACE_TARGET;
    state
        .set_trace_running(
            &credential.id,
            Some(TraceProgress {
                model: model.to_owned(),
                phase: "collecting".to_owned(),
                done: 0,
                concurrency,
            }),
        )
        .await;

    let started = std::time::Instant::now();
    let challenges = Arc::new(modeltrace::challenges());
    let cursor = Arc::new(AtomicUsize::new(0));
    let results = Arc::new(tokio::sync::Mutex::new(Vec::<(
        usize,
        modeltrace::TraceSample,
        i64,
        i64,
        i64,
    )>::new()));
    let done = Arc::new(AtomicUsize::new(0));

    let mut set = tokio::task::JoinSet::new();
    for _ in 0..concurrency.max(1) {
        let cursor = Arc::clone(&cursor);
        let results = Arc::clone(&results);
        let done = Arc::clone(&done);
        let state = Arc::clone(state);
        let host = host.clone();
        let credential = credential.clone();
        let challenges = Arc::clone(&challenges);
        let model = model.to_owned();
        set.spawn(async move {
            loop {
                if state.is_cancelled(&credential.id).await {
                    return;
                }
                let index = cursor.fetch_add(1, Ordering::SeqCst);
                let Some(challenge) = challenges.get(index) else {
                    return;
                };
                let (sample, tokens) =
                    collect_trace_sample(&state, &host, &credential, &model, challenge).await;
                if sample.attempts == 0 {
                    continue;
                }
                let current = done.fetch_add(1, Ordering::SeqCst) + 1;
                results
                    .lock()
                    .await
                    .push((index, sample, tokens.0, tokens.1, tokens.2));
                state
                    .update_trace(&credential.id, current as u32, "collecting")
                    .await;
            }
        });
    }
    while set.join_next().await.is_some() {}

    let cancelled = state.is_cancelled(&credential.id).await;
    let completed = done.load(Ordering::SeqCst) as u32;
    state
        .update_trace(
            &credential.id,
            completed,
            if cancelled { "cancelling" } else { "comparing" },
        )
        .await;

    let mut collected = results.lock().await;
    collected.sort_by_key(|(index, _, _, _, _)| *index);
    let mut samples = Vec::with_capacity(collected.len());
    let mut outputs = Vec::with_capacity(collected.len());
    let (mut input_tokens, mut output_tokens, mut reasoning_tokens) = (0i64, 0i64, 0i64);
    let mut valid = 0usize;
    for (_, sample, input, output, reasoning) in collected.drain(..) {
        input_tokens += input;
        output_tokens += output;
        reasoning_tokens += reasoning;
        if sample.accepted {
            valid += 1;
        }
        outputs.push(modeltrace::TraceOutput {
            text: sample.text.clone(),
            expected_count: sample.challenge.expected_count,
        });
        samples.push(sample);
    }
    drop(collected);

    let mut record = modeltrace::TraceResult {
        bank_revision: modeltrace::bank_revision().to_owned(),
        id: format!("{}", started.elapsed().as_nanos()),
        time: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        model: model.to_owned(),
        concurrency,
        status: if cancelled && completed < target as u32 {
            "cancelled".to_owned()
        } else {
            "completed".to_owned()
        },
        error: String::new(),
        duration_ms: started.elapsed().as_millis() as i64,
        input_tokens,
        output_tokens,
        reasoning_tokens,
        samples,
        attribution: None,
    };

    match modeltrace::analyze(&outputs) {
        Ok(attribution) => {
            record.attribution = Some(attribution);
            if valid < target && record.status != "cancelled" {
                record.status = "partial".to_owned();
            }
        }
        Err(message) if record.status != "cancelled" => {
            record.status = "failed".to_owned();
            record.error = record
                .samples
                .last()
                .map(|sample| sample.error.clone())
                .filter(|error| !error.is_empty())
                .unwrap_or(message);
        }
        Err(_) => {}
    }

    if host.storage_host().is_some() {
        state.set_trace_running(&credential.id, None).await;
        state.clear_cancel(&credential.id).await;
    }
    state.push_trace(host, &credential.id, record).await;
}

async fn collect_trace_sample(
    state: &Arc<StateManager>,
    host: &impl ExecutionContext,
    credential: &Credential,
    model: &str,
    challenge: &modeltrace::TraceChallenge,
) -> (modeltrace::TraceSample, (i64, i64, i64)) {
    let permit = trace_slots().acquire().await;
    let outcome = tokio::select! {
        biased;
        _ = wait_cancelled(state, &credential.id) => None,
        outcome = host_calls::execute_probe(
            host,
            &credential.id,
            &credential.provider,
            model,
            "",
            &challenge.prompt,
        ) => Some(outcome),
    };
    drop(permit);
    // attempts 归零让工作线程立即退出循环，不再新起样本。
    let Some((outcome, attempts)) = outcome else {
        return (modeltrace::TraceSample::cancelled(challenge), (0, 0, 0));
    };

    let mut sample = modeltrace::TraceSample {
        challenge: challenge.clone(),
        attempts,
        text: String::new(),
        error: String::new(),
        parsed_numbers: 0,
        accepted: false,
    };
    let mut tokens = (0i64, 0i64, 0i64);
    match outcome {
        Ok(answer) => {
            tokens = (
                answer.input_tokens,
                answer.output_tokens,
                answer.reasoning_tokens,
            );
            if answer.text.chars().count() > modeltrace::TRACE_MAX_TEXT {
                sample.error = "回答过长，本次未计入".to_owned();
                return (sample, tokens);
            }
            sample.text = answer.text;
            sample.parsed_numbers = modeltrace::parse_numbers(&sample.text).len();
            sample.accepted =
                sample.parsed_numbers >= modeltrace::minimum_numbers(challenge.expected_count);
            if !sample.accepted {
                sample.error = "有效数字不足".to_owned();
            }
        }
        Err(error) => sample.error = host_calls::truncate(&error.message, 500),
    }
    (sample, tokens)
}

// ------------------------------------------------------------ 定时巡检

/// 对所有启用账号跑一次糖果测试，返回聚合结论（不含回答正文）。
pub async fn inspect(state: &Arc<StateManager>, host: &impl ExecutionContext) -> serde_json::Value {
    let credentials = state.credentials().await;
    let mut accounts = Vec::new();
    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut errored = 0usize;

    for credential in credentials.iter().filter(|credential| !credential.disabled) {
        let Some(lease) = state.acquire_run(&credential.id).await else {
            continue;
        };
        state
            .set_running(&credential.id, Some(idle_progress(1)))
            .await;
        let record = tokio::select! {
            biased;
            _ = lease.cancelled() => { lease.finish().await; continue; },
            record = candy::evaluate(host, credential, INSPECT_MODEL, "low") => record,
        };
        state.push_candy(host, &credential.id, record.clone()).await;
        lease.finish().await;
        if record.skipped {
            continue;
        }
        if record.ok {
            passed += 1;
        } else if record.error.is_empty() {
            failed += 1;
        } else {
            errored += 1;
        }
        accounts.push(json!({
            "id": credential.id,
            "name": credential.name,
            "passed": record.ok,
            "error": if record.error.is_empty() { String::new() } else { UNSUPPORTED_MODEL_MESSAGE.to_owned() },
            "duration_ms": record.duration_ms,
            "answer_digits": record.answer.chars().filter(char::is_ascii_digit).count(),
        }));
    }

    let result = json!({
        "status": if failed == 0 && errored == 0 { "ok" } else { "attention" },
        "model": INSPECT_MODEL,
        "checked_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "total": accounts.len(),
        "passed": passed,
        "failed": failed,
        "errored": errored,
        "accounts": accounts,
    });
    state.set_inspection(result.clone()).await;
    result
}

/// 定时巡检固定使用的模型；与前端默认选择保持一致。
pub const INSPECT_MODEL: &str = "gpt-6.1-sol";

/// 巡检进度占位：让 CLI 输出与前端使用同一份状态。
pub fn idle_progress(total: u32) -> RunProgress {
    RunProgress { done: 0, total }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspect_model_has_no_effort_suffix() {
        assert!(host_calls::valid_model_name(INSPECT_MODEL));
    }

    #[test]
    fn idle_progress_starts_at_zero() {
        assert_eq!(idle_progress(3).done, 0);
        assert_eq!(idle_progress(3).total, 3);
    }

    #[tokio::test]
    async fn cancel_ends_the_wait_immediately() {
        let state = Arc::new(StateManager::new());
        let watcher = tokio::spawn({
            let state = Arc::clone(&state);
            async move { wait_cancelled(&state, "acct-1").await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!watcher.is_finished(), "未取消时不应提前结束");
        state.cancel("acct-1").await;
        tokio::time::timeout(std::time::Duration::from_secs(2), watcher)
            .await
            .expect("取消后必须立即结束等待")
            .expect("等待器不应 panic");
    }

    #[tokio::test]
    async fn waiting_only_observes_its_own_credential() {
        let state = Arc::new(StateManager::new());
        state.cancel("other").await;
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(400),
                wait_cancelled(&state, "acct-1")
            )
            .await
            .is_err(),
            "其他凭证的取消不能结束本凭证的等待"
        );
    }
}
