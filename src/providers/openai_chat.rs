use serde_json::Value;

use super::{Exchange, Provider, Trace};

pub struct OpenAiChat;

impl Provider for OpenAiChat {
    fn name(&self) -> &'static str {
        "openai_chat"
    }

    /// Extracts the user prompt from the request, if available.
    fn prompt<'a>(&self, request: &'a Value) -> Option<&'a Value> {
        request.get("messages")?.as_array()?.last()
    }

    fn read(&self, event: &Value, exchange: &mut Exchange) {
        // Create a helper function to join text from arrays of objects by a specific key
        let joined = |list: Option<&Value>, key: &str| -> String {
            list.and_then(Value::as_array).into_iter().flatten().filter_map(|item| item.get(key)?.as_str()).collect()
        };

        // Check for errors in the event and append them to the exchange as error traces
        if let Some(error) = event.get("error") {
            exchange.push(Trace::Error(error.to_string()));
        }

        // Extract choices from the event and process each delta for reasoning, output, and tool calls
        let choices = event.get("choices").and_then(Value::as_array).into_iter().flatten();
        
        // Iterate over each delta in the choices to extract reasoning, output, and tool calls
        for delta in choices.filter_map(|choice| choice.get("delta")) {
            // Extract text and reasoning details from the delta
            let text = |key: &str| delta.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
            
            // Extract reasoning details from the delta
            let details = delta.get("reasoning_details");

            // Determine the reasoning text to use for the thinking trace
            // DeepSeek, Fireworks use reasoning_content whereas OpenRouter, Groq, vLLM, Ollama use reasoning
            let reasoning = [text("reasoning_content"), text("reasoning")].into_iter().find(|text| !text.is_empty());
            exchange.append(Trace::Thinking(
                // Use the reasoning text if available, otherwise fall back to joined reasoning details
                reasoning.unwrap_or_else(|| joined(details, "text") + &joined(details, "summary")),
            ));
            // Append refusal and content as output trace if they are strings
            exchange.append(Trace::Output(text("refusal") + &text("content"))); 
            
            // If content is an array then we go over the chunk
            for chunk in delta.get("content").and_then(Value::as_array).into_iter().flatten() {
                // Extract thinking and text from each chunk and append them to the exchange as traces
                exchange.append(Trace::Thinking(joined(chunk.get("thinking"), "text")));
                exchange
                    .append(Trace::Output(chunk.get("text").and_then(Value::as_str).unwrap_or_default().to_string()));
            }

            // Iterate over tool calls and append them to the exchange as traces
            for call in delta.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
                // Extract the index and function details for the tool call
                let index = call.get("index").and_then(Value::as_u64).unwrap_or_default() as usize;
                // Extract the function name and arguments for the tool call
                let function = |key: &str| call.get("function")?.get(key)?.as_str();
                
                // Extract the name of the tool from the function details
                let name = function("name").unwrap_or_default().to_string();
                // Start the tool call trace with the extracted name and an empty input
                exchange.start(index, Trace::Tool { name, input: String::new() });
                // Extend the tool call trace with the extracted arguments
                exchange.extend(index, function("arguments").unwrap_or_default());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::OpenAiChat;
    use crate::providers::{Events, Exchange, Provider, Trace};

    #[test]
    fn reads_reasoning_text_and_parallel_tool_calls_in_stream_order() {
        let mut exchange = Exchange::default();
        for event in Events::default().feed(include_bytes!("../../fixtures/openai_chat.sse")) {
            OpenAiChat.read(&event, &mut exchange);
        }
        let weather = |input: &str| Trace::Tool { name: "get_weather".into(), input: input.into() };
        assert_eq!(
            exchange.traces,
            [
                Trace::Thinking("Two cities, so two calls.".into()),
                Trace::Output("Checking both.".into()),
                weather(r#"{"location":"Paris"}"#),
                weather(r#"{"location":"Bogotá"}"#),
            ]
        );
    }

    #[test]
    fn reads_the_reasoning_shapes_of_compatible_vendors_once() {
        let mut exchange = Exchange::default();
        let deltas = [
            json!({"reasoning": "a"}),
            json!({"reasoning_details": [{"type": "reasoning.text", "text": "b"}, {"type": "reasoning.encrypted"}]}),
            json!({"reasoning": "c", "reasoning_details": [{"type": "reasoning.text", "text": "c"}]}),
            json!({"content": [{"type": "thinking", "thinking": [{"type": "text", "text": "d"}]}, {"type": "text", "text": "e"}]}),
            json!({"refusal": "f"}),
        ];
        for delta in deltas {
            OpenAiChat.read(&json!({"choices": [{"index": 0, "delta": delta}]}), &mut exchange);
        }
        assert_eq!(exchange.traces, [Trace::Thinking("abcd".into()), Trace::Output("ef".into())]);
    }

    #[test]
    fn keeps_an_error_chunk_and_the_newest_message_of_the_request() {
        let mut exchange = Exchange::default();
        OpenAiChat.read(&json!({"error": {"message": "rate limited"}}), &mut exchange);
        assert_eq!(exchange.traces, [Trace::Error(r#"{"message":"rate limited"}"#.into())]);
        let request = json!({"messages": [{"role": "system", "content": "s"}, {"role": "user", "content": "u"}]});
        assert_eq!(OpenAiChat.prompt(&request), Some(&json!({"role": "user", "content": "u"})));
    }
}
