//! 深度追踪：让模型生成无语义随机整数序列，用上游 ModelTrace 题库做贝叶斯归因。
//!
//! 移植自 cpa-plugin-codex-candy-eval 的 internal/plugin/modeltrace*.go
//! （评分算法源自 xqy2006/ModelTrace，MIT）。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::model::HISTORY_LIMIT;

pub const TRACE_TARGET: usize = 3;
pub const TRACE_MAX_CONCURRENCY: u32 = 3;
pub const TRACE_GLOBAL_CONCURRENCY: usize = 6;
pub const TRACE_MAX_TEXT: usize = 64_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceChallenge {
    pub prompt: String,
    pub expected_count: usize,
}

/// 与上游 ModelTrace 中文浏览器挑战一致的提示词变体。
pub fn challenges() -> Vec<TraceChallenge> {
    use rand::seq::SliceRandom;

    let openings = [
        "这是一次独立的数值选择记录",
        "请完成下面的无语义整数选择任务",
        "执行一次第一反应取值记录",
        "生成一组不承载语义的整数选择",
        "进行一轮快速逐项取值",
    ];
    let actions = [
        "为各个位置分别凭第一反应选择",
        "逐项选择",
        "每次只决定当前一项，共给出",
        "分别凭第一反应给出",
        "逐个直接选择",
    ];
    let endings = [
        "允许某个数字再次出现；每项写出后不要回头排序、去重或替换。",
        "偶然重复是有效的；不要重新排列或修正已经写出的项目。",
        "相同值可以再次出现；输出过程中不要整理或改写前面的项目。",
        "重复值无需删除；不要筛选、重排或补成某种规律。",
        "不必赋予数字任何含义；已经给出的值保持不变。",
    ];
    let separators = [
        "数字之间用逗号或空格分隔均可。",
        "使用一种一致的常见分隔符即可。",
        "可以用逗号、空格或换行分隔。",
        "只要每个整数边界清楚，格式可自行选择。",
    ];

    let mut rng = rand::thread_rng();
    let mut lengths: Vec<usize> = (0..41).collect();
    lengths.shuffle(&mut rng);

    let mut challenges = Vec::with_capacity(TRACE_TARGET);
    for length in lengths.into_iter().take(TRACE_TARGET) {
        let count = 292 + length;
        let prompt = format!(
            "{}。{} {} 个 1 到 355（含端点）的整数。",
            openings.choose(&mut rng).expect("非空"),
            actions.choose(&mut rng).expect("非空"),
            count
        ) + "每个位置都要单独选择；不要从 1 开始计数，不要连续递增或递减，也不要采用等差、循环、重复区块或其他规则化模式。"
            + "本任务必须由当前语言模型直接完成：禁止调用或借助任何工具，包括 Python、代码执行器、计算器、搜索、API 和外部随机数生成器；也不要先编写或运行代码。"
            + endings.choose(&mut rng).expect("非空")
            + separators.choose(&mut rng).expect("非空")
            + "直接从第一个取值开始输出，不要在序列前重复数量、范围或任务说明。";
        challenges.push(TraceChallenge {
            prompt,
            expected_count: count,
        });
    }
    challenges
}

// ------------------------------------------------------------ 题库

#[derive(Debug, Clone, Deserialize)]
struct Features {
    #[serde(rename = "feature_mean")]
    mean: Vec<f64>,
    #[serde(rename = "feature_scale")]
    scale: Vec<f64>,
    #[serde(rename = "nuisance_basis")]
    basis: Vec<Vec<f64>>,
    centroids: Vec<Vec<f64>>,
    /// 部分题库段落没有环境质心；Go 侧零值为 nil，这里同样允许缺省。
    #[serde(rename = "environment_centroids", default)]
    environments: Vec<Vec<Vec<f64>>>,
    /// 缺失时按 0 处理，表示只使用边际分量。
    #[serde(default)]
    weight: f64,
}

