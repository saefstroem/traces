use serde_json::{Value, from_slice};

#[derive(Default)]
pub struct Events {
    pending: Vec<u8>,
}

impl Events {
    /// Feeds a chunk of bytes into the event parser and returns any complete events.
    /// Events can be incomplete as they come from a TCP stream
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<Value> {
        // append the incoming chunk to the pending buffer
        self.pending.extend_from_slice(chunk);

        // prepare a DS for the events that are ready to be collected
        let mut events = Vec::new();
        // process each line ending with a newline character
        while let Some(end) = self.pending.iter().position(|&byte| byte == b'\n') {
            // get the line up to and including the newline character
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            // attempt to extract text that hopefully is JSON
            let json = line.strip_prefix(b"data:").or(line.starts_with(b"{").then_some(line.as_slice()));
            // try to parse the line as JSON and add it to the events if successful
            if let Some(event) = json.and_then(|json| from_slice(json).ok()) {
                events.push(event);
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::Events;

    #[test]
    fn yields_each_data_line_once_for_any_split_of_the_stream() {
        let stream =
            "event: delta\r\ndata: {\"text\":\"hé\"}\r\n\r\n: keep-alive\n\ndata:{\"n\":2}\n\ndata: [DONE]\n\n";
        let mut sse = Events::default();
        let events: Vec<_> = stream.as_bytes().iter().flat_map(|byte| sse.feed(&[*byte])).collect();
        assert_eq!(events, [json!({"text": "hé"}), json!({"n": 2})]);
        assert_eq!(Events::default().feed(stream.as_bytes()), events);
    }

    #[test]
    fn reads_newline_delimited_json_lines_as_events() {
        let stream = b"{\"message\":{\"content\":\"a\"},\"done\":false}\n{\"done\":true}\n";
        assert_eq!(
            Events::default().feed(stream),
            [json!({"message": {"content": "a"}, "done": false}), json!({"done": true})]
        );
    }
}
