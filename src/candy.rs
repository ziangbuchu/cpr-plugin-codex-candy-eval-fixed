//! 糖果测试：判定账号是否被降智的经典计数题。
//!
//! 移植自 cpa-plugin-codex-candy-eval 的 internal/plugin/candy.go。

use crate::execution::ExecutionContext;
use crate::host_calls::{execute_probe, ModelFailure};
use crate::model::{append_recent, CandyResult, Credential, CANDY_MAX_RUNS};

pub const CANDY_PROMPT: &str = r#"不使用任何外部工具回答以下问题：

在一个黑色的袋子里放有三种口味的糖果，每种糖果有两种不同的形状（圆形和五角星形，不同的形状靠手感可以分辨）。现已知不同口味的糖和不同形状的数量统计如下表。参赛者需要在活动前决定摸出的糖果数目，那么，最少取出多少个糖果才能保证手中同时拥有不同形状的苹果味和桃子味的糖？（同时手中有圆形苹果味匹配五角星桃子味糖果，或者有圆形桃子味匹配五角星苹果味糖果都满足要求）

        苹果味  桃子味  西瓜味
圆形       7      9      8
五角星形   7      6      4
"#;

/// 正确答案：最少需要 21 颗。
pub const CANDY_ANSWER: u32 = 21;

/// 判断 `21` 是否作为独立数字出现（前后都不是数字）。
pub fn has_standalone_answer(text: &str) -> bool {
    let bytes = text.as_bytes();
    let is_digit = |index: isize| -> bool {
        index >= 0 && (index as usize) < bytes.len() && bytes[index as usize].is_ascii_digit()
    };
    for index in 0..bytes.len().saturating_sub(1) {
        if bytes[index] == b'2'
            && bytes[index + 1] == b'1'
            && !is_digit(index as isize - 1)
            && !is_digit(index as isize + 2)
        {
            return true;
        }
    }
    false
}

pub fn clamp_runs(runs: u32) -> u32 {
    runs.clamp(1, CANDY_MAX_RUNS)
}

/// 执行一次糖果测试并生成记录；失败时记录错误而不是中断批量执行。
pub async fn evaluate(
    host: &impl ExecutionContext,
    credential: &Credential,
    model: &str,
    effort: &str,
) -> CandyResult {
    let mut result = CandyResult {
        skipped: false,
        time: now_iso(),
        model: model.to_owned(),
        effort: effort.to_owned(),
        ok: false,
        answer: String::new(),
        error: String::new(),
        input_tokens: 0,
        output_tokens: 0,
        reasoning_tokens: 0,
        duration_ms: 0,
    };

    match execute_probe(
        host,
        &credential.id,
        &credential.provider,
        model,
        effort,
        CANDY_PROMPT,
    )
    .await
    {
        (Ok(answer), _) => {
            result.duration_ms = answer.elapsed.as_millis() as i64;
            result.input_tokens = answer.input_tokens;
            result.output_tokens = answer.output_tokens;
            result.reasoning_tokens = answer.reasoning_tokens;
            result.answer =
                crate::host_calls::truncate(&answer.text, crate::model::ANSWER_MAX_CHARS);
            result.ok = has_standalone_answer(&answer.text);
        }
        (Err(error), _) => {
            result.error = crate::host_calls::truncate(&error.message, 500);
        }
    }
    result
}

/// 模型不支持所选凭证时写入的跳过记录。
pub fn skipped(model: &str, effort: &str) -> CandyResult {
    CandyResult {
        skipped: true,
        time: now_iso(),
        model: model.to_owned(),
        effort: effort.to_owned(),
        ok: false,
        answer: String::new(),
        error: crate::UNSUPPORTED_MODEL_MESSAGE.to_owned(),
        input_tokens: 0,
        output_tokens: 0,
        reasoning_tokens: 0,
        duration_ms: 0,
    }
}

pub fn push(history: &mut Vec<CandyResult>, record: CandyResult) {
    append_recent(history, record);
}

pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// 模型错误是否属于值得重试的上游临时故障。
pub fn retryable(kind: ModelFailure) -> bool {
    matches!(kind, ModelFailure::Host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_standalone_answer() {
        assert!(has_standalone_answer("最少需要 21 颗糖果"));
        assert!(has_standalone_answer("答案：21。"));
        assert!(has_standalone_answer("21"));
    }

    #[test]
    fn rejects_embedded_digits() {
        assert!(!has_standalone_answer("121"));
        assert!(!has_standalone_answer("213"));
        assert!(!has_standalone_answer("答案是 8"));
        // 只要求两侧不是数字，因此小数与「21 颗」都算命中，与 Go 侧一致。
        assert!(has_standalone_answer("共 21.5 颗"));
        assert!(has_standalone_answer("最少 21 颗"));
    }

    #[test]
    fn runs_are_clamped() {
        assert_eq!(clamp_runs(0), 1);
        assert_eq!(clamp_runs(99), CANDY_MAX_RUNS);
        assert_eq!(clamp_runs(3), 3);
    }
}
