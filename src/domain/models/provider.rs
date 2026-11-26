//! Supported external API-key providers.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Provider {
    OpenRouter,
    OpenAi,
    Anthropic,
    Gemini,
}

impl Provider {
    pub const ALL: [Self; 4] = [
        Self::OpenRouter,
        Self::OpenAi,
        Self::Anthropic,
        Self::Gemini,
    ];

    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "openrouter" => Some(Self::OpenRouter),
            "openai" => Some(Self::OpenAi),
            "anthropic" => Some(Self::Anthropic),
            "gemini" => Some(Self::Gemini),
            _ => None,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::OpenRouter => "openrouter",
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::Gemini => "gemini",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::OpenRouter => "OpenRouter",
            Self::OpenAi => "OpenAI",
            Self::Anthropic => "Anthropic",
            Self::Gemini => "Gemini",
        }
    }
}
