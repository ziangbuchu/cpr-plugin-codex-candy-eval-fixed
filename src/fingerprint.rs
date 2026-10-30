//! 指纹测试：用题库探测回答分布，与基准做 Jensen–Shannon 距离与置换检验。
//!
//! 移植自 cpa-plugin-codex-candy-eval 的 internal/plugin/fingerprint*.go。
//! 置换检验使用固定种子的确定性伪随机序列，保证同一 (模型, 基准) 组合可复现。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

use crate::model::HISTORY_LIMIT;

pub const FINGERPRINT_MIN_VALID: usize = 10;
pub const FINGERPRINT_MIN_CELLS: usize = 4;
pub const FINGERPRINT_PERMUTATIONS: usize = 1000;
pub const FINGERPRINT_DEFAULT_CONCURRENCY: u32 = 2;
pub const FINGERPRINT_MAX_CONCURRENCY: u32 = 6;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FingerprintMode {
    pub id: String,
    pub name: String,
    pub cells: usize,
    pub samples_per_cell: usize,
}

pub fn modes() -> Vec<FingerprintMode> {
    vec![
        FingerprintMode {
            id: "quick".to_owned(),
            name: "快速".to_owned(),
            cells: 4,
            samples_per_cell: 15,
        },
        FingerprintMode {
            id: "standard".to_owned(),
            name: "标准".to_owned(),
            cells: 8,
            samples_per_cell: 25,
        },
        FingerprintMode {
            id: "strict".to_owned(),
            name: "严格".to_owned(),
            cells: 16,
            samples_per_cell: 25,
        },
    ]
}

pub fn mode(id: &str) -> Option<FingerprintMode> {
    modes().into_iter().find(|mode| mode.id == id)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FingerprintProbe {
    pub id: String,
    pub kind: String,
    pub lo: i64,
    pub hi: i64,
    pub instructions: String,
    pub prompts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FingerprintBaseline {
    pub model: String,
    pub cells: BTreeMap<String, Vec<String>>,
}

pub fn probes() -> &'static [FingerprintProbe] {
    static PROBES: std::sync::OnceLock<Vec<FingerprintProbe>> = std::sync::OnceLock::new();
    PROBES.get_or_init(|| {
        serde_json::from_slice(include_bytes!("../data/fingerprint_probes.json"))
            .expect("内置指纹题库必须可解析")
    })
}

pub fn baselines() -> &'static [FingerprintBaseline] {
    static BASELINES: std::sync::OnceLock<Vec<FingerprintBaseline>> = std::sync::OnceLock::new();
    BASELINES.get_or_init(|| {
        serde_json::from_slice(include_bytes!("../data/fingerprint_baselines.json"))
            .expect("内置指纹基准必须可解析")
    })
}

// ------------------------------------------------------------ 归一化

fn refusal_pattern() -> &'static regex::Regex {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| {
        regex::Regex::new(
            r"(?i)\bas an ai\b|\bi (?:cannot|can't|can not|won't|will not)\b|\bi'?m (?:unable|not able|sorry)\b|\bsorry,? (?:i|but)\b|\bcannot (?:comply|assist|help)\b|我不能|我无法|无法回答|不能回答|抱歉|对不起|作为(?:一个)?(?:AI|人工智能)",
        )
        .expect("拒绝模式必须可编译")
    })
}

fn word_pattern() -> &'static regex::Regex {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| regex::Regex::new(r"^[a-z]+$").expect("单词模式必须可编译"))
}

fn digits_pattern() -> &'static regex::Regex {
    static PATTERN: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    PATTERN.get_or_init(|| regex::Regex::new(r"^[0-9]+$").expect("数字模式必须可编译"))
}

const LETTER_NAMES: &[(&str, &str)] = &[
    ("bee", "b"),
    ("cee", "c"),
    ("dee", "d"),
    ("gee", "g"),
    ("jay", "j"),
    ("kay", "k"),
    ("el", "l"),
    ("ell", "l"),
    ("em", "m"),
    ("en", "n"),
    ("oh", "o"),
    ("pee", "p"),
    ("cue", "q"),
    ("queue", "q"),
    ("ar", "r"),
    ("es", "s"),
    ("ess", "s"),
    ("tee", "t"),
    ("vee", "v"),
    ("ex", "x"),
    ("why", "y"),
    ("zee", "z"),
    ("zed", "z"),
];

