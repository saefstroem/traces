use serde_json::Value;

use super::{Exchange, Provider, Trace};

pub struct Gemini;

impl Provider for Gemini {
    fn name(&self) -> &'static str {
        "gemini"
    }

    /// Extracts the initial prompt from the request, if available.
    fn prompt<'a>(&self, request: &'a Value) -> Option<&'a Value> {
        let contents = request.get("request").unwrap_or(request).get("contents")?;
        contents.as_array().map_or(Some(contents), |items| items.last())
    }

    /// Reads an event and updates the exchange with any relevant traces.
    fn read(&self, event: &Value, exchange: &mut Exchange) {
        // unwrap the response object if present
        let event = event.get("response").unwrap_or(event);
        // check for an error field in the event and record it if present
        if let Some(error) = event.get("error") {
            exchange.push(Trace::Error(error.to_string()));
        }

        // extract the candidates array from the event, if present
        let candidates = event.get("candidates").and_then(Value::as_array).into_iter().flatten();
        // extract the parts array from each candidate's content, if present
        let parts = candidates.filter_map(|candidate| candidate.pointer("/content/parts")?.as_array()).flatten();
        
        // iterate over each part and handle function calls, text, and thoughts accordingly
        for part in parts {
            if let Some(call) = part.get("functionCall") {
                // extract the function call name and arguments
                let name = call.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
                // push the function call trace to the exchange
                exchange.push(Trace::Tool { name, input: call.get("args").map(Value::to_string).unwrap_or_default() });
            } else if let Some(text) = part.get("text").and_then(Value::as_str) {
                // extract the text and determine if it represents a thought
                let text = text.to_string();
                // check if the part represents a thought
                let thought = part.get("thought").and_then(Value::as_bool) == Some(true);
                // append the appropriate trace to the exchange based on whether it's a thought or output
                exchange.append(if thought { Trace::Thinking(text) } else { Trace::Output(text) });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::Gemini;
    use crate::providers::{Events, Exchange, Provider, Trace};

    #[test]
    fn reads_thoughts_text_and_function_calls_in_stream_order() {
        let mut exchange = Exchange::default();
        for event in Events::default().feed(include_bytes!("../../fixtures/gemini.sse")) {
            Gemini.read(&event, &mut exchange);
        }
        assert_eq!(
            exchange.traces,
            [
                Trace::Thinking("**Planning** I need the file list.".into()),
                Trace::Output("Listing the files.".into()),
                Trace::Tool { name: "run_shell_command".into(), input: r#"{"command":"ls -la"}"#.into() },
            ]
        );
    }

    #[test]
    fn unwraps_code_assist_messages_and_keeps_errors() {
        let mut exchange = Exchange::default();
        let wrapped = json!({"response": {"candidates": [{"content": {"role": "model", "parts": [{"text": "ok"}]}}]}});
        Gemini.read(&wrapped, &mut exchange);
        Gemini.read(&json!({"error": {"code": 429}}), &mut exchange);
        assert_eq!(exchange.traces, [Trace::Output("ok".into()), Trace::Error(r#"{"code":429}"#.into())]);
        let request = json!({"model": "m", "request": {"contents": [{"role": "user", "parts": [{"text": "hi"}]}]}});
        assert_eq!(Gemini.prompt(&request), Some(&json!({"role": "user", "parts": [{"text": "hi"}]})));
        let single = json!({"contents": {"role": "user", "parts": {"text": "hi"}}});
        assert_eq!(Gemini.prompt(&single), Some(&json!({"role": "user", "parts": {"text": "hi"}})));
    }
}
