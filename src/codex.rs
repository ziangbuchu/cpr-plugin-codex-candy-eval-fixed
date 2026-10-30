//! Codex 客户端请求构造：复刻 codex-tui 0.160.0 在 Responses 协议上的首轮请求形态。
//!
//! 移植自 cpa-plugin-codex-candy-eval 的 internal/plugin/codex.go，行为与字段顺序保持一致，
//! 以便探测请求在指纹与路由层面与真实客户端不可区分。

use serde_json::{json, Map, Value};
use sha1::{Digest as _, Sha1};

pub const CODEX_USER_AGENT: &str =
    "codex-tui/0.160.0 (Ubuntu 26.4.0; aarch64) ghostty/1.3.1 (codex-tui; 0.160.0)";

const CODEX_ENVIRONMENT: &str = r#"<environment_context>
  <cwd>/home/user/workspace</cwd>
  <shell>bash</shell>
  <current_date>%s</current_date>
  <timezone>Etc/UTC</timezone>
  <filesystem><workspace_roots><root>/home/user/workspace</root></workspace_roots><permission_profile type="managed"><file_system type="restricted"><entry access="read"><special>:root</special></entry><entry access="write"><path>/home/user/workspace</path></entry><entry access="write"><special>:slash_tmp</special></entry><entry access="write"><special>:tmpdir</special></entry><entry access="read"><path>/home/user/workspace/.git</path></entry><entry access="read"><path>/home/user/workspace/.agents</path></entry><entry access="read"><path>/home/user/workspace/.codex</path></entry><entry access="read"><path>/home/user/workspace/.aws</path></entry></file_system></permission_profile></filesystem>
</environment_context>"#;

/// codex_context.json 中的一条上下文消息片段，kind 是 Codex 上报的内容类型。
#[derive(Clone, Debug)]
pub struct CodexText {
    pub kind: String,
    pub text: String,
}

/// 上下文消息：角色 + 有序片段。
#[derive(Clone, Debug)]
pub struct CodexContextMessage {
    pub role: String,
    pub content: Vec<CodexText>,
}

struct CodexContext {
    tools: Value,
    base_instructions: String,
    messages: Vec<CodexContextMessage>,
}

fn context() -> &'static CodexContext {
    use std::sync::OnceLock;
    static CONTEXT: OnceLock<CodexContext> = OnceLock::new();
    CONTEXT.get_or_init(|| {
        let raw: Value = serde_json::from_str(include_str!("../data/codex_context.json"))
            .expect("data/codex_context.json 必须是合法 JSON");
        let messages = raw["messages"]
            .as_array()
            .expect("messages 必须是数组")
            .iter()
            .map(|message| CodexContextMessage {
                role: message["role"].as_str().unwrap_or_default().to_owned(),
                content: message["content"]
                    .as_array()
                    .map(|parts| {
                        parts
                            .iter()
                            .map(|part| CodexText {
                                kind: part["kind"].as_str().unwrap_or_default().to_owned(),
                                text: part["text"].as_str().unwrap_or_default().to_owned(),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            })
            .collect();
        CodexContext {
            tools: raw["tools"].clone(),
            base_instructions: raw["base_instructions"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            messages,
        }
    })
}

/// tools 列表按 Codex 发送的紧凑形式取字节；tools 项 ID 由这些精确字节派生。
fn codex_tools_bytes() -> &'static [u8] {
    use std::sync::OnceLock;
    static TOOLS: OnceLock<Vec<u8>> = OnceLock::new();
    TOOLS.get_or_init(|| serde_json::to_vec(&context().tools).expect("tools 必须可序列化"))
}

// ---------------------------------------------------------------- UUID

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Uuid([u8; 16]);

/// RFC 9562 ISO OID 命名空间。
const UUID_OID: Uuid = Uuid([
    0x6b, 0xa7, 0xb8, 0x12, 0x9d, 0xad, 0x11, 0xd1, 0x80, 0xb4, 0x00, 0xc0, 0x4f, 0xd4, 0x30, 0xc8,
]);

fn new_uuid(bytes: &[u8], version: u8) -> Uuid {
    let mut id = [0u8; 16];
    let take = bytes.len().min(16);
    id[..take].copy_from_slice(&bytes[..take]);
    id[6] = (id[6] & 0x0f) | (version << 4);
    id[8] = (id[8] & 0x3f) | 0x80;
    Uuid(id)
}

fn uuid_v5(space: Uuid, name: &[u8]) -> Uuid {
    let mut hasher = Sha1::new();
    hasher.update(space.0);
    hasher.update(name);
    new_uuid(&hasher.finalize(), 5)
}

/// 时间有序的 UUID v7；低位随机，避免同一毫秒内碰撞。
fn uuid_v7() -> Uuid {
    let mut id = [0u8; 16];
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default();
    // 前 6 字节为 48 位大端毫秒时间戳，其余为随机位。
    id[0] = (millis >> 40) as u8;
    id[1] = (millis >> 32) as u8;
    id[2] = (millis >> 24) as u8;
    id[3] = (millis >> 16) as u8;
    id[4] = (millis >> 8) as u8;
    id[5] = millis as u8;
    let mut rng = [0u8; 10];
    fill_random(&mut rng);
    id[6..].copy_from_slice(&rng);
    new_uuid(&id, 7)
}

fn fill_random(out: &mut [u8]) {
    use std::io::Read as _;
    if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
        if file.read_exact(out).is_ok() {
            return;
        }
    }
    // 极端情况下退化为基于时间与计数器的确定性填充，仅用于非安全标识。
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seed = COUNTER.fetch_add(1, Ordering::Relaxed)
        ^ std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or_default();
    let mut state = seed | 1;
    for byte in out.iter_mut() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *byte = state as u8;
    }
}

impl std::fmt::Display for Uuid {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let b = &self.0;
        write!(
            formatter,
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
        )
    }
}