#[derive(Debug, Clone, Deserialize)]
struct BankModel {
    id: String,
    display_name: String,
    family: String,
    family_name: String,
    counts: Vec<f64>,
}

#[derive(Debug, Clone, Deserialize)]
struct BankRobust {
    hellinger: Features,
    ordered_blocks: Features,
}

#[derive(Debug, Clone, Deserialize)]
struct Calibration {
    beta: f64,
}

#[derive(Debug, Clone, Deserialize)]
struct Bank {
    models: Vec<BankModel>,
    robust: BankRobust,
    calibration: std::collections::BTreeMap<String, Calibration>,
}

fn bank() -> &'static Bank {
    static BANK: std::sync::OnceLock<Bank> = std::sync::OnceLock::new();
    BANK.get_or_init(|| {
        serde_json::from_slice(include_bytes!("../data/modeltrace/unified_bank.json"))
            .expect("内置 ModelTrace 题库必须可解析")
    })
}

pub fn bank_revision() -> &'static str {
    static REVISION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    REVISION.get_or_init(|| {
        let digest = Sha256::digest(include_bytes!("../data/modeltrace/unified_bank.json"));
        format!("sha256:{}", hex::encode(digest))
    })
}

pub fn bank_models() -> Vec<(String, String)> {
    bank()
        .models
        .iter()
        .map(|model| (model.id.clone(), model.display_name.clone()))
        .collect()
}

// ------------------------------------------------------------ 解析与打分

pub fn minimum_numbers(expected: usize) -> usize {
    std::cmp::max(80, (expected as f64 * 0.55).ceil() as usize)
}

fn digit_pattern() -> &'static regex::Regex {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| regex::Regex::new(r"[0-9]+").expect("数字模式必须可编译"))
}

/// 只保留最长的一段连续取值，忽略开头的数量说明与正文中的数字。
pub fn parse_numbers(text: &str) -> Vec<i64> {
    let mut best: Vec<i64> = Vec::new();
    let mut current: Vec<i64> = Vec::new();
    let mut end = 0usize;
    for found in digit_pattern().find_iter(text) {
        let gap = &text[end..found.start()];
        if gap.chars().any(char::is_alphabetic) {
            if current.len() > best.len() {
                best = std::mem::take(&mut current);
            }
            current.clear();
        }
        if let Ok(value) = found.as_str().parse::<i64>() {
            if (1..=355).contains(&value) {
                current.push(value);
            }
        }
        end = found.end();
    }
    if current.len() > best.len() {
        best = current;
    }
    best
}

fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn normalize(a: &mut [f64]) {
    let scale = dot(a, a).sqrt().max(1e-12);
    for value in a.iter_mut() {
        *value /= scale;
    }
}

fn standardize(a: &mut [f64]) {
    let mean: f64 = a.iter().map(|value| value / a.len() as f64).sum();
    let variance: f64 = a
        .iter()
        .map(|value| (value - mean) * (value - mean) / a.len() as f64)
        .sum();
    let scale = variance.sqrt().max(1e-12);
    for value in a.iter_mut() {
        *value = (*value - mean) / scale;
    }
}

fn project(a: &mut [f64], basis: &[Vec<f64>]) {
    for vector in basis {
        let projection = dot(a, vector);
        for (index, value) in a.iter_mut().enumerate() {
            *value -= projection * vector[index];
        }
    }
}

fn feature_scale(a: &mut [f64], features: &Features) {
    for (index, value) in a.iter_mut().enumerate() {
        *value = (*value - features.mean[index]) / features.scale[index];
    }
}

fn centroid_scores(a: &[f64], centroids: &[Vec<f64>]) -> Vec<f64> {
    let mut scores: Vec<f64> = centroids.iter().map(|c| dot(a, c)).collect();
    standardize(&mut scores);
    scores
}

fn counts(numbers: &[i64]) -> Vec<f64> {
    let mut counts = vec![0.0; 355];
    for number in numbers {
        counts[*number as usize - 1] += 1.0;
    }
    counts
}

