use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::to_vec;
use tokio::sync::Notify;

use crate::providers::Trace;

pub struct Store {
    /// The internal state of the store, protected by a mutex
    state: Mutex<State>,
    /// Maximum number of bytes allowed in memory before signaling
    max_bytes: usize,
    /// Notifies when the store reaches the memory limit
    pub full: Notify,
}

#[derive(Default)]
struct State {
    /// Maps prefix to the serialized Trace objects waiting to be uploaded
    waiting: HashMap<String, Vec<u8>>,
    /// Total number of bytes currently held in memory
    bytes: usize,
    /// Number of segments that have been created
    segments: u64,
}

pub struct Segment {
    /// Conversation identifier
    pub prefix: String,
    /// The full object key for this object
    pub key: String,
    /// All serialized Trace objects
    pub items: Vec<u8>,
}

impl Store {
    /// Creates a new store with the specified memory limit
    pub fn new(max_bytes: usize) -> Self {
        Self { state: Mutex::default(), max_bytes, full: Notify::new() }
    }

    /// Adds a set of traces to the store under the given prefix
    pub fn add(&self, prefix: String, traces: Vec<Trace>) {
        // Serializes the traces and adds them to the waiting list, updating the memory usage
        let items: Vec<Vec<u8>> = traces.iter().filter_map(|trace| to_vec(trace).ok()).collect();
        // Take the lock on the store's state to update it with the new traces
        let mut guard = self.lock();
        let state = &mut *guard;
        // Get or create the waiting list for this prefix
        let waiting = state.waiting.entry(prefix).or_default();
        let before = waiting.len();
        // Remember the length of the waiting list before adding new items
        // Add each serialized trace to the waiting list, separating with commas if necessary
        for item in items {
            if !waiting.is_empty() {
                waiting.push(b',');
            }
            // Append the serialized trace to the waiting list
            waiting.extend(item);
        }
        // Update the total number of bytes in memory after adding the new items
        state.bytes += waiting.len() - before;
        // If the memory usage exceeds the limit, notify any waiters
        if state.bytes >= self.max_bytes {
            self.full.notify_one();
        }
    }

    /// Takes all segments currently in the store, resetting the memory usage
    pub fn take(&self) -> Vec<Segment> {
        // Get the current timestamp in milliseconds to use in segment keys
        let millis = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
        // Lock the state and reset the memory usage
        let mut state = self.lock();
        // Drain the waiting list to prepare segments for return
        state.bytes = 0;
        // Collect the waiting list into a vector of (prefix, items) pairs for processing
        let waiting: Vec<(String, Vec<u8>)> = state.waiting.drain().collect();
        // Map each (prefix, items) pair into a Segment with a unique key based on the timestamp and segment counter
        waiting
            .into_iter()
            .map(|(prefix, items)| {
                state.segments += 1;
                let key = format!("{prefix}/{millis:013}-{:06}.json", state.segments);
                Segment { prefix, key, items }
            })
            .collect()
    }

    /// Restores a previously taken segment back into the store, prepending it to the waiting list for its prefix
    pub fn restore(&self, segment: Segment) {
        let Segment { prefix, mut items, .. } = segment;
        let mut state = self.lock();
        // Update the memory usage with the size of the restored items
        state.bytes += items.len();
        // Get or create the waiting list for this prefix
        let waiting = state.waiting.entry(prefix).or_default();
        if !waiting.is_empty() {
            // If there are already items in the waiting list, separate with a comma
            items.push(b',');
            items.append(waiting);
        }
        // Prepend the restored items to the waiting list
        *waiting = items;
    }

    /// Locks the store's state and returns a guard for accessing it
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::{Value, from_slice, json};
    use tokio::time::timeout;

    use super::Store;
    use crate::providers::Trace;

    fn array(items: &[u8]) -> Value {
        from_slice(&[b"[".as_slice(), items, b"]"].concat()).unwrap()
    }

    #[tokio::test]
    async fn signals_at_the_memory_limit_and_forgets_what_it_handed_over() {
        let store = Store::new(40);
        store.add("c1".into(), vec![Trace::Input(json!("ls")), Trace::Output("short".into())]);
        assert!(timeout(Duration::from_millis(50), store.full.notified()).await.is_err());
        store.add("c1".into(), vec![Trace::Tool { name: "Bash".into(), input: "{}".into() }]);
        timeout(Duration::from_secs(1), store.full.notified()).await.unwrap();
        let segments = store.take();
        assert_eq!(segments.len(), 1);
        let expected = json!([{"input": "ls"}, {"output": "short"}, {"tool": {"name": "Bash", "input": "{}"}}]);
        assert_eq!(array(&segments[0].items), expected);
        assert!(segments[0].key.starts_with("c1/") && segments[0].key.ends_with("-000001.json"), "{}", segments[0].key);
        assert!(store.take().is_empty());
    }

    #[test]
    fn puts_a_failed_segment_back_before_newer_traces() {
        let store = Store::new(1 << 20);
        store.add("c1".into(), vec![Trace::Output("old".into())]);
        let failed = store.take().pop().unwrap();
        store.add("c1".into(), vec![Trace::Output("new".into())]);
        store.restore(failed);
        let segment = store.take().pop().unwrap();
        assert_eq!(array(&segment.items), json!([{"output": "old"}, {"output": "new"}]));
        assert!(segment.key.ends_with("-000002.json"), "{}", segment.key);
    }
}