// ---------------------------------------------------------------- 请求构造

fn codex_message(
    id: String,
    role: &str,
    turn_id: &str,
    created: f64,
    parts: &[CodexText],
) -> Value {
    let mut metadata = Map::new();
    if !turn_id.is_empty() {
        metadata.insert("turn_id".to_owned(), json!(turn_id));
    }
    if created != 0.0 {
        metadata.insert("create_time".to_owned(), json!(created));
    }
    metadata.insert(
        "content_item_kinds".to_owned(),
        Value::Array(parts.iter().map(|part| json!(part.kind)).collect()),
    );

    let mut item = Map::new();
    item.insert("type".to_owned(), json!("message"));
    item.insert("id".to_owned(), json!(id));
    item.insert("role".to_owned(), json!(role));
    item.insert(
        "content".to_owned(),
        Value::Array(
            parts
                .iter()
                .map(|part| json!({"type": "input_text", "text": part.text}))
                .collect(),
        ),
    );
    item.insert(
        "internal_chat_message_metadata_passthrough".to_owned(),
        Value::Object(metadata),
    );
    Value::Object(item)
}

/// Codex 在每次请求最前面放置的 tools 与 base instructions 项，ID 由 thread ID 与内容哈希派生。
fn codex_prefix(thread: Uuid, override_instructions: Option<&str>) -> Vec<Value> {
    let space = uuid_v5(UUID_OID, thread.to_string().as_bytes());
    let instructions = override_instructions
        .map(str::to_owned)
        .unwrap_or_else(|| context().base_instructions.clone());
    vec![
        json!({
            "type": "additional_tools",
            "id": format!("at_{}", uuid_v5(space, codex_tools_bytes())),
            "role": "developer",
            "tools": context().tools,
        }),
        codex_message(
            format!("msg_{}", uuid_v5(space, instructions.as_bytes())),
            "developer",
            "",
            0.0,
            &[CodexText {
                kind: "model.base_instructions".to_owned(),
                text: instructions,
            }],
        ),
    ]
}

/// 一次 Codex 请求的完整载荷与配套请求头。
pub struct CodexTurn {
    pub body: Value,
    pub headers: Vec<(String, String)>,
    pub thread_id: String,
}

/// 构造一个全新 Codex 会话的首轮请求，首个用户消息为 `prompt`。
///
/// `effort` 为空时不写入 reasoning effort，交由上游默认值决定。
pub fn codex_turn(auth_id: &str, model: &str, effort: &str, prompt: &str) -> CodexTurn {
    codex_turn_with(auth_id, model, effort, prompt, None)
}

