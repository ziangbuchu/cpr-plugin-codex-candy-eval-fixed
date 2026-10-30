//! CLI 扩展：`inspect` 运行一次糖果自检并输出聚合结论，供 systemd 定时器调用。

use std::sync::Arc;

use gateway_plugin_sdk::{
    call::management::{CommandDescriptor, CommandInvocation, CommandRegistration, CommandResult},
    client::{TypedCall, TypedReply},
    PluginFault,
};

use crate::state::StateManager;

pub fn command_registration() -> CommandRegistration {
    CommandRegistration {
        commands: vec![CommandDescriptor {
            name: "inspect".to_owned(),
            description: "运行 Codex 糖果自检并返回各账号判定与耗时".to_owned(),
            parameters: Vec::new(),
        }],
    }
}

pub async fn command_line(
    state: &Arc<StateManager>,
    call: TypedCall<CommandInvocation>,
) -> Result<TypedReply<CommandResult>, PluginFault> {
    if call.request.name != "inspect" {
        return Ok(TypedReply::new(CommandResult {
            stdout: String::new(),
            stderr: "未知命令；用法：inspect\n".to_owned(),
            exit_code: 2,
            accounts: Vec::new(),
        }));
    }

    state.ensure_loaded(&call.host).await;
    let result = crate::runs::inspect(state, &call.host).await;

    // 只输出计数与布尔值：回答正文与账号标识不进入 stdout。
    let status = result["status"].as_str().unwrap_or("unknown");
    let total = result["total"].as_u64().unwrap_or_default();
    let passed = result["passed"].as_u64().unwrap_or_default();
    let failed = result["failed"].as_u64().unwrap_or_default();
    let errored = result["errored"].as_u64().unwrap_or_default();
    let model = result["model"].as_str().unwrap_or_default();

    let mut stdout =
        format!("model={model} total={total} passed={passed} failed={failed} errored={errored}\n");
    if let Some(accounts) = result["accounts"].as_array() {
        for (index, account) in accounts.iter().enumerate() {
            let name = account["name"].as_str().unwrap_or("account");
            let ok = account["passed"].as_bool().unwrap_or(false);
            let duration = account["duration_ms"].as_i64().unwrap_or_default();
            let error = account["error"].as_str().unwrap_or_default();
            stdout.push_str(&format!(
                "account[{index}] name={name} passed={ok} duration_ms={duration} error={error}\n"
            ));
        }
    }
    stdout.push_str(&format!("status={status}\n"));

    Ok(TypedReply::new(CommandResult {
        stdout,
        stderr: String::new(),
        exit_code: if status == "ok" { 0 } else { 1 },
        accounts: Vec::new(),
    }))
}
