//! Text requests to the providers' documented API-key endpoints.

use crate::domain::models::provider::Provider;
use reqwest::{
    blocking::{Client, RequestBuilder, Response},
    redirect::Policy,
};
use serde_json::{Value, json};
use std::{io::Read, time::Duration};

const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;

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
    #[cfg(test)]
    test_base: Option<String>,
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
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|block| block["type"] == "text")
                    .filter_map(|block| block["text"].as_str())
                    .collect::<String>()
            })
            .filter(|text| !text.is_empty()),
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
            .header("anthropic-version", "2026-10-21"),
        Provider::Gemini => request.header("x-goog-api-key", key),
    }
}

impl ProviderClient {
    pub fn new() -> Result<Self, ProviderError> {
        let http = Client::builder()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|_| ProviderError)?;
        Ok(Self {
            http,
            #[cfg(test)]
            test_base: None,
        })
    }

    #[cfg(test)]
    fn with_test_base(base: &str) -> Result<Self, ProviderError> {
        let mut client = Self::new()?;
        client.test_base = Some(base.trim_end_matches('/').to_owned());
        Ok(client)
    }

    pub fn complete(
        &self,
        provider: Provider,
        key: &str,
        model: &str,
        prompt: &str,
    ) -> Result<String, ProviderError> {
        let (url, body) = request_spec(provider, model, prompt)?;
        #[cfg(test)]
        let url = if let Some(base) = &self.test_base {
            let path = reqwest::Url::parse(&url).map_err(|_| ProviderError)?;
            format!("{base}{}", path.path())
        } else {
            url
        };
        let request = authorize(self.http.post(url).json(&body), provider, key);
        let response = request.send().map_err(|_| ProviderError)?;
        let response = response.error_for_status().map_err(|_| ProviderError)?;
        let response = bounded_json(response)?;
        response_text(provider, &response).ok_or(ProviderError)
    }
}

fn bounded_json(response: Response) -> Result<Value, ProviderError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES)
    {
        return Err(ProviderError);
    }
    let mut bytes = Vec::new();
    response
        .take(MAX_RESPONSE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ProviderError)?;
    if bytes.len() as u64 > MAX_RESPONSE_BYTES {
        return Err(ProviderError);
    }
    serde_json::from_slice(&bytes).map_err(|_| ProviderError)
}

#[cfg(test)]
mod tests {
    use super::{MAX_RESPONSE_BYTES, ProviderClient, authorize, request_spec, response_text};
    use crate::domain::models::provider::Provider;
    use reqwest::blocking::Client;
    use serde_json::json;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    fn mock_server(status: u16, body: Vec<u8>) -> (ProviderClient, thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
                assert!(request.len() < 65_536);
            }
            let headers = String::from_utf8(request.clone()).unwrap();
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            let old_len = request.len();
            request.resize(old_len + length, 0);
            stream.read_exact(&mut request[old_len..]).unwrap();
            write!(
                stream,
                "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            let _ = stream.write_all(&body);
            request
        });
        (
            ProviderClient::with_test_base(&format!("http://{address}")).unwrap(),
            worker,
        )
    }

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
        assert_eq!(
            response_text(
                Provider::Anthropic,
                &json!({"content":[{"type":"text","text":"first"},{"type":"tool_use","id":"x"},{"type":"text","text":"second"}]})
            )
            .as_deref(),
            Some("firstsecond")
        );
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
                Some("2026-10-21"),
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

    #[test]
    fn local_transport_sends_each_provider_request_and_reads_real_responses() {
        let cases = [
            (
                Provider::OpenRouter,
                "/api/v1/chat/completions",
                "authorization: bearer fixture-key",
                json!({"choices":[{"message":{"content":"openrouter answer"}}]}),
                "openrouter answer",
            ),
            (
                Provider::OpenAi,
                "/v1/chat/completions",
                "authorization: bearer fixture-key",
                json!({"choices":[{"message":{"content":"openai answer"}}]}),
                "openai answer",
            ),
            (
                Provider::Anthropic,
                "/v1/messages",
                "x-api-key: fixture-key",
                json!({"content":[{"type":"text","text":"one"},{"type":"tool_use","id":"ignored"},{"type":"text","text":" two"}]}),
                "one two",
            ),
            (
                Provider::Gemini,
                "/v1beta/models/test-model:generateContent",
                "x-goog-api-key: fixture-key",
                json!({"candidates":[{"content":{"parts":[{"text":"gemini"},{"text":" answer"}]}}]}),
                "gemini answer",
            ),
        ];
        for (provider, path, auth, response, expected) in cases {
            let (client, worker) = mock_server(200, response.to_string().into_bytes());
            assert_eq!(
                client
                    .complete(provider, "fixture-key", "test-model", "hello")
                    .unwrap(),
                expected
            );
            let request = String::from_utf8(worker.join().unwrap()).unwrap();
            let (headers, body) = request.split_once("\r\n\r\n").unwrap();
            let headers_lower = headers.to_lowercase();
            assert!(
                headers_lower.starts_with(&format!("post {} http/1.1\r\n", path.to_lowercase())),
                "unexpected fixture request line: {headers_lower:?}"
            );
            assert!(headers_lower.contains(auth));
            assert!(headers_lower.contains("content-type: application/json"));
            let sent: serde_json::Value = serde_json::from_str(body).unwrap();
            let sent_prompt = if provider == Provider::Gemini {
                assert!(headers_lower.contains("/models/test-model:generatecontent"));
                sent["contents"][0]["parts"][0]["text"].as_str()
            } else {
                assert_eq!(sent["model"], "test-model");
                sent["messages"][0]["content"].as_str()
            };
            assert_eq!(sent_prompt, Some("hello"));
            if provider == Provider::Anthropic {
                assert!(headers_lower.contains("anthropic-version: 2026-10-21"));
            }
        }
    }

    #[test]
    fn local_transport_rejects_status_malformed_and_oversized_bodies_without_echoing_them() {
        for (status, body) in [
            (500, b"provider-secret-response".to_vec()),
            (200, b"not-json".to_vec()),
            (200, vec![b'x'; MAX_RESPONSE_BYTES as usize + 1]),
        ] {
            let (client, worker) = mock_server(status, body.clone());
            let error = client
                .complete(Provider::OpenAi, "fixture-key", "test-model", "hello")
                .unwrap_err();
            let message = error.to_string();
            assert!(!message.contains("provider-secret-response"));
            assert!(!message.contains("fixture-key"));
            worker.join().unwrap();
        }
    }
}
