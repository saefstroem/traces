use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fmt::{Display, Formatter, Result};

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
/// Represents a single trace in the exchange, such as user input, model thinking, tool usage, output, or errors.
pub enum Trace {
    Input(Value),
    Thinking(String),
    Tool { name: String, input: String },
    Output(String),
    Error(String),
}

#[derive(Default)]
/// Represents an exchange of traces, maintaining the order and parts of the conversation.
/// This is from a single request/response with the LLM, so not an entire conversation.
pub struct Exchange {
    pub traces: Vec<Trace>,
    parts: HashMap<usize, usize>,
}

impl Exchange {
    pub fn push(&mut self, trace: Trace) {
        // Push a new trace to the end of the exchange's trace list.
        self.traces.push(trace);
    }

    /// Starts a new part in the exchange with the given trace, if the part hasn't been started yet.
    pub fn start(&mut self, part: usize, trace: Trace) {
        if let Entry::Vacant(slot) = self.parts.entry(part) {
            slot.insert(self.traces.len());
            self.traces.push(trace);
        }
    }

    /// Extends the trace of the specified part with additional text, if the part exists and is extendable.
    pub fn extend(&mut self, part: usize, more: &str) {
        let at = self.parts.get(&part).copied(); // Get the index of the part in the trace list, if it exists.
        
        // If the part exists and is one of the extendable trace types, append the additional text.
        if let Some(Trace::Thinking(text) | Trace::Output(text) | Trace::Tool { input: text, .. }) =
            at.and_then(|at| self.traces.get_mut(at))
        {
            // Append the additional text to the existing content of the trace.
            text.push_str(more);
        }
    }

    /// Appends a trace to the exchange, merging it with the last trace if they are of the same type and extendable.
    pub fn append(&mut self, trace: Trace) {
        // Attempt to merge the new trace with the last trace if they are of the same type and extendable.
        match (self.traces.last_mut(), trace) {
            // If the new trace is empty, do nothing.
            (_, Trace::Thinking(more) | Trace::Output(more)) if more.is_empty() => {}
            // If the new trace is not empty, attempt to merge it with the last trace if possible.
            (Some(Trace::Thinking(last)), Trace::Thinking(more)) | (Some(Trace::Output(last)), Trace::Output(more)) => {
                last.push_str(&more);
            }
            // If the new trace is a tool trace and the last trace is also a tool trace, add it to the exchange as a new entry.
            (_, trace) => self.traces.push(trace),
        }
    }
}

impl Display for Exchange {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        for trace in &self.traces {
            match trace {
                Trace::Thinking(text) | Trace::Output(text) if text.is_empty() => continue,
                Trace::Input(input) => writeln!(f, "[input]\n{input}")?,
                Trace::Thinking(text) => writeln!(f, "[thinking]\n{text}")?,
                Trace::Tool { name, input } => writeln!(f, "[tool: {name}]\n{input}")?,
                Trace::Output(text) => writeln!(f, "[output]\n{text}")?,
                Trace::Error(error) => writeln!(f, "[error]\n{error}")?,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Exchange, Trace};

    #[test]
    fn keeps_the_order_of_the_stream_and_fills_each_part_by_its_index() {
        let mut exchange = Exchange::default();
        exchange.push(Trace::Input(json!({"role": "user", "content": "ls"})));
        exchange.start(0, Trace::Thinking(String::new()));
        exchange.append(Trace::Output("I will ".into()));
        exchange.append(Trace::Output(String::new()));
        exchange.append(Trace::Output("run it.".into()));
        exchange.start(1, Trace::Tool { name: "Bash".into(), input: String::new() });
        exchange.start(1, Trace::Tool { name: "again".into(), input: String::new() });
        exchange.extend(1, r#"{"command":"ls"}"#);
        exchange.extend(7, "nobody started part 7");
        exchange.push(Trace::Error("overloaded".into()));
        assert_eq!(
            exchange.to_string(),
            "[input]\n{\"content\":\"ls\",\"role\":\"user\"}\n[output]\nI will run it.\n\
             [tool: Bash]\n{\"command\":\"ls\"}\n[error]\noverloaded\n"
        );
        assert_eq!(Exchange::default().to_string(), "");
    }
}