const ONES: &[&str] = &[
    "zero",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "eleven",
    "twelve",
    "thirteen",
    "fourteen",
    "fifteen",
    "sixteen",
    "seventeen",
    "eighteen",
    "nineteen",
];

const TENS: &[&str] = &[
    "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
];

fn chinese_digit(character: char) -> Option<i64> {
    match character {
        '零' | '〇' => Some(0),
        '一' => Some(1),
        '二' | '两' => Some(2),
        '三' => Some(3),
        '四' => Some(4),
        '五' => Some(5),
        '六' => Some(6),
        '七' => Some(7),
        '八' => Some(8),
        '九' => Some(9),
        _ => None,
    }
}

/// 解析纯数字、中文数字与英文数词；失败返回 `None`。
pub fn parse_number(text: &str) -> Option<i64> {
    if digits_pattern().is_match(text) {
        return text.parse::<i64>().ok();
    }
    let mut total = 0i64;
    let mut current = 0i64;
    let mut chinese = true;
    for character in text.chars() {
        if let Some(digit) = chinese_digit(character) {
            current = digit;
            continue;
        }
        match character {
            '十' => {
                total += current.max(1) * 10;
                current = 0;
            }
            '百' => {
                total += current.max(1) * 100;
                current = 0;
            }
            '千' => {
                total += current.max(1) * 1000;
                current = 0;
            }
            _ => chinese = false,
        }
    }
    if chinese && !text.is_empty() {
        return Some(total + current);
    }
    if let Some(index) = ONES.iter().position(|word| *word == text) {
        return Some(index as i64);
    }
    if text == "hundred" || text == "onehundred" {
        return Some(100);
    }
    for (index, tens) in TENS.iter().enumerate() {
        if text == *tens {
            return Some(((index + 2) * 10) as i64);
        }
        if let Some(rest) = text.strip_prefix(tens) {
            if let Some(unit) = ONES.iter().position(|word| *word == rest) {
                if unit > 0 {
                    return Some(((index + 2) * 10 + unit) as i64);
                }
            }
        }
    }
    None
}

/// 全中文且不超过 `limit` 个字。
fn is_chinese(text: &str, limit: usize) -> bool {
    let mut count = 0;
    for character in text.chars() {
        if !('\u{4e00}'..='\u{9fff}').contains(&character) {
            return false;
        }
        count += 1;
    }
    count > 0 && count <= limit
}

/// 归一化一次探测回答，返回 (归一化答案, 分类)；分类为 valid/invalid/empty/refusal。
pub fn normalize_answer(raw: &str, probe: &FingerprintProbe) -> (String, String) {
    let raw: String = raw.trim().nfc().collect();
    if raw.is_empty() {
        return (String::new(), "empty".to_owned());
    }
    if refusal_pattern().is_match(&raw) {
        return (String::new(), "refusal".to_owned());
    }

    let cleaned: String = raw
        .chars()
        .filter_map(|character| match character {
            '０'..='９' => char::from_u32('0' as u32 + (character as u32 - '０' as u32)),
            '\u{0660}'..='\u{0669}' => {
                char::from_u32('0' as u32 + (character as u32 - '\u{0660}' as u32))
            }
            '\u{06f0}'..='\u{06f9}' => {
                char::from_u32('0' as u32 + (character as u32 - '\u{06f0}' as u32))
            }
            other if other.is_whitespace() || other.is_alphabetic() || other.is_numeric() => {
                Some(other.to_lowercase().next().unwrap_or(other))
            }
            _ => None,
        })
        .collect();

    let words: Vec<&str> = cleaned.split_whitespace().collect();
    let Some(&first) = words.first() else {
        return (String::new(), "empty".to_owned());
    };

    let mut answer = first.to_owned();
    let mut valid = false;
    match probe.kind.as_str() {
        "int" => {
            if let Some(number) = parse_number(first) {
                answer = number.to_string();
                valid = number >= probe.lo && number <= probe.hi;
            }
        }
        "letter" => {
            if let Some((_, mapped)) = LETTER_NAMES.iter().find(|(name, _)| *name == first) {
                answer = (*mapped).to_owned();
            }
            valid = answer.chars().count() == 1
                && answer
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_lowercase());
            if !valid {
                answer = first.to_owned();
            }
        }
        "color" => {
            if first == "grey" {
                answer = "gray".to_owned();
            } else if first == "aqua" {
                answer = "cyan".to_owned();
            } else if first.chars().count() >= 2
                && is_chinese(first, first.chars().count())
                && first.ends_with('色')
            {
                answer = first.trim_end_matches('色').to_owned();
            }
            valid = word_pattern().is_match(&answer) || is_chinese(&answer, 4);
            if !valid {
                answer = first.to_owned();
            }
        }
        "coin" => {
            if first == "heads" || first == "head" || first == "字" || first.starts_with('正') {
                answer = "heads".to_owned();
                valid = true;
            } else if first == "tails"
                || first == "tail"
                || first == "花"
                || first.starts_with('反')
            {
                answer = "tails".to_owned();
                valid = true;
            }
        }
        _ => valid = word_pattern().is_match(first) || is_chinese(first, 6),
    }

    let category = if valid { "valid" } else { "invalid" };
    (answer, category.to_owned())
}

