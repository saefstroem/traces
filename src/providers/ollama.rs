use serde_json::Value;

use super::{Exchange, Provider, Trace};

pub struct Ollama;

impl Provider for Ollama {
    fn name(&self) -> &'static str {
        "ollama"
    }

    /// Extracts the user prompt from the request, if available.
    fn prompt<'a>(&self, request: &'a Value) -> Option<&'a Value> {
        let newest = request.get("messages").and_then(Value::as_array).and_then(|messages| messages.last());
        newest.or_else(|| request.get("prompt"))
    }

    /// Reads an event and updates the exchange with any relevant traces.
    fn read(&self, event: &Value, exchange: &mut Exchange) {
        // Create pointer function to simplify extracting text from the event
        let text = |pointer: &str| event.pointer(pointer).and_then(Value::as_str).unwrap_or_default();
        if let Some(error) = event.get("error") {
            exchange.push(Trace::Error(error.to_string()));
        }
        // Extract and append the thinking and output traces from the event
        exchange.append(Trace::Thinking(format!("{}{}", text("/message/thinking"), text("/thinking"))));
        exchange.append(Trace::Output(format!("{}{}", text("/message/content"), text("/response"))));

        // Iterate over each tool call in the event and append it to the exchange as a tool trace
        for call in event.pointer("/message/tool_calls").and_then(Value::as_array).into_iter().flatten() {
            let name = call.pointer("/function/name").and_then(Value::as_str).unwrap_or_default().to_string();
            let input = call.pointer("/function/arguments").map(Value::to_string).unwrap_or_default();
            exchange.push(Trace::Tool { name, input });
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::Ollama;
    use crate::providers::{Events, Exchange, Provider, Trace};

    #[test]
    fn reads_thinking_text_and_tool_calls_from_the_chat_stream() {
        let mut exchange = Exchange::default();
        for event in Events::default().feed(include_bytes!("../../fixtures/ollama.ndjson")) {
            Ollama.read(&event, &mut exchange);
        }
        assert_eq!(
            exchange.traces,
            [
                Trace::Thinking("The user wants the files.".into()),
                Trace::Output("Listing them.".into()),
                Trace::Tool { name: "run_shell".into(), input: r#"{"command":"ls -la"}"#.into() },
            ]
        );
    }

    #[test]
    fn reads_the_generate_stream_errors_and_both_prompt_shapes() {
        let mut exchange = Exchange::default();
        Ollama.read(&json!({"thinking": "hmm", "response": "", "done": false}), &mut exchange);
        Ollama.read(&json!({"response": "Hi", "done": false}), &mut exchange);
        Ollama.read(&json!({"error": "model not found"}), &mut exchange);
        let expected =
            [Trace::Thinking("hmm".into()), Trace::Output("Hi".into()), Trace::Error(r#""model not found""#.into())];
        assert_eq!(exchange.traces, expected);
        let chat = json!({"messages": [{"role": "user", "content": "a"}, {"role": "tool", "content": "b"}]});
        assert_eq!(Ollama.prompt(&chat), Some(&json!({"role": "tool", "content": "b"})));
        assert_eq!(Ollama.prompt(&json!({"model": "m", "prompt": "why"})), Some(&json!("why")));
    }
}
