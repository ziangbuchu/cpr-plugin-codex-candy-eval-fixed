//! Hourly candy dispatch uses only fresh official maintenance scopes; model work is detached.
use crate::{background, state::StateManager};
use chrono::{DateTime, Utc};
use gateway_plugin_sdk::client::HostClient;
use serde_json::{json, Value};
use std::sync::Arc;

pub const INTERVAL: i64 = 3600;

fn enabled(config: &Value) -> bool {
    config.get("hourly_candy_enabled").and_then(Value::as_bool) == Some(true)
}

fn model(config: &Value) -> &str {
    config
        .get("hourly_candy_model")
        .and_then(Value::as_str)
        .unwrap_or("gpt-6.1-sol")
}

fn effort(config: &Value) -> &str {
    config
        .get("hourly_candy_effort")
        .and_then(Value::as_str)
        .unwrap_or("low")
}

pub(crate) fn due(last: i64, now: i64) -> bool {
    last == 0 || now >= last.saturating_add(INTERVAL)
}

fn time(value: i64) -> Option<String> {
    (value > 0)
        .then(|| DateTime::<Utc>::from_timestamp(value, 0).map(|t| t.to_rfc3339()))
        .flatten()
}

pub async fn status(state: &StateManager, config: &Value) -> Value {
    let (last, dispatch) = state.hourly_candy().await;
    json!({"enabled":enabled(config), "interval_seconds":INTERVAL,
        "model":model(config), "effort":effort(config), "last_started_at":time(last),
        "next_run_at":if enabled(config) && last > 0 { time(last.saturating_add(INTERVAL)) } else { None },
        "last_dispatch":dispatch})
}

pub async fn tick(state: &Arc<StateManager>, host: &HostClient, config: &Value) {
    if !enabled(config) {
        return;
    }
    let Some(revision) = state.claim_hourly_candy(Utc::now().timestamp()).await else {
        return;
    };
    // Commit before dispatch: a crash may skip a slot, but never creates a duplicate billable run.
    state.flush_pending(host).await;
    if !state.revision_committed(revision).await {
        state
            .set_hourly_dispatch(json!({"status":"storage_error"}))
            .await;
        return;
    }
    let payload = json!({"all":true, "runs":1, "model":model(config), "effort":effort(config)})
        .to_string()
        .into_bytes();
    let dispatch = match background::submit(state, host, config, "api/run", &payload).await {
        Ok(reply) if reply.result.status == 202 => {
            let reply = serde_json::from_slice::<Value>(&reply.payload).unwrap_or_default();
            json!({"status":"dispatched", "started":reply["started"],
                "busy":reply["busy"].as_array().map_or(0, Vec::len),
                "skipped":reply["skipped"].as_array().map_or(0, Vec::len)})
        }
        _ => json!({"status":"dispatch_error"}),
    };
    state.set_hourly_dispatch(dispatch).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_are_hourly_without_catch_up_bursts_or_clock_reversal_replays() {
        assert!(due(0, 10_000));
        assert!(!due(10_000, 13_599));
        assert!(due(10_000, 13_600));
        assert!(due(10_000, 99_999));
        assert!(!due(13_600, 13_600));
        assert!(!due(10_000, 9_999));
    }

    #[test]
    fn automation_is_opt_in_and_uses_the_established_lightweight_defaults() {
        assert!(!enabled(&json!({})));
        assert!(!enabled(&json!({"hourly_candy_enabled":"true"})));
        assert!(enabled(&json!({"hourly_candy_enabled":true})));
        assert_eq!(model(&json!({})), "gpt-6.1-sol");
        assert_eq!(effort(&json!({})), "low");
    }
}
