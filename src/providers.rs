// 供应商客户端：通过 AGENT_PROVIDER 环境变量选择供应商（deepseek / bailian / moonshot / volcengine）。
// Provider client: selects a provider (deepseek / bailian / moonshot / volcengine) via the AGENT_PROVIDER env var.
// 均使用 OpenAI 兼容接口，Bailian 百炼平台、Moonshot Kimi 平台和火山引擎 Ark 通过自定义 base URL 接入。
// All providers use the OpenAI-compatible interface; Bailian, Moonshot Kimi, and Volcengine Ark connect via custom base URLs.
use anyhow::Result;
pub use rig_core::providers::openai;
use tracing::info;

/// 全应用统一使用的 HTTP 客户端类型（带可选的原始 HTTP 跟踪，见 http_trace 模块）。
/// The HTTP client type used app-wide (with optional raw HTTP tracing, see the http_trace module).
pub type HttpClient = crate::http_trace::TracingHttpClient;

/// OpenAI 兼容 CompletionsClient（H 固定为带跟踪的客户端）。
/// OpenAI-compatible CompletionsClient (H fixed to the tracing client).
pub type CompletionsClient = openai::CompletionsClient<HttpClient>;

/// OpenAI 兼容 CompletionModel（H 与 CompletionsClient 一致）。
/// OpenAI-compatible CompletionModel (H matches CompletionsClient).
pub type CompletionModel = openai::CompletionModel<HttpClient>;

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
    /// 自定义 OpenAI 兼容供应商。通过 AGENT_BASE_URL + AGENT_API_KEY 配置。
    /// Custom OpenAI-compatible provider. Configured via AGENT_BASE_URL + AGENT_API_KEY.
    Custom,
}

/// API 套餐类型：按量付费（标准）/ Coding Plan / Agent Plan。
/// 各厂商套餐端点不同，且 Coding/Agent Plan 的 API Key 与按量付费 Key 不互通。
/// API plan type: pay-as-you-go (standard) / Coding Plan / Agent Plan.
/// Plan endpoints differ per vendor, and plan API keys are not interchangeable
/// with pay-as-you-go keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiPlan {
    Standard,
    Coding,
    Agent,
}

impl ApiPlan {
    pub fn parse(raw: &str) -> Self {
        match raw.to_lowercase().as_str() {
            "coding" | "code" => ApiPlan::Coding,
            "agent" => ApiPlan::Agent,
            _ => ApiPlan::Standard,
        }
    }

    pub fn slug(&self) -> &'static str {
        match self {
            ApiPlan::Standard => "standard",
            ApiPlan::Coding => "coding",
            ApiPlan::Agent => "agent",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            ApiPlan::Standard => "按量付费 / Standard",
            ApiPlan::Coding => "Coding Plan",
            ApiPlan::Agent => "Agent Plan",
        }
    }
}