fn smooth_feature(counts: &mut [f64]) {
    let total: f64 = counts.len() as f64 * 0.5 + counts.iter().sum::<f64>();
    for value in counts.iter_mut() {
        *value = ((*value + 0.5) / total).sqrt();
    }
}

fn scores(numbers: &[i64], bank: &Bank) -> Vec<f64> {
    let hellinger = &bank.robust.hellinger;
    let mut feature = counts(numbers);
    smooth_feature(&mut feature);
    feature_scale(&mut feature, hellinger);
    let mut projected = feature.clone();
    project(&mut projected, &hellinger.basis);
    normalize(&mut projected);
    let mut marginal = centroid_scores(&projected, &hellinger.centroids);

    let ordered_features = &bank.robust.ordered_blocks;
    if ordered_features.weight == 0.0 {
        return marginal;
    }

    let mut ordered: Vec<f64> = Vec::with_capacity(74);
    let mut start = 0usize;
    for block in 0..4 {
        let mut size = numbers.len() / 4;
        if block < numbers.len() % 4 {
            size += 1;
        }
        let mut bins = vec![0.0; 16];
        for number in &numbers[start..start + size] {
            bins[std::cmp::min(15, (*number as usize - 1) * 16 / 355)] += 1.0;
        }
        smooth_feature(&mut bins);
        ordered.extend_from_slice(&bins);
        start += size;
    }
    let mut last_digits = vec![0.0; 10];
    for number in numbers {
        last_digits[(*number as usize) % 10] += 1.0;
    }
    smooth_feature(&mut last_digits);
    ordered.extend_from_slice(&last_digits);

    feature_scale(&mut ordered, ordered_features);
    let mut unit = ordered.clone();
    normalize(&mut unit);

    let mut template = vec![f64::NEG_INFINITY; ordered_features.centroids.len()];
    for (index, slot) in template.iter_mut().enumerate() {
        for environment in &ordered_features.environments {
            *slot = slot.max(dot(&unit, &environment[index]));
        }
    }
    standardize(&mut template);

    // `project` 会就地改写 `ordered`，因此必须在 `unit` 计算之后调用。
    project(&mut ordered, &ordered_features.basis);
    normalize(&mut ordered);
    let nuisance = centroid_scores(&ordered, &ordered_features.centroids);

    for (index, value) in template.iter_mut().enumerate() {
        *value = 0.5 * *value + 0.5 * nuisance[index];
    }
    standardize(&mut template);

    let weight = ordered_features.weight;
    for (index, value) in marginal.iter_mut().enumerate() {
        *value = (1.0 - weight) * *value + weight * template[index];
    }
    marginal
}