// ------------------------------------------------------------ 距离与检验

/// Jensen–Shannon 距离；`a`、`b` 为离散样本序列。
pub fn jsd(a: &[String], b: &[String]) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let mut counts: BTreeMap<&str, (f64, f64)> = BTreeMap::new();
    for value in a {
        counts.entry(value.as_str()).or_insert((0.0, 0.0)).0 += 1.0;
    }
    for value in b {
        counts.entry(value.as_str()).or_insert((0.0, 0.0)).1 += 1.0;
    }
    let mut distance = 0.0;
    for (p_count, q_count) in counts.values() {
        let p = p_count / a.len() as f64;
        let q = q_count / b.len() as f64;
        let mid = (p + q) / 2.0;
        if p > 0.0 {
            distance += p * (p / mid).log2() / 2.0;
        }
        if q > 0.0 {
            distance += q * (q / mid).log2() / 2.0;
        }
    }
    distance.clamp(0.0, 1.0)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CellComparison {
    pub cell: String,
    pub jsd: f64,
    pub valid_a: usize,
    pub valid_b: usize,
}

/// 逐格比较；返回 (各格距离, 平均距离)；有效格不足时平均为 `None`。
pub fn compare_cells(
    a: &BTreeMap<String, Vec<String>>,
    b: &BTreeMap<String, Vec<String>>,
) -> (Vec<CellComparison>, Option<f64>) {
    let empty = Vec::new();
    let mut entries = Vec::new();
    let mut total = 0.0;
    for probe in probes() {
        let left = a.get(&probe.id).unwrap_or(&empty);
        let right = b.get(&probe.id).unwrap_or(&empty);
        if left.len() < FINGERPRINT_MIN_VALID || right.len() < FINGERPRINT_MIN_VALID {
            continue;
        }
        let distance = jsd(left, right);
        total += distance;
        entries.push(CellComparison {
            cell: probe.id.clone(),
            jsd: distance,
            valid_a: left.len(),
            valid_b: right.len(),
        });
    }
    if entries.len() < FINGERPRINT_MIN_CELLS {
        return (entries, None);
    }
    let mean = total / entries.len() as f64;
    (entries, Some(mean))
}

/// 折半自一致性：同一份样本拆成两半后的平均距离。
pub fn split_half(cells: &BTreeMap<String, Vec<String>>) -> Option<f64> {
    let mut total = 0.0;
    let mut count = 0usize;
    for probe in probes() {
        let Some(answers) = cells.get(&probe.id) else {
            continue;
        };
        if answers.len() < FINGERPRINT_MIN_VALID {
            continue;
        }
        let left: Vec<String> = answers.iter().step_by(2).cloned().collect();
        let right: Vec<String> = answers.iter().skip(1).step_by(2).cloned().collect();
        total += jsd(&left, &right);
        count += 1;
    }
    (count > 0).then(|| total / count as f64)
}