impl Provider {
    /// 返回该供应商支持的套餐列表。无套餐端点的供应商只返回 Standard。
    /// Returns the plans supported by this provider. Providers without plan
    /// endpoints return only Standard.
    pub fn supported_plans(&self) -> &'static [ApiPlan] {
        match self {
            Provider::Volcengine => &[ApiPlan::Standard, ApiPlan::Coding, ApiPlan::Agent],
            Provider::Bailian | Provider::Moonshot | Provider::Zhipu => {
                &[ApiPlan::Standard, ApiPlan::Coding]
            }
            _ => &[ApiPlan::Standard],
        }
    }

    /// 是否支持非标准套餐。
    /// Whether this provider supports any non-standard plan.
    #[allow(dead_code)]
    pub fn has_plans(&self) -> bool {
        self.supported_plans().len() > 1
    }

    /// 解析当前套餐：AGENT_PLAN 环境变量优先，其次 agent.toml 的
    /// `[provider].plan`；均缺失时默认 Standard。
    /// Resolves the active plan: AGENT_PLAN env var wins, then
    /// `[provider].plan` from agent.toml; defaults to Standard.
    pub fn plan_from_env() -> ApiPlan {
        if let Ok(raw) = std::env::var("AGENT_PLAN") {
            return ApiPlan::parse(&raw);
        }
        if let Some(raw) = crate::config::config().and_then(|c| c.provider.plan.clone()) {
            return ApiPlan::parse(&raw);
        }
        ApiPlan::Standard
    }

    /// 给定套餐对应的 base URL。Standard 使用厂商默认；Coding/Agent 使用套餐专属端点。
    /// The base URL for the given plan. Standard uses the vendor default;
    /// Coding/Agent use plan-specific endpoints.
    pub fn base_url_for_plan(&self, plan: ApiPlan) -> &'static str {
        match (self, plan) {
            (Provider::Volcengine, ApiPlan::Standard) => "https://ark.cn-beijing.volces.com/api/v3",
            (Provider::Volcengine, ApiPlan::Coding) => {
                "https://ark.cn-beijing.volces.com/api/coding/v3"
            }
            (Provider::Volcengine, ApiPlan::Agent) => {
                "https://ark.cn-beijing.volces.com/api/plan/v3"
            }
            (Provider::Bailian, ApiPlan::Coding) => "https://coding.dashscope.aliyuncs.com/v1",
            (Provider::Moonshot, ApiPlan::Coding) => "https://api.kimi.com/coding/v1",
            (Provider::Zhipu, ApiPlan::Coding) => "https://open.bigmodel.cn/api/coding/paas/v4",
            (p, _) => p.default_base_url(),
        }
    }
}

impl Provider {
    /// 解析供应商：`AGENT_PROVIDER` 环境变量优先，其次 agent.toml 的
    /// `[provider].provider`；均缺失时默认 deepseek。
    /// Resolves the provider: `AGENT_PROVIDER` env var wins, then the
    /// `[provider].provider` from agent.toml; defaults to deepseek when absent.
    pub fn from_env() -> Self {
        let configured = crate::config::config().and_then(|c| c.provider.provider.clone());
        let raw = std::env::var("AGENT_PROVIDER")
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
            "custom" => Provider::Custom,
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
            Provider::Custom => "AGENT_API_KEY",
        }
        .to_string()
    }

    /// OpenAI 兼容 base URL。优先级：`AGENT_BASE_URL` env → `[provider].base_url`
    /// → 当前套餐专属端点 → 供应商默认。
    /// OpenAI-compatible base URL. Precedence: `AGENT_BASE_URL` env →
    /// `[provider].base_url` → active plan endpoint → provider default.
    fn base_url(&self) -> String {
        if let Ok(url) = std::env::var("AGENT_BASE_URL") {
            return url;
        }
        if let Some(url) = crate::config::config()
            .and_then(|c| c.provider.base_url.clone())
            .filter(|u| !u.trim().is_empty())
        {
            return url;
        }
        self.base_url_for_plan(Self::plan_from_env()).to_string()
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
            Provider::Custom => "AGENT_API_KEY",
        }
    }

    fn default_base_url(&self) -> &'static str {
        match self {
            Provider::DeepSeek => "https://api.deepseek.com/v1",
            Provider::Bailian => "https://dashscope.aliyuncs.com/compatible-mode/v1",
            Provider::Moonshot => "https://api.moonshot.cn/v1",
            Provider::Volcengine => "https://ark.cn-beijing.volces.com/api/v3",
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
        "deepseek" => Provider::DeepSeek,
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
    let plan = Provider::plan_from_env();
    let base_url = base_url_override
        .map(str::to_string)
        .or_else(|| std::env::var("AGENT_BASE_URL").ok())
        .or_else(|| {
            crate::config::config()
                .and_then(|c| c.provider.base_url.clone())
                .filter(|u| !u.trim().is_empty())
        })
        .unwrap_or_else(|| provider.base_url_for_plan(plan).to_string());
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
        "[provider] {:?} plan={} | base_url={} | api_key={}...{}",
        provider,
        plan.slug(),
        base_url,
        &api_key[..8.min(api_key.len())],
        &api_key[api_key.len().saturating_sub(4)..],
    );
    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .tcp_keepalive(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| anyhow::anyhow!("HTTP client build failed: {e}"))?;
    let http = crate::http_trace::TracingHttpClient::new(http)?;
    let client = openai::CompletionsClient::builder()
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

