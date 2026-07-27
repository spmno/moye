// 供应商客户端：通过 MY_AGENT_PROVIDER 环境变量选择供应商（deepseek / bailian / moonshot）。
// 均使用 OpenAI 兼容接口，Bailian 百炼平台和 Moonshot Kimi 平台通过自定义 base URL 接入。
use anyhow::Result;
use rig_core::providers::openai::{self, CompletionsClient};
use tracing::info;

/// 供应商类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    DeepSeek,
    Bailian,
    /// Moonshot Kimi 平台（https://platform.kimi.com）。Kimi K3 需要用原生 API。
    Moonshot,
    /// 自定义 OpenAI 兼容供应商。通过 MY_AGENT_BASE_URL + MY_AGENT_API_KEY 配置。
    Custom,
}

impl Provider {
    /// 从环境变量 MY_AGENT_PROVIDER 解析；默认 deepseek。
    fn from_env() -> Self {
        match std::env::var("MY_AGENT_PROVIDER")
            .unwrap_or_default()
            .to_lowercase()
            .as_str()
        {
            "bailian" => Provider::Bailian,
            "moonshot" | "kimi" => Provider::Moonshot,
            "custom" | "openai" | "glm" => Provider::Custom,
            _ => Provider::DeepSeek,
        }
    }

    /// API Key 环境变量名。
    fn api_key_env(&self) -> &'static str {
        match self {
            Provider::DeepSeek => "DEEPSEEK_API_KEY",
            Provider::Bailian => "DASHSCOPE_API_KEY",
            Provider::Moonshot => "MOONSHOT_API_KEY",
            Provider::Custom => "MY_AGENT_API_KEY",
        }
    }

    /// OpenAI 兼容 base URL（含 /v1 前缀）。
    fn base_url(&self) -> String {
        match self {
            Provider::DeepSeek => "https://api.deepseek.com/v1".to_string(),
            Provider::Bailian => "https://dashscope.aliyuncs.com/compatible-mode/v1".to_string(),
            Provider::Moonshot => "https://api.moonshot.cn/v1".to_string(),
            Provider::Custom => std::env::var("MY_AGENT_BASE_URL")
                .unwrap_or_else(|_| "https://api.openai.com/v1".to_string()),
        }
    }

    /// 将请求的 temperature 限制在供应商允许的范围内。
    pub fn clamp_temperature(desired: f64) -> f64 {
        match Self::from_env() {
            Provider::Moonshot => 1.0,
            _ => desired,
        }
    }
}

/// 构建当前供应商的 OpenAI 兼容客户端。
pub fn create_client() -> Result<CompletionsClient> {
    let provider = Provider::from_env();
    let api_key = std::env::var(provider.api_key_env())
        .map_err(|_| anyhow::anyhow!("{} not set", provider.api_key_env()))?;
    let base_url = provider.base_url();
    info!(
        "[provider] {:?} | base_url={} | api_key={}...{}",
        provider,
        base_url,
        &api_key[..8.min(api_key.len())],
        &api_key[api_key.len().saturating_sub(4)..],
    );
    let http = rig_core::http_client::ReqwestClient::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .tcp_keepalive(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| anyhow::anyhow!("HTTP client build failed: {e}"))?;
    let client = CompletionsClient::builder()
        .api_key(api_key)
        .base_url(&base_url)
        .http_client(http)
        .build()?;
    Ok(client)
}

/// 返回当前生效的供应商。
pub fn current_provider() -> Provider {
    Provider::from_env()
}

/// 返回当前供应商需要的额外请求参数。
pub fn provider_additional_params() -> serde_json::Value {
    match Provider::from_env() {
        Provider::Moonshot => serde_json::json!({}),
        Provider::Bailian | Provider::DeepSeek | Provider::Custom => serde_json::json!({}),
    }
}

/// 对话型 Agent 别名：基于 OpenAI CompletionModel 的 rig Agent（兼容所有供应商）。
pub type ChatAgent = rig_core::agent::Agent<openai::CompletionModel>;
