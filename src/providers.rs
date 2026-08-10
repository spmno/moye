// 供应商客户端：通过 MY_AGENT_PROVIDER 环境变量选择供应商（deepseek / bailian / moonshot / volcengine）。
// Provider client: selects a provider (deepseek / bailian / moonshot / volcengine) via the MY_AGENT_PROVIDER env var.
// 均使用 OpenAI 兼容接口，Bailian 百炼平台、Moonshot Kimi 平台和火山引擎 Ark 通过自定义 base URL 接入。
// All providers use the OpenAI-compatible interface; Bailian, Moonshot Kimi, and Volcengine Ark connect via custom base URLs.
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
    /// 火山引擎 Ark 平台（https://www.volcengine.com/product/ark）。通过 OpenAI 兼容接口接入。
    /// Volcengine Ark platform (https://www.volcengine.com/product/ark). OpenAI-compatible API.
    Volcengine,
    /// OpenAI（https://platform.openai.com）。
    OpenAI,
    /// Anthropic Claude（https://platform.claude.com）。官方提供 OpenAI 兼容层。
    /// Anthropic Claude (https://platform.claude.com). Official OpenAI-compatible layer.
    Claude,
    /// 小米 MiMo（https://mimo.mi.com）。OpenAI + Anthropic 双兼容。
    /// Xiaomi MiMo (https://mimo.mi.com). OpenAI + Anthropic dual-compatible.
    MiMo,
    /// Google Gemini（https://ai.google.dev）。通过官方 OpenAI 兼容端点接入。
    /// Google Gemini (https://ai.google.dev). Via official OpenAI-compatible endpoint.
    Gemini,
    /// 智谱 GLM（https://open.bigmodel.cn）。OpenAI 兼容接口。
    /// Zhipu GLM (https://open.bigmodel.cn). OpenAI-compatible API.
    Zhipu,
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
            "volcengine" | "volcanoark" | "ark" | "火山" => Provider::Volcengine,
            "openai" => Provider::OpenAI,
            "claude" | "anthropic" => Provider::Claude,
            "mimo" | "xiaomi" => Provider::MiMo,
            "gemini" | "google" => Provider::Gemini,
            "zhipu" | "glm" | "bigmodel" => Provider::Zhipu,
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
            Provider::Volcengine => "ARK_API_KEY",
            Provider::OpenAI => "OPENAI_API_KEY",
            Provider::Claude => "ANTHROPIC_API_KEY",
            Provider::MiMo => "MIMO_API_KEY",
            Provider::Gemini => "GEMINI_API_KEY",
            Provider::Zhipu => "ZAI_API_KEY",
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
            Provider::Volcengine => "https://ark.cn-beijing.volces.com/api/plan/v3".to_string(),
            Provider::OpenAI => "https://api.openai.com/v1".to_string(),
            Provider::Claude => "https://api.anthropic.com/v1/".to_string(),
            Provider::MiMo => "https://api.xiaomimimo.com/v1".to_string(),
            Provider::Gemini => "https://generativelanguage.googleapis.com/v1beta/openai/".to_string(),
            Provider::Zhipu => "https://open.bigmodel.cn/api/paas/v4".to_string(),
            Provider::Custom => "https://api.openai.com/v1".to_string(),
        }
    }

    /// 该供应商的默认 API key 环境变量名（不含 config 自定义覆盖）。
    /// Default API key env var name for this provider (without config overrides).
    fn default_api_key_env(&self) -> &'static str {
        match self {
            Provider::DeepSeek => "DEEPSEEK_API_KEY",
            Provider::Bailian => "DASHSCOPE_API_KEY",
            Provider::Moonshot => "MOONSHOT_API_KEY",
            Provider::Volcengine => "ARK_API_KEY",
            Provider::OpenAI => "OPENAI_API_KEY",
            Provider::Claude => "ANTHROPIC_API_KEY",
            Provider::MiMo => "MIMO_API_KEY",
            Provider::Gemini => "GEMINI_API_KEY",
            Provider::Zhipu => "ZAI_API_KEY",
            Provider::Custom => "MY_AGENT_API_KEY",
        }
    }

    fn default_base_url(&self) -> &'static str {
        match self {
            Provider::DeepSeek => "https://api.deepseek.com/v1",
            Provider::Bailian => "https://dashscope.aliyuncs.com/compatible-mode/v1",
            Provider::Moonshot => "https://api.moonshot.cn/v1",
            Provider::Volcengine => "https://ark.cn-beijing.volces.com/api/plan/v3",
            Provider::OpenAI => "https://api.openai.com/v1",
            Provider::Claude => "https://api.anthropic.com/v1/",
            Provider::MiMo => "https://api.xiaomimimo.com/v1",
            Provider::Gemini => "https://generativelanguage.googleapis.com/v1beta/openai/",
            Provider::Zhipu => "https://open.bigmodel.cn/api/paas/v4",
            Provider::Custom => "https://api.openai.com/v1",
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

/// 把 slug 字符串解析为 [`Provider`]；未知值回退 DeepSeek。
/// Parse a slug string into a [`Provider`]; unknown values fall back to DeepSeek.
pub fn parse_provider(raw: &str) -> Provider {
    match raw.to_lowercase().as_str() {
        "bailian" => Provider::Bailian,
        "moonshot" | "kimi" => Provider::Moonshot,
        "volcengine" | "volcanoark" | "ark" | "火山" => Provider::Volcengine,
        "openai" => Provider::OpenAI,
        "claude" | "anthropic" => Provider::Claude,
        "mimo" | "xiaomi" => Provider::MiMo,
        "gemini" | "google" => Provider::Gemini,
        "zhipu" | "glm" | "bigmodel" => Provider::Zhipu,
        _ => Provider::Custom,
    }
}

/// 构建客户端，支持 session 级 provider/base_url 覆盖（切回历史模型时恢复当时的网关）。
/// Build a client with optional session-level provider/base_url overrides (restores the
/// gateway used at the time when switching back to a historical model).
///
/// provider override 存在时，API key 变量名跟随该 provider 的默认——切换供应商即切换
/// key 来源，否则切了 provider 仍读旧变量名会取不到 key。base_url override 优先于
/// env / config / provider 默认。
/// When a provider override is present, the API key env var follows that provider's default
/// — switching providers switches the key source, otherwise the old var name would be read
/// and the key would be missing. base_url override takes priority over env / config / default.
pub fn create_client_with(
    provider_override: Option<&str>,
    base_url_override: Option<&str>,
) -> Result<CompletionsClient> {
    let provider = provider_override
        .map(parse_provider)
        .unwrap_or_else(Provider::from_env);
    let base_url = base_url_override
        .map(str::to_string)
        .or_else(|| std::env::var("MY_AGENT_BASE_URL").ok())
        .or_else(|| {
            crate::config::config()
                .and_then(|c| c.provider.base_url.clone())
                .filter(|u| !u.trim().is_empty())
        })
        .unwrap_or_else(|| provider.default_base_url().to_string());
    let api_key_env: String = if provider_override.is_some() {
        provider.default_api_key_env().to_string()
    } else {
        provider.api_key_env()
    };
    // 当前目录优先：env（项目 .env / export）→ 全局 config.toml [keys] 兜底。
    // Current dir first: env (project .env / export) → global config.toml [keys] fallback.
    let api_key = std::env::var(&api_key_env)
        .ok()
        .or_else(|| {
            crate::config::config()
                .and_then(|c| c.keys.get(&api_key_env).cloned())
        })
        .ok_or_else(|| anyhow::anyhow!(
            "{} \u{672a}\u{8bbe}\u{7f6e}\u{3002}\u{8bf7}\u{5728}\u{9879}\u{76ee}\u{6839}\u{76ee}\u{5f55}\u{7684} .env \u{4e2d}\u{914d}\u{7f6e}\u{ff08}\u{53c2}\u{8003} .env.example\u{ff09}\u{6216} export {}",
            api_key_env,
            api_key_env
        ))?;
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

/// 返回当前生效的供应商（小写 slug 字符串），供模型历史记录展示。
/// Return the currently active provider as a lowercase slug string, for model-history display.
pub fn current_provider_slug() -> String {
    match Provider::from_env() {
        Provider::DeepSeek => "deepseek",
        Provider::Bailian => "bailian",
        Provider::Moonshot => "moonshot",
        Provider::Volcengine => "volcengine",
        Provider::OpenAI => "openai",
        Provider::Claude => "claude",
        Provider::MiMo => "mimo",
        Provider::Gemini => "gemini",
        Provider::Zhipu => "zhipu",
        Provider::Custom => "custom",
    }
    .to_string()
}

/// 返回当前生效的 OpenAI 兼容 base URL，供模型历史记录写入。
/// Return the currently effective OpenAI-compatible base URL, written into model history.
pub fn current_base_url() -> String {
    Provider::from_env().base_url()
}

/// 返回当前供应商需要的额外请求参数。
/// Return the extra request parameters required by the current provider.
pub fn provider_additional_params() -> serde_json::Value {
    serde_json::json!({})
}

/// 对话型 Agent 别名：基于 OpenAI CompletionModel 的 rig Agent（兼容所有供应商）。
/// Chat Agent alias: a rig Agent based on OpenAI CompletionModel (compatible with all providers).
pub type ChatAgent = rig_agent::agent::Agent<openai::CompletionModel>;

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
        Provider::Volcengine => vec![
            ModelInfo { slug: "doubao-1-5-pro-256k".into(), desc: "豆包 1.5 Pro · 长上下文" },
            ModelInfo { slug: "doubao-1-5-lite-32k".into(), desc: "豆包 1.5 Lite · 轻量快速" },
            ModelInfo { slug: "deepseek-r1-250120".into(), desc: "DeepSeek R1 · 推理模型" },
        ],
        Provider::OpenAI => vec![
            ModelInfo { slug: "gpt-4o".into(), desc: "GPT-4o · 旗舰模型" },
            ModelInfo { slug: "gpt-4o-mini".into(), desc: "GPT-4o mini · 轻量快速" },
            ModelInfo { slug: "o1".into(), desc: "o1 · 推理模型" },
        ],
        Provider::Claude => vec![
            ModelInfo { slug: "claude-opus-5".into(), desc: "Claude Opus 5 · 旗舰推理" },
            ModelInfo { slug: "claude-sonnet-4-6".into(), desc: "Claude Sonnet 4.6 · 均衡" },
            ModelInfo { slug: "claude-haiku-4-5".into(), desc: "Claude Haiku 4.5 · 快速" },
        ],
        Provider::MiMo => vec![
            ModelInfo { slug: "mimo-v2.5-pro".into(), desc: "MiMo v2.5 Pro · 旗舰" },
        ],
        Provider::Gemini => vec![
            ModelInfo { slug: "gemini-2.5-pro".into(), desc: "Gemini 2.5 Pro · 长上下文" },
            ModelInfo { slug: "gemini-2.5-flash".into(), desc: "Gemini 2.5 Flash · 快速" },
        ],
        Provider::Zhipu => vec![
            ModelInfo { slug: "glm-5.2".into(), desc: "GLM-5.2 · 旗舰" },
            ModelInfo { slug: "glm-4-flash".into(), desc: "GLM-4 Flash · 轻量" },
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
            Provider::Volcengine => 256_000,
            Provider::OpenAI => 128_000,
            Provider::Claude => 200_000,
            Provider::MiMo => 128_000,
            Provider::Gemini => 1_000_000,
            Provider::Zhipu => 1_000_000,
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
    // DeepSeek V4 系列支持 1M 上下文窗口。
    // DeepSeek V4 series supports a 1M context window.
    if lower.contains("deepseek-v4-pro") || lower.contains("deepseek-v4-flash") {
        return 1_000_000;
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
    if lower.contains("glm") {
        return 1_000_000;
    }
    if lower.contains("claude") {
        return 200_000;
    }
    if lower.contains("gemini") {
        return 1_000_000;
    }
    if lower.contains("mimo") {
        return 128_000;
    }
    if lower.contains("gpt-4o") {
        return 128_000;
    }
    if lower.contains("o1") {
        return 200_000;
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
    fn new_provider_context_limits() {
        assert_eq!(Provider::OpenAI.context_limit(), 128_000);
        assert_eq!(Provider::Claude.context_limit(), 200_000);
        assert_eq!(Provider::MiMo.context_limit(), 128_000);
        assert_eq!(Provider::Gemini.context_limit(), 1_000_000);
        assert_eq!(Provider::Zhipu.context_limit(), 1_000_000);
    }

    #[test]
    fn context_limit_for_kimi_model() {
        assert_eq!(context_limit_for_model("kimi-k3"), 256_000);
    }

    #[test]
    fn context_limit_for_new_models() {
        assert_eq!(context_limit_for_model("claude-opus-5"), 200_000);
        assert_eq!(context_limit_for_model("gemini-2.5-pro"), 1_000_000);
        assert_eq!(context_limit_for_model("mimo-v2.5-pro"), 128_000);
        assert_eq!(context_limit_for_model("gpt-4o"), 128_000);
        assert_eq!(context_limit_for_model("o1"), 200_000);
    }

    #[test]
    fn context_limit_for_deepseek_v4_model() {
        assert_eq!(context_limit_for_model("deepseek-v4-pro"), 1_000_000);
        assert_eq!(context_limit_for_model("deepseek-v4-flash"), 1_000_000);
    }

    #[test]
    fn context_limit_for_deepseek_legacy_model() {
        // 非 V4 系列的 DeepSeek 模型仍为 128K。
        // Non-V4 DeepSeek models remain 128K.
        assert_eq!(context_limit_for_model("deepseek-r1-250120"), 128_000);
    }

    #[test]
    fn context_limit_for_unknown_falls_back() {
        // 未知模型应回退到供应商默认值，不 panic。
        // Unknown model should fall back to provider default, no panic.
        let limit = context_limit_for_model("unknown-model-xyz");
        assert!(limit > 0);
    }

    #[test]
    fn parse_provider_accepts_volcanoark_alias() {
        assert_eq!(parse_provider("volcanoark"), Provider::Volcengine);
        assert_eq!(parse_provider("VOLCANOARK"), Provider::Volcengine);
        assert_eq!(parse_provider("volcengine"), Provider::Volcengine);
        assert_eq!(parse_provider("ark"), Provider::Volcengine);
    }

    #[test]
    fn parse_provider_new_providers() {
        assert_eq!(parse_provider("openai"), Provider::OpenAI);
        assert_eq!(parse_provider("claude"), Provider::Claude);
        assert_eq!(parse_provider("anthropic"), Provider::Claude);
        assert_eq!(parse_provider("mimo"), Provider::MiMo);
        assert_eq!(parse_provider("xiaomi"), Provider::MiMo);
        assert_eq!(parse_provider("gemini"), Provider::Gemini);
        assert_eq!(parse_provider("google"), Provider::Gemini);
        assert_eq!(parse_provider("zhipu"), Provider::Zhipu);
        assert_eq!(parse_provider("glm"), Provider::Zhipu);
        assert_eq!(parse_provider("bigmodel"), Provider::Zhipu);
    }

    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn from_env_accepts_volcanoark_alias() {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::set_var("MY_AGENT_PROVIDER", "volcanoark");
        }
        assert_eq!(Provider::from_env(), Provider::Volcengine);
        unsafe {
            std::env::remove_var("MY_AGENT_PROVIDER");
        }
    }
}
