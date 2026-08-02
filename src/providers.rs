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
    /// 解析供应商：`MY_AGENT_PROVIDER` 环境变量优先，其次 agent.toml 的
    /// `[provider].provider`；均缺失时默认 deepseek。
    /// Resolves the provider: `MY_AGENT_PROVIDER` env var wins, then the
    /// `[provider].provider` from agent.toml; defaults to deepseek when absent.
    fn from_env() -> Self {
        let configured = crate::config::config().and_then(|c| c.provider.provider.clone());
        let raw = std::env::var("MY_AGENT_PROVIDER")
            .ok()
            .or(configured)
            .unwrap_or_default();
        match raw.to_lowercase().as_str() {
            "bailian" => Provider::Bailian,
            "moonshot" | "kimi" => Provider::Moonshot,
            "custom" | "openai" | "glm" => Provider::Custom,
            _ => Provider::DeepSeek,
        }
    }

    /// API Key 环境变量名。agent.toml 的 `[provider].api_key_env` 可自定义；
    /// 为空时按供应商默认。
    /// The env var name holding the API key. `[provider].api_key_env` in agent.toml
    /// can override; falls back to the provider default when empty.
    fn api_key_env(&self) -> String {
        if let Some(name) = crate::config::config()
            .and_then(|c| c.provider.api_key_env.clone())
            .filter(|n| !n.trim().is_empty())
        {
            return name;
        }
        match self {
            Provider::DeepSeek => "DEEPSEEK_API_KEY",
            Provider::Bailian => "DASHSCOPE_API_KEY",
            Provider::Moonshot => "MOONSHOT_API_KEY",
            Provider::Custom => "MY_AGENT_API_KEY",
        }
        .to_string()
    }

    /// OpenAI 兼容 base URL。优先级：`MY_AGENT_BASE_URL` env → `[provider].base_url`
    /// → 供应商默认。
    /// OpenAI-compatible base URL. Precedence: `MY_AGENT_BASE_URL` env →
    /// `[provider].base_url` → provider default.
    fn base_url(&self) -> String {
        if let Ok(url) = std::env::var("MY_AGENT_BASE_URL") {
            return url;
        }
        if let Some(url) = crate::config::config()
            .and_then(|c| c.provider.base_url.clone())
            .filter(|u| !u.trim().is_empty())
        {
            return url;
        }
        match self {
            Provider::DeepSeek => "https://api.deepseek.com/v1".to_string(),
            Provider::Bailian => "https://dashscope.aliyuncs.com/compatible-mode/v1".to_string(),
            Provider::Moonshot => "https://api.moonshot.cn/v1".to_string(),
            Provider::Custom => "https://api.openai.com/v1".to_string(),
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
        .map_err(|_| anyhow::anyhow!(
            "{} \u{672a}\u{8bbe}\u{7f6e}\u{3002}\u{8bf7}\u{5728}\u{9879}\u{76ee}\u{6839}\u{76ee}\u{5f55}\u{7684} .env \u{4e2d}\u{914d}\u{7f6e}\u{ff08}\u{53c2}\u{8003} .env.example\u{ff09}\u{6216} export {}",
            provider.api_key_env(),
            provider.api_key_env()
        ))?;
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

/// 模型目录条目：slug + 面向用户的中文说明。
/// Model catalog entry: slug + user-facing Chinese description.
pub struct ModelInfo {
    pub slug: String,
    pub desc: &'static str,
}

/// 返回指定供应商的推荐模型目录，供 /models 选择器使用。
/// Returns the recommended model catalog for a provider, used by the /models selector.
/// Custom 供应商无内置清单——选择器允许直接输入任意 OpenAI 兼容模型 ID。
/// Custom has no built-in list — the selector allows typing any OpenAI-compatible model ID.
pub fn provider_models(provider: Provider) -> Vec<ModelInfo> {
    match provider {
        Provider::DeepSeek => vec![
            ModelInfo { slug: "deepseek-v4-pro".into(), desc: "旗舰推理模型" },
            ModelInfo { slug: "deepseek-v4-flash".into(), desc: "快速响应，成本低" },
        ],
        Provider::Bailian => vec![
            ModelInfo { slug: "kimi/kimi-k3".into(), desc: "Kimi K3 · 长上下文" },
            ModelInfo { slug: "qwen-plus".into(), desc: "通义千问 Plus" },
            ModelInfo { slug: "qwen-max".into(), desc: "通义千问 Max" },
        ],
        Provider::Moonshot => vec![
            ModelInfo { slug: "kimi-k3".into(), desc: "Kimi K3 · 长上下文" },
            ModelInfo { slug: "kimi-k2.7-code-highspeed".into(), desc: "代码加速版" },
        ],
        Provider::Custom => vec![],
    }
}

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