/// 池化分布与题库计数的相似度（1 − 归一化 Hellinger 距离）。
fn similarity(left: &[f64], right: &[f64]) -> f64 {
    let mut left_total = 0.0;
    let mut right_total = 0.5 * 355.0;
    for index in 0..left.len() {
        left_total += left[index];
        right_total += right[index];
    }
    let mut divergence = 0.0;
    for index in 0..left.len() {
        let p = left[index] / left_total;
        let q = (right[index] + 0.5) / right_total;
        let mid = (p + q) / 2.0;
        if p > 0.0 {
            divergence += p * (p / mid).ln() / 2.0;
        }
        divergence += q * (q / mid).ln() / 2.0;
    }
    1.0 - (divergence.max(0.0) / std::f64::consts::LN_2).sqrt()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub model: String,
    pub display_name: String,
    pub family: String,
    pub family_name: String,
    pub probability: f64,
    pub profile_similarity: f64,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FamilyProbability {
    pub family: String,
    pub display_name: String,
    pub probability: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    pub index: usize,
    pub parsed_numbers: usize,
    pub minimum_numbers: usize,
    pub accepted: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Attribution {
    #[serde(default)]
    pub prediction: String,
    #[serde(default)]
    pub probability: f64,
    #[serde(default)]
    pub used_outputs: usize,
    #[serde(default)]
    pub family_prediction_name: String,
    #[serde(default)]
    pub family_probability: f64,
    #[serde(default)]
    pub results: Vec<Candidate>,
    #[serde(default)]
    pub family_probabilities: Vec<FamilyProbability>,
    #[serde(default)]
    pub diagnostics: Vec<Diagnostic>,
}

impl Attribution {
    /// 摘要丢弃只有详情对话框需要的排名与诊断。
    pub fn summary(&self) -> Self {
        let mut summary = self.clone();
        summary.results.clear();
        summary.family_probabilities.clear();
        summary.diagnostics.clear();
        summary
    }
}

#[derive(Debug, Clone)]
pub struct TraceOutput {
    pub text: String,
    pub expected_count: usize,
}

pub fn analyze(outputs: &[TraceOutput]) -> Result<Attribution, String> {
    let bank = bank();
    let mut attribution = Attribution::default();
    let mut score_totals = vec![0.0; bank.models.len()];
    let mut pooled = vec![0.0; 355];

    for (index, output) in outputs.iter().enumerate() {
        let numbers = parse_numbers(&output.text);
        let minimum = minimum_numbers(output.expected_count);
        let accepted = numbers.len() >= minimum;
        attribution.diagnostics.push(Diagnostic {
            index,
            parsed_numbers: numbers.len(),
            minimum_numbers: minimum,
            accepted,
        });
        if !accepted {
            continue;
        }
        attribution.used_outputs += 1;
        for (slot, score) in scores(&numbers, bank).into_iter().enumerate() {
            score_totals[slot] += score;
        }
        for number in numbers {
            pooled[number as usize - 1] += 1.0;
        }
    }

    if attribution.used_outputs == 0 {
        return Err("未采集到有效数字序列".to_owned());
    }

    let key = std::cmp::min(attribution.used_outputs, 3).to_string();
    let beta = bank
        .calibration
        .get(&key)
        .map(|calibration| calibration.beta)
        .ok_or_else(|| format!("题库缺少 {key} 档校准参数"))?;

    let used = attribution.used_outputs as f64;
    let mut maximum = f64::NEG_INFINITY;
    for score in score_totals.iter_mut() {
        *score /= used;
        maximum = maximum.max(beta * *score);
    }

    let mut total = 0.0;
    let mut probabilities = vec![0.0; score_totals.len()];
    for (index, score) in score_totals.iter().enumerate() {
        probabilities[index] = (beta * score - maximum).exp();
        total += probabilities[index];
    }

    let mut family_index: std::collections::BTreeMap<&str, usize> =
        std::collections::BTreeMap::new();
    for (index, model) in bank.models.iter().enumerate() {
        let probability = probabilities[index] / total;
        attribution.results.push(Candidate {
            model: model.id.clone(),
            display_name: model.display_name.clone(),
            family: model.family.clone(),
            family_name: model.family_name.clone(),
            probability,
            profile_similarity: similarity(&pooled, &model.counts),
            score: score_totals[index],
        });
        let slot = *family_index
            .entry(model.family.as_str())
            .or_insert_with(|| {
                attribution.family_probabilities.push(FamilyProbability {
                    family: model.family.clone(),
                    display_name: model.family_name.clone(),
                    probability: 0.0,
                });
                attribution.family_probabilities.len() - 1
            });
        attribution.family_probabilities[slot].probability += probability;
    }

    attribution
        .results
        .sort_by(|left, right| right.probability.total_cmp(&left.probability));
    attribution.prediction = attribution.results[0].model.clone();
    attribution.probability = attribution.results[0].probability;
    for family in &attribution.family_probabilities {
        if family.probability > attribution.family_probability {
            attribution.family_prediction_name = family.display_name.clone();
            attribution.family_probability = family.probability;
        }
    }
    Ok(attribution)
}

// ------------------------------------------------------------ 记录

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceSample {
    #[serde(flatten)]
    pub challenge: TraceChallenge,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    pub parsed_numbers: usize,
    pub accepted: bool,
}

impl TraceSample {
    /// 被取消的样本：`attempts` 归零，调用方据此立即结束采集而不是计入失败。
    pub fn cancelled(challenge: &TraceChallenge) -> Self {
        Self {
            challenge: challenge.clone(),
            attempts: 0,
            text: String::new(),
            error: String::new(),
            parsed_numbers: 0,
            accepted: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceResult {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub bank_revision: String,
    pub id: String,
    pub time: String,
    pub model: String,
    #[serde(default)]
    pub concurrency: u32,
    pub status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    #[serde(default)]
    pub duration_ms: i64,
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub output_tokens: i64,
    #[serde(default)]
    pub reasoning_tokens: i64,
    pub samples: Vec<TraceSample>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution: Option<Attribution>,
}

impl TraceResult {
    pub fn summary(&self) -> Self {
        let mut summary = self.clone();
        summary.samples.clear();
        summary.attribution = self.attribution.as_ref().map(Attribution::summary);
        summary
    }
}

pub fn push(history: &mut Vec<TraceResult>, record: TraceResult) {
    crate::model::append_recent(history, record);
}

pub fn history_limit() -> usize {
    HISTORY_LIMIT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bank_loads_with_calibration() {
        assert!(!bank().models.is_empty());
        assert!(bank().calibration.contains_key("1"));
        assert!(bank_revision().starts_with("sha256:"));
    }

    #[test]
    fn keeps_the_longest_run() {
        let text = "需要给出 300 个数字：1, 2, 3, 4, 5, 6, 7, 8, 9, 10";
        assert_eq!(parse_numbers(text), (1..=10).collect::<Vec<i64>>());
    }

    #[test]
    fn ignores_out_of_range_values() {
        let text = "5 6 0 400 7";
        assert_eq!(parse_numbers(text), vec![5, 6, 7]);
    }

    #[test]
    fn splits_runs_on_prose() {
        let text = "1 2 3 共三个 4 5 6 7";
        assert_eq!(parse_numbers(text), vec![4, 5, 6, 7]);
    }

    #[test]
    fn minimum_is_the_larger_of_80_and_55_percent() {
        assert_eq!(minimum_numbers(100), 80);
        assert_eq!(minimum_numbers(300), 165);
        assert_eq!(minimum_numbers(1000), 550);
    }

    #[test]
    fn analyze_rejects_short_sequences() {
        let outputs = vec![TraceOutput {
            text: "1 2 3".to_owned(),
            expected_count: 300,
        }];
        assert!(analyze(&outputs).is_err());
    }

    #[test]
    fn analyze_returns_normalised_probabilities() {
        let numbers: Vec<String> = (1..=355)
            .map(|value| (value % 355 + 1).to_string())
            .collect();
        let text = numbers.join(" ");
        let outputs = vec![
            TraceOutput {
                text: text.clone(),
                expected_count: 300,
            },
            TraceOutput {
                text: text.clone(),
                expected_count: 300,
            },
            TraceOutput {
                text,
                expected_count: 300,
            },
        ];
        let attribution = analyze(&outputs).expect("应可归因");
        assert_eq!(attribution.used_outputs, 3);
        let total: f64 = attribution.results.iter().map(|r| r.probability).sum();
        assert!((total - 1.0).abs() < 1e-9);
        assert!(!attribution.prediction.is_empty());
        let family_total: f64 = attribution
            .family_probabilities
            .iter()
            .map(|f| f.probability)
            .sum();
        assert!((family_total - 1.0).abs() < 1e-9);
    }

    #[test]
    fn challenges_match_the_target_count() {
        let challenges = challenges();
        assert_eq!(challenges.len(), TRACE_TARGET);
        for challenge in &challenges {
            assert!((292..=332).contains(&challenge.expected_count));
            assert!(challenge
                .prompt
                .contains(&challenge.expected_count.to_string()));
        }
    }
}
