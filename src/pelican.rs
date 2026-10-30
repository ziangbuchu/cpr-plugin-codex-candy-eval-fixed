//! 鹈鹕测试：要求模型直接输出 SVG 骑自行车动画的 HTML。
//!
//! 移植自 cpa-plugin-codex-candy-eval 的 internal/plugin/pelican.go。

use crate::execution::ExecutionContext;
use crate::host_calls::execute_probe;
use crate::model::{append_recent, Credential, PelicanResult, PELICAN_MAX_HTML};

pub const PELICAN_PROMPT: &str =
    "不使用任何工具，也不要创建或修改文件，直接在回复中完成以下任务：\n\n\
创建一个 HTML，内容是 SVG 绘制一个鹈鹕骑自行车的 2D 动画，你不需要任何测试，不要使用外部资产。\n\n\
在一个 ```html 代码块中给出完整的 HTML 文件。";

pub async fn evaluate(
    host: &impl ExecutionContext,
    credential: &Credential,
    model: &str,
    effort: &str,
) -> PelicanResult {
    let now = chrono::Utc::now();
    let mut result = PelicanResult {
        id: now.timestamp_nanos_opt().unwrap_or_default().to_string(),
        time: now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        model: model.to_owned(),
        effort: effort.to_owned(),
        skipped: false,
        html: String::new(),
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
        PELICAN_PROMPT,
    )
    .await
    {
        (Ok(answer), _) => {
            result.duration_ms = answer.elapsed.as_millis() as i64;
            result.input_tokens = answer.input_tokens;
            result.output_tokens = answer.output_tokens;
            result.reasoning_tokens = answer.reasoning_tokens;
            let html = extract_html(&answer.text);
            if html.is_empty() {
                result.error = "回答中没有 HTML 代码".to_owned();
            } else if html.len() > PELICAN_MAX_HTML {
                result.error = format!("HTML 超过 {} KB，未保存", PELICAN_MAX_HTML >> 10);
            } else {
                result.html = html;
            }
        }
        (Err(error), _) => result.error = crate::host_calls::truncate(&error.message, 500),
    }
    result
}

pub fn skipped(model: &str, effort: &str) -> PelicanResult {
    let now = chrono::Utc::now();
    PelicanResult {
        id: now.timestamp_nanos_opt().unwrap_or_default().to_string(),
        time: now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        model: model.to_owned(),
        effort: effort.to_owned(),
        skipped: true,
        html: String::new(),
        error: crate::UNSUPPORTED_MODEL_MESSAGE.to_owned(),
        input_tokens: 0,
        output_tokens: 0,
        reasoning_tokens: 0,
        duration_ms: 0,
    }
}

pub fn push(history: &mut Vec<PelicanResult>, record: PelicanResult) {
    append_recent(history, record);
}

/// 从模型回答中取出 HTML：优先 `html` 代码块，其次首个以 `<` 开头的代码块，最后是裸标记。
pub fn extract_html(answer: &str) -> String {
    let blocks = code_blocks(answer);
    for (language, body) in &blocks {
        if language.eq_ignore_ascii_case("html") {
            return body.trim().to_owned();
        }
    }
    for (_, body) in &blocks {
        let code = body.trim();
        if code.starts_with('<') {
            return code.to_owned();
        }
    }
    let text = answer.trim();
    if blocks.is_empty() && text.starts_with('<') {
        return text.to_owned();
    }
    String::new()
}

/// 解析围栏代码块，返回 (语言, 正文)；语言为空表示未标注。
fn code_blocks(answer: &str) -> Vec<(String, String)> {
    let mut blocks = Vec::new();
    let mut lines = answer.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("```") {
            continue;
        }
        let language = trimmed[3..].trim();
        // 语言标记不能含空格之后的内容；取第一段作为语言。
        let language = language
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
            .to_owned();
        let mut body = String::new();
        let mut closed = false;
        for inner in lines.by_ref() {
            if inner.trim_start().starts_with("```") {
                closed = true;
                break;
            }
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(inner);
        }
        if closed {
            blocks.push((language, body));
        }
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_html_fence() {
        let answer = "说明\n```js\nvar a = 1;\n```\n```html\n<html><body></body></html>\n```";
        assert_eq!(extract_html(answer), "<html><body></body></html>");
    }

    #[test]
    fn falls_back_to_markup_fence() {
        let answer = "```\n<div>x</div>\n```";
        assert_eq!(extract_html(answer), "<div>x</div>");
    }

    #[test]
    fn falls_back_to_bare_markup() {
        assert_eq!(extract_html("<svg></svg>"), "<svg></svg>");
    }

    #[test]
    fn rejects_plain_prose() {
        assert!(extract_html("抱歉，我不能完成这个任务。").is_empty());
        assert!(extract_html("```js\nvar a = 1;\n```").is_empty());
    }

    #[test]
    fn keeps_multiline_body() {
        let answer = "```html\n<html>\n<body>hi</body>\n</html>\n```";
        assert_eq!(extract_html(answer), "<html>\n<body>hi</body>\n</html>");
    }
}
