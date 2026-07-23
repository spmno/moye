// 供应商客户端：通过 MY_AGENT_PROVIDER 环境变量选择供应商（deepseek / bailian）。
// 均使用 OpenAI 兼容接口，Bailian 百炼平台通过自定义 base URL 接入。
use anyhow::Result;
use rig_core::providers::openai::{self, CompletionsClient};
use tracing::info;

/// 供应商类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    DeepSeek,
    Bailian,
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
            _ => Provider::DeepSeek,
        }
    }

    /// API Key 环境变量名。
    fn api_key_env(&self) -> &'static str {
        match self {
            Provider::DeepSeek => "DEEPSEEK_API_KEY",
            Provider::Bailian => "DASHSCOPE_API_KEY",
        }
    }

    /// OpenAI 兼容 base URL（含 /v1 前缀）。
    fn base_url(&self) -> &'static str {
        match self {
            Provider::DeepSeek => "https://api.deepseek.com/v1",
            Provider::Bailian => "https://dashscope.aliyuncs.com/compatible-mode/v1",
        }
    }
}

/// 构建当前供应商的 OpenAI 兼容客户端。
pub fn create_client() -> Result<CompletionsClient> {
    let provider = Provider::from_env();
    let api_key = std::env::var(provider.api_key_env())
        .map_err(|_| anyhow::anyhow!("{} not set", provider.api_key_env()))?;
    info!(
        "[provider] {:?} | base_url={} | api_key={}...{}",
        provider,
        provider.base_url(),
        &api_key[..8.min(api_key.len())],
        &api_key[api_key.len().saturating_sub(4)..],
    );
    let client = CompletionsClient::builder()
        .api_key(api_key)
        .base_url(provider.base_url())
        .build()?;
    Ok(client)
}

/// 返回当前生效的供应商。
pub fn current_provider() -> Provider {
    Provider::from_env()
}

/// 返回当前供应商需要的额外请求参数。
/// Bailian/Kimi 需要 reasoning_effort 参数才正常工作。
pub fn provider_additional_params() -> serde_json::Value {
    match Provider::from_env() {
        Provider::Bailian => serde_json::json!({"reasoning_effort": "max"}),
        Provider::DeepSeek => serde_json::json!({}),
    }
}

/// 对话型 Agent 别名：基于 OpenAI CompletionModel 的 rig Agent（兼容所有供应商）。
pub type ChatAgent = rig_core::agent::Agent<openai::CompletionModel>;