/// 同 [`codex_turn`]，但可用 `instructions` 覆盖 base instructions。
pub fn codex_turn_with(
    auth_id: &str,
    model: &str,
    effort: &str,
    prompt: &str,
    instructions: Option<&str>,
) -> CodexTurn {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let thread = uuid_v7();
    let turn_id = uuid_v7().to_string();
    let thread_id = thread.to_string();
    let window_id = format!("{thread_id}:0");

    // 每个凭据表现为一个独立的 Codex 安装，跨重启稳定。
    let mut hasher = Sha1::new();
    hasher.update(format!("codex-installation:{auth_id}").as_bytes());
    let installation_id = new_uuid(&hasher.finalize(), 4).to_string();

    let mut metadata = json!({
        "installation_id": installation_id,
        "session_id": thread_id,
        "thread_id": thread_id,
        "agent_name": "/root",
        "turn_id": turn_id,
        "window_id": window_id,
        "window_number": 0,
        "context_window_id": uuid_v7().to_string(),
        "request_kind": "turn",
        "root_turn_id": turn_id,
        "thread_source": "user",
        "turn_trigger": "user",
        "sandbox": "seccomp",
        "sandbox_mode": "workspace-write",
        "auto_review_enabled": true,
        "node_repl_auto_review_required": true,
        "node_repl_disabled": false,
        "turn_started_at_unix_ms": now.as_millis() as i64,
        "analytics_enabled": true,
        "model": model,
    });
    // 原始实现给该字段加了 omitempty：空推理强度必须整键省略。发送空字符串会让宿主
    // 无法确定推理强度，账号选择阶段直接报“没有可用上游”，与被测内容无关。
    if !effort.is_empty() {
        metadata["reasoning_effort"] = json!(effort);
    }
    let metadata_text = serde_json::to_string(&metadata).unwrap_or_default();

    let created = now.as_micros() as f64 / 1e6;
    let mut input = codex_prefix(thread, instructions);
    for message in &context().messages {
        input.push(codex_message(
            format!("msg_{}", uuid_v7()),
            &message.role,
            &turn_id,
            created,
            &message.content,
        ));
    }
    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    input.push(codex_message(
        format!("msg_{}", uuid_v7()),
        "user",
        &turn_id,
        created,
        &[CodexText {
            kind: "environments.environment_context".to_owned(),
            text: CODEX_ENVIRONMENT.replace("%s", &date),
        }],
    ));
    input.push(codex_message(
        format!("msg_{}", uuid_v7()),
        "user",
        &turn_id,
        created,
        &[CodexText {
            kind: "user.text".to_owned(),
            text: prompt.to_owned(),
        }],
    ));

    // 空推理强度整键省略，交回宿主与上游默认值（与原实现一致）。
    let mut reasoning = json!({"context": "all_turns"});
    if !effort.is_empty() {
        reasoning["effort"] = json!(effort);
    }
    let body = json!({
        "model": model,
        "input": input,
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "reasoning": reasoning,
        "store": false,
        "stream": true,
        "include": ["reasoning.encrypted_content"],
        "prompt_cache_key": thread_id,
        "text": {"verbosity": "low"},
        "client_metadata": {
            "thread_id": thread_id,
            "x-codex-window-id": window_id,
            "turn_id": turn_id,
            "root_turn_id": turn_id,
            "x-codex-turn-metadata": metadata_text,
            "x-codex-installation-id": installation_id,
            "session_id": thread_id,
        },
    });

    let headers = vec![
        ("Accept".to_owned(), "text/event-stream".to_owned()),
        ("Content-Type".to_owned(), "application/json".to_owned()),
        ("Originator".to_owned(), "codex-tui".to_owned()),
        ("User-Agent".to_owned(), CODEX_USER_AGENT.to_owned()),
        ("Session-Id".to_owned(), thread_id.clone()),
        ("Thread-Id".to_owned(), thread_id.clone()),
        ("X-Client-Request-Id".to_owned(), thread_id.clone()),
        ("X-Codex-Window-Id".to_owned(), window_id),
        ("X-Codex-Turn-Metadata".to_owned(), metadata_text),
        (
            "X-Codex-Beta-Features".to_owned(),
            "remote_compaction_v2".to_owned(),
        ),
        (
            "X-Openai-Internal-Codex-Responses-Lite".to_owned(),
            "true".to_owned(),
        ),
    ];

    CodexTurn {
        body,
        headers,
        thread_id,
    }
}

