//! Text requests to the providers' documented API-key endpoints.

use crate::domain::models::provider::Provider;
use reqwest::blocking::{Client, RequestBuilder};
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
pub struct ProviderError;

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("provider request failed or returned an invalid response")
    }
}

impl std::error::Error for ProviderError {}

pub struct ProviderClient {
    http: Client,
}

fn request_spec(
    provider: Provider,
    model: &str,
    prompt: &str,
) -> Result<(String, Value), ProviderError> {
    if model.trim().is_empty() || model.contains(['\r', '\n']) {
        return Err(ProviderError);
    }
    if provider == Provider::Gemini
        && !model
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_'))
    {
        return Err(ProviderError);
    }
    Ok(match provider {
        Provider::OpenRouter => (
            "https://openrouter.ai/api/v1/chat/completions".to_owned(),
            json!({"model": model, "messages": [{"role": "user", "content": prompt}]}),
        ),
        Provider::OpenAi => (
            "https://api.openai.com/v1/chat/completions".to_owned(),
            json!({"model": model, "messages": [{"role": "user", "content": prompt}]}),
        ),
        Provider::Anthropic => (
            "https://api.anthropic.com/v1/messages".to_owned(),
            json!({"model": model, "max_tokens": 4096, "messages": [{"role": "user", "content": prompt}]}),
        ),
        Provider::Gemini => (
            format!(
                "https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent"
            ),
            json!({"contents": [{"role": "user", "parts": [{"text": prompt}]}]}),
        ),
    })
}

fn response_text(provider: Provider, response: &Value) -> Option<String> {
    match provider {
        Provider::OpenRouter | Provider::OpenAi => response["choices"][0]["message"]["content"]
            .as_str()
            .map(str::to_owned),
        Provider::Anthropic => response["content"]
            .as_array()
            .and_then(|blocks| blocks.iter().find_map(|block| block["text"].as_str()))
            .map(str::to_owned),
        Provider::Gemini => response["candidates"][0]["content"]["parts"]
            .as_array()
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|part| part["text"].as_str())
                    .collect::<String>()
            })
            .filter(|text: &String| !text.is_empty()),
    }
}

fn authorize(request: RequestBuilder, provider: Provider, key: &str) -> RequestBuilder {
    match provider {
        Provider::OpenRouter | Provider::OpenAi => request.bearer_auth(key),
        Provider::Anthropic => request
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01"),
        Provider::Gemini => request.header("x-goog-api-key", key),
    }
}

impl ProviderClient {
    pub fn new() -> Result<Self, ProviderError> {
        let http = Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|_| ProviderError)?;
        Ok(Self { http })
    }

    pub fn complete(
        &self,
        provider: Provider,
        key: &str,
        model: &str,
        prompt: &str,
    ) -> Result<String, ProviderError> {
        let (url, body) = request_spec(provider, model, prompt)?;
        let request = authorize(self.http.post(url).json(&body), provider, key);
        let response: Value = request
            .send()
            .map_err(|_| ProviderError)?
            .error_for_status()
            .map_err(|_| ProviderError)?
            .json()
            .map_err(|_| ProviderError)?;
        response_text(provider, &response).ok_or(ProviderError)
    }
}

#[cfg(test)]
mod tests {
    use super::{authorize, request_spec, response_text};
    use crate::domain::models::provider::Provider;
    use reqwest::blocking::Client;
    use serde_json::json;

    #[test]
    fn each_provider_uses_its_documented_endpoint_and_input_shape() {
        let prompt = "hello";
        let model = "test-model";
        let cases = [
            (
                Provider::OpenRouter,
                "https://openrouter.ai/api/v1/chat/completions",
                json!({"model": model, "messages": [{"role": "user", "content": prompt}]}),
            ),
            (
                Provider::OpenAi,
                "https://api.openai.com/v1/chat/completions",
                json!({"model": model, "messages": [{"role": "user", "content": prompt}]}),
            ),
            (
                Provider::Anthropic,
                "https://api.anthropic.com/v1/messages",
                json!({"model": model, "max_tokens": 4096, "messages": [{"role": "user", "content": prompt}]}),
            ),
            (
                Provider::Gemini,
                "https://generativelanguage.googleapis.com/v1beta/models/test-model:generateContent",
                json!({"contents": [{"role": "user", "parts": [{"text": prompt}]}]}),
            ),
        ];
        for (provider, expected_url, expected_body) in cases {
            let (url, body) = request_spec(provider, model, prompt).unwrap();
            assert_eq!(url, expected_url);
            assert_eq!(body, expected_body);
        }
    }

    #[test]
    fn each_provider_response_shape_returns_only_text() {
        let responses = [
            (
                Provider::OpenRouter,
                json!({"choices":[{"message":{"content":"answer"}}]}),
            ),
            (
                Provider::OpenAi,
                json!({"choices":[{"message":{"content":"answer"}}]}),
            ),
            (
                Provider::Anthropic,
                json!({"content":[{"type":"text","text":"answer"}]}),
            ),
            (
                Provider::Gemini,
                json!({"candidates":[{"content":{"parts":[{"text":"answer"},{"text":"!"}]}}]}),
            ),
        ];
        for (provider, response) in responses {
            let text = response_text(provider, &response).unwrap();
            assert_eq!(
                text,
                if provider == Provider::Gemini {
                    "answer!"
                } else {
                    "answer"
                }
            );
        }
        assert!(response_text(Provider::Anthropic, &json!({"content":[]})).is_none());
    }

    #[test]
    fn gemini_model_cannot_change_the_request_target() {
        assert!(request_spec(Provider::Gemini, "model?x=y", "hello").is_err());
        assert!(request_spec(Provider::OpenRouter, "org/model", "hello").is_ok());
    }

    #[test]
    fn each_provider_uses_its_documented_api_key_header() {
        let cases = [
            (
                Provider::OpenRouter,
                "authorization",
                Some("Bearer test-key"),
                None,
            ),
            (
                Provider::OpenAi,
                "authorization",
                Some("Bearer test-key"),
                None,
            ),
            (
                Provider::Anthropic,
                "x-api-key",
                Some("test-key"),
                Some("2023-06-01"),
            ),
            (Provider::Gemini, "x-goog-api-key", Some("test-key"), None),
        ];
        for (provider, name, expected, version) in cases {
            let request = authorize(
                Client::new().post("https://example.invalid"),
                provider,
                "test-key",
            )
            .build()
            .unwrap();
            assert_eq!(
                request.headers().get(name).unwrap().to_str().unwrap(),
                expected.unwrap()
            );
            if let Some(version) = version {
                assert_eq!(request.headers()["anthropic-version"], version);
            }
            if provider == Provider::Gemini {
                assert!(request.headers().get("authorization").is_none());
            }
        }
    }
}