/// 置换检验：把两组合并后重复洗牌，统计平均距离不小于观测值的比例。
pub fn permutation(
    a: &BTreeMap<String, Vec<String>>,
    b: &BTreeMap<String, Vec<String>>,
    seed: &str,
) -> Option<f64> {
    use rand::seq::SliceRandom;
    use rand::SeedableRng;

    let (entries, observed) = compare_cells(a, b);
    let observed = observed?;
    let mut rng = rand::rngs::StdRng::seed_from_u64(u64::from(crc32fast::hash(seed.as_bytes())));

    let mut pools: Vec<Vec<String>> = Vec::with_capacity(entries.len());
    for entry in &entries {
        let mut pool = a.get(&entry.cell).cloned().unwrap_or_default();
        pool.extend(b.get(&entry.cell).cloned().unwrap_or_default());
        pools.push(pool);
    }

    let mut hits = 0usize;
    for _ in 0..FINGERPRINT_PERMUTATIONS {
        let mut total = 0.0;
        for (index, entry) in entries.iter().enumerate() {
            let pool = &mut pools[index];
            pool.shuffle(&mut rng);
            total += jsd(&pool[..entry.valid_a], &pool[entry.valid_a..]);
        }
        if total / entries.len() as f64 >= observed - 1e-12 {
            hits += 1;
        }
    }
    Some((hits + 1) as f64 / (FINGERPRINT_PERMUTATIONS + 1) as f64)
}

