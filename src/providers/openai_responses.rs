use serde_json::Value;

use super::{Exchange, Provider, Trace};

pub struct OpenAiResponses;

impl Provider for OpenAiResponses {
    fn name(&self) -> &'static str {
        "openai_responses"
    }

    /// Extracts the user input from the request, if available. If the input is an array, returns the last item.
    fn prompt<'a>(&self, request: &'a Value) -> Option<&'a Value> {
        let input = request.get("input")?;
        input.as_array().map_or(Some(input), |items| items.last())
    }

    /// Reads an event from the OpenAI Responses stream and updates the exchange with the appropriate traces.
    fn read(&self, event: &Value, exchange: &mut Exchange) {
        // Extract the item index from the event, defaulting to 0 if not present
        let item = event.get("output_index").and_then(Value::as_u64).unwrap_or_default() as usize;
        // Helper function to extract text from the event using a JSON pointer, defaulting to an empty string if not found
        let text = |pointer: &str| event.pointer(pointer).and_then(Value::as_str).unwrap_or_default();
        // Helper function to create a tool trace with the given name and input
        let tool = |name: &str, input: String| Trace::Tool { name: name.to_string(), input };
        match (text("/type"), text("/item/type")) { // Match on the type of the event and the type of the item within the event
            // Handle the addition of new output items based on their type
            ("response.output_item.added", "reasoning") => exchange.start(item, Trace::Thinking(String::new())),
            ("response.output_item.added", "message") => exchange.start(item, Trace::Output(String::new())),
            ("response.output_item.added", "function_call" | "custom_tool_call" | "mcp_call") => {
                exchange.start(item, tool(text("/item/name"), String::new()));
            }
            // Handle the completion of output items based on their type
            ("response.output_item.done", kind) if kind.ends_with("_call") => {
                // Extract the relevant detail from the item for the tool call trace
                let detail =
                    ["action", "operation", "code", "queries"].into_iter().find_map(|key| event.get("item")?.get(key));
                // Convert the extracted detail to a string, defaulting to an empty string if not found
                exchange.start(item, tool(kind, detail.map(Value::to_string).unwrap_or_default()));
            }
            ("response.reasoning_summary_part.added", _) // Handle the addition of reasoning summary parts
                if event.get("summary_index").and_then(Value::as_u64).is_some_and(|part| part > 0) =>
            {
                // Append a separator for the reasoning summary part to the exchange
                exchange.extend(item, "\n\n");
            }
            ( // Handle delta updates for various response types
                "response.reasoning_summary_text.delta"
                | "response.reasoning_text.delta"
                | "response.output_text.delta"
                | "response.refusal.delta"
                | "response.function_call_arguments.delta"
                | "response.custom_tool_call_input.delta"
                | "response.mcp_call_arguments.delta",
                _,
            ) => exchange.extend(item, text("/delta")), // Extend the exchange with the delta text for the item
            ("error", _) => exchange.push(Trace::Error(event.to_string())), // Handle generic error events by pushing them to the exchange as error traces
            ("response.failed", _) => { // Handle response failed events by pushing the error to the exchange as an error trace
                exchange.push(Trace::Error(event.pointer("/response/error").unwrap_or(event).to_string()));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::OpenAiResponses;
    use crate::providers::{Events, Exchange, Provider, Trace};

    #[test]
    fn reads_each_output_item_by_its_declared_type_in_stream_order() {
        let mut exchange = Exchange::default();
        for event in Events::default().feed(include_bytes!("../../fixtures/openai_responses.sse")) {
            OpenAiResponses.read(&event, &mut exchange);
        }
        let shell = Trace::Tool { name: "shell".into(), input: r#"{"command":["ls","-la"]}"#.into() };
        let hosted = r#"{"commands":["git status"],"timeout_ms":10000}"#;
        assert_eq!(
            exchange.traces,
            [
                Trace::Thinking("**Listing files** The user wants a listing.\n\n**Checking** It is safe.".into()),
                Trace::Output("Running it now.".into()),
                shell,
                Trace::Tool { name: "shell_call".into(), input: hosted.into() },
            ]
        );
    }

    #[test]
    fn keeps_failures_and_the_newest_input_item_of_the_request() {
        let mut exchange = Exchange::default();
        let failed = json!({"type": "response.failed", "response": {"error": {"code": "server_error"}}});
        OpenAiResponses.read(&failed, &mut exchange);
        assert_eq!(exchange.traces, [Trace::Error(r#"{"code":"server_error"}"#.into())]);
        let items =
            json!({"input": [{"role": "user", "content": "a"}, {"type": "function_call_output", "output": "b"}]});
        assert_eq!(OpenAiResponses.prompt(&items), Some(&json!({"type": "function_call_output", "output": "b"})));
        assert_eq!(OpenAiResponses.prompt(&json!({"input": "hi"})), Some(&json!("hi")));
    }
}