/// 返回当前生效的套餐。
/// Return the currently active plan.
pub fn current_plan() -> ApiPlan {
    Provider::plan_from_env()
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

/// 判断模型是否为推理模型（产生独立 reasoning token）。
///
/// 推理模型的 `max_tokens` 限制"推理+可见输出+工具调用"的总和。
/// 如果 `max_tokens` 设置过小，模型可能在推理阶段就用完预算，
/// 导致没有任何可见输出或工具调用。
///
/// 对推理模型应跳过 `.max_tokens()`，让模型使用自身默认输出预算。
///
/// Detect whether a model is a reasoning model (produces separate reasoning tokens).
///
/// For reasoning models, `max_tokens` limits reasoning + visible output + tool calls combined.
/// A small `max_tokens` may cause the model to exhaust its budget on reasoning alone,
/// leaving nothing for visible output or tool calls.
///
/// Skip `.max_tokens()` for reasoning models to let them use their default output budget.
pub fn is_reasoning_model(model: &str) -> bool {
    let lower = model.to_lowercase();
    // GLM 系列（glm-latest, glm-4.7, glm-5, glm-5.2 等）支持 thinking 模式
    // GLM series supports thinking mode
    lower.contains("glm-")
        // DeepSeek V4 系列（deepseek-v4-pro / deepseek-v4-flash）thinking 默认开启，
        // 输出 `reasoning_content` 字段，属于推理模型。
        // DeepSeek V4 series (pro/flash) has thinking enabled by default and
        // emits `reasoning_content`, so it is a reasoning model.
        || lower.starts_with("deepseek-v4")
        // OpenAI o 系列（o1, o3, o4）/ OpenAI o-series
        || lower.starts_with("o1") || lower.starts_with("o3") || lower.starts_with("o4")
        // GPT-5+ 系列 / GPT-5+ series
        || lower.contains("gpt-5")
        // Claude 3.7+ / 4+ 支持扩展思考 / supports extended thinking
        || lower.contains("claude-3.7") || lower.contains("claude-4")
        // 火山引擎豆包 Seed 系列（doubao-seed-evolving / doubao-seed-2-0-pro /
        // doubao-seed-2.0-code / doubao-seed-2-1-* 等）均在响应中返回
        // `reasoning_content` 字段，属于推理模型，需要跳过 max_tokens 以避免
        // 推理阶段耗尽预算导致可见输出被截断成一两个字。
        // Volcengine Doubao Seed series (all variants) emit `reasoning_content`.
        || lower.starts_with("doubao-seed")
}

/// 对话型 Agent 别名：基于 OpenAI CompletionModel 的 rig Agent（兼容所有供应商）。
/// Chat Agent alias: a rig Agent based on OpenAI CompletionModel (compatible with all providers).
pub type ChatAgent = rig_agent::agent::Agent<CompletionModel>;

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
#[allow(dead_code)]
pub fn provider_models(provider: Provider) -> Vec<ModelInfo> {
    provider_models_for_plan(provider, Provider::plan_from_env())
}

/// 返回指定供应商 + 套餐的推荐模型目录。
/// Coding/Agent Plan 的可用模型与按量付费不同（端点也不同）。
/// Returns the recommended model catalog for a provider + plan.
/// Coding/Agent Plan models differ from pay-as-you-go.
pub fn provider_models_for_plan(provider: Provider, plan: ApiPlan) -> Vec<ModelInfo> {
    match (provider, plan) {
        (Provider::Volcengine, ApiPlan::Agent) => vec![
            ModelInfo {
                slug: "doubao-seed-evolving".into(),
                desc: "Doubao Seed Evolving · 周迭代旗舰",
            },
            ModelInfo {
                slug: "deepseek-v4-pro".into(),
                desc: "DeepSeek V4 Pro · 尝鲜版，1M 上下文",
            },
            ModelInfo {
                slug: "deepseek-v4-flash".into(),
                desc: "DeepSeek V4 Flash · 快速，1M 上下文",
            },
            ModelInfo {
                slug: "kimi-k3".into(),
                desc: "Kimi K3 · 1M 上下文",
            },
            ModelInfo {
                slug: "glm-5.2".into(),
                desc: "GLM-5.2 · 1M 上下文",
            },
            ModelInfo {
                slug: "kimi-k2.7-code".into(),
                desc: "Kimi K2.7 Code · 256K",
            },
            ModelInfo {
                slug: "minimax-m3".into(),
                desc: "MiniMax M3 · 1M 上下文",
            },
            ModelInfo {
                slug: "ark-code-latest".into(),
                desc: "Ark Code · 路由模型（后台可切换）",
            },
        ],
        (Provider::Volcengine, ApiPlan::Coding) => vec![
            ModelInfo {
                slug: "doubao-seed-2.0-code".into(),
                desc: "Doubao Seed 2.0 Code · 编程专用",
            },
            ModelInfo {
                slug: "doubao-seed-2.0-pro".into(),
                desc: "Doubao Seed 2.0 Pro · 旗舰",
            },
            ModelInfo {
                slug: "doubao-seed-2.0-lite".into(),
                desc: "Doubao Seed 2.0 Lite · 轻量",
            },
            ModelInfo {
                slug: "ark-code-latest".into(),
                desc: "Ark Code · 路由模型（后台可切换）",
            },
        ],
        (Provider::Volcengine, ApiPlan::Standard) => vec![
            ModelInfo {
                slug: "doubao-seed-evolving".into(),
                desc: "Doubao Seed Evolving · 周迭代旗舰",
            },
            ModelInfo {
                slug: "doubao-seed-2-1-pro-260628".into(),
                desc: "Doubao Seed 2.1 Pro · 旗舰，256K",
            },
            ModelInfo {
                slug: "doubao-seed-2-1-turbo-260628".into(),
                desc: "Doubao Seed 2.1 Turbo · 快速，256K",
            },
            ModelInfo {
                slug: "doubao-seed-2-0-lite-260428".into(),
                desc: "Doubao Seed 2.0 Lite · 轻量",
            },
            ModelInfo {
                slug: "deepseek-r1-250120".into(),
                desc: "DeepSeek R1 · 推理模型",
            },
        ],
        (Provider::Bailian, ApiPlan::Coding) => vec![
            ModelInfo {
                slug: "qwen3.7-plus".into(),
                desc: "Qwen3.7 Plus · 均衡，1M 上下文",
            },
            ModelInfo {
                slug: "qwen3-coder-plus".into(),
                desc: "Qwen3 Coder Plus · 编程专用",
            },
            ModelInfo {
                slug: "qwen3-coder-flash".into(),
                desc: "Qwen3 Coder Flash · 编程快速",
            },
            ModelInfo {
                slug: "qwen3.7-flash".into(),
                desc: "Qwen3.7 Flash · 快速，1M",
            },
        ],
        (Provider::Bailian, _) => vec![
            ModelInfo {
                slug: "qwen3.8-max".into(),
                desc: "通义千问 3.8 Max · 旗舰，1M 上下文",
            },
            ModelInfo {
                slug: "qwen3.7-plus".into(),
                desc: "通义千问 3.7 Plus · 均衡，1M 上下文",
            },
            ModelInfo {
                slug: "qwen3.7-flash".into(),
                desc: "通义千问 3.7 Flash · 快速，1M 上下文",
            },
            ModelInfo {
                slug: "qwen-plus".into(),
                desc: "通义千问 Plus（稳定版）",
            },
        ],
        (Provider::Moonshot, ApiPlan::Coding) => vec![
            ModelInfo {
                slug: "kimi-for-coding".into(),
                desc: "Kimi for Coding · 编程专用，稳定 ID",
            },
            ModelInfo {
                slug: "kimi-for-coding-highspeed".into(),
                desc: "Kimi for Coding HighSpeed · 5–6× 加速",
            },
            ModelInfo {
                slug: "k3".into(),
                desc: "Kimi K3 · 旗舰",
            },
        ],
        (Provider::Moonshot, _) => vec![
            ModelInfo {
                slug: "kimi-k3".into(),
                desc: "Kimi K3 · 旗舰，2.8T 参数，1M 上下文",
            },
            ModelInfo {
                slug: "kimi-k2.7-code-highspeed".into(),
                desc: "Kimi K2.7 Code · 代码加速版，256K",
            },
        ],
        (Provider::Zhipu, ApiPlan::Coding) => vec![
            ModelInfo {
                slug: "glm-5.2".into(),
                desc: "GLM-5.2 · 旗舰，1M 上下文",
            },
            ModelInfo {
                slug: "glm-5".into(),
                desc: "GLM-5 · 混合推理",
            },
            ModelInfo {
                slug: "glm-4.7".into(),
                desc: "GLM-4.7 · 代码优化",
            },
        ],
        (Provider::Zhipu, _) => vec![
            ModelInfo {
                slug: "glm-5.3-flash".into(),
                desc: "GLM-5.3 Flash · 快速经济，多模态，1M 上下文",
            },
            ModelInfo {
                slug: "glm-5.2".into(),
                desc: "GLM-5.2 · 旗舰，1M 上下文",
            },
            ModelInfo {
                slug: "glm-5".into(),
                desc: "GLM-5 · 混合推理",
            },
        ],
        (Provider::DeepSeek, _) => vec![
            ModelInfo {
                slug: "deepseek-v4-pro".into(),
                desc: "DeepSeek V4 Pro · 旗舰推理，1M 上下文",
            },
            ModelInfo {
                slug: "deepseek-v4-flash".into(),
                desc: "DeepSeek V4 Flash · 快速经济，1M 上下文",
            },
        ],
        (Provider::OpenAI, _) => vec![
            ModelInfo {
                slug: "gpt-5.6-sol".into(),
                desc: "GPT-5.6 Sol · 旗舰推理与编码",
            },
            ModelInfo {
                slug: "gpt-5.6-terra".into(),
                desc: "GPT-5.6 Terra · 智能与成本均衡",
            },
            ModelInfo {
                slug: "gpt-5.6-luna".into(),
                desc: "GPT-5.6 Luna · 高性价比",
            },
        ],
        (Provider::Claude, _) => vec![
            ModelInfo {
                slug: "claude-fable-5".into(),
                desc: "Claude Fable 5 · 长程 Agent 智能",
            },
            ModelInfo {
                slug: "claude-opus-5".into(),
                desc: "Claude Opus 5 · 旗舰编码，1M 上下文",
            },
            ModelInfo {
                slug: "claude-sonnet-5".into(),
                desc: "Claude Sonnet 5 · 速度与智能均衡",
            },
            ModelInfo {
                slug: "claude-haiku-4-5".into(),
                desc: "Claude Haiku 4.5 · 最快，200K",
            },
        ],
        (Provider::MiMo, _) => vec![
            ModelInfo {
                slug: "mimo-v2.5-pro".into(),
                desc: "MiMo v2.5 Pro · 旗舰，1M 上下文",
            },
            ModelInfo {
                slug: "mimo-v2.5".into(),
                desc: "MiMo v2.5 · 多模态理解，1M 上下文",
            },
        ],
        (Provider::Gemini, _) => vec![
            ModelInfo {
                slug: "gemini-3.1-pro-preview".into(),
                desc: "Gemini 3.1 Pro · 旗舰推理，1M 上下文",
            },
            ModelInfo {
                slug: "gemini-3.6-flash".into(),
                desc: "Gemini 3.6 Flash · 前沿性能，1M 上下文",
            },
            ModelInfo {
                slug: "gemini-3.5-flash-lite".into(),
                desc: "Gemini 3.5 Flash-Lite · 低成本高吞吐",
            },
        ],
        (Provider::Custom, _) => vec![],
    }
}

impl Provider {
    /// 该供应商的默认上下文窗口大小（tokens）。
    /// Default context window size (tokens) for this provider.
    pub fn context_limit(&self) -> usize {
        match self {
            Provider::DeepSeek => 1_000_000,
            Provider::Bailian => 1_000_000,
            Provider::Moonshot => 1_000_000,
            Provider::Volcengine => 256_000,
            Provider::OpenAI => 400_000,
            Provider::Claude => 1_000_000,
            Provider::MiMo => 1_000_000,
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
    if lower.contains("kimi-k3") {
        return 1_000_000;
    }
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
    // Doubao Seed 系列上下文 256K。
    // Doubao Seed series has 256K context.
    if lower.contains("doubao-seed") {
        return 256_000;
    }
    if lower.contains("doubao") {
        return 256_000;
    }
    // Qwen3 Max/Plus/Flash 系列 1M 上下文。
    // Qwen3 Max/Plus/Flash series has 1M context.
    if lower.contains("qwen3") || lower.contains("qwen-plus") || lower.contains("qwen-flash") {
        return 1_000_000;
    }
    if lower.contains("qwen-max") {
        return 32_000;
    }
    if lower.contains("glm") {
        return 1_000_000;
    }
    // Claude Fable/Opus/Sonnet 5 及 4.6+ 系列 1M 上下文；Haiku 4.5 为 200K。
    // Claude Fable/Opus/Sonnet 5 and 4.6+ series have 1M context; Haiku 4.5 has 200K.
    if lower.contains("haiku") {
        return 200_000;
    }
    if lower.contains("claude") {
        return 1_000_000;
    }
    if lower.contains("gemini") {
        return 1_000_000;
    }
    if lower.contains("mimo") {
        return 1_000_000;
    }
    // GPT-5.6 系列 400K 上下文。
    // GPT-5.6 series has 400K context.
    if lower.contains("gpt-5") {
        return 400_000;
    }
    if lower.contains("gpt-4o") {
        return 128_000;
    }
    if lower.contains("o1") || lower.contains("o3") {
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
        assert_eq!(Provider::DeepSeek.context_limit(), 1_000_000);
    }

    #[test]
    fn moonshot_provider_context_limit() {
        assert_eq!(Provider::Moonshot.context_limit(), 1_000_000);
    }

    #[test]
    fn new_provider_context_limits() {
        assert_eq!(Provider::OpenAI.context_limit(), 400_000);
        assert_eq!(Provider::Claude.context_limit(), 1_000_000);
        assert_eq!(Provider::MiMo.context_limit(), 1_000_000);
        assert_eq!(Provider::Gemini.context_limit(), 1_000_000);
        assert_eq!(Provider::Zhipu.context_limit(), 1_000_000);
        assert_eq!(Provider::Volcengine.context_limit(), 256_000);
        assert_eq!(Provider::Bailian.context_limit(), 1_000_000);
    }

    #[test]
    fn context_limit_for_kimi_model() {
        assert_eq!(context_limit_for_model("kimi-k3"), 1_000_000);
        assert_eq!(context_limit_for_model("kimi-k2.7-code-highspeed"), 256_000);
    }

    #[test]
    fn context_limit_for_new_models() {
        assert_eq!(context_limit_for_model("claude-opus-5"), 1_000_000);
        assert_eq!(context_limit_for_model("claude-fable-5"), 1_000_000);
        assert_eq!(context_limit_for_model("claude-sonnet-5"), 1_000_000);
        assert_eq!(context_limit_for_model("claude-haiku-4-5"), 200_000);
        assert_eq!(context_limit_for_model("gemini-3.1-pro-preview"), 1_000_000);
        assert_eq!(context_limit_for_model("gemini-3.6-flash"), 1_000_000);
        assert_eq!(context_limit_for_model("mimo-v2.5-pro"), 1_000_000);
        assert_eq!(context_limit_for_model("gpt-5.6-sol"), 400_000);
        assert_eq!(context_limit_for_model("gpt-5.6-terra"), 400_000);
        assert_eq!(context_limit_for_model("gpt-5.6-luna"), 400_000);
    }

    #[test]
    fn context_limit_for_doubao_seed_models() {
        assert_eq!(context_limit_for_model("doubao-seed-evolving"), 256_000);
        assert_eq!(
            context_limit_for_model("doubao-seed-2-1-pro-260628"),
            256_000
        );
        assert_eq!(
            context_limit_for_model("doubao-seed-2-1-turbo-260628"),
            256_000
        );
    }

    #[test]
    fn context_limit_for_qwen3_models() {
        assert_eq!(context_limit_for_model("qwen3.8-max"), 1_000_000);
        assert_eq!(context_limit_for_model("qwen3.7-plus"), 1_000_000);
        assert_eq!(context_limit_for_model("qwen3.7-flash"), 1_000_000);
        assert_eq!(context_limit_for_model("qwen-plus"), 1_000_000);
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

    #[test]
    fn setup_model_catalogs_include_models_for_each_non_custom_provider() {
        for slug in [
            "deepseek",
            "bailian",
            "moonshot",
            "volcengine",
            "openai",
            "claude",
            "mimo",
            "gemini",
            "zhipu",
        ] {
            let provider = parse_provider(slug);
            assert_ne!(
                provider,
                Provider::Custom,
                "{slug} must parse to its own provider in setup",
            );
            for plan in provider.supported_plans() {
                let models = provider_models_for_plan(provider, *plan);
                assert!(
                    !models.is_empty(),
                    "{slug} {plan:?} must expose setup models",
                );
            }
        }
    }

    #[test]
    fn setup_deepseek_standard_catalog_lists_v4_models() {
        let models = provider_models_for_plan(Provider::DeepSeek, ApiPlan::Standard);
        let slugs: Vec<_> = models.iter().map(|model| model.slug.as_str()).collect();

        assert_eq!(slugs, ["deepseek-v4-pro", "deepseek-v4-flash"],);
    }

    #[test]
    fn setup_zhipu_standard_catalog_lists_flash_model() {
        let models = provider_models_for_plan(Provider::Zhipu, ApiPlan::Standard);
        let slugs: Vec<_> = models.iter().map(|model| model.slug.as_str()).collect();

        assert!(
            slugs.contains(&"glm-5.3-flash"),
            "Zhipu standard catalog must expose glm-5.3-flash, got {slugs:?}",
        );
        assert_eq!(context_limit_for_model("glm-5.3-flash"), 1_000_000);
    }

    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn from_env_accepts_volcanoark_alias() {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::set_var("AGENT_PROVIDER", "volcanoark");
        }
        assert_eq!(Provider::from_env(), Provider::Volcengine);
        unsafe {
            std::env::remove_var("AGENT_PROVIDER");
        }
    }

    #[test]
    fn is_reasoning_model_detects_known_reasoning_models() {
        assert!(is_reasoning_model("glm-latest"));
        assert!(is_reasoning_model("GLM-5.2"));
        assert!(is_reasoning_model("glm-4.7"));
        assert!(is_reasoning_model("glm-5.3-flash"));
        // DeepSeek V4 系列（pro/flash）thinking 默认开启，输出 reasoning_content，
        // 属于推理模型。
        // DeepSeek V4 series (pro/flash) has thinking enabled by default and emits
        // reasoning_content, so it is a reasoning model.
        assert!(is_reasoning_model("deepseek-v4-pro"));
        assert!(is_reasoning_model("deepseek-v4-flash"));
        assert!(is_reasoning_model("DEEPSEEK-V4-PRO"));
        assert!(is_reasoning_model("o1-mini"));
        assert!(is_reasoning_model("o3-mini"));
        assert!(is_reasoning_model("o4-mini"));
        assert!(is_reasoning_model("gpt-5"));
        assert!(is_reasoning_model("claude-3.7-sonnet"));
        assert!(is_reasoning_model("claude-4-opus"));
        // Volcengine Doubao Seed series emits reasoning_content on every turn;
        // must be detected so max_tokens is skipped (otherwise visible output
        // can be truncated to one or two characters).
        assert!(is_reasoning_model("doubao-seed-evolving"));
        assert!(is_reasoning_model("doubao-seed-2-1-pro-260628"));
        assert!(is_reasoning_model("doubao-seed-2.0-code"));
        assert!(is_reasoning_model("DOUBAO-SEED-EVOLVING"));
    }

    #[test]
    fn is_reasoning_model_rejects_non_reasoning_models() {
        assert!(!is_reasoning_model("kimi-k3"));
        assert!(!is_reasoning_model("gpt-4o"));
        assert!(!is_reasoning_model("qwen-plus"));
        assert!(!is_reasoning_model("doubao-1-5-pro-256k"));
        assert!(!is_reasoning_model("claude-3-haiku"));
    }
}
