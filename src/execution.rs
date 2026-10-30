//! Two execution paths: native SDK for existing synchronous callers, and loopback HTTP
//! with an instance-owned, single-account Key for jobs independent of any RPC scope.
use crate::host_calls::{self, ModelAnswer, ModelError, ModelFailure};
use gateway_plugin_sdk::{client::HostClient, ErrorCode, PluginFault};
use reqwest::{
    header::{HeaderValue, AUTHORIZATION},
    Url,
};
use serde_json::Value;
use std::{
    future::Future,
    net::IpAddr,
    time::{Duration, Instant},
};

/// A background worker never retains an expired SDK HostClient.
pub trait StorageContext {
    fn storage_host(&self) -> Option<&HostClient>;
}
impl StorageContext for HostClient {
    fn storage_host(&self) -> Option<&HostClient> {
        Some(self)
    }
}

pub trait ExecutionContext: StorageContext + Clone + Send + Sync + 'static {
    fn execute(
        &self,
        account: &str,
        provider: &str,
        model: &str,
        body: &Value,
    ) -> impl Future<Output = Result<ModelAnswer, ModelError>> + Send;
}
impl ExecutionContext for HostClient {
    async fn execute(
        &self,
        account: &str,
        provider: &str,
        model: &str,
        body: &Value,
    ) -> Result<ModelAnswer, ModelError> {
        host_calls::execute_body(self, account, provider, model, body).await
    }
}

/// Not Debug/Serialize: the authorization header is deliberately memory-only.
#[derive(Clone)]
pub struct HttpExecutor {
    client: reqwest::Client,
    endpoint: Url,
    account: String,
    provider: String,
    authorization: HeaderValue,
}

fn invalid(message: &str) -> PluginFault {
    PluginFault::new(ErrorCode::InvalidInput, message)
}

/// Only explicit loopback IPs are accepted; neither environment proxies nor redirects
/// may forward the Key outside the CPR process's own network namespace.
pub fn endpoint(base: &str) -> Result<Url, PluginFault> {
    let base = Url::parse(base).map_err(|_| invalid("测试接口地址格式错误"))?;
    let local = base
        .host_str()
        .and_then(|s| s.trim_matches(['[', ']']).parse::<IpAddr>().ok())
        .is_some_and(|ip| ip.is_loopback());
    if !local
        || !matches!(base.scheme(), "http" | "https")
        || !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
        || !matches!(base.path(), "" | "/")
    {
        return Err(invalid(
            "测试接口必须是本机回环地址，不能带认证、路径或查询参数",
        ));
    }
    base.join("v1/responses")
        .map_err(|_| invalid("测试接口地址格式错误"))
}

impl HttpExecutor {
    pub(crate) fn new(
        endpoint: Url,
        account: String,
        provider: String,
        plaintext_key: String,
    ) -> Result<Self, PluginFault> {
        let mut authorization = HeaderValue::from_str(&format!("Bearer {plaintext_key}"))
            .map_err(|_| invalid("测试 Key 格式错误"))?;
        authorization.set_sensitive(true);
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(360))
            .build()
            .map_err(|_| invalid("无法初始化后台测试连接"))?;
        Ok(Self {
            client,
            endpoint,
            account,
            provider,
            authorization,
        })
    }
}
impl StorageContext for HttpExecutor {
    fn storage_host(&self) -> Option<&HostClient> {
        None
    }
}
impl ExecutionContext for HttpExecutor {
    async fn execute(
        &self,
        account: &str,
        provider: &str,
        _model: &str,
        body: &Value,
    ) -> Result<ModelAnswer, ModelError> {
        if account != self.account || provider != self.provider {
            return Err(ModelError::other(
                ModelFailure::Host,
                "后台测试账号作用域不匹配",
            ));
        }
        let started = Instant::now();
        let mut response = self
            .client
            .post(self.endpoint.clone())
            .header(AUTHORIZATION, self.authorization.clone())
            .json(body)
            .send()
            .await
            .map_err(network_error)?;
        let status = response.status();
        if !status.is_success() {
            // Do not copy CPR/upstream error bodies: these may contain credentials/URLs.
            let message = match status.as_u16() {
                401 | 403 => "专用测试 Key 被拒绝，请检查 Key 状态和测试分组",
                429 => "专用测试 Key 达到预算、并发或速率上限",
                400 | 404 | 422 => "CPR 拒绝了测试模型或请求参数",
                _ => "CPR 测试接口返回错误",
            };
            return Err(ModelError::other(
                ModelFailure::Host,
                format!("{message}（HTTP {}）", status.as_u16()),
            ));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(network_error)? {
            if bytes.len().saturating_add(chunk.len()) > 8 * 1024 * 1024 {
                return Err(ModelError::other(
                    ModelFailure::Host,
                    "测试回答超过大小上限",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        host_calls::parse_response(&bytes, started)
    }
}
fn network_error(error: reqwest::Error) -> ModelError {
    let message = if error.is_timeout() {
        "后台模型请求超时"
    } else if error.is_connect() {
        "无法连接 CPR 本机测试接口，请检查插件的接口地址配置"
    } else {
        "后台模型连接中断"
    };
    ModelError::other(ModelFailure::Host, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_loopback_without_credentials_or_query_is_accepted() {
        for base in ["http://127.0.0.1:8080", "http://[::1]:8080/"] {
            assert!(endpoint(base).is_ok());
        }
        for base in [
            "https://example.com",
            "http://192.0.2.1",
            "http://localhost",
            "http://user@127.0.0.1",
            "http://127.0.0.1/?key=fixture",
            "http://127.0.0.1/v1",
        ] {
            assert!(endpoint(base).is_err());
        }
    }
    #[test]
    fn authorization_is_sensitive() {
        let executor = HttpExecutor::new(
            endpoint("http://127.0.0.1:8080").unwrap(),
            "fixture".into(),
            "openai".into(),
            "fixture-only-key".into(),
        )
        .unwrap();
        assert!(executor.authorization.is_sensitive());
        assert!(!format!("{:?}", executor.authorization).contains("fixture-only-key"));
    }
}