pub fn distance_verdict(distance: Option<f64>) -> &'static str {
    match distance {
        None => "insufficient",
        Some(value) if value <= 0.25 => "match",
        Some(value) if value <= 0.35 => "uncertain",
        Some(_) => "mismatch",
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comparison {
    pub model: String,
    pub mean_jsd: Option<f64>,
    pub p_value: Option<f64>,
    pub verdict: String,
    pub self_jsd: Option<f64>,
    pub cells: Vec<CellComparison>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attribution {
    pub status: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub nearest: String,
    pub alpha: f64,
    pub self_jsd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_p_value: Option<f64>,
    pub warnings: Vec<String>,
    pub comparisons: Vec<Comparison>,
}

impl Attribution {
    /// 摘要丢弃只有详情对话框需要的比较表与告警。
    pub fn summary(&self) -> Self {
        let mut summary = self.clone();
        summary.comparisons.clear();
        summary.warnings.clear();
        summary
    }
}

pub fn attribute(model: &str, valid: &BTreeMap<String, Vec<String>>) -> Attribution {
    let baselines = baselines();
    let mut attribution = Attribution {
        status: "insufficient".to_owned(),
        message: "有效样本不足".to_owned(),
        nearest: String::new(),
        alpha: 0.05 / (2 * baselines.len()).max(1) as f64,
        self_jsd: split_half(valid),
        reference_p_value: None,
        warnings: Vec::new(),
        comparisons: Vec::new(),
    };

    let mut same_cells: Option<BTreeMap<String, Vec<String>>> = None;
    for baseline in baselines {
        let (cells, mean) = compare_cells(valid, &baseline.cells);
        let p_value = permutation(
            valid,
            &baseline.cells,
            &format!("{model}|{}", baseline.model),
        );
        attribution.comparisons.push(Comparison {
            model: baseline.model.clone(),
            mean_jsd: mean,
            p_value,
            verdict: distance_verdict(mean).to_owned(),
            self_jsd: split_half(&baseline.cells),
            cells,
        });
        if baseline.model == model {
            same_cells = Some(baseline.cells.clone());
        }
    }

    attribution
        .comparisons
        .sort_by(|left, right| match (left.mean_jsd, right.mean_jsd) {
            (Some(a), Some(b)) => a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        });

    if attribution.comparisons.is_empty() || attribution.comparisons[0].mean_jsd.is_none() {
        return attribution;
    }

    attribution.nearest = attribution.comparisons[0].model.clone();
    if attribution.self_jsd.is_some_and(|value| value > 0.25) {
        attribution
            .warnings
            .push("本次样本自一致性较低，建议使用严格模式复测".to_owned());
    }

    let mut same_index: Option<usize> = None;
    for (index, comparison) in attribution.comparisons.iter().enumerate() {
        if comparison.model == model {
            same_index = Some(index);
        }
        if comparison.self_jsd.is_some_and(|value| value > 0.25) {
            attribution
                .warnings
                .push(format!("{} 基准自一致性较低", comparison.model));
        }
    }

    let significant = |p_value: Option<f64>| p_value.is_some_and(|value| value < attribution.alpha);
    let Some(same_index) = same_index else {
        attribution.status = "no_baseline".to_owned();
        attribution.message = "所选模型没有同名基准，仅展示相似模型".to_owned();
        return attribution;
    };
    let same = attribution.comparisons[same_index].clone();
    if same.mean_jsd.is_none() {
        return attribution;
    }

    let best = attribution.comparisons[0].clone();
    if best.model == model {
        if significant(same.p_value) {
            attribution.status = "different".to_owned();
            attribution.message = "与同名基准存在显著差异，无法归因到其他模型".to_owned();
        } else {
            attribution.status = "consistent".to_owned();
            attribution.message = "与同名基准未发现显著差异".to_owned();
        }
    } else {
        let Some(same_cells) = same_cells else {
            return attribution;
        };
        let Some(best_cells) = baselines
            .iter()
            .find(|baseline| baseline.model == best.model)
            .map(|baseline| baseline.cells.clone())
        else {
            return attribution;
        };
        // 参照分离度只使用本次实际采集到的探测格。
        let mut left = BTreeMap::new();
        let mut right = BTreeMap::new();
        for (cell, answers) in valid {
            if answers.len() >= FINGERPRINT_MIN_VALID {
                if let Some(values) = same_cells.get(cell) {
                    left.insert(cell.clone(), values.clone());
                }
                if let Some(values) = best_cells.get(cell) {
                    right.insert(cell.clone(), values.clone());
                }
            }
        }
        attribution.reference_p_value =
            permutation(&left, &right, &format!("reference|{model}|{}", best.model));

        let reference = significant(attribution.reference_p_value);
        match (
            reference,
            significant(same.p_value),
            significant(best.p_value),
        ) {
            (false, _, _) => {
                attribution.status = "ambiguous".to_owned();
                attribution.message = "同名基准与最近基准本身无法区分".to_owned();
            }
            (true, true, false) => {
                attribution.status = "substitution".to_owned();
                attribution.message = format!("回答分布更接近 {}，疑似被替换为该模型", best.model);
                for candidate in attribution.comparisons.iter().skip(1) {
                    if candidate.model != model && !significant(candidate.p_value) {
                        attribution.status = "ambiguous".to_owned();
                        attribution.message = "与同名基准不同，但多个替代模型均无法排除".to_owned();
                        break;
                    }
                }
            }
            (true, false, true) => {
                attribution.status = "consistent".to_owned();
                attribution.message = "与同名基准未发现显著差异".to_owned();
            }
            (true, true, true) => {
                attribution.status = "different".to_owned();
                attribution.message = "与同名及最近基准均存在显著差异".to_owned();
            }
            (true, false, false) => {
                attribution.status = "ambiguous".to_owned();
                attribution.message = "多个模型均无法排除，样本不足以区分".to_owned();
            }
        }
    }

    if attribution.self_jsd.is_some_and(|value| value > 0.25)
        && matches!(attribution.status.as_str(), "consistent" | "substitution")
    {
        attribution.status = "unstable".to_owned();
        attribution.message = "样本波动较大，结论不稳定，建议复测".to_owned();
    }
    attribution
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FingerprintResult {
    pub id: String,
    pub time: String,
    pub model: String,
    pub mode: String,
    #[serde(default)]
    pub concurrency: u32,
    pub effort: String,
    pub status: String,
    pub total: u32,
    pub done: u32,
    pub valid: u32,
    pub errors: u32,
    #[serde(default)]
    pub duration_ms: i64,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    pub attribution: Attribution,
}

impl FingerprintResult {
    pub fn summary(&self) -> Self {
        let mut summary = self.clone();
        summary.attribution = self.attribution.summary();
        summary
    }
}

pub fn push(history: &mut Vec<FingerprintResult>, record: FingerprintResult) {
    crate::model::append_recent(history, record);
}

pub fn history_limit() -> usize {
    HISTORY_LIMIT
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(kind: &str, lo: i64, hi: i64) -> FingerprintProbe {
        FingerprintProbe {
            id: "p".to_owned(),
            kind: kind.to_owned(),
            lo,
            hi,
            instructions: String::new(),
            prompts: vec!["x".to_owned()],
        }
    }

    #[test]
    fn bundled_data_is_valid() {
        assert!(!probes().is_empty());
        assert!(!baselines().is_empty());
        assert!(probes().iter().all(|probe| !probe.prompts.is_empty()));
    }

    #[test]
    fn parses_numbers_in_many_forms() {
        assert_eq!(parse_number("42"), Some(42));
        assert_eq!(parse_number("twenty"), Some(20));
        assert_eq!(parse_number("twentyone"), Some(21));
        assert_eq!(parse_number("hundred"), Some(100));
        assert_eq!(parse_number("十七"), Some(17));
        assert_eq!(parse_number("二百零五"), Some(205));
        assert_eq!(parse_number("banana"), None);
    }

    #[test]
    fn normalizes_integer_answers() {
        let (answer, category) = normalize_answer("  42 ", &probe("int", 1, 100));
        assert_eq!((answer.as_str(), category.as_str()), ("42", "valid"));
        let (answer, category) = normalize_answer("42", &probe("int", 1, 10));
        assert_eq!((answer.as_str(), category.as_str()), ("42", "invalid"));
        // 连字符被丢弃后合成 "fortytwo"，仍能按英文数词解析为 42（与 Go 侧一致）。
        let (answer, category) = normalize_answer("forty-two!", &probe("int", 1, 100));
        assert_eq!((answer.as_str(), category.as_str()), ("42", "valid"));
        // 超出量程时保留解析结果但判为 invalid。
        let (answer, category) = normalize_answer("forty-two!", &probe("int", 1, 10));
        assert_eq!((answer.as_str(), category.as_str()), ("42", "invalid"));
    }

    #[test]
    fn normalizes_letters_colors_and_coins() {
        assert_eq!(normalize_answer("Bee", &probe("letter", 0, 0)).0, "b");
        assert_eq!(normalize_answer("grey", &probe("color", 0, 0)).0, "gray");
        assert_eq!(normalize_answer("aqua", &probe("color", 0, 0)).0, "cyan");
        assert_eq!(normalize_answer("红色", &probe("color", 0, 0)).0, "红");
        assert_eq!(normalize_answer("heads", &probe("coin", 0, 0)).0, "heads");
        assert_eq!(normalize_answer("反面", &probe("coin", 0, 0)).0, "tails");
    }

    #[test]
    fn detects_refusals_and_empty() {
        assert_eq!(normalize_answer("   ", &probe("int", 1, 10)).1, "empty");
        assert_eq!(
            normalize_answer("I cannot help with that", &probe("int", 1, 10)).1,
            "refusal"
        );
        assert_eq!(
            normalize_answer("抱歉，我无法完成", &probe("int", 1, 10)).1,
            "refusal"
        );
    }

    #[test]
    fn jsd_is_zero_for_identical_samples() {
        let sample: Vec<String> = vec!["a", "b", "a", "c"]
            .into_iter()
            .map(String::from)
            .collect();
        assert!(jsd(&sample, &sample) < 1e-12);
    }

    #[test]
    fn jsd_is_maximal_for_disjoint_samples() {
        let left: Vec<String> = vec!["a"; 10].into_iter().map(String::from).collect();
        let right: Vec<String> = vec!["b"; 10].into_iter().map(String::from).collect();
        assert!((jsd(&left, &right) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn permutation_is_reproducible() {
        let mut left = BTreeMap::new();
        let mut right = BTreeMap::new();
        for probe in probes().iter().take(6) {
            left.insert(probe.id.clone(), vec!["a".to_owned(); 15]);
            right.insert(probe.id.clone(), vec!["a".to_owned(); 15]);
        }
        let first = permutation(&left, &right, "seed");
        let second = permutation(&left, &right, "seed");
        assert_eq!(first, second);
        assert!(first.is_some_and(|value| value > 0.5));
    }

    #[test]
    fn attribution_flags_insufficient_samples() {
        let attribution = attribute("gpt-6.1-sol", &BTreeMap::new());
        assert_eq!(attribution.status, "insufficient");
        assert!(attribution.nearest.is_empty());
    }

    #[test]
    fn distance_verdict_thresholds() {
        assert_eq!(distance_verdict(None), "insufficient");
        assert_eq!(distance_verdict(Some(0.1)), "match");
        assert_eq!(distance_verdict(Some(0.3)), "uncertain");
        assert_eq!(distance_verdict(Some(0.9)), "mismatch");
    }
}
