use serde_json::Value;

use super::{Exchange, Provider, Trace};

pub struct GeminiInteractions;

impl Provider for GeminiInteractions {
    fn name(&self) -> &'static str {
        "gemini_interactions"
    }

    fn prompt<'a>(&self, request: &'a Value) -> Option<&'a Value> {
        let input = request.get("input")?;
        input.as_array().map_or(Some(input), |items| items.last())
    }

    fn read(&self, event: &Value, exchange: &mut Exchange) {
        let step = event.get("index").and_then(Value::as_u64).unwrap_or_default() as usize;
        let text = |pointer: &str| event.pointer(pointer).and_then(Value::as_str).unwrap_or_default();
        match (text("/event_type"), text("/step/type"), text("/delta/type")) {
            ("step.start", "thought", _) => exchange.start(step, Trace::Thinking(String::new())),
            ("step.start", "model_output", _) => exchange.start(step, Trace::Output(String::new())),
            ("step.start", kind, _) if kind.ends_with("_call") => {
                let name = Some(text("/step/name")).filter(|name| !name.is_empty()).unwrap_or(kind).to_string();
                let arguments = event
                    .pointer("/step/arguments")
                    .filter(|value| value.as_object().is_some_and(|map| !map.is_empty()));
                exchange.start(step, Trace::Tool { name, input: arguments.map(Value::to_string).unwrap_or_default() });
            }
            ("step.delta", _, "text") => exchange.extend(step, text("/delta/text")),
            ("step.delta", _, "thought_summary") => exchange.extend(step, text("/delta/content/text")),
            ("step.delta", _, "arguments_delta") => exchange.extend(step, text("/delta/arguments")),
            ("error", _, _) => exchange.push(Trace::Error(event.get("error").unwrap_or(event).to_string())),
            _ => {}
        }
        if text("/event_type") == "step.start" {
            for part in event.pointer("/step/content").and_then(Value::as_array).into_iter().flatten() {
                exchange.extend(step, part.get("text").and_then(Value::as_str).unwrap_or_default());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::GeminiInteractions;
    use crate::providers::{Events, Exchange, Provider, Trace};

    #[test]
    fn reads_each_step_by_its_declared_type_in_stream_order() {
        let mut exchange = Exchange::default();
        for event in Events::default().feed(include_bytes!("../../fixtures/gemini_interactions.sse")) {
            GeminiInteractions.read(&event, &mut exchange);
        }
        assert_eq!(
            exchange.traces,
            [
                Trace::Thinking("**Weather** The user wants the forecast.".into()),
                Trace::Output("I will check the weather.".into()),
                Trace::Tool { name: "get_weather".into(), input: r#"{"location": "San Francisco, CA"}"#.into() },
                Trace::Tool {
                    name: "google_search_call".into(),
                    input: r#"{"queries":["weather San Francisco"]}"#.into()
                },
            ]
        );
    }

    #[test]
    fn keeps_errors_and_the_newest_input_step_of_the_request() {
        let mut exchange = Exchange::default();
        GeminiInteractions.read(&json!({"event_type": "error", "error": {"code": 429}}), &mut exchange);
        assert_eq!(exchange.traces, [Trace::Error(r#"{"code":429}"#.into())]);
        let result = json!({"type": "function_result", "call_id": "c1", "name": "get_weather", "result": "sunny"});
        let request = json!({"previous_interaction_id": "i1", "input": [{"type": "user_input"}, result.clone()]});
        assert_eq!(GeminiInteractions.prompt(&request), Some(&result));
        assert_eq!(GeminiInteractions.prompt(&json!({"input": "hi"})), Some(&json!("hi")));
    }
}
