use serde_json::Value;

use super::{Exchange, Provider, Trace};

pub struct Anthropic;

impl Provider for Anthropic {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn prompt<'a>(&self, request: &'a Value) -> Option<&'a Value> {
        let messages = request.get("messages")?.as_array()?;
        messages.iter().rev().find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
    }

    fn read(&self, event: &Value, exchange: &mut Exchange) {
        let block = event.get("index").and_then(Value::as_u64).unwrap_or_default() as usize;
        // extract the text from the event using a pointer
        let text = |pointer: &str| event.pointer(pointer).and_then(Value::as_str).unwrap_or_default();
        // match on the type of the event and content block to determine how to update the exchange
        match (text("/type"), text("/content_block/type"), text("/delta/type")) {
            // if it is the start of a content block, determine the type and start the appropriate trace
            ("content_block_start", "thinking", _) => exchange.start(block, Trace::Thinking(String::new())),
            // if it is the start of a text content block, start an output trace
            ("content_block_start", "text", _) => exchange.start(block, Trace::Output(String::new())),
            // if it is the start of a tool use content block, start a tool trace
            ("content_block_start", "tool_use" | "server_tool_use" | "mcp_tool_use", _) => {
                let name = text("/content_block/name").to_string();
                exchange.start(block, Trace::Tool { name, input: String::new() });
            }
            // if it is a thinking delta, extend the current thinking trace
            ("content_block_delta", _, "thinking_delta") => exchange.extend(block, text("/delta/thinking")),
            // if it is a text delta, extend the current output trace
            ("content_block_delta", _, "text_delta") => exchange.extend(block, text("/delta/text")),
            // if it is an input JSON delta, extend the current tool trace with the partial JSON
            ("content_block_delta", _, "input_json_delta") => exchange.extend(block, text("/delta/partial_json")),
            // error is error
            ("error", _, _) => exchange.push(Trace::Error(event.get("error").unwrap_or(event).to_string())),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::Anthropic;
    use crate::providers::{Events, Exchange, Provider, Trace};

    #[test]
    fn reads_each_content_block_by_its_declared_type_in_stream_order() {
        let mut exchange = Exchange::default();
        for event in Events::default().feed(include_bytes!("../../fixtures/anthropic.sse")) {
            Anthropic.read(&event, &mut exchange);
        }
        assert_eq!(
            exchange.traces,
            [
                Trace::Thinking("The user wants the files listed.".into()),
                Trace::Output("I will list the files.".into()),
                Trace::Tool { name: "Bash".into(), input: r#"{"command": "ls -la"}"#.into() },
            ]
        );
    }

    #[test]
    fn ignores_blocks_it_does_not_know_and_keeps_errors_and_the_newest_message() {
        let mut exchange = Exchange::default();
        let result =
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "web_search_tool_result"}});
        Anthropic.read(&result, &mut exchange);
        Anthropic.read(
            &json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "x"}}),
            &mut exchange,
        );
        Anthropic.read(&json!({"type": "error", "error": {"type": "overloaded_error"}}), &mut exchange);
        assert_eq!(exchange.traces, [Trace::Error(r#"{"type":"overloaded_error"}"#.into())]);
        let messages = [json!({"role": "user", "content": "a"}), json!({"role": "user", "content": "b"})];
        let context = json!({"role": "system", "content": [{"type": "text", "text": "injected context"}]});
        let request = json!({"system": "s", "messages": [messages[0], messages[1], context]});
        assert_eq!(Anthropic.prompt(&request), Some(&messages[1]));
    }
}
