use hyper::HeaderMap;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
/// Represents configuration for where a conversation or session IDs can be found. I.e. 
/// in which part of the JSON body or in which header and which headers to skip if the request has those headers
pub struct Prefix {
    headers: Vec<String>,
    body: Vec<String>,
    skip: Vec<(String, String)>,
}

impl Prefix {
    pub fn key(&self, provider: &str, path: &str, headers: &HeaderMap, request: &Value) -> Option<String> {
        // Check if any of the headers match the skip list and return None if so.
        let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
        
        // Check if the request should be skipped based on the headers and skip list.
        if self.skip.iter().any(|(name, value)| header(name) == Some(value.as_str())) {
            return None;
        }

        // Extract the model name from the request body or the path.
        let path_segment =
            |marker: &str| path.split_once(marker)?.1.split([':', '/']).next().filter(|segment| !segment.is_empty());
        let model = request
            .get("model")
            .and_then(Value::as_str)
            .map(|name| name.strip_prefix("models/").unwrap_or(name))
            .filter(|name| !name.is_empty())
            .or_else(|| path_segment("/models/"))
            .or_else(|| path_segment("/deployments/"))
            .unwrap_or("unknown");

        // Extract the conversation ID from the request body.
        // by going over the pointers that we specified in the conversation configuration
        let in_body = || self.body.iter().find_map(|pointer| request.pointer(pointer)?.as_str());
        
        // Extract the conversation ID from the headers or the body, falling back to "unknown" if not found.
        let id = self.headers.iter().find_map(|name| header(name)).or_else(in_body).unwrap_or("unknown");
        
        // Sanitize the provider, model, and conversation ID to ensure they only contain safe characters.
        let sanitize = |text: &str| -> String {
            text.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '_' }).collect()
        };
        // Return the formatted key as "provider/model/conversation_id".
        Some(format!("{}/{}/{}", sanitize(provider), sanitize(model), sanitize(id)))
    }
}

#[cfg(test)]
mod tests {
    use hyper::HeaderMap;
    use serde_json::json;

    use crate::config::Config;

    #[test]
    fn builds_provider_model_and_conversation_prefix_and_skips_side_requests() {
        let prefix = Config::load().unwrap().prefix;
        let headers = |pairs: &[(&'static str, &'static str)]| -> HeaderMap {
            pairs.iter().map(|(name, value)| (name.parse().unwrap(), value.parse().unwrap())).collect()
        };
        let key = |provider: &str, path: &str, pairs: &[(&'static str, &'static str)], body| {
            prefix.key(provider, path, &headers(pairs), &body)
        };
        let claude = [("x-claude-code-session-id", "s-1"), ("x-claude-code-request-class", "main")];
        assert_eq!(
            key("anthropic", "/v1/messages", &claude, json!({"model": "claude-sonnet-4-6"})).as_deref(),
            Some("anthropic/claude-sonnet-4-6/s-1")
        );
        let subagent = [("x-claude-code-session-id", "s-1"), ("x-claude-code-agent-id", "a-2")];
        assert_eq!(
            key("anthropic", "/v1/projects/p/models/claude-opus-4-7:streamRawPredict", &subagent, json!({})).as_deref(),
            Some("anthropic/claude-opus-4-7/a-2")
        );
        let title = [("x-claude-code-session-id", "s-1"), ("x-claude-code-request-class", "auxiliary")];
        assert_eq!(key("anthropic", "/v1/messages", &title, json!({})), None);
        assert_eq!(
            key(
                "gemini",
                "/v1internal:streamGenerateContent",
                &[],
                json!({"model": "models/gemini-2.5-pro", "request": {"session_id": "g-3"}})
            )
            .as_deref(),
            Some("gemini/gemini-2.5-pro/g-3")
        );
        assert_eq!(
            key(
                "gemini",
                "/v1beta/models/gemini-2.5-flash:streamGenerateContent",
                &[],
                json!({"request": {"sessionId": "ag-4"}})
            )
            .as_deref(),
            Some("gemini/gemini-2.5-flash/ag-4")
        );
        assert_eq!(
            key("openai_chat", "/openai/deployments/gpt-4o/chat/completions", &[], json!({"sessionId": "ag-5"}))
                .as_deref(),
            Some("openai_chat/gpt-4o/ag-5")
        );
        assert_eq!(
            key("openai_responses", "/v1/responses", &[("thread-id", "../../etc/x")], json!({"model": "openai/gpt-5"}))
                .as_deref(),
            Some("openai_responses/openai_gpt-5/.._.._etc_x")
        );
        assert_eq!(
            key("ollama", "/api/chat", &[], json!({"model": "qwen3:32b"})).as_deref(),
            Some("ollama/qwen3_32b/unknown")
        );
        assert_eq!(
            key("openai_chat", "/v1/chat/completions", &[], json!({"messages": []})).as_deref(),
            Some("openai_chat/unknown/unknown")
        );
    }
}
