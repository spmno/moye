// 供应商客户端：通过 MY_AGENT_PROVIDER 环境变量选择供应商（deepseek / bailian / moonshot）。
// Provider client: selects a provider (deepseek / bailian / moonshot) via the MY_AGENT_PROVIDER env var.
// 均使用 OpenAI 兼容接口，Bailian 百炼平台和 Moonshot Kimi 平台通过自定义 base URL 接入。
// All providers use the OpenAI-compatible interface; Bailian and Moonshot Kimi connect via custom base URLs.
use anyhow::Result;
pub use rig_core::providers::openai::{self, CompletionsClient};
use tracing::info;

/// 供应商类型。
/// Provider type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    DeepSeek,
    Bailian,
    /// Moonshot Kimi 平台（https://platform.kimi.com）。Kimi K3 需要用原生 API。
    /// Moonshot Kimi platform (https://platform.kimi.com). Kimi K3 needs the native API.
    Moonshot,
    /// 自定义 OpenAI 兼容供应商。通过 MY_AGENT_BASE_URL + MY_AGENT_API_KEY 配置。
    /// Custom OpenAI-compatible provider. Configured via MY_AGENT_BASE_URL + MY_AGENT_API_KEY.
    Custom,
}

impl Provider {
    /// 从环境变量 MY_AGENT_PROVIDER 解析；默认 deepseek。
    /// Parse from the MY_AGENT_PROVIDER env var; defaults to deepseek.
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
    /// The env var name holding the API key.
    fn api_key_env(&self) -> &'static str {
        match self {
            Provider::DeepSeek => "DEEPSEEK_API_KEY",
            Provider::Bailian => "DASHSCOPE_API_KEY",
            Provider::Moonshot => "MOONSHOT_API_KEY",
            Provider::Custom => "MY_AGENT_API_KEY",
        }
    }

    /// OpenAI 兼容 base URL（含 /v1 前缀）。
    /// OpenAI-compatible base URL (including the /v1 prefix).
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
    /// Clamp the requested temperature to the range allowed by the provider.
    pub fn clamp_temperature(desired: f64) -> f64 {
        match Self::from_env() {
            Provider::Moonshot => 1.0,
            _ => desired,
        }
    }
}

/// 构建当前供应商的 OpenAI 兼容客户端。
/// Build the OpenAI-compatible client for the current provider.
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
/// Return the currently active provider.
pub fn current_provider() -> Provider {
    Provider::from_env()
}

/// 返回当前供应商需要的额外请求参数。
/// Return the extra request parameters required by the current provider.
pub fn provider_additional_params() -> serde_json::Value {
    match Provider::from_env() {
        Provider::Moonshot => serde_json::json!({}),
        Provider::Bailian | Provider::DeepSeek | Provider::Custom => serde_json::json!({}),
    }
}

/// 对话型 Agent 别名：基于 OpenAI CompletionModel 的 rig Agent（兼容所有供应商）。
/// Chat Agent alias: a rig Agent based on OpenAI CompletionModel (compatible with all providers).
pub type ChatAgent = rig_core::agent::Agent<openai::CompletionModel>;

impl Provider {
    /// 该供应商的默认上下文窗口大小（tokens）。
    /// Default context window size (tokens) for this provider.
    pub fn context_limit(&self) -> usize {
        match self {
            Provider::DeepSeek => 128_000,
            Provider::Bailian => 128_000,
            Provider::Moonshot => 256_000,
            Provider::Custom => 128_000,
        }
    }
}

/// 根据模型 slug 解析上下文窗口大小。无法识别时回退到当前供应商默认值。
/// Resolve context window size by model slug. Falls back to the current provider default
/// when the model is unrecognized.
pub fn context_limit_for_model(model: &str) -> usize {
    let provider = Provider::from_env();
    let lower = model.to_lowercase();
    // 模型特定的覆盖 / Model-specific overrides
    if lower.contains("kimi") {
        return 256_000;
    }
    if lower.contains("deepseek") {
        return 128_000;
    }
    if lower.contains("qwen-max") {
        return 32_000;
    }
    if lower.contains("qwen-plus") {
        return 128_000;
    }
    // 回退到供应商默认值 / Fall back to provider default
    provider.context_limit()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deepseek_provider_context_limit() {
        assert_eq!(Provider::DeepSeek.context_limit(), 128_000);
    }

    #[test]
    fn moonshot_provider_context_limit() {
        assert_eq!(Provider::Moonshot.context_limit(), 256_000);
    }

    #[test]
    fn context_limit_for_kimi_model() {
        assert_eq!(context_limit_for_model("kimi-k3"), 256_000);
    }

    #[test]
    fn context_limit_for_deepseek_model() {
        assert_eq!(context_limit_for_model("deepseek-v4-pro"), 128_000);
    }

    #[test]
    fn context_limit_for_unknown_falls_back() {
        // 未知模型应回退到供应商默认值，不 panic。
        // Unknown model should fall back to provider default, no panic.
        let limit = context_limit_for_model("unknown-model-xyz");
        assert!(limit > 0);
    }
}
