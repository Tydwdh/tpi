//! P5-03：provider model catalog——已知模型的 resolved capabilities/context limits。
//!
//! - [`ModelCapabilities`]：context window / max output / 已知限制；
//! - [`lookup`]：按 (provider, model) 解析；未知模型返回默认保守值（不 panic）；
//! - 用途：context policy（P5-04）的 cache key 与 compaction 预算的权威来源。
//!
//! 不做热切换（roadmap：直到真实需求）；catalog 是静态表。

/// 模型能力（resolved）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelCapabilities {
    /// context window（token）。
    pub context_window: u64,
    /// 推荐 max output tokens。
    pub max_output_tokens: u32,
    /// 是否支持 reasoning。
    pub supports_reasoning: bool,
}

/// 保守默认（未知模型）：足够大的 context + 中等的输出预算。
pub const DEFAULT_CAPABILITIES: ModelCapabilities = ModelCapabilities {
    context_window: 128_000,
    max_output_tokens: 8192,
    supports_reasoning: false,
};

/// 已知模型目录（按 provider 分组；名字匹配前缀，如 "gpt-4o" 匹配 "gpt-4o-mini"）。
pub const KNOWN_MODELS: &[(&str, &str, ModelCapabilities)] = &[
    (
        "openai",
        "gpt-4o",
        ModelCapabilities {
            context_window: 128_000,
            max_output_tokens: 16_384,
            supports_reasoning: false,
        },
    ),
    (
        "openai",
        "gpt-4o-mini",
        ModelCapabilities {
            context_window: 128_000,
            max_output_tokens: 16_384,
            supports_reasoning: false,
        },
    ),
    (
        "openai",
        "o3",
        ModelCapabilities {
            context_window: 200_000,
            max_output_tokens: 100_000,
            supports_reasoning: true,
        },
    ),
    (
        "openai",
        "o4-mini",
        ModelCapabilities {
            context_window: 200_000,
            max_output_tokens: 100_000,
            supports_reasoning: true,
        },
    ),
    (
        "anthropic",
        "claude-sonnet",
        ModelCapabilities {
            context_window: 200_000,
            max_output_tokens: 64_000,
            supports_reasoning: true,
        },
    ),
];

/// 按 (provider, model) 解析能力；未知模型返回保守默认（不 panic）。
pub fn lookup(provider: &str, model: &str) -> ModelCapabilities {
    let model_lower = model.to_ascii_lowercase();
    for (p, prefix, caps) in KNOWN_MODELS {
        if *p == provider && model_lower.starts_with(prefix) {
            return *caps;
        }
    }
    DEFAULT_CAPABILITIES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_model_resolves_capabilities() {
        let caps = lookup("openai", "gpt-4o-mini");
        assert_eq!(caps.context_window, 128_000);
        assert_eq!(caps.max_output_tokens, 16_384);
    }

    #[test]
    fn reasoning_model_flag() {
        assert!(lookup("openai", "o3").supports_reasoning);
        assert!(!lookup("openai", "gpt-4o").supports_reasoning);
    }

    #[test]
    fn unknown_model_falls_back_to_default() {
        assert_eq!(
            lookup("unknown-provider", "mystery-model"),
            DEFAULT_CAPABILITIES
        );
        assert_eq!(lookup("openai", "gpt-9999"), DEFAULT_CAPABILITIES);
    }
}