/// 指纹测试使用的精简 Responses 请求体。
///
/// 与糖果/追踪不同，指纹需要原始模型分布而不是 Codex 外壳，因此不携带工具、
/// 上下文与客户端元数据，只保留探测自带的 instructions 与提示词。
pub fn fingerprint_body(model: &str, instructions: &str, prompt: &str) -> Value {
    json!({
        "model": model,
        "stream": true,
        "instructions": instructions,
        "input": prompt,
        "temperature": 1.0,
        "reasoning": {"effort": "low"},
        "store": false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_v5_matches_rfc_dns_example() {
        // RFC 9562 附录 A 的 DNS 命名空间示例值。
        let dns = Uuid([
            0x6b, 0xa7, 0xb8, 0x10, 0x9d, 0xad, 0x11, 0xd1, 0x80, 0xb4, 0x00, 0xc0, 0x4f, 0xd4,
            0x30, 0xc8,
        ]);
        assert_eq!(
            uuid_v5(dns, b"www.example.com").to_string(),
            "2ed6657d-e927-568b-95e1-2665a8aea6a2"
        );
    }

    #[test]
    fn turn_carries_codex_identity() {
        let turn = codex_turn("acct-1", "gpt-6.1-sol", "low", "hello");
        assert_eq!(turn.body["model"], "gpt-6.1-sol");
        assert_eq!(turn.body["reasoning"]["effort"], "low");
        assert_eq!(turn.body["stream"], true);
        assert_eq!(turn.body["tool_choice"], "auto");
        assert_eq!(turn.body["client_metadata"]["thread_id"], turn.thread_id);
        let input = turn.body["input"].as_array().expect("input 必须是数组");
        // additional_tools + base instructions + 上下文消息 + 环境 + 用户提示
        assert!(input.len() >= 5);
        assert_eq!(input[0]["type"], "additional_tools");
        assert_eq!(input[1]["role"], "developer");
        assert_eq!(input[input.len() - 1]["content"][0]["text"], "hello");
    }

    #[test]
    fn empty_effort_is_omitted_instead_of_sent_blank() {
        // 原始 Go 实现对推理强度用 omitempty：空值整键省略。发空字符串会让宿主选不到账号。
        let turn = codex_turn("acct-1", "gpt-6.1-sol", "", "hello");
        assert!(
            turn.body["reasoning"].get("effort").is_none(),
            "空推理强度不得输出 effort 字段"
        );
        assert_eq!(turn.body["reasoning"]["context"], "all_turns");
        let metadata_text = turn.body["client_metadata"]["x-codex-turn-metadata"]
            .as_str()
            .expect("turn metadata 必须是字符串");
        let metadata: serde_json::Value =
            serde_json::from_str(metadata_text).expect("turn metadata 必须是 JSON");
        assert!(
            metadata.get("reasoning_effort").is_none(),
            "空推理强度不得写入 client_metadata"
        );
        assert!(turn
            .headers
            .iter()
            .any(|(name, _)| name == "X-Openai-Internal-Codex-Responses-Lite"));
    }

    #[test]
    fn installation_id_is_stable_per_credential() {
        let first = codex_turn("acct-1", "gpt-6.1-sol", "low", "a");
        let second = codex_turn("acct-1", "gpt-6.1-sol", "low", "b");
        let other = codex_turn("acct-2", "gpt-6.1-sol", "low", "a");
        let id = |turn: &CodexTurn| {
            turn.body["client_metadata"]["x-codex-installation-id"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        assert_eq!(id(&first), id(&second));
        assert_ne!(id(&first), id(&other));
    }

    #[test]
    fn instructions_can_be_overridden() {
        let turn = codex_turn_with("acct-1", "gpt-6.1-sol", "low", "hi", Some("probe rules"));
        let input = turn.body["input"].as_array().expect("input 必须是数组");
        assert_eq!(input[1]["content"][0]["text"], "probe rules");
        let default = codex_turn("acct-1", "gpt-6.1-sol", "low", "hi");
        let default_input = default.body["input"].as_array().unwrap();
        assert_ne!(default_input[1]["content"][0]["text"], "probe rules");
    }

    #[test]
    fn fingerprint_body_omits_the_codex_shell() {
        let body = fingerprint_body("gpt-6.1-sol", "rules", "pick a number");
        assert_eq!(body["instructions"], "rules");
        assert_eq!(body["input"], "pick a number");
        assert_eq!(body["store"], false);
        assert!(body.get("client_metadata").is_none());
        assert!(body.get("input").is_some_and(|value| value.is_string()));
    }
}
